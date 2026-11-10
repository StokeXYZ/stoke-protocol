//! # stoke-amm
//!
//! Stoke for Jupiter: the [`jupiter_amm_interface::Amm`] for a Stoke coin.
//!
//! A Stoke coin is one pool of Stoke's PoolManager (a concentrated-liquidity singleton with
//! Uniswap v4's hook model) paired with wrapped SOL, with Stoke's launch hook on it. The coin
//! trades on a bonding curve inside the hook until the curve sells out, and in the pool after
//! that. Either way a trade is the same instruction, the PoolManager's `swap`, with the same
//! accounts, so one `Amm` covers a coin for its whole life.
//!
//! - **Keyed account:** the pool (owned by the PoolManager). No params.
//! - **Quoting** is [`stoke_quote`], which runs the programs' own code: the curve, the pool
//!   engine, the fee split, fee zones and Stoke's two hooks. No network, no clock of its own:
//!   the clock comes from [`AmmContext`].
//! - **Accounts to update** never change for a coin (`has_dynamic_accounts` is false). Some of
//!   them only exist for coins launched with fee zones or a hook; a missing one is fine.
//! - **The swap** is one top-level-capable instruction signed by the trader alone. Its extra
//!   accounts are the list the launch hook publishes for the pool, passed as it is.
//!
//! ## What isn't quoted
//!
//! - Exact-output trades (`supports_exact_out` is false).
//! - A coin whose creator added their own hook program: only that program knows what it does
//!   to a trade, so `quote` returns an error and `is_active` is false. Coins with one of Stoke's
//!   hooks (Launch tax, Max buy) are quoted exactly.
//!
//! ## What a quote can't know
//!
//! The trader. A coin's creator sells fee-free up to what they bought at launch, so the creator
//! selling through a route receives more than the quote; everyone else receives the quote.

use std::sync::atomic::Ordering;

use jupiter_amm_interface::{
    AccountProvider, Amm, AmmContext, AmmError, AmmLabel, ClockRef, KeyedAccount, Quote, QuoteParams, SingleProgramAmm, Swap, SwapAndAccountMetas, SwapMode, SwapParams,
};
use rust_decimal::Decimal;
use solana_account::ReadableAccount;
use solana_instruction::AccountMeta;
use solana_pubkey::Pubkey;
use stoke_quote::{Clock, Market, QuoteError, HOOK_LAUNCH_TAX_ID, HOOK_MAX_BUY_ID, LAUNCH_HOOK_ID, POOL_MANAGER_ID, WSOL_MINT};

pub use stoke_quote::swap_data;

/// A trader who isn't any coin's creator: quotes are for everyone else.
const ANYONE: Pubkey = Pubkey::new_from_array([0; 32]);

/// The accounts of the PoolManager's `swap` before the hook's own.
const SWAP_ACCOUNTS: usize = 16;

/// A Stoke coin: its pool, and everything read from its accounts at the last `update`.
#[derive(Clone)]
pub struct StokeAmm {
    key: Pubkey,
    /// The coin (the pool's other currency is wrapped SOL).
    mint: Pubkey,
    accounts: Vec<Pubkey>,
    /// `None` until the first `update`.
    market: Option<Market>,
    /// The extra accounts the launch hook publishes for this pool's swaps.
    hook_accounts: Vec<(Pubkey, bool)>,
    clock: ClockRef,
}

impl SingleProgramAmm for StokeAmm {
    const PROGRAM_ID: Pubkey = POOL_MANAGER_ID;
    const LABEL: AmmLabel = "Stoke";
}

fn err(e: QuoteError) -> AmmError {
    AmmError::from(e.to_string())
}

impl StokeAmm {
    /// The coin's mint.
    pub fn mint(&self) -> Pubkey {
        self.mint
    }

    /// The coin as it was at the last `update`.
    pub fn market(&self) -> Option<&Market> {
        self.market.as_ref()
    }

    fn loaded(&self) -> Result<&Market, AmmError> {
        self.market.as_ref().ok_or_else(|| AmmError::from("the coin's accounts haven't been loaded yet: call update first"))
    }

    /// Whether a trade from one mint to the other is a buy (SOL in) or a sell (the coin in).
    fn is_buy(&self, input: &Pubkey, output: &Pubkey) -> Result<bool, AmmError> {
        if *input == WSOL_MINT && *output == self.mint {
            Ok(true)
        } else if *input == self.mint && *output == WSOL_MINT {
            Ok(false)
        } else {
            Err(AmmError::from(format!("this pool trades {} against wrapped SOL, not {input} for {output}", self.mint)))
        }
    }
}

impl Amm for StokeAmm {
    fn from_keyed_account(keyed_account: &KeyedAccount, amm_context: &AmmContext) -> Result<Self, AmmError> {
        if keyed_account.account.owner() != &POOL_MANAGER_ID {
            return Err(AmmError::from("the account isn't a pool of Stoke's PoolManager"));
        }
        let pool = stoke_quote::decode_pool(keyed_account.account.data()).map_err(err)?;
        if pool.hooks != LAUNCH_HOOK_ID {
            return Err(AmmError::from("the pool doesn't have Stoke's launch hook: it isn't a Stoke coin"));
        }
        let mint = match (pool.currency0 == WSOL_MINT, pool.currency1 == WSOL_MINT) {
            (true, false) => pool.currency1,
            (false, true) => pool.currency0,
            _ => return Err(AmmError::from("the pool isn't a coin paired with wrapped SOL")),
        };
        Ok(Self {
            key: keyed_account.key,
            mint,
            accounts: Market::accounts(&keyed_account.key, &pool, &mint),
            market: None,
            hook_accounts: Vec::new(),
            clock: amm_context.clock_ref.clone(),
        })
    }

    fn label(&self) -> AmmLabel {
        Self::LABEL
    }

    fn program_id(&self) -> Pubkey {
        Self::PROGRAM_ID
    }

    fn key(&self) -> Pubkey {
        self.key
    }

    fn get_reserve_mints(&self) -> Vec<Pubkey> {
        vec![WSOL_MINT, self.mint]
    }

    fn get_accounts_to_update(&self) -> Vec<Pubkey> {
        self.accounts.clone()
    }

    fn update(&mut self, account_provider: impl AccountProvider) -> Result<(), AmmError> {
        let get = |key: &Pubkey| account_provider.get(key).map(|a| a.data().to_vec());
        let market = Market::load(self.key, &self.mint, get).map_err(err)?;
        if market.launch.pool != self.key {
            return Err(AmmError::from("the coin's launch account belongs to another pool"));
        }
        let published = get(&stoke_quote::hook_accounts_address(&self.key)).ok_or_else(|| AmmError::from("the pool's published hook accounts are missing"))?;
        self.hook_accounts = stoke_quote::decode_hook_accounts(&published).ok_or_else(|| AmmError::from("the pool's published hook accounts can't be read"))?;
        self.market = Some(market);
        Ok(())
    }

    fn quote(&self, quote_params: &QuoteParams) -> Result<Quote, AmmError> {
        if quote_params.swap_mode != SwapMode::ExactIn {
            return Err(AmmError::from("Stoke coins are quoted for exact input only"));
        }
        let is_buy = self.is_buy(&quote_params.input_mint, &quote_params.output_mint)?;
        let clock = Clock { slot: self.clock.slot.load(Ordering::Relaxed), unix_timestamp: self.clock.unix_timestamp.load(Ordering::Relaxed) };
        let q = self.loaded()?.quote(is_buy, quote_params.amount, &ANYONE, &clock).map_err(err)?;
        Ok(Quote {
            in_amount: q.amount_in,
            out_amount: q.amount_out,
            // Every Stoke fee is taken in SOL, whichever way the trade goes.
            fee_amount: q.fee,
            fee_mint: WSOL_MINT,
            fee_pct: Decimal::new(q.fee_bps as i64, 4),
        })
    }

    fn get_swap_and_account_metas(&self, swap_params: &SwapParams) -> Result<SwapAndAccountMetas, AmmError> {
        let market = self.loaded()?;
        let is_buy = self.is_buy(&swap_params.source_mint, &swap_params.destination_mint)?;
        let (sol, tokens) = if is_buy { (swap_params.source_token_account, swap_params.destination_token_account) } else { (swap_params.destination_token_account, swap_params.source_token_account) };
        // The PoolManager's `swap`, in its own order: the trader signs and pays from their token
        // account; the hook's accounts follow as the pool publishes them.
        let account_metas: Vec<AccountMeta> = market.swap_accounts(&swap_params.token_transfer_authority, &sol, &tokens, &self.hook_accounts);
        Ok(SwapAndAccountMetas {
            // The one thing the instruction's data needs besides the amount: the direction.
            swap: Swap::Placeholder { data: Some(vec![market.zero_for_one(is_buy) as u8]) },
            account_metas,
        })
    }

    fn program_dependencies(&self) -> Vec<(Pubkey, String)> {
        vec![
            (LAUNCH_HOOK_ID, "stoke_launch_hook".into()),
            (HOOK_LAUNCH_TAX_ID, "stoke_hook_launch_tax".into()),
            (HOOK_MAX_BUY_ID, "stoke_hook_max_buy".into()),
        ]
    }

    fn get_accounts_len(&self) -> usize {
        // A coin with a hook passes the most: its launch, the hook's position, its fee zones or
        // its hook's three accounts.
        SWAP_ACCOUNTS + if self.market.is_some() { self.hook_accounts.len() } else { 5 }
    }

    /// False for a coin this crate can't quote (its creator's own hook is at work).
    fn is_active(&self) -> bool {
        self.market.as_ref().is_none_or(|m| m.quotable().is_ok())
    }
}

/// The native swap's instruction data from what [`Amm::get_swap_and_account_metas`] returned
/// and the amount going in: what Jupiter's test kit calls the encoder. `min_out` is left at
/// zero there (the kit asserts the exact amount out itself).
pub fn encode_swap(swap: &Swap, in_amount: u64) -> Vec<u8> {
    match swap {
        Swap::Placeholder { data: Some(data) } if data.len() == 1 => swap_data(data[0] != 0, in_amount, 0),
        other => panic!("not a Stoke swap: {other:?}"),
    }
}
