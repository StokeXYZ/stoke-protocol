//! Concentrated-liquidity math, ported from Uniswap v4 to Q64.64 fixed point.

pub mod full_math;
pub mod liquidity;
pub mod sqrt_price_math;
pub mod swap_math;
pub mod tick_math;
pub mod u256;

pub use full_math::*;
pub use liquidity::*;
pub use sqrt_price_math::*;
pub use swap_math::*;
pub use tick_math::*;
pub use u256::{U256, U512};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MathError {
    Overflow,
    InvalidTick,
    InvalidSqrtPrice,
    PriceOverflow,
    NotEnoughLiquidity,
    InvalidPriceOrLiquidity,
    LiquidityOverflow,
    LiquidityUnderflow,
}

/// 2^64 as a u128: the Q64.64 one.
pub const Q64: u128 = 1 << 64;

/// sqrt(num/den) in Q64.64, rounded down: the sqrt price of a raw price ratio.
pub fn sqrt_price_from_ratio(num: u128, den: u128) -> Option<u128> {
    if den == 0 {
        return None;
    }
    let x = (U256::from(num) << 128) / U256::from(den);
    isqrt(x).to_u128()
}

/// The ratio a sqrt price stands for, as currency1 per currency0 scaled by 2^64 (Q64.64 price).
pub fn price_x64(sqrt_price: u128) -> Option<u128> {
    mul_div(U256::from(sqrt_price), U256::from(sqrt_price), U256::one() << 64)?.to_u128()
}

#[cfg(test)]
mod tests;
