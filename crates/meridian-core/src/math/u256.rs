//! Fixed-width big integers for the 256/512-bit intermediates of v4's FullMath.
#![allow(clippy::all, clippy::pedantic, missing_docs)]

use uint::construct_uint;

construct_uint! {
    pub struct U256(4);
}

construct_uint! {
    pub struct U512(8);
}

impl U256 {
    #[inline]
    pub fn to_u512(self) -> U512 {
        let mut limbs = [0u64; 8];
        limbs[..4].copy_from_slice(&self.0);
        U512(limbs)
    }

    #[inline]
    pub fn to_u128(self) -> Option<u128> {
        if self.0[2] != 0 || self.0[3] != 0 {
            return None;
        }
        Some((self.0[0] as u128) | ((self.0[1] as u128) << 64))
    }
}

impl U512 {
    #[inline]
    pub fn to_u256(self) -> Option<U256> {
        if self.0[4..].iter().any(|&l| l != 0) {
            return None;
        }
        let mut limbs = [0u64; 4];
        limbs.copy_from_slice(&self.0[..4]);
        Some(U256(limbs))
    }
}
