//! The hook interface: what the PoolManager sends a hook and what a hook may send back.
//!
//! Every callback is an Anchor-compatible instruction (discriminator = sha256("global:<name>")[..8],
//! then the Borsh-encoded arguments below), so an Anchor hook implements them as ordinary
//! instructions, e.g.
//!
//! ```ignore
//! pub fn before_swap(ctx: Context<HookCall>, sender: Pubkey, key: PoolKey, params: SwapParams,
//!                    hook_data: Vec<u8>) -> Result<BeforeSwapReturn>
//! ```
//!
//! Accounts: `[hook_caller (signer), pool (read-only), ...remaining accounts of the operation]`.
//! `hook_caller` is a PoolManager PDA (["hook-caller", pool]) that signs only hook calls, so a
//! hook can prove the PoolManager called it with [`verify_call`]. It has no authority over funds.
//! The trader's signature is never forwarded: every remaining account arrives as a non-signer.
//!
//! Solana forbids a program from re-entering a caller that is still on the stack, so a hook
//! can't call the PoolManager from inside a callback the way a v4 hook calls `take`, `settle`,
//! `mint`, `burn`, `modifyLiquidity`, `swap`, `donate` or `updateDynamicLPFee`. Instead:
//! - **Deltas** (return values) are settled against the hook's claims at the end of the
//!   operation (v4's mint/burn of ERC-6909 claims). A hook may also pay a vault directly during
//!   a callback; whatever lands in the vault is credited to it (v4's sync/settle).
//! - **Effects** let a callback set the dynamic LP fee and queue ModifyLiquidity / Swap / Donate
//!   on its own pool. The PoolManager runs them as the hook, right after the callback returns
//!   and before it continues, which is the same point a v4 hook's nested call would run.
//! A callback that returns nothing (`Result<()>`) gets zero deltas and no effects.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::account_info::AccountInfo;

use crate::state::PoolState;
use crate::types::*;

pub mod discriminator {
    pub const BEFORE_INITIALIZE: [u8; 8] = [34, 92, 163, 44, 187, 213, 131, 15];
    pub const AFTER_INITIALIZE: [u8; 8] = [130, 194, 188, 9, 34, 6, 71, 141];
    pub const BEFORE_ADD_LIQUIDITY: [u8; 8] = [114, 27, 123, 181, 200, 252, 178, 240];
    pub const AFTER_ADD_LIQUIDITY: [u8; 8] = [124, 24, 168, 110, 143, 56, 35, 14];
    pub const BEFORE_REMOVE_LIQUIDITY: [u8; 8] = [66, 129, 139, 63, 196, 84, 231, 221];
    pub const AFTER_REMOVE_LIQUIDITY: [u8; 8] = [238, 138, 28, 220, 35, 167, 8, 42];
    pub const BEFORE_SWAP: [u8; 8] = [227, 59, 240, 68, 164, 9, 29, 254];
    pub const AFTER_SWAP: [u8; 8] = [235, 215, 232, 183, 152, 109, 5, 35];
    pub const BEFORE_DONATE: [u8; 8] = [118, 183, 52, 95, 183, 213, 86, 210];
    pub const AFTER_DONATE: [u8; 8] = [10, 154, 106, 6, 144, 160, 146, 2];
}

// ------------------------------------------------------------------ call arguments

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, PartialEq)]
pub struct BeforeInitializeArgs {
    pub sender: Pubkey,
    pub key: PoolKey,
    pub sqrt_price: u128,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, PartialEq)]
pub struct AfterInitializeArgs {
    pub sender: Pubkey,
    pub key: PoolKey,
    pub sqrt_price: u128,
    pub tick: i32,
}

/// before_add_liquidity / before_remove_liquidity.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, PartialEq)]
pub struct BeforeModifyLiquidityArgs {
    pub sender: Pubkey,
    pub key: PoolKey,
    pub params: ModifyLiquidityParams,
    pub hook_data: Vec<u8>,
}

/// after_add_liquidity / after_remove_liquidity. `delta` is the caller's delta (principal + fees).
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, PartialEq)]
pub struct AfterModifyLiquidityArgs {
    pub sender: Pubkey,
    pub key: PoolKey,
    pub params: ModifyLiquidityParams,
    pub delta: BalanceDelta,
    pub fees_accrued: BalanceDelta,
    pub hook_data: Vec<u8>,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, PartialEq)]
pub struct BeforeSwapArgs {
    pub sender: Pubkey,
    pub key: PoolKey,
    pub params: SwapParams,
    pub hook_data: Vec<u8>,
}

/// `delta` is the pool's swap delta, before any hook delta is applied.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, PartialEq)]
pub struct AfterSwapArgs {
    pub sender: Pubkey,
    pub key: PoolKey,
    pub params: SwapParams,
    pub delta: BalanceDelta,
    pub hook_data: Vec<u8>,
}

/// before_donate / after_donate.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, PartialEq)]
pub struct DonateArgs {
    pub sender: Pubkey,
    pub key: PoolKey,
    pub amount0: u64,
    pub amount1: u64,
    pub hook_data: Vec<u8>,
}

// ------------------------------------------------------------------ return values

/// Work a hook asks the PoolManager to do on its own pool, as the hook, right after the callback.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, PartialEq)]
pub enum HookAction {
    /// modifyLiquidity on an existing position the hook owns (its ticks/salt come from the account,
    /// which must be among the operation's remaining accounts).
    ModifyLiquidity { position: Pubkey, liquidity_delta: i128 },
    /// swap on this pool; the hook's own callbacks are skipped.
    Swap { params: SwapParams },
    /// donate to in-range LPs.
    Donate { amount0: u64, amount1: u64 },
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, Default, PartialEq)]
pub struct HookEffects {
    /// updateDynamicLPFee (dynamic-fee pools only).
    pub lp_fee: Option<u32>,
    pub actions: Vec<HookAction>,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, Default, PartialEq)]
pub struct BeforeSwapReturn {
    pub delta: BeforeSwapDelta,
    /// A fee with OVERRIDE_FEE_FLAG set replaces the LP fee for this swap (dynamic pools only).
    pub lp_fee_override: u32,
    pub effects: HookEffects,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, Default, PartialEq)]
pub struct AfterSwapReturn {
    /// Taken from (positive) or given to (negative) the swapper in the unspecified currency.
    pub unspecified_delta: i128,
    pub effects: HookEffects,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, Default, PartialEq)]
pub struct AfterModifyLiquidityReturn {
    pub delta: BalanceDelta,
    pub effects: HookEffects,
}

/// Instruction data for a hook callback.
pub fn encode_call<T: AnchorSerialize>(discriminator: [u8; 8], args: &T) -> Vec<u8> {
    let mut data = Vec::with_capacity(256);
    data.extend_from_slice(&discriminator);
    args.serialize(&mut data).expect("serialize hook call");
    data
}

/// For hooks: checks that `caller` is the PoolManager's hook-caller signer for `pool`, and that
/// `pool` is a PoolManager pool whose hook is `hook_program_id`. Returns the pool's state.
pub fn verify_call(hook_program_id: &Pubkey, caller: &AccountInfo, pool: &AccountInfo) -> core::result::Result<PoolState, ProgramError> {
    if pool.owner != &crate::POOL_MANAGER_ID {
        return Err(ProgramError::IllegalOwner);
    }
    let state = PoolState::from_account_data(&pool.try_borrow_data()?).ok_or(ProgramError::InvalidAccountData)?;
    if state.hooks != *hook_program_id {
        return Err(ProgramError::InvalidAccountData);
    }
    if !caller.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    let expected = Pubkey::create_program_address(
        &[crate::HOOK_CALLER_SEED, pool.key.as_ref(), &[state.hook_caller_bump]],
        &crate::POOL_MANAGER_ID,
    )
    .map_err(|_| ProgramError::InvalidSeeds)?;
    if *caller.key != expected {
        return Err(ProgramError::MissingRequiredSignature);
    }
    Ok(state)
}

// ------------------------------------------------------------------ account resolution

/// A hook can publish the extra accounts its callbacks need for a pool in a PDA it owns at
/// seeds ["hook-accounts", pool], so any client can build a working transaction:
/// `MAGIC | count: u8 | count × (pubkey: [u8; 32], writable: u8)`.
pub const HOOK_ACCOUNTS_MAGIC: [u8; 8] = *b"MRDNHKAC";

pub fn encode_hook_accounts(accounts: &[(Pubkey, bool)]) -> Vec<u8> {
    let mut v = Vec::with_capacity(9 + accounts.len() * 33);
    v.extend_from_slice(&HOOK_ACCOUNTS_MAGIC);
    v.push(accounts.len() as u8);
    for (k, w) in accounts {
        v.extend_from_slice(k.as_ref());
        v.push(*w as u8);
    }
    v
}

pub fn decode_hook_accounts(data: &[u8]) -> Option<Vec<(Pubkey, bool)>> {
    if data.len() < 9 || data[..8] != HOOK_ACCOUNTS_MAGIC {
        return None;
    }
    let n = data[8] as usize;
    if data.len() < 9 + n * 33 {
        return None;
    }
    Some((0..n).map(|i| {
        let o = 9 + i * 33;
        (Pubkey::new_from_array(data[o..o + 32].try_into().unwrap()), data[o + 32] != 0)
    }).collect())
}

pub fn hook_accounts_space(n: usize) -> usize {
    9 + n * 33
}
