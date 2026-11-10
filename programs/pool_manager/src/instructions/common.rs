//! Shared account plumbing for pool operations.

use anchor_lang::prelude::*;
use meridian_core::PoolState;

use crate::errors::PmError;
use crate::hooks::HookCtx;
use crate::settle::Vaults;
use crate::state::Pool;

/// Owned clones of the accounts every pool operation touches.
pub struct Infos<'info> {
    pub pool: AccountInfo<'info>,
    pub ticks: AccountInfo<'info>,
    pub hooks: AccountInfo<'info>,
    pub hook_caller: AccountInfo<'info>,
    pub vault: [AccountInfo<'info>; 2],
    pub mint: [AccountInfo<'info>; 2],
    pub token_program: [AccountInfo<'info>; 2],
    pub authority: AccountInfo<'info>,
}

impl<'info> Infos<'info> {
    /// v4's noSelfCall: an operation the hook signs itself never calls back into the hook.
    pub fn hook_ctx<'a>(&'a self, st: &PoolState, sender: &Pubkey, remaining: &'a [AccountInfo<'info>], measure: bool) -> HookCtx<'a, 'info> {
        HookCtx {
            program: &self.hooks,
            caller: &self.hook_caller,
            pool: &self.pool,
            vaults: if measure { Some([&self.vault[0], &self.vault[1]]) } else { None },
            remaining,
            flags: st.hook_flags,
            active: st.hooks != Pubkey::default() && *sender != st.hook_authority,
            caller_bump: st.hook_caller_bump,
        }
    }

    pub fn vaults<'a>(&'a self, st: &PoolState) -> Vaults<'a, 'info> {
        Vaults {
            vault: [&self.vault[0], &self.vault[1]],
            mint: [&self.mint[0], &self.mint[1]],
            token_program: [&self.token_program[0], &self.token_program[1]],
            decimals: [st.decimals0, st.decimals1],
            authority: Some(&self.authority),
            authority_bump: st.authority_bump,
        }
    }
}

macro_rules! infos {
    ($a:expr) => {
        $crate::instructions::common::Infos {
            pool: $a.pool.to_account_info(),
            ticks: $a.ticks.to_account_info(),
            hooks: $a.hooks.to_account_info(),
            hook_caller: $a.hook_caller.to_account_info(),
            vault: [$a.vault0.to_account_info(), $a.vault1.to_account_info()],
            mint: [$a.mint0.to_account_info(), $a.mint1.to_account_info()],
            token_program: [$a.token_program0.to_account_info(), $a.token_program1.to_account_info()],
            authority: $a.authority.to_account_info(),
        }
    };
}
pub(crate) use infos;

/// Every account a pool operation names must be the pool's own.
pub fn check_pool_accounts(st: &PoolState, i: &Infos) -> Result<()> {
    require!(st.sqrt_price.get() != 0, PmError::PoolNotInitialized);
    require!(
        *i.ticks.key == st.ticks
            && *i.vault[0].key == st.vault0
            && *i.vault[1].key == st.vault1
            && *i.mint[0].key == st.currency0
            && *i.mint[1].key == st.currency1
            && *i.token_program[0].key == st.token_program0
            && *i.token_program[1].key == st.token_program1,
        PmError::WrongPoolAccount
    );
    require!(*i.hooks.key == st.hooks, PmError::WrongHookProgram);
    Ok(())
}

pub fn write_pool(loader: &AccountLoader<Pool>, st: &PoolState) -> Result<()> {
    loader.load_mut()?.state = *st;
    Ok(())
}
