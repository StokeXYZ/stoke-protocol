//! Locked staking: lock a coin for 7 days (1.5×) or 30 days (2×) to weigh more in the holders'
//! share of its trading fee, which the keeper pays out each hourly epoch (holding unstaked earns 1×).
//! Each tier is its own stake account; adding to a locked tier restarts its lock. The old flexible
//! tier takes no new stakes, and `claim_rewards` only pays rewards earned before the change.

use anchor_lang::prelude::*;
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};
use meridian_core::*;

use crate::errors::LaunchError;
use crate::events::{RewardsClaimed, Staked};
use crate::pm_cpi::{self, ordered, PmWithdraw};
use crate::state::*;

#[derive(Accounts)]
#[instruction(tier: u8)]
pub struct StakeTokens<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(mut, seeds = [LAUNCH_SEED, launch.mint.as_ref()], bump = launch.bump)]
    pub launch: Box<Account<'info, Launch>>,
    #[account(
        init_if_needed,
        payer = owner,
        space = 8 + Stake::INIT_SPACE,
        seeds = [STAKE_SEED, launch.key().as_ref(), owner.key().as_ref(), &[tier]],
        bump
    )]
    pub stake: Box<Account<'info, Stake>>,
    #[account(address = launch.mint, mint::token_program = token_program)]
    pub mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, token::mint = mint, token::authority = owner, token::token_program = token_program)]
    pub owner_token: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        init_if_needed,
        payer = owner,
        seeds = [STAKE_VAULT_SEED, mint.key().as_ref()],
        bump,
        token::mint = mint,
        token::authority = hook_authority,
        token::token_program = token_program,
    )]
    pub stake_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: this program's signing PDA.
    #[account(seeds = [HOOK_AUTHORITY_SEED], bump)]
    pub hook_authority: UncheckedAccount<'info>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

fn weight_of(amount: u64, tier: u8) -> u128 {
    amount as u128 * STAKE_TIERS[tier as usize].1 as u128 / BPS as u128
}

pub fn handle_stake(ctx: Context<StakeTokens>, tier: u8, amount: u64) -> Result<()> {
    // Tier 0 (flexible, 1×) is retired: holding the coin already earns 1×.
    require!(tier != 0 && (tier as usize) < STAKE_TIERS.len(), LaunchError::BadTier);
    require!(amount >= MIN_STAKE, LaunchError::StakeTooSmall);
    token_interface::transfer_checked(
        CpiContext::new(
            ctx.accounts.token_program.key(),
            TransferChecked {
                from: ctx.accounts.owner_token.to_account_info(),
                mint: ctx.accounts.mint.to_account_info(),
                to: ctx.accounts.stake_vault.to_account_info(),
                authority: ctx.accounts.owner.to_account_info(),
            },
        ),
        amount,
        ctx.accounts.mint.decimals,
    )?;
    let now = Clock::get()?.unix_timestamp;
    let (launch_key, owner) = (ctx.accounts.launch.key(), ctx.accounts.owner.key());
    let l = &mut ctx.accounts.launch;
    let s = &mut ctx.accounts.stake;
    if s.owner == Pubkey::default() {
        s.launch = launch_key;
        s.owner = owner;
        s.tier = tier;
        s.bump = ctx.bumps.stake;
    }
    if s.amount == 0 {
        l.stakers += 1;
    }
    s.settle(l.reward_index)?;
    let w = weight_of(amount, tier);
    s.amount = s.amount.checked_add(amount).ok_or(LaunchError::MathError)?;
    s.weight = s.weight.checked_add(w).ok_or(LaunchError::MathError)?;
    s.reward_debt = accrued(s.weight, l.reward_index)?;
    let lock = STAKE_TIERS[tier as usize].0;
    if lock > 0 {
        s.lock_until = now + lock;
    }
    l.staked = l.staked.checked_add(amount).ok_or(LaunchError::MathError)?;
    l.total_weight = l.total_weight.checked_add(w).ok_or(LaunchError::MathError)?;
    emit!(Staked { mint: l.mint, owner, tier, amount: amount as i64, lock_until: s.lock_until, total_staked: l.staked });
    Ok(())
}

#[derive(Accounts)]
pub struct UnstakeTokens<'info> {
    pub owner: Signer<'info>,
    #[account(mut, seeds = [LAUNCH_SEED, launch.mint.as_ref()], bump = launch.bump)]
    pub launch: Box<Account<'info, Launch>>,
    #[account(
        mut,
        seeds = [STAKE_SEED, launch.key().as_ref(), owner.key().as_ref(), &[stake.tier]],
        bump = stake.bump,
        has_one = owner @ LaunchError::Unauthorized,
    )]
    pub stake: Box<Account<'info, Stake>>,
    #[account(address = launch.mint, mint::token_program = token_program)]
    pub mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(mut, token::mint = mint, token::token_program = token_program)]
    pub owner_token: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(mut, seeds = [STAKE_VAULT_SEED, mint.key().as_ref()], bump)]
    pub stake_vault: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: this program's signing PDA.
    #[account(seeds = [HOOK_AUTHORITY_SEED], bump)]
    pub hook_authority: UncheckedAccount<'info>,
    pub token_program: Interface<'info, TokenInterface>,
}

pub fn handle_unstake(ctx: Context<UnstakeTokens>, amount: u64) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let s = &ctx.accounts.stake;
    require!(now >= s.lock_until, LaunchError::StillLocked);
    require!(amount > 0 && amount <= s.amount, LaunchError::NotEnoughStaked);
    require_keys_eq!(ctx.accounts.owner_token.owner, ctx.accounts.owner.key(), LaunchError::Unauthorized);
    let bump = ctx.bumps.hook_authority;
    token_interface::transfer_checked(
        CpiContext::new_with_signer(
            ctx.accounts.token_program.key(),
            TransferChecked {
                from: ctx.accounts.stake_vault.to_account_info(),
                mint: ctx.accounts.mint.to_account_info(),
                to: ctx.accounts.owner_token.to_account_info(),
                authority: ctx.accounts.hook_authority.to_account_info(),
            },
            &[&[HOOK_AUTHORITY_SEED, &[bump]]],
        ),
        amount,
        ctx.accounts.mint.decimals,
    )?;
    let owner = ctx.accounts.owner.key();
    let l = &mut ctx.accounts.launch;
    let s = &mut ctx.accounts.stake;
    s.settle(l.reward_index)?;
    let w = if amount == s.amount { s.weight } else { weight_of(amount, s.tier).min(s.weight) };
    s.amount -= amount;
    s.weight -= w;
    s.reward_debt = accrued(s.weight, l.reward_index)?;
    l.staked -= amount;
    l.total_weight -= w;
    if s.amount == 0 {
        l.stakers = l.stakers.saturating_sub(1);
    }
    emit!(Staked { mint: l.mint, owner, tier: s.tier, amount: -(amount as i64), lock_until: s.lock_until, total_staked: l.staked });
    Ok(())
}

#[derive(Accounts)]
pub struct ClaimRewards<'info> {
    pub owner: Signer<'info>,
    #[account(
        mut,
        seeds = [STAKE_SEED, launch.key().as_ref(), owner.key().as_ref(), &[stake.tier]],
        bump = stake.bump,
        has_one = owner @ LaunchError::Unauthorized,
    )]
    pub stake: Box<Account<'info, Stake>>,
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
    pub hook_claims: UncheckedAccount<'info>,
    /// CHECK: the PoolManager checks it.
    #[account(mut)]
    pub vault0: UncheckedAccount<'info>,
    /// CHECK: the PoolManager checks it.
    #[account(mut)]
    pub vault1: UncheckedAccount<'info>,
    /// CHECK: the owner's wrapped-SOL account (owner checked here, mint by the PoolManager).
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

/// Pay a stake's rewards, in SOL, to the owner's wrapped-SOL account.
pub fn handle_claim_rewards(ctx: Context<ClaimRewards>) -> Result<()> {
    let a = &ctx.accounts;
    require_keys_eq!(pm_cpi::token_owner(&a.destination)?, a.owner.key(), LaunchError::Unauthorized);
    let index = a.launch.reward_index;
    ctx.accounts.stake.settle(index)?;
    let a = &ctx.accounts;
    // Per-stake rounding can leave a lamport of difference; never pay more than is owed in total.
    let amount = a.stake.pending.min(a.launch.staker_owed);
    if amount == 0 {
        return Ok(());
    }
    let pm = PmWithdraw {
        hook_authority: a.hook_authority.as_ref(),
        pool: a.pool.as_ref(),
        hook_claims: a.hook_claims.as_ref(),
        vault0: a.vault0.as_ref(),
        vault1: a.vault1.as_ref(),
        mint0: a.mint0.as_ref(),
        mint1: a.mint1.as_ref(),
        token_program0: a.token_program0.as_ref(),
        token_program1: a.token_program1.as_ref(),
        pm_authority: a.pm_authority.as_ref(),
        pool_manager: a.pool_manager.as_ref(),
    };
    let (x0, x1) = ordered(a.launch.token_is_0, amount, 0);
    pm.withdraw(ctx.bumps.hook_authority, a.destination.as_ref(), x0, x1)?;
    let owner = ctx.accounts.owner.key();
    let s = &mut ctx.accounts.stake;
    s.pending -= amount;
    s.claimed += amount;
    let l = &mut ctx.accounts.launch;
    l.staker_owed -= amount;
    emit!(RewardsClaimed { mint: l.mint, owner, amount });
    Ok(())
}
