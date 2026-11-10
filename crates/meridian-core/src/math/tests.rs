use super::*;

/// Vectors from an independent big-integer reference (Python) of the same algorithm.
const VECTORS: [(i32, u128); 9] = [
    (-443_636, 4_295_048_017),
    (-100_000, 124_324_258_982_887_575),
    (-54_321, 1_220_149_845_509_316_394),
    (-1, 18_445_821_805_675_392_312),
    (0, 18_446_744_073_709_551_616),
    (1, 18_447_666_387_855_959_851),
    (12_345, 34_195_943_348_800_206_621),
    (100_000, 2_737_055_259_406_582_257_881),
    (443_636, 79_226_673_515_401_279_992_447_579_062),
];

#[test]
fn sqrt_price_at_tick_matches_reference() {
    for (t, p) in VECTORS {
        assert_eq!(get_sqrt_price_at_tick(t).unwrap(), p, "tick {t}");
    }
    assert_eq!(get_sqrt_price_at_tick(MIN_TICK).unwrap(), MIN_SQRT_PRICE);
    assert_eq!(get_sqrt_price_at_tick(MAX_TICK).unwrap(), MAX_SQRT_PRICE);
    assert_eq!(get_sqrt_price_at_tick(MIN_TICK - 1), Err(MathError::InvalidTick));
    assert_eq!(get_sqrt_price_at_tick(MAX_TICK + 1), Err(MathError::InvalidTick));
}

#[test]
fn tick_at_sqrt_price_round_trips() {
    let mut t = MIN_TICK;
    while t < MAX_TICK {
        let p = get_sqrt_price_at_tick(t).unwrap();
        assert_eq!(get_tick_at_sqrt_price(p).unwrap(), t, "exact {t}");
        let next = get_sqrt_price_at_tick(t + 1).unwrap();
        assert!(next > p);
        assert_eq!(get_tick_at_sqrt_price(next - 1).unwrap(), t, "just below {}", t + 1);
        t += if (-300..300).contains(&t) || t > MAX_TICK - 300 || t < MIN_TICK + 300 { 1 } else { 97 };
    }
    assert_eq!(get_tick_at_sqrt_price(MIN_SQRT_PRICE - 1), Err(MathError::InvalidSqrtPrice));
    assert_eq!(get_tick_at_sqrt_price(MAX_SQRT_PRICE), Err(MathError::InvalidSqrtPrice));
    assert_eq!(get_tick_at_sqrt_price(MAX_SQRT_PRICE - 1).unwrap(), MAX_TICK - 1);
}

#[test]
fn amount_deltas_round_in_the_pools_favour() {
    let (a, b) = (get_sqrt_price_at_tick(-1000).unwrap(), get_sqrt_price_at_tick(2000).unwrap());
    let l = 1_000_000_000_000u128;
    let d0u = get_amount0_delta(a, b, l, true).unwrap();
    let d0d = get_amount0_delta(a, b, l, false).unwrap();
    assert!(d0u == d0d || d0u == d0d + 1);
    let d1u = get_amount1_delta(a, b, l, true).unwrap();
    let d1d = get_amount1_delta(a, b, l, false).unwrap();
    assert!(d1u == d1d || d1u == d1d + 1);
    // Order of the bounds doesn't matter.
    assert_eq!(get_amount0_delta(b, a, l, true).unwrap(), d0u);
    // Signed: adding owes (negative, rounded up), removing pays (positive, rounded down).
    assert_eq!(get_amount0_delta_signed(a, b, l as i128).unwrap(), -(d0u as i128));
    assert_eq!(get_amount0_delta_signed(a, b, -(l as i128)).unwrap(), d0d as i128);
    // At price 1 over a symmetric-ish range the amounts are close to L·Δ.
    let one = Q64;
    let up = get_sqrt_price_at_tick(10).unwrap();
    let x = get_amount1_delta(one, up, 1_000_000, false).unwrap();
    assert!((499..=500).contains(&x), "{x}");
}

#[test]
fn next_price_from_input_and_output_are_consistent() {
    let p = Q64; // price 1
    let l = 10u128.pow(12);
    // Selling token0 lowers the price; the amount needed to get back is >= what went in.
    let down = get_next_sqrt_price_from_input(p, l, 1_000_000, true).unwrap();
    assert!(down < p);
    let back = get_amount0_delta(down, p, l, true).unwrap();
    assert!(back >= 1_000_000 && back <= 1_000_001, "{back}");
    let up = get_next_sqrt_price_from_input(p, l, 1_000_000, false).unwrap();
    assert!(up > p);
    // Taking out more than the range holds fails instead of wrapping.
    assert!(get_next_sqrt_price_from_output(p, 1, u64::MAX as u128, true).is_err());
}

#[test]
fn swap_step_exact_in_and_out() {
    let p = Q64;
    let target = get_sqrt_price_at_tick(-100).unwrap();
    let l = 10u128.pow(15);
    // Exact in, 0.3% fee, doesn't reach the target.
    let s = compute_swap_step(p, target, l, -1_000_000, 3000).unwrap();
    assert!(s.sqrt_price_next < p && s.sqrt_price_next > target);
    assert_eq!(s.amount_in + s.fee_amount, 1_000_000);
    assert!(s.amount_out < 1_000_000 && s.amount_out > 990_000);
    // Exact in, reaches the target: fee is charged on the input actually used.
    let s = compute_swap_step(p, target, 1_000, -1_000_000_000, 3000).unwrap();
    assert_eq!(s.sqrt_price_next, target);
    assert!(s.amount_in + s.fee_amount <= 1_000_000_000);
    // Exact out, doesn't reach the target.
    let s = compute_swap_step(p, target, l, 1_000_000, 3000).unwrap();
    assert_eq!(s.amount_out, 1_000_000);
    assert!(s.amount_in > 1_000_000);
    assert!(s.fee_amount >= s.amount_in * 3 / 997);
    // Zero liquidity jumps straight to the target for free.
    let s = compute_swap_step(p, target, 0, -1_000, 3000).unwrap();
    assert_eq!((s.sqrt_price_next, s.amount_in, s.amount_out, s.fee_amount), (target, 0, 0, 0));
    // A 100% fee eats exact input.
    let s = compute_swap_step(p, target, l, -1_000, MAX_SWAP_FEE).unwrap();
    assert_eq!((s.amount_in, s.amount_out, s.fee_amount), (0, 0, 1_000));
}

#[test]
fn liquidity_for_amounts_never_needs_more_than_given() {
    let p = get_sqrt_price_at_tick(123).unwrap();
    let (a, b) = (get_sqrt_price_at_tick(-6000).unwrap(), get_sqrt_price_at_tick(6000).unwrap());
    for (x, y) in [(1_000_000u128, 1_000_000u128), (5, 9_000_000_000), (7_777_777_777, 3)] {
        let l = get_liquidity_for_amounts(p, a, b, x, y).unwrap();
        let need0 = get_amount0_delta(p, b, l, true).unwrap();
        let need1 = get_amount1_delta(a, p, l, true).unwrap();
        assert!(need0 <= x + 1 && need1 <= y + 1, "{x} {y} → {need0} {need1}");
    }
}

#[test]
fn sqrt_price_from_ratio_squares_back() {
    let p = sqrt_price_from_ratio(410_814, 1_000_000_000).unwrap();
    let px = price_x64(p).unwrap();
    let want = (410_814u128 << 64) / 1_000_000_000;
    assert!(px <= want && want - px < 1_000_000, "{px} vs {want}");
    assert_eq!(sqrt_price_from_ratio(1, 1).unwrap(), Q64);
}
