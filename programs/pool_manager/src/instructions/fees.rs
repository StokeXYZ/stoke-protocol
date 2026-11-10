use anchor_lang::prelude::*;
use meridian_core::fees::*;
use meridian_core::CONFIG_SEED;

use crate::errors::PmError;
use crate::events;
use crate::settle::Vaults;
use crate::state::{Config, Pool};

#[derive(Accounts)]
pub struct UpdateDynamicLpFee<'info> {
    /// The pool's hook authority PDA, signing through the hook program.
    pub hook_authority: Signer<'info>,
    #[account(mut)]
    pub pool: AccountLoader<'info, Pool>,
}

/// v4's updateDynamicLPFee: only a dynamic-fee pool's hook, at any time.
pub fn handle_update_dynamic_lp_fee(ctx: Context<UpdateDynamicLpFee>, lp_fee: u32) -> Result<()> {
    let mut pool = ctx.accounts.pool.load_mut()?;
    let st = &mut pool.state;
    require!(st.sqrt_price.get() != 0, PmError::PoolNotInitialized);
    require!(
        is_dynamic_fee(st.fee) && st.hooks != Pubkey::default() && ctx.accounts.hook_authority.key() == st.hook_authority,
        PmError::UnauthorizedDynamicLPFeeUpdate
    );
    require!(is_valid_lp_fee(lp_fee), PmError::LPFeeTooLarge);
    st.lp_fee = lp_fee;
    emit!(events::LpFeeUpdated { pool: ctx.accounts.pool.key(), lp_fee });
    Ok(())
}

#[derive(Accounts)]
pub struct SetProtocolFee<'info> {
    pub protocol_fee_controller: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump, has_one = protocol_fee_controller @ PmError::Unauthorized)]
    pub config: Account<'info, Config>,
    #[account(mut)]
    pub pool: AccountLoader<'info, Pool>,
}

/// v4's setProtocolFee.
pub fn handle_set_protocol_fee(ctx: Context<SetProtocolFee>, protocol_fee: u32) -> Result<()> {
    require!(is_valid_protocol_fee(protocol_fee), PmError::ProtocolFeeTooLarge);
    let mut pool = ctx.accounts.pool.load_mut()?;
    require!(pool.state.sqrt_price.get() != 0, PmError::PoolNotInitialized);
    pool.state.protocol_fee = protocol_fee;
    emit!(events::ProtocolFeeUpdated { pool: ctx.accounts.pool.key(), protocol_fee });
    Ok(())
}

#[derive(Accounts)]
pub struct CollectProtocolFees<'info> {
    pub protocol_fee_controller: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump, has_one = protocol_fee_controller @ PmError::Unauthorized)]
    pub config: Account<'info, Config>,
    #[account(mut)]
    pub pool: AccountLoader<'info, Pool>,
    /// CHECK: matched against the pool.
    #[account(mut)]
    pub vault0: UncheckedAccount<'info>,
    /// CHECK: matched against the pool.
    #[account(mut)]
    pub vault1: UncheckedAccount<'info>,
    /// CHECK: currency0 destination.
    #[account(mut)]
    pub recipient0: Option<UncheckedAccount<'info>>,
    /// CHECK: currency1 destination.
    #[account(mut)]
    pub recipient1: Option<UncheckedAccount<'info>>,
    /// CHECK: matched against the pool.
    pub mint0: UncheckedAccount<'info>,
    /// CHECK: matched against the pool.
    pub mint1: UncheckedAccount<'info>,
    /// CHECK: matched against the pool.
    pub token_program0: UncheckedAccount<'info>,
    /// CHECK: matched against the pool.
    pub token_program1: UncheckedAccount<'info>,
    /// CHECK: PDA, verified by the vault transfer.
    pub authority: UncheckedAccount<'info>,
}

/// v4's collectProtocolFees (0 = everything accrued).
pub fn handle_collect_protocol_fees(ctx: Context<CollectProtocolFees>, amount0: u64, amount1: u64) -> Result<()> {
    let a = &ctx.accounts;
    let mut st = a.pool.load()?.state;
    require!(
        a.vault0.key() == st.vault0 && a.vault1.key() == st.vault1 && a.mint0.key() == st.currency0 && a.mint1.key() == st.currency1
            && a.token_program0.key() == st.token_program0 && a.token_program1.key() == st.token_program1,
        PmError::WrongPoolAccount
    );
    let take0 = if amount0 == 0 { st.protocol_fees0 } else { amount0 };
    let take1 = if amount1 == 0 { st.protocol_fees1 } else { amount1 };
    st.protocol_fees0 = st.protocol_fees0.checked_sub(take0).ok_or(PmError::InsufficientClaims)?;
    st.protocol_fees1 = st.protocol_fees1.checked_sub(take1).ok_or(PmError::InsufficientClaims)?;
    let (v0, v1, m0, m1, p0, p1, auth) = (
        a.vault0.to_account_info(),
        a.vault1.to_account_info(),
        a.mint0.to_account_info(),
        a.mint1.to_account_info(),
        a.token_program0.to_account_info(),
        a.token_program1.to_account_info(),
        a.authority.to_account_info(),
    );
    let vaults = Vaults { vault: [&v0, &v1], mint: [&m0, &m1], token_program: [&p0, &p1], decimals: [st.decimals0, st.decimals1], authority: Some(&auth), authority_bump: st.authority_bump };
    if take0 > 0 {
        vaults.push(0, &a.recipient0.as_ref().ok_or(PmError::TokenAccountRequired)?.to_account_info(), take0)?;
    }
    if take1 > 0 {
        vaults.push(1, &a.recipient1.as_ref().ok_or(PmError::TokenAccountRequired)?.to_account_info(), take1)?;
    }
    ctx.accounts.pool.load_mut()?.state = st;
    Ok(())
}
