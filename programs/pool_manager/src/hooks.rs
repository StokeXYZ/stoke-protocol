//! Calling hooks and applying what they return.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::solana_program::program::{get_return_data, invoke_signed};
use meridian_core::flags::has;
use meridian_core::fees::{is_dynamic_fee, is_valid_lp_fee};
use meridian_core::{BalanceDelta, HookAction, HookEffects, PoolState, HOOK_CALLER_SEED};

use crate::engine::{self, PositionData};
use crate::errors::PmError;
use crate::events;
use crate::settle::vault_amount;
use crate::state::Position;
use crate::ticks::TicksView;

pub struct HookCtx<'a, 'info> {
    pub program: &'a AccountInfo<'info>,
    pub caller: &'a AccountInfo<'info>,
    pub pool: &'a AccountInfo<'info>,
    pub vaults: Option<[&'a AccountInfo<'info>; 2]>,
    pub remaining: &'a [AccountInfo<'info>],
    pub flags: u16,
    /// False when there's no hook, or when the hook itself is the sender (v4's noSelfCall).
    pub active: bool,
    pub caller_bump: u8,
}

impl<'a, 'info> HookCtx<'a, 'info> {
    pub fn on(&self, flag: u16) -> bool {
        self.active && has(self.flags, flag)
    }

    /// CPI into the hook. Returns its decoded response (default if it returned nothing) and
    /// whatever it paid into the vaults during the call.
    pub fn call<R: AnchorDeserialize + Default>(&self, data: Vec<u8>) -> Result<(R, BalanceDelta)> {
        let before = match self.vaults {
            Some([v0, v1]) => (vault_amount(v0)?, vault_amount(v1)?),
            None => (0, 0),
        };

        let mut metas = Vec::with_capacity(2 + self.remaining.len());
        metas.push(AccountMeta::new_readonly(*self.caller.key, true));
        metas.push(AccountMeta::new_readonly(*self.pool.key, false));
        let mut infos = Vec::with_capacity(3 + self.remaining.len());
        infos.push(self.caller.clone());
        infos.push(self.pool.clone());
        for a in self.remaining {
            // Never forward a signature: the hook sees every remaining account as a non-signer.
            metas.push(if a.is_writable { AccountMeta::new(*a.key, false) } else { AccountMeta::new_readonly(*a.key, false) });
            infos.push(a.clone());
        }
        infos.push(self.program.clone());
        let ix = Instruction { program_id: *self.program.key, accounts: metas, data };
        invoke_signed(&ix, &infos, &[&[HOOK_CALLER_SEED, self.pool.key.as_ref(), &[self.caller_bump]]])?;

        let paid = match self.vaults {
            Some([v0, v1]) => {
                let (a0, a1) = (vault_amount(v0)?, vault_amount(v1)?);
                require!(a0 >= before.0 && a1 >= before.1, PmError::VaultBalanceDecreased);
                BalanceDelta::new((a0 - before.0) as i128, (a1 - before.1) as i128)
            }
            None => BalanceDelta::ZERO,
        };
        let ret = match get_return_data() {
            Some((program, bytes)) if program == *self.program.key => {
                R::deserialize(&mut bytes.as_slice()).map_err(|_| error!(PmError::InvalidHookResponse))?
            }
            _ => R::default(),
        };
        Ok((ret, paid))
    }
}

pub fn add(a: BalanceDelta, b: BalanceDelta) -> Result<BalanceDelta> {
    a.checked_add(b).ok_or_else(|| error!(PmError::MathOverflow))
}

pub fn sub(a: BalanceDelta, b: BalanceDelta) -> Result<BalanceDelta> {
    a.checked_sub(b).ok_or_else(|| error!(PmError::MathOverflow))
}

fn load_position(info: &AccountInfo) -> Result<Position> {
    require!(info.owner == &crate::ID, PmError::PositionNotFound);
    let data = info.try_borrow_data()?;
    Position::try_deserialize(&mut &data[..])
}

fn store_position(info: &AccountInfo, pos: &Position) -> Result<()> {
    let mut data = info.try_borrow_mut_data()?;
    let mut w: &mut [u8] = &mut data;
    pos.try_serialize(&mut w)
}

/// Apply a callback's effects: a dynamic LP fee update, then queued actions run as the hook
/// (its own callbacks skipped). Action deltas accrue to `hook_delta`.
pub fn apply_effects<'info>(
    st: &mut PoolState,
    pool: &Pubkey,
    ticks: &AccountInfo<'info>,
    remaining: &[AccountInfo<'info>],
    effects: HookEffects,
    hook_delta: &mut BalanceDelta,
) -> Result<()> {
    if let Some(fee) = effects.lp_fee {
        require!(is_dynamic_fee(st.fee), PmError::UnauthorizedDynamicLPFeeUpdate);
        require!(is_valid_lp_fee(fee), PmError::LPFeeTooLarge);
        st.lp_fee = fee;
        emit!(events::LpFeeUpdated { pool: *pool, lp_fee: fee });
    }
    if effects.actions.is_empty() {
        return Ok(());
    }
    for action in effects.actions {
        match action {
            HookAction::ModifyLiquidity { position, liquidity_delta } => {
                let info = remaining.iter().find(|a| *a.key == position).ok_or(PmError::PositionNotFound)?;
                let mut pos = load_position(info)?;
                require!(pos.pool == *pool && pos.owner == st.hook_authority, PmError::NotPositionOwner);
                let mut data = PositionData {
                    liquidity: pos.liquidity,
                    fee_growth_inside0_last: pos.fee_growth_inside0_last,
                    fee_growth_inside1_last: pos.fee_growth_inside1_last,
                };
                let res = {
                    let mut raw = ticks.try_borrow_mut_data()?;
                    let mut tv = TicksView::load(&mut raw, pool)?;
                    engine::modify_liquidity(st, &mut tv, &mut data, pos.tick_lower, pos.tick_upper, liquidity_delta)?
                };
                pos.liquidity = data.liquidity;
                pos.fee_growth_inside0_last = data.fee_growth_inside0_last;
                pos.fee_growth_inside1_last = data.fee_growth_inside1_last;
                store_position(info, &pos)?;
                let delta = add(res.principal, res.fees)?;
                *hook_delta = add(*hook_delta, delta)?;
                emit!(events::ModifyLiquidity {
                    pool: *pool,
                    sender: st.hook_authority,
                    tick_lower: pos.tick_lower,
                    tick_upper: pos.tick_upper,
                    liquidity_delta,
                    salt: pos.salt,
                    amount0: delta.amount0,
                    amount1: delta.amount1,
                });
            }
            HookAction::Swap { params } => {
                require!(params.amount_specified != 0, PmError::SwapAmountCannotBeZero);
                let res = {
                    let mut raw = ticks.try_borrow_mut_data()?;
                    let mut tv = TicksView::load(&mut raw, pool)?;
                    engine::swap(st, &mut tv, params.zero_for_one, params.amount_specified, params.sqrt_price_limit, 0)?
                };
                accrue_protocol_fee(st, params.zero_for_one, res.amount_to_protocol)?;
                *hook_delta = add(*hook_delta, res.delta)?;
                emit!(events::Swap {
                    pool: *pool,
                    sender: st.hook_authority,
                    amount0: res.delta.amount0,
                    amount1: res.delta.amount1,
                    sqrt_price: st.sqrt_price.get(),
                    liquidity: st.liquidity.get(),
                    tick: st.tick,
                    fee: res.swap_fee,
                    swapper_amount0: res.delta.amount0,
                    swapper_amount1: res.delta.amount1,
                });
            }
            HookAction::Donate { amount0, amount1 } => {
                let d = engine::donate(st, amount0, amount1)?;
                *hook_delta = add(*hook_delta, d)?;
                emit!(events::Donate { pool: *pool, sender: st.hook_authority, amount0, amount1 });
            }
        }
    }
    Ok(())
}

pub fn accrue_protocol_fee(st: &mut PoolState, zero_for_one: bool, amount: u64) -> Result<()> {
    if amount == 0 {
        return Ok(());
    }
    if zero_for_one {
        st.protocol_fees0 = st.protocol_fees0.checked_add(amount).ok_or(PmError::MathOverflow)?;
    } else {
        st.protocol_fees1 = st.protocol_fees1.checked_add(amount).ok_or(PmError::MathOverflow)?;
    }
    Ok(())
}
