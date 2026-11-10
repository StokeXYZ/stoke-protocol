use anchor_lang::prelude::*;

#[event]
pub struct Initialize {
    pub pool: Pubkey,
    pub currency0: Pubkey,
    pub currency1: Pubkey,
    pub fee: u32,
    pub tick_spacing: i32,
    pub hooks: Pubkey,
    pub sqrt_price: u128,
    pub tick: i32,
}

#[event]
pub struct ModifyLiquidity {
    pub pool: Pubkey,
    pub sender: Pubkey,
    pub tick_lower: i32,
    pub tick_upper: i32,
    pub liquidity_delta: i128,
    pub salt: [u8; 32],
    /// The owner's delta after any hook delta (positive: paid out to the owner).
    pub amount0: i128,
    pub amount1: i128,
}

#[event]
pub struct Swap {
    pub pool: Pubkey,
    pub sender: Pubkey,
    /// The pool's own swap delta, as v4's Swap event.
    pub amount0: i128,
    pub amount1: i128,
    pub sqrt_price: u128,
    pub liquidity: u128,
    pub tick: i32,
    pub fee: u32,
    /// What the swapper actually paid / received once hook deltas are applied.
    pub swapper_amount0: i128,
    pub swapper_amount1: i128,
}

#[event]
pub struct Donate {
    pub pool: Pubkey,
    pub sender: Pubkey,
    pub amount0: u64,
    pub amount1: u64,
}

#[event]
pub struct LpFeeUpdated {
    pub pool: Pubkey,
    pub lp_fee: u32,
}

#[event]
pub struct ProtocolFeeUpdated {
    pub pool: Pubkey,
    pub protocol_fee: u32,
}
