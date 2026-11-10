use anchor_lang::prelude::*;
use meridian_core::flags::*;
use meridian_core::hook::discriminator as disc;
use meridian_core::*;

use super::common::*;
use crate::engine::{self, PositionData};
use crate::errors::PmError;
use crate::events;
use crate::hooks::{add, apply_effects, sub};
use crate::settle::{check_claims, settle_caller, settle_hook};
use crate::state::{Claims, Pool, Position};
use crate::ticks::{self, TicksView};

#[derive(Accounts)]
#[instruction(params: ModifyLiquidityParams)]
pub struct ModifyLiquidity<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub owner: Signer<'info>,
    #[account(mut)]
    pub pool: AccountLoader<'info, Pool>,
    /// CHECK: matched against the pool.
    #[account(mut)]
    pub ticks: UncheckedAccount<'info>,
    #[account(
        init_if_needed,
        payer = payer,
        space = 8 + Position::INIT_SPACE,
        seeds = [POSITION_SEED, pool.key().as_ref(), owner.key().as_ref(), &params.tick_lower.to_le_bytes(), &params.tick_upper.to_le_bytes(), &params.salt],
        bump,
    )]
    pub position: Account<'info, Position>,
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
    pub owner_claims: Option<Account<'info, Claims>>,
    /// CHECK: matched against the pool.
    pub mint0: UncheckedAccount<'info>,
    /// CHECK: matched against the pool.
    pub mint1: UncheckedAccount<'info>,
    /// CHECK: matched against the pool.
    pub token_program0: UncheckedAccount<'info>,
    /// CHECK: matched against the pool.
    pub token_program1: UncheckedAccount<'info>,
    /// CHECK: matched against the pool.
    pub hooks: UncheckedAccount<'info>,
    /// CHECK: PDA, verified by invoke_signed.
    pub hook_caller: UncheckedAccount<'info>,
    #[account(mut)]
    pub hook_claims: Option<Account<'info, Claims>>,
    /// CHECK: PDA, verified by the vault transfer.
    pub authority: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// v4's PoolManager.modifyLiquidity (fees accrued are paid out with the principal), plus settlement.
pub fn handle_modify_liquidity<'info>(
    ctx: Context<'info, ModifyLiquidity<'info>>,
    params: ModifyLiquidityParams,
    hook_data: Vec<u8>,
    settle: Settle,
) -> Result<()> {
    let pool_key = ctx.accounts.pool.key();
    let mut st = ctx.accounts.pool.load()?.state;
    let a = &ctx.accounts;
    let infos = infos!(a);
    check_pool_accounts(&st, &infos)?;
    let owner = a.owner.key();
    let hooks = infos.hook_ctx(&st, &owner, ctx.remaining_accounts, true);
    let key = st.key();
    let adding = params.liquidity_delta > 0;
    let mut hook_delta = BalanceDelta::ZERO;

    if (adding && hooks.on(BEFORE_ADD_LIQUIDITY)) || (!adding && hooks.on(BEFORE_REMOVE_LIQUIDITY)) {
        write_pool(&a.pool, &st)?;
        let d = if adding { disc::BEFORE_ADD_LIQUIDITY } else { disc::BEFORE_REMOVE_LIQUIDITY };
        let (effects, paid): (HookEffects, _) =
            hooks.call(encode_call(d, &BeforeModifyLiquidityArgs { sender: owner, key, params, hook_data: hook_data.clone() }))?;
        hook_delta = add(hook_delta, paid)?;
        apply_effects(&mut st, &pool_key, &infos.ticks, ctx.remaining_accounts, effects, &mut hook_delta)?;
    }

    if adding {
        ticks::ensure_free(&infos.ticks, &a.payer.to_account_info(), &a.system_program.to_account_info(), 2)?;
    }
    let pos = &ctx.accounts.position;
    let mut data = PositionData {
        liquidity: pos.liquidity,
        fee_growth_inside0_last: pos.fee_growth_inside0_last,
        fee_growth_inside1_last: pos.fee_growth_inside1_last,
    };
    let res = {
        let mut raw = infos.ticks.try_borrow_mut_data()?;
        let mut tv = TicksView::load(&mut raw, &pool_key)?;
        engine::modify_liquidity(&mut st, &mut tv, &mut data, params.tick_lower, params.tick_upper, params.liquidity_delta)?
    };
    let mut caller = add(res.principal, res.fees)?;

    let after_flag = if adding { AFTER_ADD_LIQUIDITY } else { AFTER_REMOVE_LIQUIDITY };
    if hooks.on(after_flag) {
        write_pool(&ctx.accounts.pool, &st)?;
        let d = if adding { disc::AFTER_ADD_LIQUIDITY } else { disc::AFTER_REMOVE_LIQUIDITY };
        let (ret, paid): (AfterModifyLiquidityReturn, _) = hooks.call(encode_call(
            d,
            &AfterModifyLiquidityArgs { sender: owner, key, params, delta: caller, fees_accrued: res.fees, hook_data },
        ))?;
        hook_delta = add(hook_delta, paid)?;
        apply_effects(&mut st, &pool_key, &infos.ticks, ctx.remaining_accounts, ret.effects, &mut hook_delta)?;
        let returns = if adding { AFTER_ADD_LIQUIDITY_RETURNS_DELTA } else { AFTER_REMOVE_LIQUIDITY_RETURNS_DELTA };
        if has(st.hook_flags, returns) {
            caller = sub(caller, ret.delta)?;
            hook_delta = add(hook_delta, ret.delta)?;
        }
    }

    let pos = &mut ctx.accounts.position;
    if pos.pool == Pubkey::default() {
        pos.pool = pool_key;
        pos.owner = owner;
        pos.tick_lower = params.tick_lower;
        pos.tick_upper = params.tick_upper;
        pos.salt = params.salt;
        pos.bump = ctx.bumps.position;
    }
    pos.liquidity = data.liquidity;
    pos.fee_growth_inside0_last = data.fee_growth_inside0_last;
    pos.fee_growth_inside1_last = data.fee_growth_inside1_last;

    emit!(events::ModifyLiquidity {
        pool: pool_key,
        sender: owner,
        tick_lower: params.tick_lower,
        tick_upper: params.tick_upper,
        liquidity_delta: params.liquidity_delta,
        salt: params.salt,
        amount0: caller.amount0,
        amount1: caller.amount1,
    });

    let vaults = infos.vaults(&st);
    if let Some(c) = ctx.accounts.owner_claims.as_deref() {
        check_claims(c, &pool_key, &owner)?;
    }
    if let Some(c) = ctx.accounts.hook_claims.as_deref() {
        check_claims(c, &pool_key, &st.hook_authority)?;
    }
    let owner_info = ctx.accounts.owner.to_account_info();
    let t0 = ctx.accounts.token0.as_ref().map(|t| t.to_account_info());
    let t1 = ctx.accounts.token1.as_ref().map(|t| t.to_account_info());
    // The hook settles first: what it owes must be covered by its claims before anyone is paid.
    settle_hook(&mut st, hook_delta, ctx.accounts.hook_claims.as_deref_mut())?;
    settle_caller(&vaults, &mut st, caller, settle, &owner_info, [t0.as_ref(), t1.as_ref()], ctx.accounts.owner_claims.as_deref_mut())?;
    require!(st.sqrt_price.get() != 0, PmError::PoolNotInitialized);
    write_pool(&ctx.accounts.pool, &st)
}
