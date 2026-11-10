use anchor_lang::prelude::*;
use meridian_core::flags::*;
use meridian_core::fees::is_dynamic_fee;
use meridian_core::hook::discriminator as disc;
use meridian_core::*;

use super::common::*;
use crate::engine;
use crate::errors::PmError;
use crate::events;
use crate::hooks::{accrue_protocol_fee, add, apply_effects, sub};
use crate::settle::{check_claims, settle_caller, settle_hook};
use crate::state::{Claims, Pool};
use crate::ticks::TicksView;

#[derive(Accounts)]
pub struct Swap<'info> {
    pub sender: Signer<'info>,
    #[account(mut)]
    pub pool: AccountLoader<'info, Pool>,
    /// CHECK: matched against the pool.
    #[account(mut)]
    pub ticks: UncheckedAccount<'info>,
    /// CHECK: matched against the pool.
    #[account(mut)]
    pub vault0: UncheckedAccount<'info>,
    /// CHECK: matched against the pool.
    #[account(mut)]
    pub vault1: UncheckedAccount<'info>,
    /// CHECK: currency0 source/destination; the token program checks mint and authority.
    #[account(mut)]
    pub token0: Option<UncheckedAccount<'info>>,
    /// CHECK: currency1 source/destination; the token program checks mint and authority.
    #[account(mut)]
    pub token1: Option<UncheckedAccount<'info>>,
    #[account(mut)]
    pub sender_claims: Option<Account<'info, Claims>>,
    /// CHECK: matched against the pool.
    pub mint0: UncheckedAccount<'info>,
    /// CHECK: matched against the pool.
    pub mint1: UncheckedAccount<'info>,
    /// CHECK: matched against the pool.
    pub token_program0: UncheckedAccount<'info>,
    /// CHECK: matched against the pool.
    pub token_program1: UncheckedAccount<'info>,
    /// CHECK: matched against the pool (the system program when the pool has no hook).
    pub hooks: UncheckedAccount<'info>,
    /// CHECK: PDA ["hook-caller", pool]; invoke_signed fails for any other account.
    pub hook_caller: UncheckedAccount<'info>,
    #[account(mut)]
    pub hook_claims: Option<Account<'info, Claims>>,
    /// CHECK: PDA ["authority"]; the vault transfer fails for any other account.
    pub authority: UncheckedAccount<'info>,
}

/// v4's PoolManager.swap, plus settlement and a slippage bound (`amount_limit`: the minimum
/// output for exact input, the maximum input for exact output).
pub fn handle_swap<'info>(
    ctx: Context<'info, Swap<'info>>,
    params: SwapParams,
    hook_data: Vec<u8>,
    settle: Settle,
    amount_limit: u64,
) -> Result<()> {
    let pool_key = ctx.accounts.pool.key();
    let mut st = ctx.accounts.pool.load()?.state;
    require!(params.amount_specified != 0, PmError::SwapAmountCannotBeZero);
    let a = &ctx.accounts;
    let infos = infos!(a);
    check_pool_accounts(&st, &infos)?;

    let sender = a.sender.key();
    let hooks = infos.hook_ctx(&st, &sender, ctx.remaining_accounts, true);
    let key = st.key();
    let mut hook_delta = BalanceDelta::ZERO;

    // beforeSwap
    let mut amount_to_swap = params.amount_specified;
    let mut before = BeforeSwapDelta::default();
    let mut fee_override = 0u32;
    if hooks.on(BEFORE_SWAP) {
        write_pool(&a.pool, &st)?;
        let (ret, paid): (BeforeSwapReturn, _) = hooks.call(encode_call(
            disc::BEFORE_SWAP,
            &BeforeSwapArgs { sender, key, params, hook_data: hook_data.clone() },
        ))?;
        hook_delta = add(hook_delta, paid)?;
        apply_effects(&mut st, &pool_key, &infos.ticks, ctx.remaining_accounts, ret.effects, &mut hook_delta)?;
        if is_dynamic_fee(st.fee) {
            fee_override = ret.lp_fee_override;
        }
        if has(st.hook_flags, BEFORE_SWAP_RETURNS_DELTA) {
            before = ret.delta;
            if before.specified != 0 {
                let exact_in = amount_to_swap < 0;
                amount_to_swap = amount_to_swap.checked_add(before.specified).ok_or(PmError::MathOverflow)?;
                require!(if exact_in { amount_to_swap <= 0 } else { amount_to_swap >= 0 }, PmError::HookDeltaExceedsSwapAmount);
            }
        }
    }

    // The pool's swap.
    let res = {
        let mut raw = infos.ticks.try_borrow_mut_data()?;
        let mut tv = TicksView::load(&mut raw, &pool_key)?;
        engine::swap(&mut st, &mut tv, params.zero_for_one, amount_to_swap, params.sqrt_price_limit, fee_override)?
    };
    accrue_protocol_fee(&mut st, params.zero_for_one, res.amount_to_protocol)?;
    let pool_delta = res.delta;

    // afterSwap
    let mut hook_specified = before.specified;
    let mut hook_unspecified = before.unspecified;
    if hooks.on(AFTER_SWAP) {
        write_pool(&a.pool, &st)?;
        let (ret, paid): (AfterSwapReturn, _) = hooks.call(encode_call(
            disc::AFTER_SWAP,
            &AfterSwapArgs { sender, key, params, delta: pool_delta, hook_data },
        ))?;
        hook_delta = add(hook_delta, paid)?;
        apply_effects(&mut st, &pool_key, &infos.ticks, ctx.remaining_accounts, ret.effects, &mut hook_delta)?;
        if has(st.hook_flags, AFTER_SWAP_RETURNS_DELTA) {
            hook_unspecified = hook_unspecified.checked_add(ret.unspecified_delta).ok_or(PmError::MathOverflow)?;
        }
    }
    let mut swapper = pool_delta;
    if hook_specified != 0 || hook_unspecified != 0 {
        // Specified currency is currency0 when (exact input) == zero_for_one.
        let hook_return = if (params.amount_specified < 0) == params.zero_for_one {
            BalanceDelta::new(hook_specified, hook_unspecified)
        } else {
            BalanceDelta::new(hook_unspecified, hook_specified)
        };
        swapper = sub(swapper, hook_return)?;
        hook_delta = add(hook_delta, hook_return)?;
        hook_specified = 0;
        hook_unspecified = 0;
    }
    let _ = (hook_specified, hook_unspecified);

    // Slippage bound on what the swapper actually pays/receives.
    let out_is_1 = params.zero_for_one;
    let (in_d, out_d) = if out_is_1 { (swapper.amount0, swapper.amount1) } else { (swapper.amount1, swapper.amount0) };
    if params.amount_specified < 0 {
        require!(out_d >= amount_limit as i128, PmError::TooLittleReceived);
    } else {
        require!(in_d >= -(amount_limit as i128), PmError::TooMuchRequested);
    }

    emit!(events::Swap {
        pool: pool_key,
        sender,
        amount0: pool_delta.amount0,
        amount1: pool_delta.amount1,
        sqrt_price: st.sqrt_price.get(),
        liquidity: st.liquidity.get(),
        tick: st.tick,
        fee: res.swap_fee,
        swapper_amount0: swapper.amount0,
        swapper_amount1: swapper.amount1,
    });

    // Settle.
    let vaults = infos.vaults(&st);
    if let Some(c) = ctx.accounts.sender_claims.as_deref() {
        check_claims(c, &pool_key, &sender)?;
    }
    if let Some(c) = ctx.accounts.hook_claims.as_deref() {
        check_claims(c, &pool_key, &st.hook_authority)?;
    }
    let sender_info = ctx.accounts.sender.to_account_info();
    let t0 = ctx.accounts.token0.as_ref().map(|t| t.to_account_info());
    let t1 = ctx.accounts.token1.as_ref().map(|t| t.to_account_info());
    // The hook settles first: what it owes must be covered by its claims before anyone is paid.
    settle_hook(&mut st, hook_delta, ctx.accounts.hook_claims.as_deref_mut())?;
    settle_caller(&vaults, &mut st, swapper, settle, &sender_info, [t0.as_ref(), t1.as_ref()], ctx.accounts.sender_claims.as_deref_mut())?;
    write_pool(&ctx.accounts.pool, &st)
}
