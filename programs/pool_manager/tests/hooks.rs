//! Uniswap v4 hook semantics, one capability at a time, against the mock hook.

mod common;

use anchor_lang::{
    prelude::Pubkey,
    solana_program::instruction::{AccountMeta, Instruction},
    InstructionData, ToAccountMetas,
};
use common::*;
use meridian_core::{fees::*, flags::*, math::*, *};
use meridian_mock_hook::{MockConfig, Pay, Steal};
use meridian_pool_manager::{self as pm, errors::PmError};
use solana_signer::Signer;

const ALL_ACTIONS: [u16; 10] = [
    BEFORE_INITIALIZE,
    AFTER_INITIALIZE,
    BEFORE_ADD_LIQUIDITY,
    AFTER_ADD_LIQUIDITY,
    BEFORE_REMOVE_LIQUIDITY,
    AFTER_REMOVE_LIQUIDITY,
    BEFORE_SWAP,
    AFTER_SWAP,
    BEFORE_DONATE,
    AFTER_DONATE,
];

fn slot(flag: u16) -> usize {
    13 - flag.trailing_zeros() as usize
}

struct Hooked {
    env: Env,
    pk: PoolKeys,
    mock: Mock,
}

fn hooked(flags: u16, fee: u32, cfg: MockConfig) -> Hooked {
    let mut env = Env::new();
    let (a, b) = env.pair(TOKEN, TOKEN);
    let mock = env.add_mock(flags, 1, &cfg);
    let pk = env.init(a, b, fee, 10, mock.program, P1, &mock.metas());
    Hooked { env, pk, mock }
}

/// Give the hook claims to settle deltas with: tokens owned by its authority, deposited as it.
fn fund_hook(h: &mut Hooked, amount0: u64, amount1: u64) {
    let auth = h.pk.hook_authority;
    let t0 = h.env.token_account(&h.pk.mint0, &auth);
    let t1 = h.env.token_account(&h.pk.mint1, &auth);
    h.env.mint_to(&h.pk.mint0, &t0, amount0);
    h.env.mint_to(&h.pk.mint1, &t1, amount1);
    let ix = Instruction {
        program_id: pm::ID,
        accounts: pm::accounts::Deposit {
            payer: h.env.payer.pubkey(),
            owner: auth,
            pool: h.pk.pool,
            claims: h.pk.hook_claims,
            vault0: h.pk.vault0,
            vault1: h.pk.vault1,
            token0: Some(t0),
            token1: Some(t1),
            mint0: h.pk.mint0,
            mint1: h.pk.mint1,
            token_program0: h.pk.tp0,
            token_program1: h.pk.tp1,
            system_program: anchor_lang::system_program::ID,
        }
        .to_account_metas(None),
        data: pm::instruction::Deposit { amount0, amount1 }.data(),
    };
    let mock = h.mock;
    ok(h.env.act(&mock, ix, &[]));
}

fn hook_claims(h: &Hooked) -> (u64, u64) {
    h.env.claims(&h.pk.hook_claims).map(|c| (c.amount0, c.amount1)).unwrap_or((0, 0))
}

fn delta_of(env: &Env, u: &User, f: impl FnOnce()) -> (i128, i128) {
    let _ = (env, u);
    f();
    (0, 0)
}

#[test]
fn every_permission_triggers_exactly_its_callback() {
    for flag in ALL_ACTIONS {
        let mut h = hooked(flag, 3000, MockConfig::default());
        let m = h.mock.metas();
        let lp = h.env.user(&h.pk, 1_000_000_000_000);
        ok(h.env.liquidity(&h.pk, &lp, -600, 600, 1_000_000_000, &m));
        let t = h.env.user(&h.pk, 1_000_000_000);
        ok(h.env.swap(&h.pk, &t, true, -1_000_000, &m));
        ok(h.env.donate(&h.pk, &t, 1_000, 1_000, &m));
        ok(h.env.liquidity(&h.pk, &lp, -600, 600, 0, &m)); // a poke counts as a removal, as in v4
        ok(h.env.liquidity(&h.pk, &lp, -600, 600, -1_000_000_000, &m));

        let mut want = [0u32; 10];
        want[slot(flag)] = if flag == BEFORE_REMOVE_LIQUIDITY || flag == AFTER_REMOVE_LIQUIDITY { 2 } else { 1 };
        assert_eq!(h.env.all_calls(&h.mock), want, "flag {flag:#06x}");
    }
}

#[test]
fn callbacks_receive_v4s_arguments() {
    let all = ALL_ACTIONS.iter().fold(0, |a, f| a | f);
    let mut h = hooked(all, 3000, MockConfig::default());
    let m = h.mock.metas();
    let key = h.env.state(&h.pk.pool).key();

    let init: AfterInitializeArgs = h.env.last_call(&h.mock, 1);
    assert_eq!((init.sender, init.key, init.sqrt_price, init.tick), (h.env.payer.pubkey(), key, P1, 0));

    let lp = h.env.user(&h.pk, 1_000_000_000_000);
    ok(h.env.liquidity(&h.pk, &lp, -600, 600, 1_000_000_000, &m));
    let add: AfterModifyLiquidityArgs = h.env.last_call(&h.mock, 3);
    assert_eq!(add.sender, lp.key());
    assert_eq!((add.params.tick_lower, add.params.tick_upper, add.params.liquidity_delta), (-600, 600, 1_000_000_000));
    assert!(add.delta.amount0 < 0 && add.delta.amount1 < 0);
    assert!(add.fees_accrued.is_zero());

    let t = h.env.user(&h.pk, 1_000_000_000);
    let p = SwapParams { zero_for_one: false, amount_specified: 777_777, sqrt_price_limit: MAX_SQRT_PRICE - 1 };
    let q = h.env.quote(&h.pk, false, 777_777, 0);
    let ix = h.env.swap_ix(&h.pk, &t.key(), Some(t.t0), Some(t.t1), p, Settle::default(), u64::MAX, false, false, &m, b"gm from the router".to_vec());
    let kp = t.kp.insecure_clone();
    ok(h.env.pay(&[ix], &[&kp]));
    let before: BeforeSwapArgs = h.env.last_call(&h.mock, 6);
    assert_eq!((before.sender, before.key, before.params), (t.key(), key, p));
    assert_eq!(before.hook_data, b"gm from the router");
    let after: AfterSwapArgs = h.env.last_call(&h.mock, 7);
    assert_eq!(after.delta, q);
    assert_eq!(after.hook_data, b"gm from the router");

    ok(h.env.donate(&h.pk, &t, 5, 6, &m));
    let d: DonateArgs = h.env.last_call(&h.mock, 9);
    assert_eq!((d.amount0, d.amount1, d.sender), (5, 6, t.key()));
}

#[test]
fn hook_initiated_operations_skip_its_own_callbacks() {
    let all = ALL_ACTIONS.iter().fold(0, |a, f| a | f);
    let mut h = hooked(all, 3000, MockConfig::default());
    let before = h.env.all_calls(&h.mock);
    // The hook adds liquidity as itself (signed by its authority PDA): no callbacks (noSelfCall).
    let auth = h.pk.hook_authority;
    let t0 = h.env.token_account(&h.pk.mint0, &auth);
    let t1 = h.env.token_account(&h.pk.mint1, &auth);
    h.env.mint_to(&h.pk.mint0, &t0, 1_000_000_000);
    h.env.mint_to(&h.pk.mint1, &t1, 1_000_000_000);
    let params = ModifyLiquidityParams { tick_lower: -60, tick_upper: 60, liquidity_delta: 10_000_000, salt: [1; 32] };
    let ix = h.env.modify_ix(&h.pk, &auth, Some(t0), Some(t1), params, Settle::default(), false, false, &h.mock.metas(), vec![]);
    let mock = h.mock;
    ok(h.env.act(&mock, ix, &[]));
    assert_eq!(h.env.all_calls(&h.mock), before);
    assert_eq!(h.env.state(&h.pk.pool).liquidity.get(), 10_000_000);
    // Anyone else's add does call back.
    let lp = h.env.user(&h.pk, 1_000_000_000);
    ok(h.env.liquidity(&h.pk, &lp, -60, 60, 1_000_000, &h.mock.metas()));
    assert_eq!(h.env.calls(&h.mock, slot(BEFORE_ADD_LIQUIDITY)), before[slot(BEFORE_ADD_LIQUIDITY)] + 1);
}

#[test]
fn before_swap_delta_replaces_the_pool_with_a_custom_curve() {
    // A constant-sum hook (v4's CSMM example): 1:1, no pool liquidity at all.
    let x = 5_000_000i128;
    let mut cfg = MockConfig::default();
    cfg.before_swap.delta = BeforeSwapDelta { specified: x, unspecified: -x };
    let mut h = hooked(BEFORE_SWAP | BEFORE_SWAP_RETURNS_DELTA, 3000, cfg.clone());
    fund_hook(&mut h, 0, 100_000_000);
    let t = h.env.user(&h.pk, 100_000_000);
    let (b0, b1) = (h.env.balance(&t.t0), h.env.balance(&t.t1));
    ok(h.env.swap(&h.pk, &t, true, -x, &h.mock.metas()));
    // Exactly 1:1 even though the pool has no liquidity and its price never moved.
    assert_eq!(b0 - h.env.balance(&t.t0), x as u64);
    assert_eq!(h.env.balance(&t.t1) - b1, x as u64);
    assert_eq!(h.env.state(&h.pk.pool).sqrt_price.get(), P1);
    assert_eq!(hook_claims(&h), (x as u64, 100_000_000 - x as u64));

    // The hook can't take more than the swap's specified amount.
    cfg.before_swap.delta = BeforeSwapDelta { specified: x + 1, unspecified: -x };
    h.env.set_mock(&h.mock, &cfg);
    assert_err(h.env.swap(&h.pk, &t, true, -x, &h.mock.metas()), PmError::HookDeltaExceedsSwapAmount);

    // A hook that owes more than its claims cover fails the swap.
    cfg.before_swap.delta = BeforeSwapDelta { specified: x, unspecified: -1_000_000_000 };
    h.env.set_mock(&h.mock, &cfg);
    assert_err(h.env.swap(&h.pk, &t, true, -x, &h.mock.metas()), PmError::InsufficientClaims);
}

#[test]
fn before_swap_delta_can_fill_part_and_route_the_rest_through_the_pool() {
    let x = 1_000_000i128;
    let mut cfg = MockConfig::default();
    cfg.before_swap.delta = BeforeSwapDelta { specified: x / 4, unspecified: -(x / 4) };
    let mut h = hooked(BEFORE_SWAP | BEFORE_SWAP_RETURNS_DELTA, 3000, cfg);
    fund_hook(&mut h, 0, 10_000_000);
    let lp = h.env.user(&h.pk, 1_000_000_000_000);
    ok(h.env.liquidity(&h.pk, &lp, -6000, 6000, 1_000_000_000_000, &h.mock.metas()));
    let pool_part = h.env.quote(&h.pk, true, -(x - x / 4), 0);
    let t = h.env.user(&h.pk, 100_000_000);
    let b1 = h.env.balance(&t.t1);
    ok(h.env.swap(&h.pk, &t, true, -x, &h.mock.metas()));
    assert_eq!((h.env.balance(&t.t1) - b1) as i128, pool_part.amount1 + x / 4);
}

#[test]
fn a_delta_without_its_permission_is_ignored() {
    // Same response, but the address lacks beforeSwapReturnDelta: an ordinary swap happens.
    let mut cfg = MockConfig::default();
    cfg.before_swap.delta = BeforeSwapDelta { specified: 1_000, unspecified: -1_000 };
    let mut h = hooked(BEFORE_SWAP, 3000, cfg);
    let lp = h.env.user(&h.pk, 1_000_000_000_000);
    ok(h.env.liquidity(&h.pk, &lp, -6000, 6000, 1_000_000_000_000, &h.mock.metas()));
    let q = h.env.quote(&h.pk, true, -1_000, 0);
    let t = h.env.user(&h.pk, 100_000_000);
    let b1 = h.env.balance(&t.t1);
    ok(h.env.swap(&h.pk, &t, true, -1_000, &h.mock.metas()));
    assert_eq!((h.env.balance(&t.t1) - b1) as i128, q.amount1);
}

#[test]
fn after_swap_delta_takes_a_cut_of_the_output() {
    let mut cfg = MockConfig::default();
    cfg.after_swap.unspecified_delta = 12_345;
    let mut h = hooked(AFTER_SWAP | AFTER_SWAP_RETURNS_DELTA, 3000, cfg);
    h.env.open_claims(&h.pk, &h.pk.hook_authority);
    let lp = h.env.user(&h.pk, 1_000_000_000_000);
    ok(h.env.liquidity(&h.pk, &lp, -6000, 6000, 1_000_000_000_000, &h.mock.metas()));
    let t = h.env.user(&h.pk, 1_000_000_000);
    // Exact input zero-for-one: the unspecified currency is the output (currency1).
    let q = h.env.quote(&h.pk, true, -10_000_000, 0);
    let b1 = h.env.balance(&t.t1);
    ok(h.env.swap(&h.pk, &t, true, -10_000_000, &h.mock.metas()));
    assert_eq!((h.env.balance(&t.t1) - b1) as i128, q.amount1 - 12_345);
    assert_eq!(hook_claims(&h), (0, 12_345));
    // Exact output: the unspecified currency is the input, so the trader pays 12,345 more.
    let q = h.env.quote(&h.pk, true, 1_000_000, 0);
    let b0 = h.env.balance(&t.t0);
    ok(h.env.swap(&h.pk, &t, true, 1_000_000, &h.mock.metas()));
    assert_eq!((b0 - h.env.balance(&t.t0)) as i128, -q.amount0 + 12_345);
    assert_eq!(hook_claims(&h), (12_345, 12_345));
}

#[test]
fn liquidity_return_deltas_charge_deposits_and_withdrawals() {
    let mut cfg = MockConfig::default();
    cfg.after_add.delta = BalanceDelta::new(1_000, 0); // the hook keeps 1,000 currency0 on adds
    cfg.after_remove.delta = BalanceDelta::new(0, 2_000); // and 2,000 currency1 on removes
    let flags = AFTER_ADD_LIQUIDITY | AFTER_ADD_LIQUIDITY_RETURNS_DELTA | AFTER_REMOVE_LIQUIDITY | AFTER_REMOVE_LIQUIDITY_RETURNS_DELTA;
    let mut h = hooked(flags, 3000, cfg);
    h.env.open_claims(&h.pk, &h.pk.hook_authority);
    let lp = h.env.user(&h.pk, 1_000_000_000_000);
    let sa = get_sqrt_price_at_tick(-600).unwrap();
    let sb = get_sqrt_price_at_tick(600).unwrap();
    let l = 1_000_000_000u128;
    let b0 = h.env.balance(&lp.t0);
    ok(h.env.liquidity(&h.pk, &lp, -600, 600, l as i128, &h.mock.metas()));
    assert_eq!((b0 - h.env.balance(&lp.t0)) as u128, get_amount0_delta(P1, sb, l, true).unwrap() + 1_000);
    let b1 = h.env.balance(&lp.t1);
    ok(h.env.liquidity(&h.pk, &lp, -600, 600, -(l as i128), &h.mock.metas()));
    assert_eq!((h.env.balance(&lp.t1) - b1) as u128, get_amount1_delta(sa, P1, l, false).unwrap() - 2_000);
    assert_eq!(hook_claims(&h), (1_000, 2_000));
}

#[test]
fn dynamic_fees_set_in_after_initialize_and_overridden_per_swap() {
    let mut cfg = MockConfig::default();
    cfg.effects = vec![(1, HookEffects { lp_fee: Some(5_000), actions: vec![] })];
    cfg.before_swap.lp_fee_override = 20_000 | OVERRIDE_FEE_FLAG;
    let mut h = hooked(AFTER_INITIALIZE | BEFORE_SWAP, DYNAMIC_FEE_FLAG, cfg.clone());
    assert_eq!(h.env.state(&h.pk.pool).lp_fee, 5_000);
    let lp = h.env.user(&h.pk, 1_000_000_000_000);
    ok(h.env.liquidity(&h.pk, &lp, -6000, 6000, 1_000_000_000_000, &h.mock.metas()));
    let t = h.env.user(&h.pk, 1_000_000_000);

    // 2% for this swap only.
    let q = h.env.quote(&h.pk, true, -10_000_000, 20_000 | OVERRIDE_FEE_FLAG);
    let b1 = h.env.balance(&t.t1);
    ok(h.env.swap(&h.pk, &t, true, -10_000_000, &h.mock.metas()));
    assert_eq!((h.env.balance(&t.t1) - b1) as i128, q.amount1);
    assert_eq!(h.env.state(&h.pk.pool).lp_fee, 5_000);

    // Without the override flag the stored fee applies.
    cfg.before_swap.lp_fee_override = 20_000;
    h.env.set_mock(&h.mock, &cfg);
    let q = h.env.quote(&h.pk, true, -10_000_000, 0);
    let b1 = h.env.balance(&t.t1);
    ok(h.env.swap(&h.pk, &t, true, -10_000_000, &h.mock.metas()));
    assert_eq!((h.env.balance(&t.t1) - b1) as i128, q.amount1);

    // An override above 100% is refused.
    cfg.before_swap.lp_fee_override = 1_000_001 | OVERRIDE_FEE_FLAG;
    h.env.set_mock(&h.mock, &cfg);
    assert_err(h.env.swap(&h.pk, &t, true, -10_000, &h.mock.metas()), PmError::LPFeeTooLarge);

    // update_dynamic_lp_fee: the hook may, anyone else may not.
    let upd = |signer: Pubkey, fee: u32| Instruction {
        program_id: pm::ID,
        accounts: pm::accounts::UpdateDynamicLpFee { hook_authority: signer, pool: h.pk.pool }.to_account_metas(None),
        data: pm::instruction::UpdateDynamicLpFee { lp_fee: fee }.data(),
    };
    let mock = h.mock;
    ok(h.env.act(&mock, upd(h.pk.hook_authority, 42), &[]));
    assert_eq!(h.env.state(&h.pk.pool).lp_fee, 42);
    let stranger = solana_keypair::Keypair::new();
    h.env.svm.airdrop(&stranger.pubkey(), SOL).unwrap();
    let ix = upd(stranger.pubkey(), 1);
    assert_err(h.env.send(&[ix], &stranger, &[]), PmError::UnauthorizedDynamicLPFeeUpdate);
}

#[test]
fn fee_overrides_and_updates_need_a_dynamic_fee_pool() {
    let mut cfg = MockConfig::default();
    cfg.before_swap.lp_fee_override = 500_000 | OVERRIDE_FEE_FLAG;
    let mut h = hooked(BEFORE_SWAP, 3000, cfg.clone());
    let lp = h.env.user(&h.pk, 1_000_000_000_000);
    ok(h.env.liquidity(&h.pk, &lp, -6000, 6000, 1_000_000_000_000, &h.mock.metas()));
    let t = h.env.user(&h.pk, 1_000_000_000);
    let q = h.env.quote(&h.pk, true, -1_000_000, 0);
    let b1 = h.env.balance(&t.t1);
    ok(h.env.swap(&h.pk, &t, true, -1_000_000, &h.mock.metas()));
    assert_eq!((h.env.balance(&t.t1) - b1) as i128, q.amount1, "a static pool ignores the override");

    cfg.before_swap.effects.lp_fee = Some(1);
    h.env.set_mock(&h.mock, &cfg);
    assert_err(h.env.swap(&h.pk, &t, true, -1_000_000, &h.mock.metas()), PmError::UnauthorizedDynamicLPFeeUpdate);
}

#[test]
fn hooks_can_add_and_remove_liquidity_inside_a_swap() {
    // Just-in-time liquidity: add in beforeSwap, remove in afterSwap, keep the fees.
    let flags = BEFORE_SWAP | AFTER_SWAP;
    let mut h = hooked(flags, 10_000, MockConfig::default());
    fund_hook(&mut h, 50_000_000_000, 50_000_000_000);
    let auth = h.pk.hook_authority;
    let salt = [9u8; 32];
    let position = Env::position_key(&h.pk, &auth, -600, 600, salt);
    let open = Instruction {
        program_id: pm::ID,
        accounts: pm::accounts::OpenPosition { payer: h.env.payer.pubkey(), owner: auth, pool: h.pk.pool, position, system_program: anchor_lang::system_program::ID }.to_account_metas(None),
        data: pm::instruction::OpenPosition { tick_lower: -600, tick_upper: 600, salt }.data(),
    };
    let mock = h.mock;
    ok(h.env.act(&mock, open, &[]));

    let jit: i128 = 1_000_000_000_000;
    let mut cfg = MockConfig::default();
    cfg.before_swap.effects.actions = vec![HookAction::ModifyLiquidity { position, liquidity_delta: jit }];
    cfg.after_swap.effects.actions = vec![HookAction::ModifyLiquidity { position, liquidity_delta: -jit }];
    h.env.set_mock(&h.mock, &cfg);

    let mut metas = h.mock.metas();
    metas.push(AccountMeta::new(position, false));
    let (c0, c1) = hook_claims(&h);
    let t = h.env.user(&h.pk, 1_000_000_000);
    let b1 = h.env.balance(&t.t1);
    ok(h.env.swap(&h.pk, &t, true, -100_000_000, &metas));
    // The swap filled against the hook's liquidity, which is gone again afterwards.
    assert!(h.env.balance(&t.t1) - b1 > 98_000_000);
    assert_eq!(h.env.state(&h.pk.pool).liquidity.get(), 0);
    assert_eq!(h.env.position(&position).liquidity, 0);
    // The hook ends up with the swap's input plus its 1% fee, minus the output.
    let (n0, n1) = hook_claims(&h);
    // (less a couple of units of add/remove rounding, as in v4)
    assert!((99_999_990..=100_000_000).contains(&(n0 - c0)), "{}", n0 - c0);
    assert!(c1 - n1 < 100_000_000 && c1 - n1 > 98_000_000);
    assert!(h.env.ticks(&h.pk).is_empty());
}

#[test]
fn hooks_can_queue_swaps_and_donations() {
    // afterAddLiquidity: the hook swaps on its own pool and tips the LPs.
    let flags = AFTER_ADD_LIQUIDITY;
    let mut cfg = MockConfig::default();
    cfg.effects = vec![(
        3,
        HookEffects {
            lp_fee: None,
            actions: vec![
                HookAction::Swap { params: SwapParams { zero_for_one: false, amount_specified: -1_000_000, sqrt_price_limit: MAX_SQRT_PRICE - 1 } },
                HookAction::Donate { amount0: 0, amount1: 5_000 },
            ],
        },
    )];
    cfg.after_add.effects = cfg.effects[0].1.clone();
    let mut h = hooked(flags, 3000, cfg);
    fund_hook(&mut h, 0, 10_000_000);
    let lp = h.env.user(&h.pk, 1_000_000_000_000);
    ok(h.env.liquidity(&h.pk, &lp, -6000, 6000, 1_000_000_000_000, &h.mock.metas()));
    let st = h.env.state(&h.pk.pool);
    assert!(st.sqrt_price.get() > P1, "the hook's one-for-zero swap moved the price up");
    let (c0, c1) = hook_claims(&h);
    assert_eq!(c1, 10_000_000 - 1_000_000 - 5_000);
    assert!(c0 > 990_000 && c0 < 1_000_000);
}

#[test]
fn a_hook_can_pay_a_vault_directly_during_a_callback() {
    // v4's sync/settle: tokens the hook moves into a vault are credited to it.
    let mut h = hooked(BEFORE_SWAP, 3000, MockConfig::default());
    h.env.open_claims(&h.pk, &h.pk.hook_authority);
    let lp = h.env.user(&h.pk, 1_000_000_000_000);
    ok(h.env.liquidity(&h.pk, &lp, -6000, 6000, 1_000_000_000_000, &h.mock.metas()));
    let auth = h.pk.hook_authority;
    let src = h.env.token_account(&h.pk.mint0, &auth);
    h.env.mint_to(&h.pk.mint0, &src, 777);
    // accounts as the hook sees them: [caller, pool, config, log, src, mint0, vault0, token program, authority]
    let cfg = MockConfig { pay: Some(Pay { slot: 6, amount: 777, from: 4, mint: 5, vault: 6, token_program: 7, decimals: h.env.state(&h.pk.pool).decimals0 }), ..Default::default() };
    h.env.set_mock(&h.mock, &cfg);
    let mut metas = h.mock.metas();
    metas.extend([
        AccountMeta::new(src, false),
        AccountMeta::new_readonly(h.pk.mint0, false),
        AccountMeta::new(h.pk.vault0, false),
        AccountMeta::new_readonly(h.pk.tp0, false),
        AccountMeta::new_readonly(auth, false),
    ]);
    let t = h.env.user(&h.pk, 1_000_000_000);
    ok(h.env.swap(&h.pk, &t, true, -1_000_000, &metas));
    assert_eq!(hook_claims(&h), (777, 0));
}

#[test]
fn a_hook_never_receives_the_traders_signature() {
    let mut h = hooked(BEFORE_SWAP, 3000, MockConfig::default());
    let lp = h.env.user(&h.pk, 1_000_000_000_000);
    ok(h.env.liquidity(&h.pk, &lp, -6000, 6000, 1_000_000_000_000, &h.mock.metas()));
    let t = h.env.user(&h.pk, 1_000_000_000);
    let thief = h.env.token_account(&h.pk.mint0, &Pubkey::new_unique());
    // accounts: [caller, pool, config, log, victim, thief, owner, mint0, token program]
    let cfg = MockConfig {
        steal: Some(Steal { slot: 6, from: 4, to: 5, owner: 6, mint: 7, token_program: 8, decimals: h.env.state(&h.pk.pool).decimals0, amount: 1 }),
        ..Default::default()
    };
    h.env.set_mock(&h.mock, &cfg);
    let mut metas = h.mock.metas();
    metas.extend([
        AccountMeta::new(t.t0, false),
        AccountMeta::new(thief, false),
        AccountMeta::new_readonly(t.key(), true), // the trader is a signer of the transaction...
        AccountMeta::new_readonly(h.pk.mint0, false),
        AccountMeta::new_readonly(h.pk.tp0, false),
    ]);
    // ...but the hook only ever sees it as a non-signer, so the theft fails.
    assert_fails_with(h.env.swap(&h.pk, &t, true, -1_000_000, &metas), "signer privilege escalated");
    assert_eq!(h.env.balance(&thief), 0);
}

#[test]
fn only_the_pool_manager_can_call_a_hook() {
    let mut h = hooked(BEFORE_SWAP, 3000, MockConfig::default());
    let key = h.env.state(&h.pk.pool).key();
    let fake_caller = solana_keypair::Keypair::new();
    h.env.svm.airdrop(&fake_caller.pubkey(), SOL).unwrap();
    let data = encode_call(
        hook::discriminator::BEFORE_SWAP,
        &BeforeSwapArgs { sender: fake_caller.pubkey(), key, params: SwapParams::default(), hook_data: vec![] },
    );
    let mut accounts = vec![AccountMeta::new_readonly(fake_caller.pubkey(), true), AccountMeta::new_readonly(h.pk.pool, false)];
    accounts.extend(h.mock.metas());
    let ix = Instruction { program_id: h.mock.program, accounts, data };
    assert_fails_with(h.env.send(&[ix], &fake_caller, &[]), "missing required signature");
    // And a forged hook_caller can't get a PoolManager signature either.
    let lp = h.env.user(&h.pk, 1_000_000_000);
    let mut pk = h.pk;
    pk.hook_caller = Pubkey::new_unique();
    let mut lpk = h.pk;
    lpk.hook_caller = h.pk.hook_caller;
    ok(h.env.liquidity(&lpk, &lp, -600, 600, 1_000_000_000, &h.mock.metas()));
    let p = SwapParams { zero_for_one: true, amount_specified: -1_000, sqrt_price_limit: MIN_SQRT_PRICE + 1 };
    let ix = h.env.swap_ix(&pk, &lp.key(), Some(lp.t0), Some(lp.t1), p, Settle::default(), 0, false, false, &h.mock.metas(), vec![]);
    let kp = lp.kp.insecure_clone();
    assert_fails_with(h.env.pay(&[ix], &[&kp]), "signer privilege escalated");
}

#[test]
fn reverts_and_unreadable_responses_fail_the_operation() {
    let mut h = hooked(BEFORE_SWAP | AFTER_ADD_LIQUIDITY, 3000, MockConfig::default());
    let lp = h.env.user(&h.pk, 1_000_000_000_000);
    ok(h.env.liquidity(&h.pk, &lp, -6000, 6000, 1_000_000_000, &h.mock.metas()));
    let t = h.env.user(&h.pk, 1_000_000_000);
    h.env.set_mock(&h.mock, &MockConfig { revert_mask: 1 << 6, ..Default::default() });
    assert_fails_with(h.env.swap(&h.pk, &t, true, -1_000, &h.mock.metas()), "mock hook: reverting");
    h.env.set_mock(&h.mock, &MockConfig { garbage_mask: 1 << 6, ..Default::default() });
    assert_err(h.env.swap(&h.pk, &t, true, -1_000, &h.mock.metas()), PmError::InvalidHookResponse);
    // A silent hook (no return data) means "no delta, no effects".
    h.env.set_mock(&h.mock, &MockConfig { silent_mask: 1 << 6 | 1 << 3, ..Default::default() });
    ok(h.env.swap(&h.pk, &t, true, -1_000, &h.mock.metas()));
    ok(h.env.liquidity(&h.pk, &lp, -6000, 6000, 1_000, &h.mock.metas()));
    let _ = delta_of(&h.env, &t, || {});
}

#[test]
fn hook_actions_need_their_own_position_and_capacity() {
    let mut h = hooked(BEFORE_SWAP, 3000, MockConfig::default());
    fund_hook(&mut h, 1_000_000_000, 1_000_000_000);
    let lp = h.env.user(&h.pk, 1_000_000_000_000);
    ok(h.env.liquidity(&h.pk, &lp, -6000, 6000, 1_000_000_000, &h.mock.metas()));
    // Someone else's position can't be touched by the hook.
    let theirs = Env::position_key(&h.pk, &lp.key(), -6000, 6000, [0; 32]);
    let mut cfg = MockConfig::default();
    cfg.before_swap.effects.actions = vec![HookAction::ModifyLiquidity { position: theirs, liquidity_delta: -1 }];
    h.env.set_mock(&h.mock, &cfg);
    let mut metas = h.mock.metas();
    metas.push(AccountMeta::new(theirs, false));
    let t = h.env.user(&h.pk, 1_000_000_000);
    assert_err(h.env.swap(&h.pk, &t, true, -1_000, &metas), PmError::NotPositionOwner);
    // A position that isn't passed in can't be found.
    assert_err(h.env.swap(&h.pk, &t, true, -1_000, &h.mock.metas()), PmError::PositionNotFound);
}
