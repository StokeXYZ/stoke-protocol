use anchor_lang::prelude::Pubkey;
use meridian_core::fees::*;
use meridian_core::flags::*;

fn hook(flags: u16) -> Pubkey {
    let mut b = [7u8; 32];
    b[30] = (flags >> 8) as u8 | 0xc0; // high bits outside the mask are ignored
    b[31] = flags as u8;
    Pubkey::new_from_array(b)
}

#[test]
fn flags_live_in_the_low_14_bits() {
    assert_eq!(hook_flags(&hook(BEFORE_SWAP | AFTER_SWAP)), BEFORE_SWAP | AFTER_SWAP);
    assert_eq!(hook_flags(&hook(ALL_HOOK_MASK)), ALL_HOOK_MASK);
    assert_eq!(hook_flags(&Pubkey::default()), 0);
}

#[test]
fn validity_matches_v4() {
    // A return-delta flag needs its action flag.
    assert!(!is_valid_hook_address(&hook(BEFORE_SWAP_RETURNS_DELTA), 3000));
    assert!(is_valid_hook_address(&hook(BEFORE_SWAP | BEFORE_SWAP_RETURNS_DELTA), 3000));
    assert!(!is_valid_hook_address(&hook(AFTER_SWAP_RETURNS_DELTA), 3000));
    assert!(!is_valid_hook_address(&hook(AFTER_ADD_LIQUIDITY_RETURNS_DELTA), 3000));
    assert!(!is_valid_hook_address(&hook(AFTER_REMOVE_LIQUIDITY_RETURNS_DELTA), 3000));
    // No hook: no dynamic fee.
    assert!(is_valid_hook_address(&Pubkey::default(), 3000));
    assert!(!is_valid_hook_address(&Pubkey::default(), DYNAMIC_FEE_FLAG));
    // A hook needs a flag or a dynamic fee.
    assert!(!is_valid_hook_address(&hook(0), 3000));
    assert!(is_valid_hook_address(&hook(0), DYNAMIC_FEE_FLAG));
}

#[test]
fn fee_libraries() {
    assert!(is_valid_protocol_fee(1000 | (1000 << 12)));
    assert!(!is_valid_protocol_fee(1001));
    assert!(!is_valid_protocol_fee(1001 << 12));
    assert_eq!(calculate_swap_fee(1000, 3000), 3997); // 0.1% + 0.3% − 0.0003%
    assert_eq!(calculate_swap_fee(0, 3000), 3000);
    assert!(is_override(3000 | OVERRIDE_FEE_FLAG));
    assert_eq!(remove_override_flag(3000 | OVERRIDE_FEE_FLAG), 3000);
}
