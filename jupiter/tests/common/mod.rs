//! A snapshotted coin, and the two ways the tests look at it: through Jupiter's test kit
//! (`test`), and through the `Amm` directly, built and updated the way Jupiter does it.

#![allow(dead_code)]

use std::{collections::HashMap, path::PathBuf};

use jupiter_amm_interface::{Amm, AmmContext, ClockRef, FlowType, KeyedAccount, Quote, QuoteParams, SwapMode};
use jupiter_amm_test_kit::{assert_pool_parity, PoolSnapshot, PoolTest};
use rust_decimal::Decimal;
use solana_pubkey::{pubkey, Pubkey};
use stoke_amm::{encode_swap, StokeAmm};

pub const WSOL: Pubkey = pubkey!("So11111111111111111111111111111111111111112");
pub const SOL: u64 = 1_000_000_000;
/// One whole coin (6 decimals).
pub const COIN: u64 = 1_000_000;

pub struct Coin {
    /// Its snapshot: a folder of `<address>.bin` accounts.
    pub dir: PathBuf,
    pub pool: Pubkey,
    pub mint: Pubkey,
}

impl Coin {
    /// A live mainnet coin, snapshotted by the kit into `tests/fixtures/accounts/<pool>`.
    pub fn mainnet(pool: Pubkey, mint: Pubkey) -> Self {
        Self { dir: PathBuf::from("tests/fixtures/accounts").join(pool.to_string()), pool, mint }
    }

    /// A coin built by the programs' test harness into `tests/fixtures/local/<name>`; its
    /// `coin.json` says which pool and mint it is.
    pub fn local(name: &str) -> Self {
        let dir = PathBuf::from("tests/fixtures/local").join(name);
        let about: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("coin.json")).expect("coin.json")).expect("coin.json is JSON");
        let key = |field: &str| about[field].as_str().expect(field).parse::<Pubkey>().expect(field);
        Self { pool: key("pool"), mint: key("mint"), dir }
    }

    /// The kit's check, for each trade: quote it, run the PoolManager's real `swap` in LiteSVM,
    /// and require what the trader received to equal the quote. Buys are in lamports, sells in
    /// the coin's units.
    pub fn test(&self, buys: &[u64], sells: &[u64]) {
        let mut test = PoolTest::new(self.pool);
        test.fixtures_dir = self.dir.clone();
        for lamports in buys {
            test = test.add_swap(WSOL, self.mint, *lamports);
        }
        for units in sells {
            test = test.add_swap(self.mint, WSOL, *units);
        }
        assert_pool_parity::<StokeAmm>(&test, encode_swap);
    }

    pub fn snapshot(&self) -> PoolSnapshot {
        PoolSnapshot::load_dir(&self.dir).expect("snapshot")
    }

    /// The `Amm`, built from the pool account and updated from the snapshot.
    pub fn amm(&self) -> StokeAmm {
        let snapshot = self.snapshot();
        let context = AmmContext { clock_ref: ClockRef::from(snapshot.clock().expect("the snapshot has the clock")) };
        let keyed = KeyedAccount { key: self.pool, account: snapshot.get(&self.pool).expect("pool").clone(), params: None };
        let mut amm = StokeAmm::from_keyed_account(&keyed, &context).expect("from_keyed_account");
        let accounts: HashMap<Pubkey, _> = amm.get_accounts_to_update().into_iter().filter_map(|key| Some((key, snapshot.get(&key)?))).collect();
        amm.update(&accounts).expect("update");
        amm
    }

    pub fn quote(&self, is_buy: bool, amount: u64) -> Result<Quote, String> {
        let (input_mint, output_mint) = if is_buy { (WSOL, self.mint) } else { (self.mint, WSOL) };
        self.amm().quote(&QuoteParams { amount, input_mint, output_mint, swap_mode: SwapMode::ExactIn, flow_type: FlowType::default() }).map_err(|e| e.to_string())
    }

    /// The fee a trade is quoted, in bps of its SOL side.
    pub fn fee_bps(&self, is_buy: bool, amount: u64) -> u32 {
        let pct = self.quote(is_buy, amount).expect("quote").fee_pct;
        u32::try_from(pct * Decimal::from(10_000)).expect("bps")
    }

    /// Whether a buy this size sells the curve out, graduating the coin inside the swap.
    pub fn graduates(&self, lamports: u64) -> bool {
        let clock = self.snapshot().clock().expect("clock");
        let amm = self.amm();
        let market = amm.market().expect("updated");
        market.quote(true, lamports, &Pubkey::default(), &stoke_quote::Clock { slot: clock.slot, unix_timestamp: clock.unix_timestamp }).expect("quote").graduates
    }
}
