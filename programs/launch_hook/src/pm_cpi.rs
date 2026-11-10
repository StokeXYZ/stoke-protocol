//! Calls into the PoolManager, signed by this hook's authority PDA (so they skip this hook's
//! own callbacks: noSelfCall).

use anchor_lang::prelude::*;
use anchor_lang::solana_program::{instruction::Instruction, program::invoke_signed};
use meridian_core::HOOK_AUTHORITY_SEED;

pub fn call<'info>(data: Vec<u8>, metas: Vec<AccountMeta>, infos: &[AccountInfo<'info>], authority_bump: u8) -> Result<()> {
    let ix = Instruction { program_id: meridian_core::POOL_MANAGER_ID, accounts: metas, data };
    invoke_signed(&ix, infos, &[&[HOOK_AUTHORITY_SEED, &[authority_bump]]])?;
    Ok(())
}

/// Reads a PoolManager claims account's balances.
pub fn claims_balances(info: &AccountInfo) -> Result<(u64, u64)> {
    let data = info.try_borrow_data()?;
    let c = meridian_pool_manager::Claims::try_deserialize(&mut &data[..])?;
    Ok((c.amount0, c.amount1))
}

/// The owner field of a token account.
pub fn token_owner(info: &AccountInfo) -> Result<Pubkey> {
    let d = info.try_borrow_data()?;
    require!(d.len() >= 64, crate::errors::LaunchError::Unauthorized);
    Ok(Pubkey::new_from_array(d[32..64].try_into().unwrap()))
}

/// The PoolManager accounts for moving the hook's claims on one pool.
pub struct PmWithdraw<'a, 'info> {
    pub hook_authority: &'a AccountInfo<'info>,
    pub pool: &'a AccountInfo<'info>,
    pub hook_claims: &'a AccountInfo<'info>,
    pub vault0: &'a AccountInfo<'info>,
    pub vault1: &'a AccountInfo<'info>,
    pub mint0: &'a AccountInfo<'info>,
    pub mint1: &'a AccountInfo<'info>,
    pub token_program0: &'a AccountInfo<'info>,
    pub token_program1: &'a AccountInfo<'info>,
    pub pm_authority: &'a AccountInfo<'info>,
    pub pool_manager: &'a AccountInfo<'info>,
}

impl<'a, 'info> PmWithdraw<'a, 'info> {
    /// Withdraw the hook's claims to `destination` (one side only: the other amount must be 0).
    pub fn withdraw(&self, bump: u8, destination: &AccountInfo<'info>, amount0: u64, amount1: u64) -> Result<()> {
        use anchor_lang::{InstructionData, ToAccountMetas};
        use meridian_pool_manager as pm;
        if amount0 == 0 && amount1 == 0 {
            return Ok(());
        }
        require!(amount0 == 0 || amount1 == 0, crate::errors::LaunchError::MathError);
        let infos = [
            self.hook_authority.clone(),
            self.pool.clone(),
            self.hook_claims.clone(),
            self.vault0.clone(),
            self.vault1.clone(),
            destination.clone(),
            self.mint0.clone(),
            self.mint1.clone(),
            self.token_program0.clone(),
            self.token_program1.clone(),
            self.pm_authority.clone(),
            self.pool_manager.clone(),
        ];
        call(
            pm::instruction::Withdraw { amount0, amount1 }.data(),
            pm::accounts::Withdraw {
                owner: *self.hook_authority.key,
                pool: *self.pool.key,
                claims: *self.hook_claims.key,
                vault0: *self.vault0.key,
                vault1: *self.vault1.key,
                token0: (amount0 > 0).then(|| *destination.key),
                token1: (amount1 > 0).then(|| *destination.key),
                mint0: *self.mint0.key,
                mint1: *self.mint1.key,
                token_program0: *self.token_program0.key,
                token_program1: *self.token_program1.key,
                authority: *self.pm_authority.key,
            }
            .to_account_metas(None),
            &infos,
            bump,
        )
    }
}

/// (amount0, amount1) for a (quote, token) pair.
pub fn ordered(token_is_0: bool, quote: u64, token: u64) -> (u64, u64) {
    if token_is_0 { (token, quote) } else { (quote, token) }
}
