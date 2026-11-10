//! # Meridian core
//!
//! Uniswap v4's hook model on Solana. This crate is everything a hook developer or client
//! needs without depending on the PoolManager program itself:
//! - [`math`]: v4's TickMath, SqrtPriceMath, SwapMath and LiquidityAmounts in Q64.64.
//! - [`flags`]: the 14 hook permissions, encoded in the hook program's address.
//! - [`fees`]: LP / dynamic / override / protocol fee encoding.
//! - [`types`]: PoolKey, ModifyLiquidityParams, SwapParams, BalanceDelta, BeforeSwapDelta.
//! - [`hook`]: callback discriminators, arguments, return values and [`hook::verify_call`].
//! - [`state`]: the pool and tick-store layouts.
//! - [`coin_hook`]: Stoke v3 coin hooks, the programs a creator adds to a coin (interface and checks).

pub mod coin_hook;
pub mod fees;
pub mod flags;
pub mod hook;
pub mod math;
pub mod state;
pub mod types;

use anchor_lang::prelude::Pubkey;

pub use coin_hook::{
    encode_coin_hook_call, verify_coin_hook_call, AttachArgs, CoinInfo, TradeArgs, TradeReturn, HOOK_LISTING_DISCRIMINATOR, HOOK_LISTING_SEED, LAUNCH_HOOK_ID,
    MAX_COIN_HOOK_CONFIG, MAX_COIN_HOOK_FEE_BPS,
};
pub use fees::*;
pub use hook::*;
pub use state::*;
pub use types::*;

/// The Meridian PoolManager program.
pub const POOL_MANAGER_ID: Pubkey = anchor_lang::pubkey!("3EK8eA2473durhf2oKJzYk827GN56DHEZRMnSbuuBngm");

pub const CONFIG_SEED: &[u8] = b"config";
pub const POOL_SEED: &[u8] = b"pool";
pub const TICKS_SEED: &[u8] = b"ticks";
pub const VAULT_SEED: &[u8] = b"vault";
pub const AUTHORITY_SEED: &[u8] = b"authority";
pub const HOOK_CALLER_SEED: &[u8] = b"hook-caller";
pub const POSITION_SEED: &[u8] = b"position";
pub const CLAIMS_SEED: &[u8] = b"claims";
/// Under the hook program: the PDA a hook signs PoolManager operations with.
pub const HOOK_AUTHORITY_SEED: &[u8] = b"hook";
/// Under the hook program: the account list clients append to a pool's operations.
pub const HOOK_ACCOUNTS_SEED: &[u8] = b"hook-accounts";

/// Anchor discriminators of the PoolManager's accounts (sha256("account:<Name>")[..8]).
pub const POOL_DISCRIMINATOR: [u8; 8] = [241, 154, 109, 4, 17, 177, 109, 188];
pub const POSITION_DISCRIMINATOR: [u8; 8] = [170, 188, 143, 228, 122, 64, 247, 208];
pub const CLAIMS_DISCRIMINATOR: [u8; 8] = [145, 206, 157, 143, 3, 32, 10, 146];

pub const MIN_TICK_SPACING: i32 = 1;
pub const MAX_TICK_SPACING: i32 = i16::MAX as i32;

/// The hook authority PDA for a hook program (Pubkey::default() for no hook).
pub fn hook_authority(hooks: &Pubkey) -> (Pubkey, u8) {
    if *hooks == Pubkey::default() {
        return (Pubkey::default(), 0);
    }
    Pubkey::find_program_address(&[HOOK_AUTHORITY_SEED], hooks)
}

/// v4's tickSpacingToMaxLiquidityPerTick.
pub fn max_liquidity_per_tick(tick_spacing: i32) -> u128 {
    let mut min = math::MIN_TICK / tick_spacing;
    if math::MIN_TICK % tick_spacing != 0 {
        min -= 1;
    }
    let max = math::MAX_TICK / tick_spacing;
    let num_ticks = (max - min + 1) as u128;
    u128::MAX / num_ticks
}

/// The widest range a tick spacing allows: (min usable tick, max usable tick).
pub fn full_range(tick_spacing: i32) -> (i32, i32) {
    let lo = (math::MIN_TICK / tick_spacing) * tick_spacing;
    let hi = (math::MAX_TICK / tick_spacing) * tick_spacing;
    (lo, hi)
}
