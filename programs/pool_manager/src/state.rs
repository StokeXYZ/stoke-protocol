use anchor_lang::prelude::*;
use meridian_core::PoolState;

#[account]
#[derive(InitSpace)]
pub struct Config {
    pub admin: Pubkey,
    /// Sets and collects protocol fees (v4's protocolFeeController).
    pub protocol_fee_controller: Pubkey,
    pub bump: u8,
}

/// One pool. The layout is `meridian_core::PoolState`, so hooks decode it without this crate.
#[account(zero_copy)]
pub struct Pool {
    pub state: PoolState,
}

/// v4's Position.State, keyed by (pool, owner, tick_lower, tick_upper, salt).
#[account]
#[derive(InitSpace)]
pub struct Position {
    pub pool: Pubkey,
    pub owner: Pubkey,
    pub tick_lower: i32,
    pub tick_upper: i32,
    pub salt: [u8; 32],
    pub liquidity: u128,
    pub fee_growth_inside0_last: u128,
    pub fee_growth_inside1_last: u128,
    pub bump: u8,
}

/// ERC-6909-style claims on a pool's two currencies, backed by the pool's vaults.
#[account]
#[derive(InitSpace)]
pub struct Claims {
    pub pool: Pubkey,
    pub owner: Pubkey,
    pub amount0: u64,
    pub amount1: u64,
    pub bump: u8,
}
