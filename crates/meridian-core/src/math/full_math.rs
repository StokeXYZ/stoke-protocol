//! FullMath: a·b/d with a 512-bit intermediate, floor and ceiling variants.

use super::u256::U256;

/// floor(a·b/d). `None` if d == 0 or the result does not fit 256 bits.
pub fn mul_div(a: U256, b: U256, d: U256) -> Option<U256> {
    if d.is_zero() {
        return None;
    }
    let (p, overflow) = a.overflowing_mul(b);
    if !overflow {
        return Some(p / d);
    }
    (a.to_u512() * b.to_u512() / d.to_u512()).to_u256()
}

/// ceil(a·b/d). `None` if d == 0 or the result does not fit 256 bits.
pub fn mul_div_round_up(a: U256, b: U256, d: U256) -> Option<U256> {
    if d.is_zero() {
        return None;
    }
    let (p, overflow) = a.overflowing_mul(b);
    if !overflow {
        let q = p / d;
        return if (p % d).is_zero() { Some(q) } else { q.checked_add(U256::one()) };
    }
    let p = a.to_u512() * b.to_u512();
    let d = d.to_u512();
    let q = p / d;
    let q = if (p % d).is_zero() { q } else { q + 1 };
    q.to_u256()
}

/// ceil(a/d); d must be non-zero.
#[inline]
pub fn div_round_up(a: U256, d: U256) -> U256 {
    let q = a / d;
    if (a % d).is_zero() {
        q
    } else {
        q + 1
    }
}

/// floor(a·b/d) for u128 operands.
#[inline]
pub fn mul_div_u128(a: u128, b: u128, d: u128) -> Option<u128> {
    mul_div(U256::from(a), U256::from(b), U256::from(d))?.to_u128()
}

/// ceil(a·b/d) for u128 operands.
#[inline]
pub fn mul_div_round_up_u128(a: u128, b: u128, d: u128) -> Option<u128> {
    mul_div_round_up(U256::from(a), U256::from(b), U256::from(d))?.to_u128()
}

/// floor(sqrt(x)) for a 256-bit x (Newton's method).
pub fn isqrt(x: U256) -> U256 {
    if x.is_zero() {
        return x;
    }
    let mut z = U256::one() << ((x.bits() + 1) / 2);
    loop {
        let y = (z + x / z) >> 1;
        if y >= z {
            return z;
        }
        z = y;
    }
}
