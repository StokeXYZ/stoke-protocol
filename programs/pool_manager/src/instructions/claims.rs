use anchor_lang::prelude::*;
use meridian_core::{PoolState, CLAIMS_SEED};

use crate::errors::PmError;
use crate::settle::{credit, debit, Vaults};
use crate::state::{Claims, Pool};

fn check_pool(st: &PoolState, vault: [&AccountInfo; 2], mint: [&AccountInfo; 2], tp: [&AccountInfo; 2]) -> Result<()> {
    require!(st.sqrt_price.get() != 0, PmError::PoolNotInitialized);
    require!(
        *vault[0].key == st.vault0
            && *vault[1].key == st.vault1
            && *mint[0].key == st.currency0
            && *mint[1].key == st.currency1
            && *tp[0].key == st.token_program0
            && *tp[1].key == st.token_program1,
        PmError::WrongPoolAccount
    );
    Ok(())
}

#[derive(Accounts)]
#[instruction(owner: Pubkey)]
pub struct OpenClaims<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub pool: AccountLoader<'info, Pool>,
    #[account(init, payer = payer, space = 8 + Claims::INIT_SPACE, seeds = [CLAIMS_SEED, pool.key().as_ref(), owner.as_ref()], bump)]
    pub claims: Account<'info, Claims>,
    pub system_program: Program<'info, System>,
}

/// Open a claims account for any owner (a hook's, before its first delta).
pub fn handle_open_claims(ctx: Context<OpenClaims>, owner: Pubkey) -> Result<()> {
    let c = &mut ctx.accounts.claims;
    c.pool = ctx.accounts.pool.key();
    c.owner = owner;
    c.bump = ctx.bumps.claims;
    Ok(())
}

#[derive(Accounts)]
pub struct Deposit<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub owner: Signer<'info>,
    #[account(mut)]
    pub pool: AccountLoader<'info, Pool>,
    #[account(
        init_if_needed,
        payer = payer,
        space = 8 + Claims::INIT_SPACE,
        seeds = [CLAIMS_SEED, pool.key().as_ref(), owner.key().as_ref()],
        bump,
    )]
    pub claims: Account<'info, Claims>,
    /// CHECK: matched against the pool.
    #[account(mut)]
    pub vault0: UncheckedAccount<'info>,
    /// CHECK: matched against the pool.
    #[account(mut)]
    pub vault1: UncheckedAccount<'info>,
    /// CHECK: currency0 source.
    #[account(mut)]
    pub token0: Option<UncheckedAccount<'info>>,
    /// CHECK: currency1 source.
    #[account(mut)]
    pub token1: Option<UncheckedAccount<'info>>,
    /// CHECK: matched against the pool.
    pub mint0: UncheckedAccount<'info>,
    /// CHECK: matched against the pool.
    pub mint1: UncheckedAccount<'info>,
    /// CHECK: matched against the pool.
    pub token_program0: UncheckedAccount<'info>,
    /// CHECK: matched against the pool.
    pub token_program1: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// v4's mint: pay tokens into the pool and hold claims on them instead.
pub fn handle_deposit(ctx: Context<Deposit>, amount0: u64, amount1: u64) -> Result<()> {
    let a = &ctx.accounts;
    let mut st = a.pool.load()?.state;
    let (v0, v1, m0, m1, p0, p1) = (
        a.vault0.to_account_info(),
        a.vault1.to_account_info(),
        a.mint0.to_account_info(),
        a.mint1.to_account_info(),
        a.token_program0.to_account_info(),
        a.token_program1.to_account_info(),
    );
    check_pool(&st, [&v0, &v1], [&m0, &m1], [&p0, &p1])?;
    let vaults = Vaults { vault: [&v0, &v1], mint: [&m0, &m1], token_program: [&p0, &p1], decimals: [st.decimals0, st.decimals1], authority: None, authority_bump: 0 };
    let owner = a.owner.to_account_info();
    if amount0 > 0 {
        vaults.pull(0, &a.token0.as_ref().ok_or(PmError::TokenAccountRequired)?.to_account_info(), &owner, amount0)?;
    }
    if amount1 > 0 {
        vaults.pull(1, &a.token1.as_ref().ok_or(PmError::TokenAccountRequired)?.to_account_info(), &owner, amount1)?;
    }
    let pool_key = ctx.accounts.pool.key();
    let owner_key = ctx.accounts.owner.key();
    let bump = ctx.bumps.claims;
    let c = &mut ctx.accounts.claims;
    if c.pool == Pubkey::default() {
        c.pool = pool_key;
        c.owner = owner_key;
        c.bump = bump;
    }
    credit(c, &mut st, 0, amount0)?;
    credit(c, &mut st, 1, amount1)?;
    ctx.accounts.pool.load_mut()?.state = st;
    Ok(())
}

#[derive(Accounts)]
pub struct Withdraw<'info> {
    pub owner: Signer<'info>,
    #[account(mut)]
    pub pool: AccountLoader<'info, Pool>,
    #[account(mut, seeds = [CLAIMS_SEED, pool.key().as_ref(), owner.key().as_ref()], bump = claims.bump)]
    pub claims: Account<'info, Claims>,
    /// CHECK: matched against the pool.
    #[account(mut)]
    pub vault0: UncheckedAccount<'info>,
    /// CHECK: matched against the pool.
    #[account(mut)]
    pub vault1: UncheckedAccount<'info>,
    /// CHECK: currency0 destination.
    #[account(mut)]
    pub token0: Option<UncheckedAccount<'info>>,
    /// CHECK: currency1 destination.
    #[account(mut)]
    pub token1: Option<UncheckedAccount<'info>>,
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

/// v4's burn + take: burn claims and receive the tokens.
pub fn handle_withdraw(ctx: Context<Withdraw>, amount0: u64, amount1: u64) -> Result<()> {
    let a = &ctx.accounts;
    let mut st = a.pool.load()?.state;
    let (v0, v1, m0, m1, p0, p1, auth) = (
        a.vault0.to_account_info(),
        a.vault1.to_account_info(),
        a.mint0.to_account_info(),
        a.mint1.to_account_info(),
        a.token_program0.to_account_info(),
        a.token_program1.to_account_info(),
        a.authority.to_account_info(),
    );
    check_pool(&st, [&v0, &v1], [&m0, &m1], [&p0, &p1])?;
    let vaults = Vaults { vault: [&v0, &v1], mint: [&m0, &m1], token_program: [&p0, &p1], decimals: [st.decimals0, st.decimals1], authority: Some(&auth), authority_bump: st.authority_bump };
    let t0 = a.token0.as_ref().map(|t| t.to_account_info());
    let t1 = a.token1.as_ref().map(|t| t.to_account_info());
    let c = &mut ctx.accounts.claims;
    debit(c, &mut st, 0, amount0)?;
    debit(c, &mut st, 1, amount1)?;
    if amount0 > 0 {
        vaults.push(0, t0.as_ref().ok_or(PmError::TokenAccountRequired)?, amount0)?;
    }
    if amount1 > 0 {
        vaults.push(1, t1.as_ref().ok_or(PmError::TokenAccountRequired)?, amount1)?;
    }
    ctx.accounts.pool.load_mut()?.state = st;
    Ok(())
}

#[derive(Accounts)]
pub struct TransferClaims<'info> {
    pub owner: Signer<'info>,
    pub pool: AccountLoader<'info, Pool>,
    #[account(mut, seeds = [CLAIMS_SEED, pool.key().as_ref(), owner.key().as_ref()], bump = from.bump)]
    pub from: Account<'info, Claims>,
    #[account(mut, constraint = to.pool == pool.key() @ PmError::WrongPoolAccount)]
    pub to: Account<'info, Claims>,
}

/// ERC-6909 transfer of claims within a pool.
pub fn handle_transfer_claims(ctx: Context<TransferClaims>, amount0: u64, amount1: u64) -> Result<()> {
    let from = &mut ctx.accounts.from;
    from.amount0 = from.amount0.checked_sub(amount0).ok_or(PmError::InsufficientClaims)?;
    from.amount1 = from.amount1.checked_sub(amount1).ok_or(PmError::InsufficientClaims)?;
    let to = &mut ctx.accounts.to;
    to.amount0 = to.amount0.checked_add(amount0).ok_or(PmError::MathOverflow)?;
    to.amount1 = to.amount1.checked_add(amount1).ok_or(PmError::MathOverflow)?;
    Ok(())
}
