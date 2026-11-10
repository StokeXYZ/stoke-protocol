//! Hook permissions, encoded in the hook program's address exactly as Uniswap v4 encodes them
//! in the hook contract's address: the low 14 bits of the last two bytes of the program id.
//! A program id is fixed at deploy time, so a hook can never gain or drop a permission.

use anchor_lang::prelude::Pubkey;

use crate::fees::is_dynamic_fee;

pub const BEFORE_INITIALIZE: u16 = 1 << 13;
pub const AFTER_INITIALIZE: u16 = 1 << 12;
pub const BEFORE_ADD_LIQUIDITY: u16 = 1 << 11;
pub const AFTER_ADD_LIQUIDITY: u16 = 1 << 10;
pub const BEFORE_REMOVE_LIQUIDITY: u16 = 1 << 9;
pub const AFTER_REMOVE_LIQUIDITY: u16 = 1 << 8;
pub const BEFORE_SWAP: u16 = 1 << 7;
pub const AFTER_SWAP: u16 = 1 << 6;
pub const BEFORE_DONATE: u16 = 1 << 5;
pub const AFTER_DONATE: u16 = 1 << 4;
pub const BEFORE_SWAP_RETURNS_DELTA: u16 = 1 << 3;
pub const AFTER_SWAP_RETURNS_DELTA: u16 = 1 << 2;
pub const AFTER_ADD_LIQUIDITY_RETURNS_DELTA: u16 = 1 << 1;
pub const AFTER_REMOVE_LIQUIDITY_RETURNS_DELTA: u16 = 1;
pub const ALL_HOOK_MASK: u16 = (1 << 14) - 1;

/// The permission bits a hook program's address encodes.
pub fn hook_flags(hooks: &Pubkey) -> u16 {
    let b = hooks.to_bytes();
    u16::from_be_bytes([b[30], b[31]]) & ALL_HOOK_MASK
}

#[inline]
pub fn has(flags: u16, flag: u16) -> bool {
    flags & flag != 0
}

/// v4's Hooks.isValidHookAddress: a return-delta flag needs its action flag; no hook means no
/// dynamic fee; a hook must have at least one flag or a dynamic fee.
pub fn is_valid_hook_address(hooks: &Pubkey, fee: u32) -> bool {
    let f = hook_flags(hooks);
    if !has(f, BEFORE_SWAP) && has(f, BEFORE_SWAP_RETURNS_DELTA) {
        return false;
    }
    if !has(f, AFTER_SWAP) && has(f, AFTER_SWAP_RETURNS_DELTA) {
        return false;
    }
    if !has(f, AFTER_ADD_LIQUIDITY) && has(f, AFTER_ADD_LIQUIDITY_RETURNS_DELTA) {
        return false;
    }
    if !has(f, AFTER_REMOVE_LIQUIDITY) && has(f, AFTER_REMOVE_LIQUIDITY_RETURNS_DELTA) {
        return false;
    }
    if *hooks == Pubkey::default() {
        !is_dynamic_fee(fee)
    } else {
        f > 0 || is_dynamic_fee(fee)
    }
}

/// Human-readable names, in v4's bit order (highest first).
pub const FLAG_NAMES: [(u16, &str); 14] = [
    (BEFORE_INITIALIZE, "beforeInitialize"),
    (AFTER_INITIALIZE, "afterInitialize"),
    (BEFORE_ADD_LIQUIDITY, "beforeAddLiquidity"),
    (AFTER_ADD_LIQUIDITY, "afterAddLiquidity"),
    (BEFORE_REMOVE_LIQUIDITY, "beforeRemoveLiquidity"),
    (AFTER_REMOVE_LIQUIDITY, "afterRemoveLiquidity"),
    (BEFORE_SWAP, "beforeSwap"),
    (AFTER_SWAP, "afterSwap"),
    (BEFORE_DONATE, "beforeDonate"),
    (AFTER_DONATE, "afterDonate"),
    (BEFORE_SWAP_RETURNS_DELTA, "beforeSwapReturnDelta"),
    (AFTER_SWAP_RETURNS_DELTA, "afterSwapReturnDelta"),
    (AFTER_ADD_LIQUIDITY_RETURNS_DELTA, "afterAddLiquidityReturnDelta"),
    (AFTER_REMOVE_LIQUIDITY_RETURNS_DELTA, "afterRemoveLiquidityReturnDelta"),
];
