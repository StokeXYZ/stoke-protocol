//! TickMath in Q64.64. Same algorithm and constants as Uniswap v4's TickMath (which works
//! in Q128.128 internally); only the final shift differs, so prices are sqrt(1.0001^tick)·2^64.
//! The tick range is ±443636 so that every sqrt price fits in a u128.

use super::u256::U256;
use super::MathError;

pub const MIN_TICK: i32 = -443_636;
pub const MAX_TICK: i32 = 443_636;
/// get_sqrt_price_at_tick(MIN_TICK)
pub const MIN_SQRT_PRICE: u128 = 4_295_048_017;
/// get_sqrt_price_at_tick(MAX_TICK)
pub const MAX_SQRT_PRICE: u128 = 79_226_673_515_401_279_992_447_579_062;

/// 2^128 / sqrt(1.0001)^(2^i), exactly as in v4.
const RATIOS: [u128; 20] = [
    0xfffcb933bd6fad37aa2d162d1a594001,
    0xfff97272373d413259a46990580e213a,
    0xfff2e50f5f656932ef12357cf3c7fdcc,
    0xffe5caca7e10e4e61c3624eaa0941cd0,
    0xffcb9843d60f6159c9db58835c926644,
    0xff973b41fa98c081472e6896dfb254c0,
    0xff2ea16466c96a3843ec78b326b52861,
    0xfe5dee046a99a2a811c461f1969c3053,
    0xfcbe86c7900a88aedcffc83b479aa3a4,
    0xf987a7253ac413176f2b074cf7815e54,
    0xf3392b0822b70005940c7a398e4b70f3,
    0xe7159475a2c29b7443b29c7fa6e889d9,
    0xd097f3bdfd2022b8845ad8f792aa5825,
    0xa9f746462d870fdf8a65dc1f90e061e5,
    0x70d869a156d2a1b890bb3df62baf32f7,
    0x31be135f97d08fd981231505542fcfa6,
    0x09aa508b5b7a84e1c677de54f3e99bc9,
    0x005d6af8dedb81196699c329225ee604,
    0x00002216e584f5fa1ea926041bedfe98,
    0x00000000048a170391f7dc42444e8fa2,
];

/// sqrt(1.0001^tick) · 2^64, rounded up.
pub fn get_sqrt_price_at_tick(tick: i32) -> Result<u128, MathError> {
    if !(MIN_TICK..=MAX_TICK).contains(&tick) {
        return Err(MathError::InvalidTick);
    }
    let abs = tick.unsigned_abs();
    let mut ratio = if abs & 1 != 0 { U256::from(RATIOS[0]) } else { U256::one() << 128 };
    for (i, r) in RATIOS.iter().enumerate().skip(1) {
        if abs & (1 << i) != 0 {
            ratio = (ratio * U256::from(*r)) >> 128;
        }
    }
    if tick > 0 {
        ratio = U256::MAX / ratio;
    }
    let round = u128::from(ratio.low_u64() != 0);
    (ratio >> 64).to_u128().map(|p| p + round).ok_or(MathError::Overflow)
}

/// The greatest tick whose sqrt price is <= `sqrt_price`. Requires MIN_SQRT_PRICE <= p < MAX_SQRT_PRICE.
pub fn get_tick_at_sqrt_price(sqrt_price: u128) -> Result<i32, MathError> {
    if !(MIN_SQRT_PRICE..MAX_SQRT_PRICE).contains(&sqrt_price) {
        return Err(MathError::InvalidSqrtPrice);
    }
    let ratio = U256::from(sqrt_price) << 64; // Q128.128
    let msb = ratio.bits() - 1;
    let mut r = if msb >= 128 { ratio >> (msb - 127) } else { ratio << (127 - msb) };
    let mut log_2: i128 = (msb as i128 - 128) << 64;
    for shift in (50..64).rev() {
        r = (r * r) >> 127;
        let f = (r >> 128).low_u64();
        log_2 |= (f as i128) << shift;
        r >>= f as usize;
    }
    // log_sqrt10001 = log_2 · 255738958999603826347141 as a signed 256-bit number (two's complement).
    let log_sqrt10001 = twos(log_2).overflowing_mul(U256::from(255_738_958_999_603_826_347_141u128)).0;
    let tick_low = sar128(log_sqrt10001.overflowing_sub(U256::from(3_402_992_956_809_132_418_596_140_100_660_247_210u128)).0);
    let tick_high = sar128(log_sqrt10001.overflowing_add(U256::from(291_339_464_771_989_622_907_027_621_153_398_088_495u128)).0);
    if tick_low == tick_high {
        Ok(tick_low)
    } else if get_sqrt_price_at_tick(tick_high)? <= sqrt_price {
        Ok(tick_high)
    } else {
        Ok(tick_low)
    }
}

fn twos(x: i128) -> U256 {
    if x >= 0 {
        U256::from(x as u128)
    } else {
        (!U256::from(x.unsigned_abs())).overflowing_add(U256::one()).0
    }
}

/// Arithmetic shift right by 128, truncated to i32.
fn sar128(x: U256) -> i32 {
    let mut s = x >> 128;
    if x.bit(255) {
        s = s | (U256::MAX << 128);
    }
    s.low_u64() as i64 as i32
}
