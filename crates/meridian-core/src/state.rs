//! Account layouts that hooks and clients read. The PoolManager wraps `PoolState` in its
//! zero-copy `Pool` account (8-byte Anchor discriminator + this struct), so any program can
//! decode a pool with `bytemuck::pod_read_unaligned`.

use anchor_lang::prelude::Pubkey;
use bytemuck::{Pod, Zeroable};

/// A u128 stored as two little-endian u64 limbs, so layouts are 8-aligned on every target.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, Pod, Zeroable)]
pub struct PodU128(pub [u64; 2]);

impl PodU128 {
    #[inline]
    pub fn get(&self) -> u128 {
        (self.0[0] as u128) | ((self.0[1] as u128) << 64)
    }
    #[inline]
    pub fn set(&mut self, v: u128) {
        self.0 = [v as u64, (v >> 64) as u64];
    }
    pub fn new(v: u128) -> Self {
        let mut p = Self::default();
        p.set(v);
        p
    }
}

/// An i128 stored as two u64 limbs (two's complement).
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, Pod, Zeroable)]
pub struct PodI128(pub [u64; 2]);

impl PodI128 {
    #[inline]
    pub fn get(&self) -> i128 {
        ((self.0[0] as u128) | ((self.0[1] as u128) << 64)) as i128
    }
    #[inline]
    pub fn set(&mut self, v: i128) {
        let u = v as u128;
        self.0 = [u as u64, (u >> 64) as u64];
    }
}

/// Slot0 + liquidity + fee growth + key + bookkeeping for one pool.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Pod, Zeroable)]
pub struct PoolState {
    pub currency0: Pubkey,
    pub currency1: Pubkey,
    /// Hook program (Pubkey::default() for none).
    pub hooks: Pubkey,
    /// The hook's signing PDA (seeds ["hook"] under the hook program). Operations it signs skip
    /// the hook's own callbacks (v4's noSelfCall).
    pub hook_authority: Pubkey,
    pub vault0: Pubkey,
    pub vault1: Pubkey,
    pub ticks: Pubkey,
    pub token_program0: Pubkey,
    pub token_program1: Pubkey,
    /// Q64.64. Zero means not initialized.
    pub sqrt_price: PodU128,
    /// In-range liquidity.
    pub liquidity: PodU128,
    /// Q64.64 fee growth per unit of liquidity (wrapping).
    pub fee_growth_global0: PodU128,
    pub fee_growth_global1: PodU128,
    pub protocol_fees0: u64,
    pub protocol_fees1: u64,
    /// Outstanding claims (ERC-6909-style balances) backed by this pool's vaults.
    pub claims0: u64,
    pub claims1: u64,
    /// The key's fee: a static LP fee in pips, or DYNAMIC_FEE_FLAG.
    pub fee: u32,
    /// The LP fee in force (for dynamic pools, set by the hook).
    pub lp_fee: u32,
    /// Two 12-bit halves: zero-for-one (low) and one-for-zero (high), in pips.
    pub protocol_fee: u32,
    pub tick_spacing: i32,
    pub tick: i32,
    pub hook_flags: u16,
    pub bump: u8,
    pub authority_bump: u8,
    pub hook_caller_bump: u8,
    pub decimals0: u8,
    pub decimals1: u8,
    pub _reserved: [u8; 69],
}

impl PoolState {
    pub fn key(&self) -> crate::types::PoolKey {
        crate::types::PoolKey {
            currency0: self.currency0,
            currency1: self.currency1,
            fee: self.fee,
            tick_spacing: self.tick_spacing,
            hooks: self.hooks,
        }
    }

    /// Decode from a Pool account's data (discriminator included).
    pub fn from_account_data(data: &[u8]) -> Option<PoolState> {
        let size = core::mem::size_of::<PoolState>();
        if data.len() < 8 + size || data[..8] != crate::POOL_DISCRIMINATOR {
            return None;
        }
        Some(bytemuck::pod_read_unaligned(&data[8..8 + size]))
    }
}

/// Header of a pool's tick store: initialized ticks, sorted ascending, stored inline.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct TicksHeader {
    pub discriminator: [u8; 8],
    pub pool: Pubkey,
    pub len: u32,
    pub capacity: u32,
}

/// v4's TickInfo, keyed by `tick`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
pub struct TickEntry {
    pub tick: i32,
    pub _pad: u32,
    pub liquidity_gross: PodU128,
    pub liquidity_net: PodI128,
    pub fee_growth_outside0: PodU128,
    pub fee_growth_outside1: PodU128,
}

pub const TICKS_DISCRIMINATOR: [u8; 8] = *b"MRDTICKS";
pub const TICKS_HEADER_SIZE: usize = core::mem::size_of::<TicksHeader>();
pub const TICK_ENTRY_SIZE: usize = core::mem::size_of::<TickEntry>();

const _: () = assert!(core::mem::size_of::<PoolState>() == 480);
const _: () = assert!(TICKS_HEADER_SIZE == 48);
const _: () = assert!(TICK_ENTRY_SIZE == 72);
