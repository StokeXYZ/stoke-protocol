//! Where the trading fee goes: the protocol's share (to the config's fee recipient), the
//! creator's share (claimed to the creator), the liquidity share (`deepen` adds it to the coin's
//! locked buy-side liquidity), and burns. Every crank here is permissionless and can only pay the
//! right accounts.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::invoke_signed;
use anchor_lang::{InstructionData, ToAccountMetas};
use anchor_spl::token_2022::spl_token_2022;
use meridian_core::math::*;
use meridian_core::*;
use meridian_pool_manager as pm;

use super::launch::FLOOR_SALT;
use crate::errors::LaunchError;
use crate::events::{Deepened, HolderRewardsWithdrawn};
use crate::pm_cpi::{self, ordered, PmWithdraw};
use crate::state::*;

/// Accounts for moving the hook's claims on one launch's pool to one destination.
#[derive(Accounts)]
pub struct HookTreasury<'info> {
    #[account(mut, seeds = [LAUNCH_SEED, launch.mint.as_ref()], bump = launch.bump)]
    pub launch: Box<Account<'info, Launch>>,
    /// CHECK: this program's signing PDA.
    #[account(seeds = [HOOK_AUTHORITY_SEED], bump)]
    pub hook_authority: UncheckedAccount<'info>,
    /// CHECK: address-checked.
    #[account(mut, address = launch.pool)]
    pub pool: UncheckedAccount<'info>,
    /// CHECK: the PoolManager checks it (its claims PDA for this hook).
    #[account(mut)]
    pub hook_claims: UncheckedAccount<'info>,
    /// CHECK: the PoolManager checks it.
    #[account(mut)]
    pub vault0: UncheckedAccount<'info>,
    /// CHECK: the PoolManager checks it.
    #[account(mut)]
    pub vault1: UncheckedAccount<'info>,
    /// CHECK: owner checked in each handler; the PoolManager checks the mint.
    #[account(mut)]
    pub destination: UncheckedAccount<'info>,
    /// CHECK: the PoolManager checks it.
    #[account(mut)]
    pub mint0: UncheckedAccount<'info>,
    /// CHECK: the PoolManager checks it.
    #[account(mut)]
    pub mint1: UncheckedAccount<'info>,
    /// CHECK: the PoolManager checks it.
    pub token_program0: UncheckedAccount<'info>,
    /// CHECK: the PoolManager checks it.
    pub token_program1: UncheckedAccount<'info>,
    /// CHECK: PoolManager PDA.
    pub pm_authority: UncheckedAccount<'info>,
    /// CHECK: address-checked.
    #[account(address = POOL_MANAGER_ID)]
    pub pool_manager: UncheckedAccount<'info>,
}

impl<'info> HookTreasury<'info> {
    fn pm(&self) -> PmWithdraw<'_, 'info> {
        PmWithdraw {
            hook_authority: self.hook_authority.as_ref(),
            pool: self.pool.as_ref(),
            hook_claims: self.hook_claims.as_ref(),
            vault0: self.vault0.as_ref(),
            vault1: self.vault1.as_ref(),
            mint0: self.mint0.as_ref(),
            mint1: self.mint1.as_ref(),
            token_program0: self.token_program0.as_ref(),
            token_program1: self.token_program1.as_ref(),
            pm_authority: self.pm_authority.as_ref(),
            pool_manager: self.pool_manager.as_ref(),
        }
    }
}

/// Pay the creator's share (SOL) to the creator's own wrapped-SOL account. Anyone can crank it.
pub fn handle_claim_creator_fees(ctx: Context<HookTreasury>) -> Result<()> {
    let a = &ctx.accounts;
    require_keys_eq!(pm_cpi::token_owner(&a.destination)?, a.launch.creator, LaunchError::Unauthorized);
    let amount = a.launch.creator_quote;
    let (x0, x1) = ordered(a.launch.token_is_0, amount, 0);
    a.pm().withdraw(ctx.bumps.hook_authority, a.destination.as_ref(), x0, x1)?;
    ctx.accounts.launch.creator_quote = 0;
    Ok(())
}

/// The protocol's share of one coin's fees, paid only to the config's fee recipient.
#[derive(Accounts)]
pub struct ProtocolTreasury<'info> {
    #[account(seeds = [crate::state::CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, LaunchConfig>>,
    pub treasury: HookTreasury<'info>,
}

/// Pay the protocol's share (SOL) to the fee recipient's wrapped-SOL account. Anyone can crank it.
pub fn handle_withdraw_protocol_fees(ctx: Context<ProtocolTreasury>) -> Result<()> {
    let a = &ctx.accounts.treasury;
    require_keys_eq!(pm_cpi::token_owner(&a.destination)?, ctx.accounts.config.fee_recipient, LaunchError::Unauthorized);
    let amount = a.launch.protocol_quote;
    let (x0, x1) = ordered(a.launch.token_is_0, amount, 0);
    a.pm().withdraw(ctx.bumps.treasury.hook_authority, a.destination.as_ref(), x0, x1)?;
    ctx.accounts.treasury.launch.protocol_quote = 0;
    Ok(())
}

/// Move the holders' share to the keeper, which pays it out to holders and locked stakers each
/// epoch. Anyone can crank it; `destination` must be a wrapped-SOL account owned by HOLDER_PAYOUT.
pub fn handle_withdraw_holder_rewards(ctx: Context<HookTreasury>) -> Result<()> {
    let a = &ctx.accounts;
    require_keys_eq!(pm_cpi::token_owner(&a.destination)?, HOLDER_PAYOUT, LaunchError::Unauthorized);
    let amount = a.launch.staker_owed;
    if amount == 0 {
        return Ok(());
    }
    let (x0, x1) = ordered(a.launch.token_is_0, amount, 0);
    a.pm().withdraw(ctx.bumps.hook_authority, a.destination.as_ref(), x0, x1)?;
    let l = &mut ctx.accounts.launch;
    l.staker_owed = 0;
    emit!(HolderRewardsWithdrawn { mint: l.mint, amount });
    Ok(())
}

/// Burn the tokens the sniper tax bought (and graduation dust). Anyone can crank it.
/// `destination` must be this launch's reserve account.
pub fn handle_burn_pending(ctx: Context<HookTreasury>) -> Result<()> {
    let a = &ctx.accounts;
    let amount = a.launch.burn_pending;
    if amount == 0 {
        return Ok(());
    }
    let (mint, tp) = if a.launch.token_is_0 { (&a.mint0, &a.token_program0) } else { (&a.mint1, &a.token_program1) };
    let expected = Pubkey::find_program_address(&[RESERVE_SEED, a.launch.mint.as_ref()], &crate::ID).0;
    require_keys_eq!(a.destination.key(), expected, LaunchError::Unauthorized);
    let (x0, x1) = ordered(a.launch.token_is_0, 0, amount);
    a.pm().withdraw(ctx.bumps.hook_authority, a.destination.as_ref(), x0, x1)?;
    invoke_signed(
        &spl_token_2022::instruction::burn(tp.key, a.destination.key, mint.key, &a.hook_authority.key(), &[], amount)?,
        &[a.destination.to_account_info(), mint.to_account_info(), a.hook_authority.to_account_info(), tp.to_account_info()],
        &[&[HOOK_AUTHORITY_SEED, &[ctx.bumps.hook_authority]]],
    )?;
    let l = &mut ctx.accounts.launch;
    l.burned += amount;
    l.burn_pending = 0;
    Ok(())
}

#[derive(Accounts)]
pub struct Deepen<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(mut, seeds = [LAUNCH_SEED, launch.mint.as_ref()], bump = launch.bump)]
    pub launch: Box<Account<'info, Launch>>,
    /// CHECK: this program's signing PDA.
    #[account(seeds = [HOOK_AUTHORITY_SEED], bump)]
    pub hook_authority: UncheckedAccount<'info>,
    /// CHECK: address-checked.
    #[account(mut, address = launch.pool)]
    pub pool: UncheckedAccount<'info>,
    /// CHECK: the PoolManager checks it.
    #[account(mut)]
    pub ticks: UncheckedAccount<'info>,
    /// CHECK: the floor position's PDA (checked in the handler; opened on first use).
    #[account(mut)]
    pub position: UncheckedAccount<'info>,
    /// CHECK: the PoolManager checks it.
    #[account(mut)]
    pub hook_claims: UncheckedAccount<'info>,
    /// CHECK: the PoolManager checks it.
    #[account(mut)]
    pub vault0: UncheckedAccount<'info>,
    /// CHECK: the PoolManager checks it.
    #[account(mut)]
    pub vault1: UncheckedAccount<'info>,
    /// CHECK: the PoolManager checks it.
    pub mint0: UncheckedAccount<'info>,
    /// CHECK: the PoolManager checks it.
    pub mint1: UncheckedAccount<'info>,
    /// CHECK: the PoolManager checks it.
    pub token_program0: UncheckedAccount<'info>,
    /// CHECK: the PoolManager checks it.
    pub token_program1: UncheckedAccount<'info>,
    /// CHECK: PoolManager PDA.
    pub hook_caller: UncheckedAccount<'info>,
    /// CHECK: PoolManager PDA.
    pub pm_authority: UncheckedAccount<'info>,
    /// CHECK: address-checked.
    #[account(address = POOL_MANAGER_ID)]
    pub pool_manager: UncheckedAccount<'info>,
    /// CHECK: address-checked.
    #[account(address = crate::ID)]
    pub hook_program: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// Below this, `deepen` waits for more (a position update costs more than it adds).
pub const MIN_DEEPEN: u64 = 1_000_000;

/// Add the liquidity share to the coin's locked buy-side position, which sits between the launch
/// and graduation prices, so dips are absorbed by more SOL the more the coin trades. It only
/// adds while the price is above that range (the position then holds only SOL); otherwise the
/// SOL waits. Anyone can crank it; the position belongs to the hook and can't be removed.
pub fn handle_deepen(ctx: Context<Deepen>) -> Result<()> {
    let a = &ctx.accounts;
    let l = &a.launch;
    require!(l.status == Status::Graduated, LaunchError::NotGraduated);
    let pending = l.liquidity_pending;
    if pending < MIN_DEEPEN {
        return Ok(());
    }
    let st = PoolState::from_account_data(&a.pool.try_borrow_data()?).ok_or(LaunchError::WrongLaunch)?;
    let (lo, hi) = (l.floor_tick_lower, l.floor_tick_upper);
    let sol_only = if l.token_is_0 { st.tick >= hi } else { st.tick < lo };
    if !sol_only {
        return Ok(());
    }
    let owner = a.hook_authority.key();
    let (expected, _) = Pubkey::find_program_address(
        &[POSITION_SEED, a.pool.key().as_ref(), owner.as_ref(), &lo.to_le_bytes(), &hi.to_le_bytes(), &FLOOR_SALT],
        &POOL_MANAGER_ID,
    );
    require_keys_eq!(a.position.key(), expected, LaunchError::Unauthorized);
    let bump = ctx.bumps.hook_authority;
    let infos = [
        a.payer.to_account_info(),
        a.hook_authority.to_account_info(),
        a.pool.to_account_info(),
        a.ticks.to_account_info(),
        a.position.to_account_info(),
        a.vault0.to_account_info(),
        a.vault1.to_account_info(),
        a.hook_claims.to_account_info(),
        a.mint0.to_account_info(),
        a.mint1.to_account_info(),
        a.token_program0.to_account_info(),
        a.token_program1.to_account_info(),
        a.hook_program.to_account_info(),
        a.hook_caller.to_account_info(),
        a.pm_authority.to_account_info(),
        a.system_program.to_account_info(),
        a.pool_manager.to_account_info(),
    ];
    if a.position.data_is_empty() {
        pm_cpi::call(
            pm::instruction::OpenPosition { tick_lower: lo, tick_upper: hi, salt: FLOOR_SALT }.data(),
            pm::accounts::OpenPosition {
                payer: a.payer.key(),
                owner,
                pool: a.pool.key(),
                position: a.position.key(),
                system_program: a.system_program.key(),
            }
            .to_account_metas(None),
            &infos,
            bump,
        )?;
    }
    let m = |r: core::result::Result<u128, MathError>| r.map_err(|_| error!(LaunchError::MathError));
    let (sa, sb, sp) = (m(get_sqrt_price_at_tick(lo))?, m(get_sqrt_price_at_tick(hi))?, st.sqrt_price.get());
    let amount = (pending - 1) as u128;
    let liquidity = if l.token_is_0 { m(get_liquidity_for_amounts(sp, sa, sb, 0, amount))? } else { m(get_liquidity_for_amounts(sp, sa, sb, amount, 0))? };
    if liquidity == 0 {
        return Ok(());
    }
    let before = pm_cpi::claims_balances(&a.hook_claims)?;
    pm_cpi::call(
        pm::instruction::ModifyLiquidity {
            params: ModifyLiquidityParams { tick_lower: lo, tick_upper: hi, liquidity_delta: liquidity as i128, salt: FLOOR_SALT },
            hook_data: vec![],
            settle: Settle { pay_with_claims: true, take_as_claims: true },
        }
        .data(),
        pm::accounts::ModifyLiquidity {
            payer: a.payer.key(),
            owner,
            pool: a.pool.key(),
            ticks: a.ticks.key(),
            position: a.position.key(),
            vault0: a.vault0.key(),
            vault1: a.vault1.key(),
            token0: None,
            token1: None,
            owner_claims: Some(a.hook_claims.key()),
            mint0: a.mint0.key(),
            mint1: a.mint1.key(),
            token_program0: a.token_program0.key(),
            token_program1: a.token_program1.key(),
            hooks: crate::ID,
            hook_caller: a.hook_caller.key(),
            hook_claims: None,
            authority: a.pm_authority.key(),
            system_program: a.system_program.key(),
        }
        .to_account_metas(None),
        &infos,
        bump,
    )?;
    let after = pm_cpi::claims_balances(&a.hook_claims)?;
    let token_is_0 = a.launch.token_is_0;
    let (q_before, q_after) = if token_is_0 { (before.1, after.1) } else { (before.0, after.0) };
    let paid = q_before.checked_sub(q_after).ok_or(LaunchError::MathError)?;
    require!(paid <= pending, LaunchError::MathError);
    let l = &mut ctx.accounts.launch;
    l.liquidity_pending -= paid;
    l.liquidity_added += paid;
    l.floor_liquidity += liquidity;
    emit!(Deepened { mint: l.mint, quote: paid, liquidity });
    Ok(())
}
