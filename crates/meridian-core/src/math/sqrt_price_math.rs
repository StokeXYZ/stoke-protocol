//! SqrtPriceMath in Q64.64 (v4's SqrtPriceMath with a 64-bit fractional part instead of 96).

use super::full_math::{div_round_up, mul_div, mul_div_round_up};
use super::u256::U256;
use super::MathError;

/// Amount of currency0 between two sqrt prices for `liquidity`:
/// L·(sb − sa)·2^64 / (sa·sb).
pub fn get_amount0_delta(mut sa: u128, mut sb: u128, liquidity: u128, round_up: bool) -> Result<u128, MathError> {
    if sa > sb {
        core::mem::swap(&mut sa, &mut sb);
    }
    if sa == 0 {
        return Err(MathError::InvalidSqrtPrice);
    }
    let num1 = U256::from(liquidity) << 64;
    let num2 = U256::from(sb - sa);
    let out = if round_up {
        div_round_up(mul_div_round_up(num1, num2, U256::from(sb)).ok_or(MathError::Overflow)?, U256::from(sa))
    } else {
        mul_div(num1, num2, U256::from(sb)).ok_or(MathError::Overflow)? / U256::from(sa)
    };
    out.to_u128().ok_or(MathError::Overflow)
}

/// Amount of currency1 between two sqrt prices for `liquidity`: L·(sb − sa) / 2^64.
pub fn get_amount1_delta(mut sa: u128, mut sb: u128, liquidity: u128, round_up: bool) -> Result<u128, MathError> {
    if sa > sb {
        core::mem::swap(&mut sa, &mut sb);
    }
    let l = U256::from(liquidity);
    let d = U256::from(sb - sa);
    let q64 = U256::one() << 64;
    let out = if round_up { mul_div_round_up(l, d, q64) } else { mul_div(l, d, q64) };
    out.and_then(U256::to_u128).ok_or(MathError::Overflow)
}

/// Signed amount0 for a liquidity change: negative (owed to the pool, rounded up) when adding.
pub fn get_amount0_delta_signed(sa: u128, sb: u128, liquidity: i128) -> Result<i128, MathError> {
    if liquidity < 0 {
        to_i128(get_amount0_delta(sa, sb, liquidity.unsigned_abs(), false)?)
    } else {
        Ok(-to_i128(get_amount0_delta(sa, sb, liquidity as u128, true)?)?)
    }
}

/// Signed amount1 for a liquidity change: negative (owed to the pool, rounded up) when adding.
pub fn get_amount1_delta_signed(sa: u128, sb: u128, liquidity: i128) -> Result<i128, MathError> {
    if liquidity < 0 {
        to_i128(get_amount1_delta(sa, sb, liquidity.unsigned_abs(), false)?)
    } else {
        Ok(-to_i128(get_amount1_delta(sa, sb, liquidity as u128, true)?)?)
    }
}

fn to_i128(x: u128) -> Result<i128, MathError> {
    i128::try_from(x).map_err(|_| MathError::Overflow)
}

/// Next sqrt price after adding/removing `amount` of currency0, rounded up.
pub fn get_next_sqrt_price_from_amount0_rounding_up(sqrt_price: u128, liquidity: u128, amount: u128, add: bool) -> Result<u128, MathError> {
    if amount == 0 {
        return Ok(sqrt_price);
    }
    let num1 = U256::from(liquidity) << 64;
    let product = U256::from(amount) * U256::from(sqrt_price);
    let denominator = if add {
        num1 + product
    } else {
        if num1 <= product {
            return Err(MathError::PriceOverflow);
        }
        num1 - product
    };
    mul_div_round_up(num1, U256::from(sqrt_price), denominator)
        .and_then(U256::to_u128)
        .ok_or(MathError::PriceOverflow)
}

/// Next sqrt price after adding/removing `amount` of currency1, rounded down.
pub fn get_next_sqrt_price_from_amount1_rounding_down(sqrt_price: u128, liquidity: u128, amount: u128, add: bool) -> Result<u128, MathError> {
    let shifted = U256::from(amount) << 64;
    let l = U256::from(liquidity);
    if add {
        let q = (shifted / l).to_u128().ok_or(MathError::PriceOverflow)?;
        sqrt_price.checked_add(q).ok_or(MathError::PriceOverflow)
    } else {
        let q = div_round_up(shifted, l).to_u128().ok_or(MathError::PriceOverflow)?;
        if sqrt_price <= q {
            return Err(MathError::NotEnoughLiquidity);
        }
        Ok(sqrt_price - q)
    }
}

pub fn get_next_sqrt_price_from_input(sqrt_price: u128, liquidity: u128, amount_in: u128, zero_for_one: bool) -> Result<u128, MathError> {
    if sqrt_price == 0 || liquidity == 0 {
        return Err(MathError::InvalidPriceOrLiquidity);
    }
    if zero_for_one {
        get_next_sqrt_price_from_amount0_rounding_up(sqrt_price, liquidity, amount_in, true)
    } else {
        get_next_sqrt_price_from_amount1_rounding_down(sqrt_price, liquidity, amount_in, true)
    }
}

pub fn get_next_sqrt_price_from_output(sqrt_price: u128, liquidity: u128, amount_out: u128, zero_for_one: bool) -> Result<u128, MathError> {
    if sqrt_price == 0 || liquidity == 0 {
        return Err(MathError::InvalidPriceOrLiquidity);
    }
    if zero_for_one {
        get_next_sqrt_price_from_amount1_rounding_down(sqrt_price, liquidity, amount_out, false)
    } else {
        get_next_sqrt_price_from_amount0_rounding_up(sqrt_price, liquidity, amount_out, false)
    }
}
