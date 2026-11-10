//! Quote parity on live mainnet coins, checked with Jupiter's test kit: for each coin and each
//! trade below, the kit takes the coin's accounts as they were snapshotted, asks `StokeAmm` for
//! a quote, runs the PoolManager's real `swap` in LiteSVM with the deployed programs, and
//! requires the tokens the trader received to equal the quote to the unit.
//!
//!   cargo test                       # offline, from the committed fixtures
//!   ./refresh-fixtures.sh            # snapshot the coins and programs again from mainnet
//!
//! Every coin here is a live mainnet coin, snapshotted with the kit (`tests/fixtures/accounts/
//! <pool>`), and the four programs are the ones deployed on mainnet, dumped by the kit
//! (`tests/fixtures/*.so`). No mainnet coin has graduated, set fee zones or added a hook yet:
//! those are in `local.rs`.
//!
//! Buys are in lamports, sells in the coin's units (6 decimals). A coin's curve raises 400 SOL,
//! so a buy larger than what's left sells the curve out, graduates the coin inside the swap and
//! fills the rest from its new pool: those are the buys marked "graduates". Sells are sized to
//! what each coin had in circulation when it was snapshotted; after a refresh they may need
//! resizing.

mod common;

use common::{Coin, COIN, SOL};
use solana_pubkey::pubkey;

/// On its curve with a 2.5% fee, about 0.6 SOL raised.
#[test]
fn a_coin_on_its_curve_with_the_top_fee() {
    let coin = Coin::mainnet(pubkey!("2gECyML3pDLJwDz613SA4vPreDLYSTtLnMnMMKVBYWvB"), pubkey!("7uRj55Xf2VfPNgBjCYM8MoapGGge3kFs96ZYHPbbZyP6"));
    assert_eq!((coin.fee_bps(true, SOL), coin.fee_bps(false, COIN)), (250, 250));
    assert!(coin.graduates(1_000 * SOL));
    coin.test(
        &[
            1_000,       // dust
            SOL / 20,    // 0.05 SOL
            5 * SOL,     //
            120 * SOL,   // a third of the curve in one buy
            1_000 * SOL, // graduates, with most of it left over for the pool
        ],
        &[COIN, 50_000 * COIN, 2_000_000 * COIN],
    );
}

/// On its curve with a 1% fee, about 0.1 SOL raised.
#[test]
fn a_coin_on_its_curve_with_a_one_percent_fee() {
    let coin = Coin::mainnet(pubkey!("4oWGanzKF5C3WjbV3khzS9CBrdZEUPTJxdfweRApNhcM"), pubkey!("7hT2575rj634Jah4NG4Jc83wZS4iQxw5UU6dTQoLAFM1"));
    assert_eq!((coin.fee_bps(true, SOL), coin.fee_bps(false, COIN)), (100, 100));
    assert!(coin.graduates(500 * SOL));
    coin.test(
        &[
            SOL / 10,
            25 * SOL,
            500 * SOL, // graduates
        ],
        &[10_000 * COIN, 400_000 * COIN],
    );
}

/// Launched and never traded: the curve is untouched, so there's nothing to sell into.
#[test]
fn a_coin_nobody_has_bought_yet() {
    let coin = Coin::mainnet(pubkey!("aroh8BCbkKR9BqbU7EAvdVh68qbUvBLHJXwcNsbK2s9"), pubkey!("DfE5o97uKKbJaBmubEhDsJcviwf3zJRyPb5WC6FbWyGE"));
    assert!(coin.graduates(450 * SOL));
    coin.test(
        &[
            SOL / 100,
            399 * SOL, // almost the whole curve
            450 * SOL, // graduates
        ],
        &[],
    );
}

/// The two most traded coins so far, both at 2.5%.
#[test]
fn the_most_traded_coins() {
    let ekots = Coin::mainnet(pubkey!("99Gh9h6g7zJXngBRyhKvzevdqo6ZnFYP3RqVY8P6ehFC"), pubkey!("FsJuXy1PeBmSqTEQeCrC6fj58ufCgJXdiuhzEocxpX9G"));
    ekots.test(&[SOL, 40 * SOL], &[100_000 * COIN]);
    let stkecat = Coin::mainnet(pubkey!("G99cEjG88SBctYJUwVcVrQkvktXNM5VmyvNz7cUcnTVg"), pubkey!("CUr6NRJEArkdUjTLo1DF78X8FDWRcAoxQJYEemtRXKi2"));
    assert!(stkecat.graduates(2_000 * SOL));
    stkecat.test(
        &[
            3 * SOL / 10,
            2_000 * SOL, // graduates, and four fifths of it trades in the new pool
        ],
        &[1_000 * COIN, 200_000 * COIN],
    );
}
