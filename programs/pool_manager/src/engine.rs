//! v4's Pool library: modifyLiquidity, swap, donate, tick updates and fee growth, over a
//! `PoolState`, its tick store and a position. No accounts, CPIs or hooks here, so hooks'
//! queued actions and the top-level instructions share exactly the same code.

use anchor_lang::prelude::*;
use meridian_core::fees::*;
use meridian_core::math::*;
use meridian_core::{max_liquidity_per_tick, BalanceDelta, PoolState};

use crate::errors::{m, PmError};
use crate::ticks::TicksView;

/// The numeric part of a position (the account adds pool, owner, ticks and salt).
#[derive(Clone, Copy, Debug, Default)]
pub struct PositionData {
    pub liquidity: u128,
    pub fee_growth_inside0_last: u128,
    pub fee_growth_inside1_last: u128,
}

pub struct ModifyResult {
    /// What the owner pays (negative) or receives (positive) for the liquidity change.
    pub principal: BalanceDelta,
    /// Fees the position earned since it was last touched (always >= 0).
    pub fees: BalanceDelta,
}

pub struct SwapResult {
    /// The pool's swap delta, from the swapper's side (positive: paid out to the swapper).
    pub delta: BalanceDelta,
    /// Protocol fee taken from the input currency.
    pub amount_to_protocol: u64,
    /// LP + protocol fee charged, in pips.
    pub swap_fee: u32,
}

fn to_i128(x: u128) -> Result<i128> {
    i128::try_from(x).map_err(|_| error!(PmError::MathOverflow))
}

/// `amount · 2^64 / liquidity`, wrapping like v4's unchecked fee-growth arithmetic.
fn fee_growth_delta(amount: u128, liquidity: u128) -> u128 {
    let x = (U256::from(amount) << 64) / U256::from(liquidity);
    x.low_u128()
}

pub fn check_ticks(tick_lower: i32, tick_upper: i32, tick_spacing: i32) -> Result<()> {
    require!(tick_lower < tick_upper, PmError::TicksMisordered);
    require!(tick_lower >= MIN_TICK, PmError::TickLowerOutOfBounds);
    require!(tick_upper <= MAX_TICK, PmError::TickUpperOutOfBounds);
    require!(tick_lower % tick_spacing == 0 && tick_upper % tick_spacing == 0, PmError::TickMisaligned);
    Ok(())
}

/// v4's updateTick. Returns (flipped, liquidity_gross_after).
fn update_tick(st: &PoolState, ticks: &mut TicksView, tick: i32, liquidity_delta: i128, upper: bool) -> Result<(bool, u128)> {
    let (g0, g1, current) = (st.fee_growth_global0.get(), st.fee_growth_global1.get(), st.tick);
    let info = ticks.get_or_insert(tick)?;
    let gross_before = info.liquidity_gross.get();
    let gross_after = m(add_delta(gross_before, liquidity_delta))?;
    let flipped = (gross_after == 0) != (gross_before == 0);
    if gross_before == 0 && tick <= current {
        // By convention all growth before a tick was initialized happened below it.
        info.fee_growth_outside0.set(g0);
        info.fee_growth_outside1.set(g1);
    }
    let net_before = info.liquidity_net.get();
    let net = if upper { net_before.checked_sub(liquidity_delta) } else { net_before.checked_add(liquidity_delta) }
        .ok_or(PmError::MathOverflow)?;
    info.liquidity_gross.set(gross_after);
    info.liquidity_net.set(net);
    Ok((flipped, gross_after))
}

/// v4's getFeeGrowthInside.
pub fn fee_growth_inside(st: &PoolState, ticks: &TicksView, tick_lower: i32, tick_upper: i32) -> (u128, u128) {
    let lower = ticks.get(tick_lower);
    let upper = ticks.get(tick_upper);
    let (lo0, lo1) = (lower.fee_growth_outside0.get(), lower.fee_growth_outside1.get());
    let (up0, up1) = (upper.fee_growth_outside0.get(), upper.fee_growth_outside1.get());
    let (g0, g1) = (st.fee_growth_global0.get(), st.fee_growth_global1.get());
    if st.tick < tick_lower {
        (lo0.wrapping_sub(up0), lo1.wrapping_sub(up1))
    } else if st.tick >= tick_upper {
        (up0.wrapping_sub(lo0), up1.wrapping_sub(lo1))
    } else {
        (g0.wrapping_sub(lo0).wrapping_sub(up0), g1.wrapping_sub(lo1).wrapping_sub(up1))
    }
}

/// v4's Position.update. Returns fees owed (computed on the liquidity before the change).
fn update_position(pos: &mut PositionData, liquidity_delta: i128, inside0: u128, inside1: u128) -> Result<(u128, u128)> {
    let liquidity = pos.liquidity;
    if liquidity_delta == 0 {
        require!(liquidity != 0, PmError::CannotUpdateEmptyPosition);
    } else {
        pos.liquidity = m(add_delta(liquidity, liquidity_delta))?;
    }
    let q64 = U256::one() << 64;
    let owed0 = mul_div(U256::from(inside0.wrapping_sub(pos.fee_growth_inside0_last)), U256::from(liquidity), q64)
        .and_then(U256::to_u128)
        .ok_or(PmError::MathOverflow)?;
    let owed1 = mul_div(U256::from(inside1.wrapping_sub(pos.fee_growth_inside1_last)), U256::from(liquidity), q64)
        .and_then(U256::to_u128)
        .ok_or(PmError::MathOverflow)?;
    pos.fee_growth_inside0_last = inside0;
    pos.fee_growth_inside1_last = inside1;
    Ok((owed0, owed1))
}

/// v4's Pool.modifyLiquidity.
pub fn modify_liquidity(
    st: &mut PoolState,
    ticks: &mut TicksView,
    pos: &mut PositionData,
    tick_lower: i32,
    tick_upper: i32,
    liquidity_delta: i128,
) -> Result<ModifyResult> {
    check_ticks(tick_lower, tick_upper, st.tick_spacing)?;
    let mut flipped_lower = false;
    let mut flipped_upper = false;
    if liquidity_delta != 0 {
        let (fl, gross_lower) = update_tick(st, ticks, tick_lower, liquidity_delta, false)?;
        let (fu, gross_upper) = update_tick(st, ticks, tick_upper, liquidity_delta, true)?;
        if liquidity_delta >= 0 {
            let max = max_liquidity_per_tick(st.tick_spacing);
            require!(gross_lower <= max && gross_upper <= max, PmError::TickLiquidityOverflow);
        }
        flipped_lower = fl;
        flipped_upper = fu;
    }

    let (inside0, inside1) = fee_growth_inside(st, ticks, tick_lower, tick_upper);
    let (owed0, owed1) = update_position(pos, liquidity_delta, inside0, inside1)?;
    let fees = BalanceDelta::new(to_i128(owed0)?, to_i128(owed1)?);

    if liquidity_delta < 0 {
        if flipped_lower {
            ticks.remove(tick_lower);
        }
        if flipped_upper {
            ticks.remove(tick_upper);
        }
    }
    // Ticks that were only just inserted for an add and flipped stay; ticks a removal never
    // initialized can't exist (add_delta would have underflowed).

    let mut principal = BalanceDelta::ZERO;
    if liquidity_delta != 0 {
        let sp = st.sqrt_price.get();
        let sa = m(get_sqrt_price_at_tick(tick_lower))?;
        let sb = m(get_sqrt_price_at_tick(tick_upper))?;
        if st.tick < tick_lower {
            principal.amount0 = m(get_amount0_delta_signed(sa, sb, liquidity_delta))?;
        } else if st.tick < tick_upper {
            principal.amount0 = m(get_amount0_delta_signed(sp, sb, liquidity_delta))?;
            principal.amount1 = m(get_amount1_delta_signed(sa, sp, liquidity_delta))?;
            let l = m(add_delta(st.liquidity.get(), liquidity_delta))?;
            st.liquidity.set(l);
        } else {
            principal.amount1 = m(get_amount1_delta_signed(sa, sb, liquidity_delta))?;
        }
    }
    Ok(ModifyResult { principal, fees })
}

/// v4's crossTick: flips the outside fee growth and returns liquidity_net.
fn cross_tick(ticks: &mut TicksView, tick: i32, g0: u128, g1: u128) -> i128 {
    match ticks.get_mut(tick) {
        Some(info) => {
            let o0 = g0.wrapping_sub(info.fee_growth_outside0.get());
            let o1 = g1.wrapping_sub(info.fee_growth_outside1.get());
            info.fee_growth_outside0.set(o0);
            info.fee_growth_outside1.set(o1);
            info.liquidity_net.get()
        }
        None => 0,
    }
}

/// v4's Pool.swap. `amount_specified` < 0 is exact input. `lp_fee_override` is a beforeSwap
/// return (only honoured if it carries OVERRIDE_FEE_FLAG).
pub fn swap(
    st: &mut PoolState,
    ticks: &mut TicksView,
    zero_for_one: bool,
    amount_specified: i128,
    sqrt_price_limit: u128,
    lp_fee_override: u32,
) -> Result<SwapResult> {
    let protocol_fee = if zero_for_one { zero_for_one_protocol_fee(st.protocol_fee) } else { one_for_zero_protocol_fee(st.protocol_fee) };
    let lp_fee = if is_override(lp_fee_override) {
        let f = remove_override_flag(lp_fee_override);
        require!(is_valid_lp_fee(f), PmError::LPFeeTooLarge);
        f
    } else {
        st.lp_fee
    };
    let swap_fee = if protocol_fee == 0 { lp_fee } else { calculate_swap_fee(protocol_fee, lp_fee) };
    if swap_fee >= MAX_SWAP_FEE && amount_specified > 0 {
        return err!(PmError::InvalidFeeForExactOut);
    }
    if amount_specified == 0 {
        return Ok(SwapResult { delta: BalanceDelta::ZERO, amount_to_protocol: 0, swap_fee });
    }

    let start_price = st.sqrt_price.get();
    if zero_for_one {
        require!(sqrt_price_limit < start_price, PmError::PriceLimitAlreadyExceeded);
        require!(sqrt_price_limit > MIN_SQRT_PRICE, PmError::PriceLimitOutOfBounds);
    } else {
        require!(sqrt_price_limit > start_price, PmError::PriceLimitAlreadyExceeded);
        require!(sqrt_price_limit < MAX_SQRT_PRICE, PmError::PriceLimitOutOfBounds);
    }

    let exact_out = amount_specified > 0;
    let mut remaining = amount_specified;
    let mut calculated: i128 = 0;
    let mut sqrt_price = start_price;
    let mut tick = st.tick;
    let mut liquidity = st.liquidity.get();
    let mut fee_growth = if zero_for_one { st.fee_growth_global0.get() } else { st.fee_growth_global1.get() };
    let mut to_protocol: u128 = 0;

    while remaining != 0 && sqrt_price != sqrt_price_limit {
        let step_start = sqrt_price;
        let (mut tick_next, initialized) = match ticks.next_initialized(tick, zero_for_one) {
            Some(t) => (t, true),
            None => (if zero_for_one { MIN_TICK } else { MAX_TICK }, false),
        };
        tick_next = tick_next.clamp(MIN_TICK, MAX_TICK);
        let price_next = m(get_sqrt_price_at_tick(tick_next))?;
        let step = m(compute_swap_step(
            sqrt_price,
            get_sqrt_price_target(zero_for_one, price_next, sqrt_price_limit),
            liquidity,
            remaining,
            swap_fee,
        ))?;
        sqrt_price = step.sqrt_price_next;

        let in_plus_fee = step.amount_in.checked_add(step.fee_amount).ok_or(PmError::MathOverflow)?;
        if exact_out {
            remaining = remaining.checked_sub(to_i128(step.amount_out)?).ok_or(PmError::MathOverflow)?;
            calculated = calculated.checked_sub(to_i128(in_plus_fee)?).ok_or(PmError::MathOverflow)?;
        } else {
            remaining = remaining.checked_add(to_i128(in_plus_fee)?).ok_or(PmError::MathOverflow)?;
            calculated = calculated.checked_add(to_i128(step.amount_out)?).ok_or(PmError::MathOverflow)?;
        }

        let mut fee_amount = step.fee_amount;
        if protocol_fee > 0 {
            // Rounds down, in LPs' favour.
            let delta = if swap_fee == protocol_fee {
                fee_amount
            } else {
                in_plus_fee.checked_mul(protocol_fee as u128).ok_or(PmError::MathOverflow)? / PIPS_DENOMINATOR as u128
            };
            fee_amount -= delta;
            to_protocol += delta;
        }
        if liquidity > 0 {
            fee_growth = fee_growth.wrapping_add(fee_growth_delta(fee_amount, liquidity));
        }

        if sqrt_price == price_next {
            if initialized {
                let (g0, g1) = if zero_for_one {
                    (fee_growth, st.fee_growth_global1.get())
                } else {
                    (st.fee_growth_global0.get(), fee_growth)
                };
                let mut net = cross_tick(ticks, tick_next, g0, g1);
                if zero_for_one {
                    net = -net;
                }
                liquidity = m(add_delta(liquidity, net))?;
            }
            tick = if zero_for_one { tick_next - 1 } else { tick_next };
        } else if sqrt_price != step_start {
            tick = m(get_tick_at_sqrt_price(sqrt_price))?;
        }
    }

    st.sqrt_price.set(sqrt_price);
    st.tick = tick;
    st.liquidity.set(liquidity);
    if zero_for_one {
        st.fee_growth_global0.set(fee_growth);
    } else {
        st.fee_growth_global1.set(fee_growth);
    }

    let specified_used = amount_specified.checked_sub(remaining).ok_or(PmError::MathOverflow)?;
    let delta = if zero_for_one != (amount_specified < 0) {
        BalanceDelta::new(calculated, specified_used)
    } else {
        BalanceDelta::new(specified_used, calculated)
    };
    let amount_to_protocol = u64::try_from(to_protocol).map_err(|_| error!(PmError::AmountOverflow))?;
    Ok(SwapResult { delta, amount_to_protocol, swap_fee })
}

/// v4's Pool.donate. Returns the donor's delta (negative).
pub fn donate(st: &mut PoolState, amount0: u64, amount1: u64) -> Result<BalanceDelta> {
    let liquidity = st.liquidity.get();
    require!(liquidity > 0, PmError::NoLiquidityToReceiveFees);
    if amount0 > 0 {
        let g = st.fee_growth_global0.get().wrapping_add(fee_growth_delta(amount0 as u128, liquidity));
        st.fee_growth_global0.set(g);
    }
    if amount1 > 0 {
        let g = st.fee_growth_global1.get().wrapping_add(fee_growth_delta(amount1 as u128, liquidity));
        st.fee_growth_global1.set(g);
    }
    Ok(BalanceDelta::new(-(amount0 as i128), -(amount1 as i128)))
}
