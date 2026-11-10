mod common;

use anchor_lang::{solana_program::instruction::Instruction, InstructionData, ToAccountMetas};
use common::*;
use meridian_core::{fees::*, math::*, *};
use meridian_mock_hook::MockConfig;
use meridian_pool_manager::{self as pm, errors::PmError};
use solana_signer::Signer;

fn plain_pool(env: &mut Env, fee: u32, spacing: i32) -> PoolKeys {
    let (a, b) = env.pair(TOKEN, TOKEN);
    env.init(a, b, fee, spacing, NO_HOOK, P1, &[])
}

#[test]
fn initialize_validates_like_v4() {
    let mut env = Env::new();
    let (a, b) = env.pair(TOKEN, TOKEN22);

    // Currencies must be sorted.
    let pk = env.keys(b, a, 3000, 60, NO_HOOK);
    assert_err(env.try_init(&pk, P1, &[]), PmError::CurrenciesOutOfOrderOrEqual);
    // Tick spacing bounds.
    assert_err(env.try_init(&env.keys(a, b, 3000, 0, NO_HOOK), P1, &[]), PmError::TickSpacingTooSmall);
    assert_err(env.try_init(&env.keys(a, b, 3000, 40_000, NO_HOOK), P1, &[]), PmError::TickSpacingTooLarge);
    // A static fee above 100%, and a dynamic fee with no hook.
    assert_err(env.try_init(&env.keys(a, b, 1_000_001, 60, NO_HOOK), P1, &[]), PmError::LPFeeTooLarge);
    assert_err(env.try_init(&env.keys(a, b, DYNAMIC_FEE_FLAG, 60, NO_HOOK), P1, &[]), PmError::HookAddressNotValid);
    // Sqrt price out of range.
    assert_err(env.try_init(&env.keys(a, b, 3000, 60, NO_HOOK), MAX_SQRT_PRICE, &[]), PmError::InvalidSqrtPrice);

    let pk = env.init(a, b, 3000, 60, NO_HOOK, P1, &[]);
    let st = env.state(&pk.pool);
    assert_eq!(st.sqrt_price.get(), P1);
    assert_eq!(st.tick, 0);
    assert_eq!(st.lp_fee, 3000);
    assert_eq!((st.decimals0, st.decimals1), (env.state(&pk.pool).decimals0, env.state(&pk.pool).decimals1));
    assert_eq!(st.token_program0, env.token_program_of(&a));
    assert_eq!(st.token_program1, env.token_program_of(&b));
    // Once only.
    assert!(env.try_init(&pk, P1, &[]).is_err());
    // Same pair, different key: a separate pool, like v4.
    env.init(a, b, 500, 10, NO_HOOK, P1, &[]);
}

#[test]
fn hook_addresses_must_be_valid_for_their_bits() {
    let mut env = Env::new();
    let (a, b) = env.pair(TOKEN, TOKEN);
    // beforeSwapReturnDelta without beforeSwap.
    let bad = env.add_mock(flags::BEFORE_SWAP_RETURNS_DELTA, 1, &MockConfig::default());
    assert_err(env.try_init(&env.keys(a, b, 3000, 60, bad.program), P1, &bad.metas()), PmError::HookAddressNotValid);
    // A hook with no permissions and a static fee.
    let none = env.add_mock(0, 2, &MockConfig::default());
    assert_err(env.try_init(&env.keys(a, b, 3000, 60, none.program), P1, &none.metas()), PmError::HookAddressNotValid);
    // ...but with a dynamic fee it's fine.
    env.init(a, b, DYNAMIC_FEE_FLAG, 60, none.program, P1, &none.metas());
    // A "hook" that isn't a program.
    let fake = hook_address(flags::BEFORE_SWAP, 9);
    assert_err(env.try_init(&env.keys(a, b, 3000, 60, fake), P1, &[]), PmError::HookNotExecutable);
}

#[test]
fn token_2022_mints_with_unsafe_extensions_are_refused() {
    let mut env = Env::new();
    let fee_mint = env.mint_with_transfer_fee();
    let other = env.mint(6, TOKEN);
    let (a, b) = if fee_mint < other { (fee_mint, other) } else { (other, fee_mint) };
    assert_err(env.try_init(&env.keys(a, b, 3000, 60, NO_HOOK), P1, &[]), PmError::UnsupportedMintExtension);
}

#[test]
fn liquidity_amounts_follow_the_range() {
    let mut env = Env::new();
    let pk = plain_pool(&mut env, 3000, 60);
    let lp = env.user(&pk, 1_000_000_000_000);
    let l: i128 = 1_000_000_000;

    // In range: both currencies, and the pool's liquidity rises.
    let (b0, b1) = (env.balance(&lp.t0), env.balance(&lp.t1));
    ok(env.liquidity(&pk, &lp, -600, 600, l, &[]));
    let paid0 = b0 - env.balance(&lp.t0);
    let paid1 = b1 - env.balance(&lp.t1);
    let sa = get_sqrt_price_at_tick(-600).unwrap();
    let sb = get_sqrt_price_at_tick(600).unwrap();
    assert_eq!(paid0 as u128, get_amount0_delta(P1, sb, l as u128, true).unwrap());
    assert_eq!(paid1 as u128, get_amount1_delta(sa, P1, l as u128, true).unwrap());
    assert_eq!(env.state(&pk.pool).liquidity.get(), l as u128);
    assert_eq!(env.balance(&pk.vault0), paid0);

    // Above the price: only currency0; below: only currency1. Pool liquidity unchanged.
    let (b0, b1) = (env.balance(&lp.t0), env.balance(&lp.t1));
    ok(env.liquidity(&pk, &lp, 120, 240, l, &[]));
    assert!(b0 > env.balance(&lp.t0));
    assert_eq!(b1, env.balance(&lp.t1));
    let (b0, b1) = (env.balance(&lp.t0), env.balance(&lp.t1));
    ok(env.liquidity(&pk, &lp, -240, -120, l, &[]));
    assert_eq!(b0, env.balance(&lp.t0));
    assert!(b1 > env.balance(&lp.t1));
    assert_eq!(env.state(&pk.pool).liquidity.get(), l as u128);

    // Six initialized ticks, sorted, with v4's liquidity_net signs.
    let ticks = env.ticks(&pk);
    assert_eq!(ticks.iter().map(|t| t.tick).collect::<Vec<_>>(), vec![-600, -240, -120, 120, 240, 600]);
    assert_eq!(ticks[0].liquidity_net.get(), l);
    assert_eq!(ticks[5].liquidity_net.get(), -l);

    // Misaligned or misordered ranges.
    assert_err(env.liquidity(&pk, &lp, -61, 60, l, &[]), PmError::TickMisaligned);
    assert_err(env.liquidity(&pk, &lp, 60, -60, l, &[]), PmError::TicksMisordered);

    // Removing everything gives back what was paid, less rounding, and clears the ticks.
    ok(env.liquidity(&pk, &lp, -240, -120, -l, &[]));
    ok(env.liquidity(&pk, &lp, 120, 240, -l, &[]));
    ok(env.liquidity(&pk, &lp, -600, 600, -l, &[]));
    assert!(env.ticks(&pk).is_empty());
    assert_eq!(env.state(&pk.pool).liquidity.get(), 0);
    assert!(env.balance(&pk.vault0) <= 3 && env.balance(&pk.vault1) <= 3, "only rounding dust stays");
    // A position can't be poked once empty.
    assert_err(env.liquidity(&pk, &lp, -600, 600, 0, &[]), PmError::CannotUpdateEmptyPosition);
}

#[test]
fn swaps_execute_exactly_as_quoted_and_cross_ticks() {
    let mut env = Env::new();
    let pk = plain_pool(&mut env, 3000, 10);
    let lp = env.user(&pk, 10_000_000_000_000);
    ok(env.liquidity(&pk, &lp, -1000, 1000, 50_000_000_000, &[]));
    ok(env.liquidity(&pk, &lp, -200, 200, 200_000_000_000, &[]));
    ok(env.liquidity(&pk, &lp, 300, 2000, 80_000_000_000, &[]));
    let trader = env.user(&pk, 1_000_000_000_000);

    for (zero_for_one, amount) in [(true, -2_000_000_000i128), (false, -3_000_000_000), (true, 1_000_000_000), (false, 2_000_000_000), (false, -9_000_000_000)] {
        let q = env.quote(&pk, zero_for_one, amount, 0);
        let (b0, b1) = (env.balance(&trader.t0) as i128, env.balance(&trader.t1) as i128);
        ok(env.swap(&pk, &trader, zero_for_one, amount, &[]));
        assert_eq!(env.balance(&trader.t0) as i128 - b0, q.amount0, "currency0 for {zero_for_one} {amount}");
        assert_eq!(env.balance(&trader.t1) as i128 - b1, q.amount1, "currency1 for {zero_for_one} {amount}");
        // Exact output gives exactly the output asked for.
        if amount > 0 {
            assert_eq!(if zero_for_one { q.amount1 } else { q.amount0 }, amount);
        }
    }
    // The big one-for-zero swap crossed 200, 300 and 1000: liquidity is just the 300..2000 range.
    let st = env.state(&pk.pool);
    assert!(st.tick >= 1000, "tick {}", st.tick);
    assert_eq!(st.liquidity.get(), 80_000_000_000);
}

#[test]
fn swap_guards() {
    let mut env = Env::new();
    let pk = plain_pool(&mut env, 3000, 60);
    let lp = env.user(&pk, 10_000_000_000_000);
    ok(env.liquidity(&pk, &lp, -6000, 6000, 1_000_000_000_000, &[]));
    let t = env.user(&pk, 100_000_000_000);
    assert_err(env.swap(&pk, &t, true, 0, &[]), PmError::SwapAmountCannotBeZero);

    // Price limit on the wrong side of the current price.
    let bad = SwapParams { zero_for_one: true, amount_specified: -1000, sqrt_price_limit: P1 + 1 };
    let ix = env.swap_ix(&pk, &t.key(), Some(t.t0), Some(t.t1), bad, Settle::default(), 0, false, false, &[], vec![]);
    let kp = t.kp.insecure_clone();
    assert_err(env.pay(&[ix], &[&kp]), PmError::PriceLimitAlreadyExceeded);
    let bad = SwapParams { zero_for_one: true, amount_specified: -1000, sqrt_price_limit: MIN_SQRT_PRICE };
    let ix = env.swap_ix(&pk, &t.key(), Some(t.t0), Some(t.t1), bad, Settle::default(), 0, false, false, &[], vec![]);
    assert_err(env.pay(&[ix], &[&kp]), PmError::PriceLimitOutOfBounds);

    // A price limit stops the swap part-way, and the trader pays only for what filled.
    let limit = get_sqrt_price_at_tick(-60).unwrap();
    let p = SwapParams { zero_for_one: true, amount_specified: -50_000_000_000, sqrt_price_limit: limit };
    let ix = env.swap_ix(&pk, &t.key(), Some(t.t0), Some(t.t1), p, Settle::default(), 0, false, false, &[], vec![]);
    let b0 = env.balance(&t.t0);
    ok(env.pay(&[ix], &[&kp]));
    assert_eq!(env.state(&pk.pool).sqrt_price.get(), limit);
    let paid = b0 - env.balance(&t.t0);
    assert!(paid < 50_000_000_000 && paid > 2_000_000_000, "paid {paid}");

    // Slippage: minimum out for exact input, maximum in for exact output.
    let q = env.quote(&pk, false, -1_000_000, 0);
    let p = SwapParams { zero_for_one: false, amount_specified: -1_000_000, sqrt_price_limit: MAX_SQRT_PRICE - 1 };
    let ix = env.swap_ix(&pk, &t.key(), Some(t.t0), Some(t.t1), p, Settle::default(), q.amount0 as u64 + 1, false, false, &[], vec![]);
    assert_err(env.pay(&[ix], &[&kp]), PmError::TooLittleReceived);
    let p = SwapParams { zero_for_one: false, amount_specified: 1_000_000, sqrt_price_limit: MAX_SQRT_PRICE - 1 };
    let q = env.quote(&pk, false, 1_000_000, 0);
    let ix = env.swap_ix(&pk, &t.key(), Some(t.t0), Some(t.t1), p, Settle::default(), (-q.amount1) as u64 - 1, false, false, &[], vec![]);
    assert_err(env.pay(&[ix], &[&kp]), PmError::TooMuchRequested);
    let ix = env.swap_ix(&pk, &t.key(), Some(t.t0), Some(t.t1), p, Settle::default(), (-q.amount1) as u64, false, false, &[], vec![]);
    ok(env.pay(&[ix], &[&kp]));
}

#[test]
fn fees_go_to_in_range_liquidity_and_are_paid_on_modify() {
    let mut env = Env::new();
    let pk = plain_pool(&mut env, 10_000, 60); // 1%
    let a = env.user(&pk, 10_000_000_000_000);
    let b = env.user(&pk, 10_000_000_000_000);
    ok(env.liquidity(&pk, &a, -600, 600, 1_000_000_000_000, &[]));
    ok(env.liquidity(&pk, &b, 600, 1200, 1_000_000_000_000, &[])); // out of range
    let t = env.user(&pk, 100_000_000_000);
    ok(env.swap(&pk, &t, true, -1_000_000_000, &[]));
    ok(env.swap(&pk, &t, false, -1_000_000_000, &[]));

    // Poke (zero liquidity delta) pays out the fees: about 1% of each leg's input.
    let (a0, a1) = (env.balance(&a.t0), env.balance(&a.t1));
    ok(env.liquidity(&pk, &a, -600, 600, 0, &[]));
    let fee0 = env.balance(&a.t0) - a0;
    let fee1 = env.balance(&a.t1) - a1;
    assert!((9_999_000..=10_000_000).contains(&fee0), "fee0 {fee0}");
    assert!((9_999_000..=10_000_000).contains(&fee1), "fee1 {fee1}");
    // The out-of-range position earned nothing; poking again pays nothing more.
    let (b0, b1) = (env.balance(&b.t0), env.balance(&b.t1));
    ok(env.liquidity(&pk, &b, 600, 1200, 0, &[]));
    assert_eq!((env.balance(&b.t0), env.balance(&b.t1)), (b0, b1));
    let (a0, a1) = (env.balance(&a.t0), env.balance(&a.t1));
    ok(env.liquidity(&pk, &a, -600, 600, 0, &[]));
    assert_eq!((env.balance(&a.t0), env.balance(&a.t1)), (a0, a1));
}

#[test]
fn donate_pays_in_range_liquidity() {
    let mut env = Env::new();
    let pk = plain_pool(&mut env, 3000, 60);
    let a = env.user(&pk, 10_000_000_000_000);
    let donor = env.user(&pk, 1_000_000_000);
    // No liquidity yet: nothing to donate to.
    assert_err(env.donate(&pk, &donor, 1, 1, &[]), PmError::NoLiquidityToReceiveFees);
    ok(env.liquidity(&pk, &a, -60, 60, 1_000_000_000, &[]));
    ok(env.donate(&pk, &donor, 1_000_000, 2_000_000, &[]));
    assert_eq!(env.balance(&donor.t0), 1_000_000_000 - 1_000_000);
    let (a0, a1) = (env.balance(&a.t0), env.balance(&a.t1));
    ok(env.liquidity(&pk, &a, -60, 60, 0, &[]));
    assert!(env.balance(&a.t0) - a0 >= 999_999);
    assert!(env.balance(&a.t1) - a1 >= 1_999_999);
}

#[test]
fn claims_mint_burn_transfer_and_settle_swaps() {
    let mut env = Env::new();
    let pk = plain_pool(&mut env, 3000, 60);
    let lp = env.user(&pk, 10_000_000_000_000);
    ok(env.liquidity(&pk, &lp, -6000, 6000, 1_000_000_000_000, &[]));
    let t = env.user(&pk, 1_000_000_000);
    let kp = t.kp.insecure_clone();
    let claims = Env::claims_key(&pk, &t.key());

    // Deposit (v4's mint).
    let ix = Instruction {
        program_id: pm::ID,
        accounts: pm::accounts::Deposit {
            payer: env.payer.pubkey(),
            owner: t.key(),
            pool: pk.pool,
            claims,
            vault0: pk.vault0,
            vault1: pk.vault1,
            token0: Some(t.t0),
            token1: Some(t.t1),
            mint0: pk.mint0,
            mint1: pk.mint1,
            token_program0: pk.tp0,
            token_program1: pk.tp1,
            system_program: anchor_lang::system_program::ID,
        }
        .to_account_metas(None),
        data: pm::instruction::Deposit { amount0: 400_000_000, amount1: 0 }.data(),
    };
    ok(env.pay(&[ix], &[&kp]));
    assert_eq!(env.claims(&claims).unwrap().amount0, 400_000_000);
    assert_eq!(env.state(&pk.pool).claims0, 400_000_000);

    // Swap paying with claims and taking the output as claims: no token transfers at all.
    let (b0, b1) = (env.balance(&t.t0), env.balance(&t.t1));
    let q = env.quote(&pk, true, -100_000_000, 0);
    let p = SwapParams { zero_for_one: true, amount_specified: -100_000_000, sqrt_price_limit: MIN_SQRT_PRICE + 1 };
    let ix = env.swap_ix(&pk, &t.key(), None, None, p, Settle { pay_with_claims: true, take_as_claims: true }, 0, true, false, &[], vec![]);
    ok(env.pay(&[ix], &[&kp]));
    assert_eq!((env.balance(&t.t0), env.balance(&t.t1)), (b0, b1));
    let c = env.claims(&claims).unwrap();
    assert_eq!(c.amount0, 300_000_000);
    assert_eq!(c.amount1 as i128, q.amount1);

    // Can't pay with claims you don't have.
    let p = SwapParams { zero_for_one: true, amount_specified: -400_000_000, sqrt_price_limit: MIN_SQRT_PRICE + 1 };
    let ix = env.swap_ix(&pk, &t.key(), None, None, p, Settle { pay_with_claims: true, take_as_claims: true }, 0, true, false, &[], vec![]);
    assert_err(env.pay(&[ix], &[&kp]), PmError::InsufficientClaims);

    // Transfer claims to another owner, who withdraws (v4's burn + take).
    let other = env.user(&pk, 0);
    env.open_claims(&pk, &other.key());
    let other_claims = Env::claims_key(&pk, &other.key());
    let ix = Instruction {
        program_id: pm::ID,
        accounts: pm::accounts::TransferClaims { owner: t.key(), pool: pk.pool, from: claims, to: other_claims }.to_account_metas(None),
        data: pm::instruction::TransferClaims { amount0: 100_000_000, amount1: 0 }.data(),
    };
    ok(env.pay(&[ix], &[&kp]));
    let ix = Instruction {
        program_id: pm::ID,
        accounts: pm::accounts::Withdraw {
            owner: other.key(),
            pool: pk.pool,
            claims: other_claims,
            vault0: pk.vault0,
            vault1: pk.vault1,
            token0: Some(other.t0),
            token1: None,
            mint0: pk.mint0,
            mint1: pk.mint1,
            token_program0: pk.tp0,
            token_program1: pk.tp1,
            authority: pm_pda(&[AUTHORITY_SEED]),
        }
        .to_account_metas(None),
        data: pm::instruction::Withdraw { amount0: 100_000_000, amount1: 0 }.data(),
    };
    let okp = other.kp.insecure_clone();
    ok(env.pay(&[ix.clone()], &[&okp]));
    assert_eq!(env.balance(&other.t0), 100_000_000);
    assert_err(env.pay(&[ix], &[&okp]), PmError::InsufficientClaims);
}

#[test]
fn protocol_fee_is_set_by_the_controller_and_collected() {
    let mut env = Env::new();
    let pk = plain_pool(&mut env, 3000, 60);
    let lp = env.user(&pk, 10_000_000_000_000);
    ok(env.liquidity(&pk, &lp, -6000, 6000, 1_000_000_000_000, &[]));
    let admin = env.admin.insecure_clone();
    let set = |fee: u32, signer: &solana_keypair::Keypair| Instruction {
        program_id: pm::ID,
        accounts: pm::accounts::SetProtocolFee { protocol_fee_controller: signer.pubkey(), config: pm_pda(&[CONFIG_SEED]), pool: pk.pool }.to_account_metas(None),
        data: pm::instruction::SetProtocolFee { protocol_fee: fee }.data(),
    };
    let stranger = solana_keypair::Keypair::new();
    env.svm.airdrop(&stranger.pubkey(), SOL).unwrap();
    assert_err(env.send(&[set(500, &stranger)], &stranger, &[]), PmError::Unauthorized);
    assert_err(env.send(&[set(1001, &admin)], &admin, &[]), PmError::ProtocolFeeTooLarge);
    ok(env.send(&[set(1000 | (1000 << 12), &admin)], &admin, &[]));

    let t = env.user(&pk, 1_000_000_000);
    ok(env.swap(&pk, &t, true, -100_000_000, &[]));
    let st = env.state(&pk.pool);
    // 0.1% of the 100M input, rounded down.
    assert!((99_000..=100_000).contains(&st.protocol_fees0), "{}", st.protocol_fees0);

    let recipient = env.token_account(&pk.mint0, &admin.pubkey());
    let ix = Instruction {
        program_id: pm::ID,
        accounts: pm::accounts::CollectProtocolFees {
            protocol_fee_controller: admin.pubkey(),
            config: pm_pda(&[CONFIG_SEED]),
            pool: pk.pool,
            vault0: pk.vault0,
            vault1: pk.vault1,
            recipient0: Some(recipient),
            recipient1: None,
            mint0: pk.mint0,
            mint1: pk.mint1,
            token_program0: pk.tp0,
            token_program1: pk.tp1,
            authority: pm_pda(&[AUTHORITY_SEED]),
        }
        .to_account_metas(None),
        data: pm::instruction::CollectProtocolFees { amount0: 0, amount1: 0 }.data(),
    };
    ok(env.send(&[ix], &admin, &[]));
    assert_eq!(env.balance(&recipient), st.protocol_fees0);
    assert_eq!(env.state(&pk.pool).protocol_fees0, 0);
}

#[test]
fn only_the_upgrade_authority_initializes_config() {
    let mut env = Env::new();
    // Already initialized by the harness; a second attempt fails.
    let admin = env.admin.insecure_clone();
    let ix = Instruction {
        program_id: pm::ID,
        accounts: pm::accounts::InitializeConfig {
            admin: admin.pubkey(),
            config: pm_pda(&[CONFIG_SEED]),
            program_data: anchor_lang::prelude::Pubkey::find_program_address(&[pm::ID.as_ref()], &anchor_lang::solana_program::bpf_loader_upgradeable::ID).0,
            system_program: anchor_lang::system_program::ID,
        }
        .to_account_metas(None),
        data: pm::instruction::InitializeConfig { protocol_fee_controller: admin.pubkey() }.data(),
    };
    assert!(env.send(&[ix], &admin, &[]).is_err());
}

/// Random liquidity and swaps from many accounts; afterwards everyone exits. Every exit must
/// succeed (the vaults always cover what's owed) and only rounding dust may be left.
#[test]
fn solvency_under_random_activity() {
    use rand::{Rng, SeedableRng};
    let mut rng = rand::rngs::StdRng::seed_from_u64(7);
    let mut env = Env::new();
    let pk = plain_pool(&mut env, 3000, 10);
    let lps: Vec<User> = (0..4).map(|_| env.user(&pk, 1_000_000_000_000_000)).collect();
    let mut ranges = vec![];
    for (i, lp) in lps.iter().enumerate() {
        for _ in 0..3 {
            let lo = rng.gen_range(-200..100) * 10;
            let hi = lo + rng.gen_range(1..150) * 10;
            let l = rng.gen_range(1_000_000_000u64..100_000_000_000) as i128;
            ok(env.liquidity(&pk, lp, lo, hi, l, &[]));
            ranges.push((i, lo, hi, l));
        }
    }
    let traders: Vec<User> = (0..3).map(|_| env.user(&pk, 1_000_000_000_000)).collect();
    for _ in 0..60 {
        let t = &traders[rng.gen_range(0..traders.len())];
        let z = rng.gen_bool(0.5);
        let amt = rng.gen_range(1_000_000i128..3_000_000_000);
        let exact_in = rng.gen_bool(0.6);
        let _ = env.swap(&pk, t, z, if exact_in { -amt } else { amt }, &[]);
    }
    for (i, lo, hi, l) in ranges {
        ok(env.liquidity(&pk, &lps[i], lo, hi, -l, &[]));
    }
    let st = env.state(&pk.pool);
    assert_eq!(st.liquidity.get(), 0);
    assert!(env.ticks(&pk).is_empty());
    assert!(env.balance(&pk.vault0) < 200 && env.balance(&pk.vault1) < 200, "dust {} {}", env.balance(&pk.vault0), env.balance(&pk.vault1));
}
