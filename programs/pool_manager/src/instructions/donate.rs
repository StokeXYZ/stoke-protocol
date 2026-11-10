use anchor_lang::prelude::*;
use meridian_core::flags::*;
use meridian_core::hook::discriminator as disc;
use meridian_core::*;

use super::common::*;
use crate::engine;
use crate::events;
use crate::hooks::{add, apply_effects};
use crate::settle::{check_claims, settle_caller, settle_hook};
use crate::state::{Claims, Pool};

#[derive(Accounts)]
pub struct Donate<'info> {
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
    /// CHECK: currency0 source; the token program checks mint and authority.
    #[account(mut)]
    pub token0: Option<UncheckedAccount<'info>>,
    /// CHECK: currency1 source; the token program checks mint and authority.
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
    /// CHECK: matched against the pool.
    pub hooks: UncheckedAccount<'info>,
    /// CHECK: PDA, verified by invoke_signed.
    pub hook_caller: UncheckedAccount<'info>,
    #[account(mut)]
    pub hook_claims: Option<Account<'info, Claims>>,
    /// CHECK: PDA, verified by the vault transfer.
    pub authority: UncheckedAccount<'info>,
}

/// v4's PoolManager.donate: pays in-range LPs directly, plus settlement.
pub fn handle_donate<'info>(
    ctx: Context<'info, Donate<'info>>,
    amount0: u64,
    amount1: u64,
    hook_data: Vec<u8>,
    settle: Settle,
) -> Result<()> {
    let pool_key = ctx.accounts.pool.key();
    let mut st = ctx.accounts.pool.load()?.state;
    let a = &ctx.accounts;
    let infos = infos!(a);
    check_pool_accounts(&st, &infos)?;
    let sender = a.sender.key();
    let hooks = infos.hook_ctx(&st, &sender, ctx.remaining_accounts, true);
    let key = st.key();
    let mut hook_delta = BalanceDelta::ZERO;
    let args = DonateArgs { sender, key, amount0, amount1, hook_data };

    if hooks.on(BEFORE_DONATE) {
        write_pool(&a.pool, &st)?;
        let (effects, paid): (HookEffects, _) = hooks.call(encode_call(disc::BEFORE_DONATE, &args))?;
        hook_delta = add(hook_delta, paid)?;
        apply_effects(&mut st, &pool_key, &infos.ticks, ctx.remaining_accounts, effects, &mut hook_delta)?;
    }
    let delta = engine::donate(&mut st, amount0, amount1)?;
    emit!(events::Donate { pool: pool_key, sender, amount0, amount1 });
    if hooks.on(AFTER_DONATE) {
        write_pool(&a.pool, &st)?;
        let (effects, paid): (HookEffects, _) = hooks.call(encode_call(disc::AFTER_DONATE, &args))?;
        hook_delta = add(hook_delta, paid)?;
        apply_effects(&mut st, &pool_key, &infos.ticks, ctx.remaining_accounts, effects, &mut hook_delta)?;
    }

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
    settle_caller(&vaults, &mut st, delta, settle, &sender_info, [t0.as_ref(), t1.as_ref()], ctx.accounts.sender_claims.as_deref_mut())?;
    write_pool(&ctx.accounts.pool, &st)
}
