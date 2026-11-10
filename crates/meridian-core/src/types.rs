//! v4's PoolKey, ModifyLiquidityParams, SwapParams, BalanceDelta and BeforeSwapDelta.
//! Sign conventions match v4: a positive delta is owed *to* the party, a negative one is owed
//! *by* it; a negative `amount_specified` is exact input.

use anchor_lang::prelude::*;

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PoolKey {
    pub currency0: Pubkey,
    pub currency1: Pubkey,
    pub fee: u32,
    pub tick_spacing: i32,
    pub hooks: Pubkey,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModifyLiquidityParams {
    pub tick_lower: i32,
    pub tick_upper: i32,
    pub liquidity_delta: i128,
    pub salt: [u8; 32],
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SwapParams {
    pub zero_for_one: bool,
    /// Negative: exact input. Positive: exact output.
    pub amount_specified: i128,
    pub sqrt_price_limit: u128,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BalanceDelta {
    pub amount0: i128,
    pub amount1: i128,
}

impl BalanceDelta {
    pub const ZERO: BalanceDelta = BalanceDelta { amount0: 0, amount1: 0 };

    pub fn new(amount0: i128, amount1: i128) -> Self {
        Self { amount0, amount1 }
    }

    pub fn is_zero(&self) -> bool {
        self.amount0 == 0 && self.amount1 == 0
    }

    pub fn checked_add(self, o: Self) -> Option<Self> {
        Some(Self { amount0: self.amount0.checked_add(o.amount0)?, amount1: self.amount1.checked_add(o.amount1)? })
    }

    pub fn checked_sub(self, o: Self) -> Option<Self> {
        Some(Self { amount0: self.amount0.checked_sub(o.amount0)?, amount1: self.amount1.checked_sub(o.amount1)? })
    }
}

/// What a beforeSwap hook takes from or gives to the swap, in the specified and unspecified
/// currencies, from the hook's side (positive: owed to the hook).
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BeforeSwapDelta {
    pub specified: i128,
    pub unspecified: i128,
}

/// Which side of each settlement uses claims instead of token transfers.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Settle {
    /// Pay what you owe by burning claims instead of transferring tokens in.
    pub pay_with_claims: bool,
    /// Receive what you're owed as claims instead of a token transfer out.
    pub take_as_claims: bool,
}
