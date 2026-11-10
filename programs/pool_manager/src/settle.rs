//! Moving tokens in and out of vaults, and claims bookkeeping.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::{invoke, invoke_signed};
use anchor_spl::token_2022::spl_token_2022;
use meridian_core::{BalanceDelta, PoolState, Settle, AUTHORITY_SEED};

use crate::errors::PmError;
use crate::state::Claims;

/// The `amount` field of an SPL Token / Token-2022 account.
pub fn vault_amount(info: &AccountInfo) -> Result<u64> {
    let d = info.try_borrow_data()?;
    require!(d.len() >= 72, PmError::WrongPoolAccount);
    Ok(u64::from_le_bytes(d[64..72].try_into().unwrap()))
}

pub fn to_amount(x: i128) -> Result<u64> {
    u64::try_from(x.unsigned_abs()).map_err(|_| error!(PmError::AmountOverflow))
}

/// The pool's side of every transfer.
pub struct Vaults<'a, 'info> {
    pub vault: [&'a AccountInfo<'info>; 2],
    pub mint: [&'a AccountInfo<'info>; 2],
    pub token_program: [&'a AccountInfo<'info>; 2],
    pub decimals: [u8; 2],
    /// Needed only to pay out.
    pub authority: Option<&'a AccountInfo<'info>>,
    pub authority_bump: u8,
}

impl<'a, 'info> Vaults<'a, 'info> {
    /// `from` → vault, signed by `owner` (a transaction signer or a PDA that signed this call).
    pub fn pull(&self, i: usize, from: &AccountInfo<'info>, owner: &AccountInfo<'info>, amount: u64) -> Result<()> {
        if amount == 0 {
            return Ok(());
        }
        let ix = spl_token_2022::instruction::transfer_checked(
            self.token_program[i].key,
            from.key,
            self.mint[i].key,
            self.vault[i].key,
            owner.key,
            &[],
            amount,
            self.decimals[i],
        )?;
        invoke(&ix, &[from.clone(), self.mint[i].clone(), self.vault[i].clone(), owner.clone(), self.token_program[i].clone()])?;
        Ok(())
    }

    /// vault → `to`, signed by the PoolManager's authority PDA.
    pub fn push(&self, i: usize, to: &AccountInfo<'info>, amount: u64) -> Result<()> {
        if amount == 0 {
            return Ok(());
        }
        let authority = self.authority.ok_or(PmError::WrongPoolAccount)?;
        let ix = spl_token_2022::instruction::transfer_checked(
            self.token_program[i].key,
            self.vault[i].key,
            self.mint[i].key,
            to.key,
            authority.key,
            &[],
            amount,
            self.decimals[i],
        )?;
        invoke_signed(
            &ix,
            &[self.vault[i].clone(), self.mint[i].clone(), to.clone(), authority.clone(), self.token_program[i].clone()],
            &[&[AUTHORITY_SEED, &[self.authority_bump]]],
        )?;
        Ok(())
    }
}

pub fn credit(claims: &mut Claims, st: &mut PoolState, i: usize, amount: u64) -> Result<()> {
    if i == 0 {
        claims.amount0 = claims.amount0.checked_add(amount).ok_or(PmError::MathOverflow)?;
        st.claims0 = st.claims0.checked_add(amount).ok_or(PmError::MathOverflow)?;
    } else {
        claims.amount1 = claims.amount1.checked_add(amount).ok_or(PmError::MathOverflow)?;
        st.claims1 = st.claims1.checked_add(amount).ok_or(PmError::MathOverflow)?;
    }
    Ok(())
}

pub fn debit(claims: &mut Claims, st: &mut PoolState, i: usize, amount: u64) -> Result<()> {
    if i == 0 {
        claims.amount0 = claims.amount0.checked_sub(amount).ok_or(PmError::InsufficientClaims)?;
        st.claims0 = st.claims0.checked_sub(amount).ok_or(PmError::MathOverflow)?;
    } else {
        claims.amount1 = claims.amount1.checked_sub(amount).ok_or(PmError::InsufficientClaims)?;
        st.claims1 = st.claims1.checked_sub(amount).ok_or(PmError::MathOverflow)?;
    }
    Ok(())
}

pub fn check_claims(claims: &Claims, pool: &Pubkey, owner: &Pubkey) -> Result<()> {
    require!(claims.pool == *pool && claims.owner == *owner, PmError::WrongPoolAccount);
    Ok(())
}

/// Settle a caller's delta: pay what they owe (tokens in, or burn claims), then pay out what
/// they're owed (tokens out, or mint claims).
#[allow(clippy::too_many_arguments)]
pub fn settle_caller<'a, 'info>(
    v: &Vaults<'a, 'info>,
    st: &mut PoolState,
    delta: BalanceDelta,
    settle: Settle,
    owner: &AccountInfo<'info>,
    tokens: [Option<&AccountInfo<'info>>; 2],
    mut claims: Option<&mut Claims>,
) -> Result<()> {
    let d = [delta.amount0, delta.amount1];
    for i in 0..2 {
        if d[i] < 0 {
            let amount = to_amount(d[i])?;
            if settle.pay_with_claims {
                debit(claims.as_deref_mut().ok_or(PmError::ClaimsAccountRequired)?, st, i, amount)?;
            } else {
                v.pull(i, tokens[i].ok_or(PmError::TokenAccountRequired)?, owner, amount)?;
            }
        }
    }
    for i in 0..2 {
        if d[i] > 0 {
            let amount = to_amount(d[i])?;
            if settle.take_as_claims {
                credit(claims.as_deref_mut().ok_or(PmError::ClaimsAccountRequired)?, st, i, amount)?;
            } else {
                v.push(i, tokens[i].ok_or(PmError::TokenAccountRequired)?, amount)?;
            }
        }
    }
    Ok(())
}

/// Settle a hook's delta against its claims (v4's mint/burn of the hook's ERC-6909 balance).
pub fn settle_hook(st: &mut PoolState, delta: BalanceDelta, claims: Option<&mut Claims>) -> Result<()> {
    if delta.is_zero() {
        return Ok(());
    }
    let claims = claims.ok_or(PmError::HookClaimsRequired)?;
    require!(claims.owner == st.hook_authority, PmError::WrongPoolAccount);
    let d = [delta.amount0, delta.amount1];
    for (i, x) in d.iter().enumerate() {
        if *x > 0 {
            credit(claims, st, i, to_amount(*x)?)?;
        } else if *x < 0 {
            debit(claims, st, i, to_amount(*x)?)?;
        }
    }
    Ok(())
}
