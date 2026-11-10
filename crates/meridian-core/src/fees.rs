//! LP and protocol fee encoding, identical to v4's LPFeeLibrary and ProtocolFeeLibrary.

/// 100% in pips (hundredths of a basis point).
pub const MAX_LP_FEE: u32 = 1_000_000;
/// A pool key whose fee is exactly this has a dynamic LP fee, set by its hook.
pub const DYNAMIC_FEE_FLAG: u32 = 0x80_0000;
/// Set on a beforeSwap fee override to use it for that swap.
pub const OVERRIDE_FEE_FLAG: u32 = 0x40_0000;
/// 0.1% per direction.
pub const MAX_PROTOCOL_FEE: u32 = 1000;
pub const PIPS_DENOMINATOR: u32 = 1_000_000;

#[inline]
pub fn is_dynamic_fee(fee: u32) -> bool {
    fee == DYNAMIC_FEE_FLAG
}

#[inline]
pub fn is_valid_lp_fee(fee: u32) -> bool {
    fee <= MAX_LP_FEE
}

#[inline]
pub fn is_override(fee: u32) -> bool {
    fee & OVERRIDE_FEE_FLAG != 0
}

#[inline]
pub fn remove_override_flag(fee: u32) -> u32 {
    fee & !OVERRIDE_FEE_FLAG
}

/// Fee charged on zero-for-one swaps (low 12 bits).
#[inline]
pub fn zero_for_one_protocol_fee(protocol_fee: u32) -> u32 {
    protocol_fee & 0xfff
}

/// Fee charged on one-for-zero swaps (high 12 bits).
#[inline]
pub fn one_for_zero_protocol_fee(protocol_fee: u32) -> u32 {
    (protocol_fee >> 12) & 0xfff
}

pub fn is_valid_protocol_fee(protocol_fee: u32) -> bool {
    protocol_fee >> 24 == 0
        && zero_for_one_protocol_fee(protocol_fee) <= MAX_PROTOCOL_FEE
        && one_for_zero_protocol_fee(protocol_fee) <= MAX_PROTOCOL_FEE
}

/// protocolFee + lpFee − protocolFee·lpFee/1e6: the fee taken from the input, in pips.
#[inline]
pub fn calculate_swap_fee(protocol_fee: u32, lp_fee: u32) -> u32 {
    let p = protocol_fee & 0xfff;
    let l = lp_fee & 0xff_ffff;
    (p + l) - ((p as u64 * l as u64) / PIPS_DENOMINATOR as u64) as u32
}
