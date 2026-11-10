//! SwapMath.computeSwapStep from v4, including its sign convention: a negative
//! `amount_remaining` is exact input, a positive one is exact output.

use super::full_math::{mul_div_round_up_u128, mul_div_u128};
use super::sqrt_price_math::*;
use super::MathError;

/// 100% in pips.
pub const MAX_SWAP_FEE: u32 = 1_000_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SwapStep {
    pub sqrt_price_next: u128,
    pub amount_in: u128,
    pub amount_out: u128,
    pub fee_amount: u128,
}

/// The price to move toward this step: the next tick's price, unless the limit comes first.
pub fn get_sqrt_price_target(zero_for_one: bool, sqrt_price_next: u128, sqrt_price_limit: u128) -> u128 {
    if zero_for_one {
        sqrt_price_next.max(sqrt_price_limit)
    } else {
        sqrt_price_next.min(sqrt_price_limit)
    }
}

/// Amount needed to reach a price; `u128::MAX` stands for "more than any token amount".
fn reach(r: Result<u128, MathError>) -> u128 {
    r.unwrap_or(u128::MAX)
}

pub fn compute_swap_step(
    sqrt_price_current: u128,
    sqrt_price_target: u128,
    liquidity: u128,
    amount_remaining: i128,
    fee_pips: u32,
) -> Result<SwapStep, MathError> {
    let zero_for_one = sqrt_price_current >= sqrt_price_target;
    let exact_in = amount_remaining < 0;
    let fee = u128::from(fee_pips);
    let max = u128::from(MAX_SWAP_FEE);
    let mut s = SwapStep::default();

    if exact_in {
        let remaining = amount_remaining.unsigned_abs();
        let less_fee = mul_div_u128(remaining, max - fee, max).ok_or(MathError::Overflow)?;
        let to_target = reach(if zero_for_one {
            get_amount0_delta(sqrt_price_target, sqrt_price_current, liquidity, true)
        } else {
            get_amount1_delta(sqrt_price_current, sqrt_price_target, liquidity, true)
        });
        if less_fee >= to_target {
            s.sqrt_price_next = sqrt_price_target;
            s.amount_in = to_target;
            s.fee_amount = if fee_pips == MAX_SWAP_FEE {
                to_target
            } else {
                mul_div_round_up_u128(to_target, fee, max - fee).ok_or(MathError::Overflow)?
            };
        } else {
            s.amount_in = less_fee;
            s.sqrt_price_next = get_next_sqrt_price_from_input(sqrt_price_current, liquidity, less_fee, zero_for_one)?;
            s.fee_amount = remaining - less_fee;
        }
        s.amount_out = if zero_for_one {
            get_amount1_delta(s.sqrt_price_next, sqrt_price_current, liquidity, false)?
        } else {
            get_amount0_delta(sqrt_price_current, s.sqrt_price_next, liquidity, false)?
        };
    } else {
        let remaining = amount_remaining as u128;
        let to_target = reach(if zero_for_one {
            get_amount1_delta(sqrt_price_target, sqrt_price_current, liquidity, false)
        } else {
            get_amount0_delta(sqrt_price_current, sqrt_price_target, liquidity, false)
        });
        if remaining >= to_target {
            s.sqrt_price_next = sqrt_price_target;
            s.amount_out = to_target;
        } else {
            s.amount_out = remaining;
            s.sqrt_price_next = get_next_sqrt_price_from_output(sqrt_price_current, liquidity, remaining, zero_for_one)?;
        }
        s.amount_in = if zero_for_one {
            get_amount0_delta(s.sqrt_price_next, sqrt_price_current, liquidity, true)?
        } else {
            get_amount1_delta(sqrt_price_current, s.sqrt_price_next, liquidity, true)?
        };
        // A 100% fee can't be paid on exact output; the pool rejects that before stepping.
        s.fee_amount = mul_div_round_up_u128(s.amount_in, fee, max - fee).ok_or(MathError::Overflow)?;
    }
    Ok(s)
}
