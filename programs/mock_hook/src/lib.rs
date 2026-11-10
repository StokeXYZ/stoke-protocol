//! A hook for tests. State lives in two accounts the tests create (owned by this program):
//! `accounts[2]` = config (a Borsh `MockConfig`), `accounts[3]` = log.
//! Extra instruction `ACT` proxies any PoolManager instruction, signed by this hook's authority
//! PDA, to exercise noSelfCall and `update_dynamic_lp_fee`.
//!
//! It's also a coin hook for tests (Stoke v3, `meridian_core::coin_hook`): the coin's settings
//! say how it behaves (`coin_mode`), and it logs what it was called with.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::{
    account_info::AccountInfo,
    instruction::Instruction,
    program::{invoke_signed, set_return_data},
    program_error::ProgramError,
};
use anchor_spl::token_2022::spl_token_2022;
use meridian_core::coin_hook::discriminator as cd;
use meridian_core::hook::discriminator as d;
use meridian_core::*;

pub const ACT: [u8; 8] = *b"mock_act";
pub const SLOTS: [[u8; 8]; 10] = [
    d::BEFORE_INITIALIZE,
    d::AFTER_INITIALIZE,
    d::BEFORE_ADD_LIQUIDITY,
    d::AFTER_ADD_LIQUIDITY,
    d::BEFORE_REMOVE_LIQUIDITY,
    d::AFTER_REMOVE_LIQUIDITY,
    d::BEFORE_SWAP,
    d::AFTER_SWAP,
    d::BEFORE_DONATE,
    d::AFTER_DONATE,
];
pub const LOG_SLOT: usize = 600;
pub const LOG_SIZE: usize = 40 + 10 * (4 + LOG_SLOT);

/// Pay `amount` from a token account the hook authority owns into a vault, during `slot`.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, Default, PartialEq)]
pub struct Pay {
    pub slot: u8,
    pub amount: u64,
    pub from: u8,
    pub mint: u8,
    pub vault: u8,
    pub token_program: u8,
    pub decimals: u8,
}

/// Try to move someone else's tokens with an authority the hook only sees as a non-signer.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, Default, PartialEq)]
pub struct Steal {
    pub slot: u8,
    pub from: u8,
    pub to: u8,
    pub owner: u8,
    pub mint: u8,
    pub token_program: u8,
    pub decimals: u8,
    pub amount: u64,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, Default, PartialEq)]
pub struct MockConfig {
    /// Bit i: fail in slot i.
    pub revert_mask: u16,
    /// Bit i: return nothing in slot i.
    pub silent_mask: u16,
    /// Bit i: return bytes that don't decode in slot i.
    pub garbage_mask: u16,
    pub before_swap: BeforeSwapReturn,
    pub after_swap: AfterSwapReturn,
    pub after_add: AfterModifyLiquidityReturn,
    pub after_remove: AfterModifyLiquidityReturn,
    /// Effects for slots that return plain HookEffects (after_initialize, before_add/remove, donates).
    pub effects: Vec<(u8, HookEffects)>,
    pub pay: Option<Pay>,
    pub steal: Option<Steal>,
}

#[cfg(not(feature = "no-entrypoint"))]
anchor_lang::solana_program::entrypoint!(process);

pub fn process(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> core::result::Result<(), ProgramError> {
    if data.len() < 8 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let disc: [u8; 8] = data[..8].try_into().unwrap();
    if disc == ACT {
        return act(program_id, accounts, &data[8..]);
    }
    if disc == cd::ON_ATTACH || disc == cd::ON_TRADE {
        return coin_hook(program_id, accounts, disc == cd::ON_TRADE, &data[8..]);
    }
    let slot = SLOTS.iter().position(|s| *s == disc).ok_or(ProgramError::InvalidInstructionData)?;
    if accounts.len() < 4 {
        return Err(ProgramError::NotEnoughAccountKeys);
    }
    // Only the PoolManager may call.
    verify_call(program_id, &accounts[0], &accounts[1])?;

    let cfg = MockConfig::deserialize(&mut &accounts[2].try_borrow_data()?[..]).map_err(|_| ProgramError::InvalidAccountData)?;
    {
        let log = &accounts[3];
        if log.owner != program_id {
            return Err(ProgramError::IllegalOwner);
        }
        let mut l = log.try_borrow_mut_data()?;
        let c = u32::from_le_bytes(l[slot * 4..slot * 4 + 4].try_into().unwrap()) + 1;
        l[slot * 4..slot * 4 + 4].copy_from_slice(&c.to_le_bytes());
        let off = 40 + slot * (4 + LOG_SLOT);
        let n = (data.len() - 8).min(LOG_SLOT);
        l[off..off + 4].copy_from_slice(&(n as u32).to_le_bytes());
        l[off + 4..off + 4 + n].copy_from_slice(&data[8..8 + n]);
    }
    if cfg.revert_mask & (1 << slot) != 0 {
        msg!("mock hook: reverting in slot {}", slot);
        return Err(ProgramError::Custom(0xbad));
    }
    let authority_seeds: &[&[u8]] = &[HOOK_AUTHORITY_SEED];
    let (_, bump) = Pubkey::find_program_address(authority_seeds, program_id);
    if let Some(p) = cfg.pay.as_ref().filter(|p| p.slot as usize == slot) {
        let (from, mint, vault, tp) = (&accounts[p.from as usize], &accounts[p.mint as usize], &accounts[p.vault as usize], &accounts[p.token_program as usize]);
        let authority = accounts.iter().find(|a| *a.key == Pubkey::create_program_address(&[HOOK_AUTHORITY_SEED, &[bump]], program_id).unwrap())
            .ok_or(ProgramError::NotEnoughAccountKeys)?;
        let ix = spl_token_2022::instruction::transfer_checked(tp.key, from.key, mint.key, vault.key, authority.key, &[], p.amount, p.decimals)?;
        invoke_signed(&ix, &[from.clone(), mint.clone(), vault.clone(), authority.clone(), tp.clone()], &[&[HOOK_AUTHORITY_SEED, &[bump]]])?;
    }
    if let Some(s) = cfg.steal.as_ref().filter(|s| s.slot as usize == slot) {
        let (from, to, owner, mint, tp) = (&accounts[s.from as usize], &accounts[s.to as usize], &accounts[s.owner as usize], &accounts[s.mint as usize], &accounts[s.token_program as usize]);
        let ix = spl_token_2022::instruction::transfer_checked(tp.key, from.key, mint.key, to.key, owner.key, &[], s.amount, s.decimals)?;
        anchor_lang::solana_program::program::invoke(&ix, &[from.clone(), mint.clone(), to.clone(), owner.clone(), tp.clone()])?;
    }
    if cfg.silent_mask & (1 << slot) != 0 {
        return Ok(());
    }
    if cfg.garbage_mask & (1 << slot) != 0 {
        set_return_data(&[0xff, 0xff, 0xff]);
        return Ok(());
    }
    let effects = || cfg.effects.iter().find(|(s, _)| *s as usize == slot).map(|(_, e)| e.clone()).unwrap_or_default();
    let ret = match slot {
        0 => return Ok(()),
        6 => borsh::to_vec(&cfg.before_swap),
        7 => borsh::to_vec(&cfg.after_swap),
        3 => borsh::to_vec(&cfg.after_add),
        5 => borsh::to_vec(&cfg.after_remove),
        _ => borsh::to_vec(&effects()),
    }
    .map_err(|_| ProgramError::InvalidAccountData)?;
    set_return_data(&ret);
    Ok(())
}

/// accounts[0] = PoolManager program, accounts[1..] = the inner instruction's accounts.
/// The hook authority PDA among them is marked as a signer.
fn act(program_id: &Pubkey, accounts: &[AccountInfo], inner: &[u8]) -> core::result::Result<(), ProgramError> {
    let (authority, bump) = Pubkey::find_program_address(&[HOOK_AUTHORITY_SEED], program_id);
    let metas = accounts[1..]
        .iter()
        .map(|a| {
            let signer = *a.key == authority || a.is_signer;
            if a.is_writable { AccountMeta::new(*a.key, signer) } else { AccountMeta::new_readonly(*a.key, signer) }
        })
        .collect();
    let ix = Instruction { program_id: *accounts[0].key, accounts: metas, data: inner.to_vec() };
    invoke_signed(&ix, accounts, &[&[HOOK_AUTHORITY_SEED, &[bump]]])
}

/// How the mock behaves as a coin hook: the first byte of the coin's settings.
pub mod coin_mode {
    /// Leave every trade as it is.
    pub const NONE: u8 = 0;
    /// Set every trade's fee to the u16 (little-endian) in bytes 1..3.
    pub const FEE: u8 = 1;
    /// Fail every trade.
    pub const REVERT: u8 = 2;
    /// Return bytes that aren't a trade decision.
    pub const GARBAGE: u8 = 3;
    /// Return nothing.
    pub const SILENT: u8 = 4;
    /// Refuse the settings when the hook is added.
    pub const REFUSE_ATTACH: u8 = 5;
}

/// Tag of the mock's coin hook log line: `Program data: <tag> <args> <[accounts, signer, writable]>`.
pub const COIN_LOG_TAG: &[u8] = b"coin-hook";

fn coin_hook(program_id: &Pubkey, accounts: &[AccountInfo], is_trade: bool, body: &[u8]) -> core::result::Result<(), ProgramError> {
    if accounts.is_empty() {
        return Err(ProgramError::NotEnoughAccountKeys);
    }
    verify_coin_hook_call(program_id, &accounts[0])?;
    anchor_lang::solana_program::log::sol_log_data(&[COIN_LOG_TAG, body, &[accounts.len() as u8, accounts[0].is_signer as u8, accounts[0].is_writable as u8]]);
    let config = if is_trade {
        TradeArgs::deserialize(&mut &body[..]).map_err(|_| ProgramError::InvalidInstructionData)?.config
    } else {
        AttachArgs::deserialize(&mut &body[..]).map_err(|_| ProgramError::InvalidInstructionData)?.config
    };
    let mode = config.first().copied().unwrap_or(coin_mode::NONE);
    if !is_trade {
        return if mode == coin_mode::REFUSE_ATTACH { Err(ProgramError::Custom(0xbad)) } else { Ok(()) };
    }
    let ret = match mode {
        coin_mode::FEE if config.len() >= 3 => TradeReturn { fee_bps: Some(u16::from_le_bytes([config[1], config[2]])) },
        coin_mode::REVERT => return Err(ProgramError::Custom(0xbad)),
        coin_mode::GARBAGE => {
            set_return_data(&[0xff, 0xff, 0xff]);
            return Ok(());
        }
        coin_mode::SILENT => return Ok(()),
        _ => TradeReturn::default(),
    };
    set_return_data(&borsh::to_vec(&ret).map_err(|_| ProgramError::InvalidAccountData)?);
    Ok(())
}
