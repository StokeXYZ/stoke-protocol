//! LiquidityMath and the periphery's LiquidityAmounts (Q64.64).

use super::full_math::mul_div;
use super::sqrt_price_math::{get_amount0_delta, get_amount1_delta};
use super::u256::U256;
use super::MathError;

pub fn add_delta(x: u128, y: i128) -> Result<u128, MathError> {
    if y < 0 {
        x.checked_sub(y.unsigned_abs()).ok_or(MathError::LiquidityUnderflow)
    } else {
        x.checked_add(y as u128).ok_or(MathError::LiquidityOverflow)
    }
}

fn sort(a: u128, b: u128) -> (u128, u128) {
    if a > b { (b, a) } else { (a, b) }
}

/// Liquidity a range [sa, sb] gets for `amount0` of currency0.
pub fn get_liquidity_for_amount0(sa: u128, sb: u128, amount0: u128) -> Result<u128, MathError> {
    let (sa, sb) = sort(sa, sb);
    let intermediate = mul_div(U256::from(sa), U256::from(sb), U256::one() << 64).ok_or(MathError::Overflow)?;
    mul_div(U256::from(amount0), intermediate, U256::from(sb - sa))
        .and_then(U256::to_u128)
        .ok_or(MathError::Overflow)
}

/// Liquidity a range [sa, sb] gets for `amount1` of currency1.
pub fn get_liquidity_for_amount1(sa: u128, sb: u128, amount1: u128) -> Result<u128, MathError> {
    let (sa, sb) = sort(sa, sb);
    mul_div(U256::from(amount1), U256::one() << 64, U256::from(sb - sa))
        .and_then(U256::to_u128)
        .ok_or(MathError::Overflow)
}

/// The most liquidity [sa, sb] can get at `sqrt_price` without exceeding either amount.
pub fn get_liquidity_for_amounts(sqrt_price: u128, sa: u128, sb: u128, amount0: u128, amount1: u128) -> Result<u128, MathError> {
    let (sa, sb) = sort(sa, sb);
    if sqrt_price <= sa {
        get_liquidity_for_amount0(sa, sb, amount0)
    } else if sqrt_price < sb {
        Ok(get_liquidity_for_amount0(sqrt_price, sb, amount0)?.min(get_liquidity_for_amount1(sa, sqrt_price, amount1)?))
    } else {
        get_liquidity_for_amount1(sa, sb, amount1)
    }
}

/// Token amounts a position of `liquidity` over [sa, sb] holds at `sqrt_price` (rounded down).
pub fn get_amounts_for_liquidity(sqrt_price: u128, sa: u128, sb: u128, liquidity: u128) -> Result<(u128, u128), MathError> {
    let (sa, sb) = sort(sa, sb);
    if sqrt_price <= sa {
        Ok((get_amount0_delta(sa, sb, liquidity, false)?, 0))
    } else if sqrt_price < sb {
        Ok((get_amount0_delta(sqrt_price, sb, liquidity, false)?, get_amount1_delta(sa, sqrt_price, liquidity, false)?))
    } else {
        Ok((0, get_amount1_delta(sa, sb, liquidity, false)?))
    }
}
