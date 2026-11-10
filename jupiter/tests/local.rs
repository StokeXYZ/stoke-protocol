//! Quote parity for the coins mainnet doesn't have yet: graduated, with fee zones, with one of
//! Stoke's hooks. Same check as `mainnet.rs`, by Jupiter's test kit, against the same deployed
//! programs: quote, run the real `swap` in LiteSVM, and require the two to match to the unit.
//!
//! The snapshots (`tests/fixtures/local/<name>`) come from the programs' own test harness, which
//! launches each coin and trades it into the state named, priced in wrapped SOL like mainnet:
//!
//!   cd .. && cargo test -p meridian_pool_manager --test launch jupiter_fixtures -- --ignored
//!
//! Each folder's `coin.json` says which pool and mint it holds, and what state the coin is in.
//!
//! Buys are in lamports, sells in the coin's units (6 decimals). Every coin here has a 1% fee.

mod common;

use common::{Coin, COIN, SOL, WSOL};
use jupiter_amm_interface::{Amm, AmmContext, FlowType, KeyedAccount, QuoteParams, SwapMode, SwapParams};
use solana_pubkey::Pubkey;
use stoke_amm::StokeAmm;

#[test]
fn a_coin_in_its_first_second() {
    let coin = Coin::local("first-second");
    // The anti-snipe tax isn't a fee: 90% of the tokens a buy takes off the curve are burned,
    // so the buyer is quoted the tenth they keep. The fee is the coin's own.
    assert_eq!(coin.fee_bps(true, SOL), 100);
    let untaxed = coin.amm().market().unwrap().launch.curve().buy_exact_in(SOL, 100, 0).tokens;
    let taxed = coin.quote(true, SOL).unwrap().out_amount;
    assert!(taxed * 10 < untaxed && taxed * 12 > untaxed, "{taxed} of {untaxed}");
    assert!(coin.graduates(800 * SOL));
    coin.test(
        &[
            SOL / 10,
            5 * SOL,
            800 * SOL, // sells the curve out even so, and graduates
        ],
        &[],
    );
}

#[test]
fn a_graduated_coin_in_its_pool() {
    let coin = Coin::local("graduated");
    assert!(coin.amm().market().unwrap().graduated());
    assert_eq!((coin.fee_bps(true, SOL), coin.fee_bps(false, 1_000 * COIN)), (100, 100));
    coin.test(&[1_000, SOL / 5, 30 * SOL, 400 * SOL], &[COIN, 250_000 * COIN, 40_000_000 * COIN]);
}

#[test]
fn fee_zones_on_the_curve() {
    let coin = Coin::local("zones-on-the-curve");
    // Buys pay the coin's fee; a sell pays the highest zone its price passes through.
    assert_eq!((coin.fee_bps(true, SOL), coin.fee_bps(false, 1_000 * COIN)), (100, 1_000));
    assert!(coin.graduates(300 * SOL));
    coin.test(
        &[
            SOL,
            40 * SOL,
            300 * SOL, // graduates
        ],
        &[
            1_000 * COIN,       // inside the 10% zone
            60_000_000 * COIN,  // down through the 7% zone
            250_000_000 * COIN, // down through all three and out below
        ],
    );
}

#[test]
fn fee_zones_in_the_pool() {
    let coin = Coin::local("zones-in-the-pool");
    assert!(coin.amm().market().unwrap().graduated());
    assert_eq!((coin.fee_bps(true, SOL), coin.fee_bps(false, 1_000 * COIN)), (100, 800));
    coin.test(
        &[SOL, 200 * SOL],
        &[
            10_000 * COIN,      // inside the 8% zone
            30_000_000 * COIN,  // down into the 5% zone
            200_000_000 * COIN, // down through both and out below
        ],
    );
}

#[test]
fn the_launch_tax_hook_on_the_curve() {
    let coin = Coin::local("launch-tax");
    // 10 minutes in: a third of the way down the buy tax (8% to 1% over 30 minutes), a twelfth
    // of the way down the sell tax (10% to 1% over 2 hours).
    assert_eq!((coin.fee_bps(true, SOL), coin.fee_bps(false, 1_000 * COIN)), (566, 925));
    assert!(coin.graduates(600 * SOL));
    coin.test(
        &[
            SOL / 2,
            50 * SOL,
            600 * SOL, // graduates, paying the hook's fee on both parts
        ],
        &[1_000 * COIN, 20_000_000 * COIN],
    );
}

#[test]
fn the_launch_tax_hook_in_the_pool() {
    let coin = Coin::local("launch-tax-in-the-pool");
    assert!(coin.amm().market().unwrap().graduated());
    // The same coin, 5 minutes later.
    assert_eq!((coin.fee_bps(true, SOL), coin.fee_bps(false, 1_000 * COIN)), (450, 887));
    coin.test(&[SOL, 150 * SOL], &[5_000 * COIN, 25_000_000 * COIN]);
}

#[test]
fn the_max_buy_hook_inside_its_window() {
    let coin = Coin::local("max-buy");
    coin.test(
        &[
            SOL / 100,
            2 * SOL, // exactly the most the hook lets through
        ],
        &[100_000 * COIN, 5_000_000 * COIN],
    );
    // A lamport more and the hook refuses the buy, so there's no quote for it.
    let refused = coin.quote(true, 2 * SOL + 1).expect_err("a buy above the max isn't quoted");
    assert!(refused.contains("hook refuses this trade"), "{refused}");
}

#[test]
fn fee_zones_and_a_hook_together() {
    let coin = Coin::local("zones-and-a-hook");
    // The hook taxes sells 5%; the zone at this price says 10%: the sell pays the higher.
    assert_eq!((coin.fee_bps(true, SOL), coin.fee_bps(false, 1_000 * COIN)), (100, 1_000));
    coin.test(&[2 * SOL, 300 * SOL], &[1_000 * COIN, 60_000_000 * COIN, 250_000_000 * COIN]);
}

#[test]
fn a_paused_hook_changes_nothing() {
    let coin = Coin::local("paused-hook");
    // Launched with a 10% tax both ways; paused, every trade pays the coin's 1%.
    assert_eq!((coin.fee_bps(true, SOL), coin.fee_bps(false, 1_000 * COIN)), (100, 100));
    coin.test(&[SOL, 100 * SOL], &[1_000_000 * COIN, 50_000_000 * COIN]);
}

#[test]
fn a_creators_own_hook_isnt_quoted() {
    let coin = Coin::local("own-hook");
    assert!(!coin.amm().is_active());
    let refused = coin.quote(true, SOL).expect_err("no quote");
    assert!(refused.contains("isn't one this crate can quote"), "{refused}");
}

/// What the `Amm` says about itself, and what it refuses.
#[test]
fn the_amm_describes_one_coin_against_wrapped_sol() {
    let coin = Coin::local("zones-and-a-hook");
    let amm = coin.amm();
    assert_eq!((amm.label(), amm.program_id(), amm.key()), ("Stoke", stoke_quote::POOL_MANAGER_ID, coin.pool));
    assert_eq!(amm.get_reserve_mints(), vec![WSOL, coin.mint]);
    assert!(amm.is_active() && !amm.supports_exact_out() && !amm.has_dynamic_accounts() && !amm.unidirectional());
    let quote = |amm: &StokeAmm, input_mint, output_mint, swap_mode| amm.quote(&QuoteParams { amount: SOL, input_mint, output_mint, swap_mode, flow_type: FlowType::default() });

    // The accounts it reads never change: the same before any update and after.
    let snapshot = coin.snapshot();
    let keyed = KeyedAccount { key: coin.pool, account: snapshot.get(&coin.pool).unwrap().clone(), params: None };
    let fresh = StokeAmm::from_keyed_account(&keyed, &AmmContext::default()).unwrap();
    assert_eq!(fresh.get_accounts_to_update(), amm.get_accounts_to_update());
    // Before its first update it has nothing to quote from, and says so.
    assert!(quote(&fresh, WSOL, coin.mint, SwapMode::ExactIn).is_err());

    // The swap: the PoolManager's 16 accounts, then the hook's as the pool publishes them (for
    // this coin: its launch, the hook's position, its fee zones, and its coin hook's three).
    // The trader is the only signer.
    let user = Pubkey::new_unique();
    let (source, destination, jupiter) = (Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique());
    let swap = amm
        .get_swap_and_account_metas(&SwapParams {
            swap_mode: SwapMode::ExactIn,
            in_amount: SOL,
            out_amount: 0,
            source_mint: WSOL,
            destination_mint: coin.mint,
            source_token_account: source,
            destination_token_account: destination,
            token_transfer_authority: user,
            user,
            payer: user,
            quote_mint_to_referrer: None,
            jupiter_program_id: &jupiter,
            missing_dynamic_accounts_as_default: false,
        })
        .unwrap();
    assert_eq!(swap.account_metas.len(), 16 + 6);
    assert_eq!(amm.get_accounts_len(), swap.account_metas.len());
    let signers: Vec<Pubkey> = swap.account_metas.iter().filter(|m| m.is_signer).map(|m| m.pubkey).collect();
    assert_eq!(signers, vec![user]);
    assert!(swap.account_metas.iter().any(|m| m.pubkey == source && m.is_writable) && swap.account_metas.iter().any(|m| m.pubkey == destination && m.is_writable));

    // Not quoted: exact output, and any pair but this coin and wrapped SOL.
    let other = Pubkey::new_unique();
    assert!(quote(&amm, WSOL, coin.mint, SwapMode::ExactIn).is_ok());
    assert!(quote(&amm, WSOL, coin.mint, SwapMode::ExactOut).is_err());
    assert!(quote(&amm, WSOL, other, SwapMode::ExactIn).is_err());
    assert!(quote(&amm, other, WSOL, SwapMode::ExactIn).is_err());

    // Only a Stoke coin's pool makes one: not, say, the coin's launch account.
    let launch = stoke_quote::launch_address(&coin.mint);
    let not_a_pool = KeyedAccount { key: launch, account: snapshot.get(&launch).unwrap().clone(), params: None };
    assert!(StokeAmm::from_keyed_account(&not_a_pool, &AmmContext::default()).is_err());
}
