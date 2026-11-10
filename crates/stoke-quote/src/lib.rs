//! # stoke-quote
//!
//! What an exact-input trade of a Stoke coin returns, computed off-chain from the coin's
//! accounts. No network, no clock of its own: the caller passes the accounts and the cluster's
//! clock, and gets the amount the on-chain swap would pay out.
//!
//! It is the programs' own code wherever that's possible: the curve (`meridian_launch::curve`),
//! the pool engine (`meridian_pool_manager::engine`), the fee split, fee zones and the two Stoke
//! hooks are the same functions the programs run. The only thing written again here is the order
//! the launch hook's `before_swap` / `after_swap` call them in.
//!
//! A coin trades in one of two places:
//! - **On its curve**, until the curve sells out. The buy that sells the last curve token also
//!   graduates the coin: the curve's reserves become the pool's liquidity and the rest of that buy
//!   fills from the pool, all in one swap. `quote` follows it through.
//! - **In its pool**, after that.
//!
//! Every fee is taken in SOL: the coin's own fee, a fee zone's on a sell, or the fee its hook
//! sets. A coin whose hook isn't one of Stoke's can't be quoted here (its own program decides),
//! and `quote` says so with [`QuoteError::UnknownHook`].

use anchor_lang::prelude::{AccountMeta, Pubkey};
use anchor_lang::{AccountDeserialize, InstructionData, ToAccountMetas};
use meridian_core::coin_hook::{CoinInfo, TradeArgs, MAX_COIN_HOOK_FEE_BPS};
use meridian_core::math::{get_amount0_delta, get_amount1_delta, get_liquidity_for_amounts, get_sqrt_price_at_tick, MAX_SQRT_PRICE, MIN_SQRT_PRICE};
use meridian_core::{full_range, PoolState, Settle, SwapParams};
use meridian_launch::instructions::{curve_mcap, pool_mcap, sell_override, TICK_SPACING, TOTAL_SUPPLY};
use meridian_launch::state::{CoinHook, FeeZones, HookListing, Launch, Status, BPS, MAX_BUY_TAX_BPS};
use meridian_pool_manager::engine::{self, PositionData};
use meridian_pool_manager::ticks::TicksView;
use stoke_hook_launch_tax as launch_tax;
use stoke_hook_max_buy as max_buy;

pub use meridian_core::hook::decode_hook_accounts;
pub use meridian_core::POOL_MANAGER_ID;
pub use meridian_launch::ID as LAUNCH_HOOK_ID;

/// Stoke's hooks: the ones this crate can quote.
pub const HOOK_LAUNCH_TAX_ID: Pubkey = anchor_lang::pubkey!("3zbuMvvfy73QL6U8hsceeZxCpju2mDNed3YaQfTcdtjS");
pub const HOOK_MAX_BUY_ID: Pubkey = anchor_lang::pubkey!("AXDdW1eY1YcxrxsrR6wGnj45hAFNtGtHrXCRsxZzKMqo");
/// Wrapped SOL: what every coin is priced in.
pub const WSOL_MINT: Pubkey = anchor_lang::pubkey!("So11111111111111111111111111111111111111112");

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuoteError {
    /// An account isn't what it should be (wrong owner's data, wrong coin, too short).
    BadAccount(&'static str),
    /// The coin has fee zones or a hook, and that account wasn't given.
    MissingAccount(&'static str),
    /// The trade is too small to move a token or a lamport.
    AmountTooSmall,
    /// The coin's hook refuses this trade (its program's error code).
    HookRefuses(u32),
    /// The coin's hook isn't one of Stoke's, so only its own program knows what it does.
    UnknownHook(Pubkey),
    /// The programs' math refused the trade (an overflow, a pool with nothing to trade against).
    Math,
}

impl core::fmt::Display for QuoteError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BadAccount(what) => write!(f, "{what} isn't a valid account of its kind"),
            Self::MissingAccount(what) => write!(f, "the coin's {what} account is needed to quote it"),
            Self::AmountTooSmall => write!(f, "amount too small to trade"),
            Self::HookRefuses(code) => write!(f, "the coin's hook refuses this trade (error {code})"),
            Self::UnknownHook(program) => write!(f, "the coin's hook {program} isn't one this crate can quote"),
            Self::Math => write!(f, "the trade can't be computed"),
        }
    }
}

impl std::error::Error for QuoteError {}

/// The cluster's clock, as the launch hook reads it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Clock {
    pub slot: u64,
    pub unix_timestamp: i64,
}

/// A coin's hook and whether Stoke has paused it (a paused hook isn't called).
#[derive(Clone)]
pub struct Hook {
    pub state: CoinHook,
    pub paused: bool,
}

/// Everything that decides what a trade of one coin returns.
#[derive(Clone)]
pub struct Market {
    pub pool_key: Pubkey,
    pub pool: PoolState,
    /// The pool's tick store, as its account holds it.
    pub ticks: Vec<u8>,
    pub launch_key: Pubkey,
    pub launch: Launch,
    pub zones: Option<FeeZones>,
    pub hook: Option<Hook>,
}

/// What an exact-input trade returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TradeQuote {
    /// What the trader gives: lamports for a buy, token units for a sell.
    pub amount_in: u64,
    /// What they receive: token units for a buy, lamports for a sell.
    pub amount_out: u64,
    /// The fee, in lamports. (The anti-snipe tax of a coin's first second isn't a fee: it buys tokens that are burned.)
    pub fee: u64,
    /// The fee's rate, in bps of the trade's SOL side.
    pub fee_bps: u16,
    /// This buy sells the curve's last token: it graduates the coin and fills the rest from its new pool.
    pub graduates: bool,
}

// ---------------------------------------------------------------------------- decoding accounts

pub fn decode_pool(data: &[u8]) -> Result<PoolState, QuoteError> {
    PoolState::from_account_data(data).ok_or(QuoteError::BadAccount("pool"))
}

pub fn decode_launch(data: &[u8]) -> Result<Launch, QuoteError> {
    Launch::try_deserialize(&mut &data[..]).map_err(|_| QuoteError::BadAccount("launch"))
}

pub fn decode_zones(data: &[u8]) -> Result<FeeZones, QuoteError> {
    FeeZones::try_deserialize(&mut &data[..]).map_err(|_| QuoteError::BadAccount("fee zones"))
}

pub fn decode_coin_hook(data: &[u8]) -> Result<CoinHook, QuoteError> {
    CoinHook::try_deserialize(&mut &data[..]).map_err(|_| QuoteError::BadAccount("coin hook"))
}

pub fn decode_listing(data: &[u8]) -> Result<HookListing, QuoteError> {
    HookListing::try_deserialize(&mut &data[..]).map_err(|_| QuoteError::BadAccount("hook listing"))
}

// ---------------------------------------------------------------------------- addresses

fn launch_pda(seeds: &[&[u8]]) -> Pubkey {
    Pubkey::find_program_address(seeds, &LAUNCH_HOOK_ID).0
}

fn manager_pda(seeds: &[&[u8]]) -> Pubkey {
    Pubkey::find_program_address(seeds, &POOL_MANAGER_ID).0
}

/// A coin's launch account: its curve, its fee and its fee buckets.
pub fn launch_address(mint: &Pubkey) -> Pubkey {
    launch_pda(&[meridian_launch::state::LAUNCH_SEED, mint.as_ref()])
}

/// A coin's fee zones (it only exists for coins launched with them).
pub fn zones_address(launch: &Pubkey) -> Pubkey {
    launch_pda(&[meridian_launch::state::ZONES_SEED, launch.as_ref()])
}

/// A coin's hook (it only exists for coins launched with one).
pub fn coin_hook_address(launch: &Pubkey) -> Pubkey {
    launch_pda(&[meridian_launch::state::COIN_HOOK_SEED, launch.as_ref()])
}

/// A hook program's listing.
pub fn listing_address(hook_program: &Pubkey) -> Pubkey {
    launch_pda(&[meridian_core::HOOK_LISTING_SEED, hook_program.as_ref()])
}

/// The launch hook's full-range position in a coin's pool: empty until graduation, locked after.
pub fn position_address(pool: &Pubkey) -> Pubkey {
    let (lo, hi) = full_range(TICK_SPACING);
    let authority = meridian_core::hook_authority(&LAUNCH_HOOK_ID).0;
    manager_pda(&[meridian_core::POSITION_SEED, pool.as_ref(), authority.as_ref(), &lo.to_le_bytes(), &hi.to_le_bytes(), &[0u8; 32]])
}

/// The PoolManager's signer for a pool's hook calls.
pub fn hook_caller_address(pool: &Pubkey) -> Pubkey {
    manager_pda(&[meridian_core::HOOK_CALLER_SEED, pool.as_ref()])
}

/// The launch hook's claims on a pool: where the curve's tokens and every fee sit.
pub fn hook_claims_address(pool: &Pubkey) -> Pubkey {
    let authority = meridian_core::hook_authority(&LAUNCH_HOOK_ID).0;
    manager_pda(&[meridian_core::CLAIMS_SEED, pool.as_ref(), authority.as_ref()])
}

/// The PoolManager's vault authority.
pub fn manager_authority() -> Pubkey {
    manager_pda(&[meridian_core::AUTHORITY_SEED])
}

/// Where the launch hook publishes the extra accounts a swap of this pool has to pass, in order
/// (decode it with [`decode_hook_accounts`]).
pub fn hook_accounts_address(pool: &Pubkey) -> Pubkey {
    launch_pda(&[meridian_core::HOOK_ACCOUNTS_SEED, pool.as_ref()])
}

// ---------------------------------------------------------------------------- swapping

/// The data of the PoolManager's `swap` instruction for an exact-input trade: `amount_in` in,
/// at least `min_out` out or the swap fails, the price limit left open (what [`Market::quote`]
/// assumes), settled in tokens.
pub fn swap_data(zero_for_one: bool, amount_in: u64, min_out: u64) -> Vec<u8> {
    meridian_pool_manager::instruction::Swap {
        params: SwapParams {
            zero_for_one,
            amount_specified: -(amount_in as i128),
            sqrt_price_limit: if zero_for_one { MIN_SQRT_PRICE + 1 } else { MAX_SQRT_PRICE - 1 },
        },
        hook_data: Vec::new(),
        settle: Settle::default(),
        amount_limit: min_out,
    }
    .data()
}

// ---------------------------------------------------------------------------- quoting

fn fee_on_gross(amount: u64, bps: u16) -> u64 {
    (amount as u128 * bps as u128 / BPS as u128) as u64
}

fn amount(x: i128) -> Result<u64, QuoteError> {
    u64::try_from(x.unsigned_abs()).map_err(|_| QuoteError::Math)
}

impl Market {
    /// Every account a quote of this coin can depend on, whatever the coin has: its pool and
    /// tick store, its launch account, its fee zones and hook (accounts that only exist for coins
    /// launched with them), the list of accounts its swaps pass, and the listings of Stoke's two
    /// hooks. The list never changes for a coin.
    pub fn accounts(pool_key: &Pubkey, pool: &PoolState, mint: &Pubkey) -> Vec<Pubkey> {
        let launch = launch_address(mint);
        vec![
            *pool_key,
            pool.ticks,
            launch,
            zones_address(&launch),
            coin_hook_address(&launch),
            hook_accounts_address(pool_key),
            listing_address(&HOOK_LAUNCH_TAX_ID),
            listing_address(&HOOK_MAX_BUY_ID),
        ]
    }

    /// Read a coin from its accounts. `get` returns an account's data, or `None` if it doesn't
    /// exist. A hook whose listing `get` doesn't return counts as not paused.
    pub fn load(pool_key: Pubkey, mint: &Pubkey, get: impl Fn(&Pubkey) -> Option<Vec<u8>>) -> Result<Market, QuoteError> {
        let pool = decode_pool(&get(&pool_key).ok_or(QuoteError::MissingAccount("pool"))?)?;
        let ticks = get(&pool.ticks).ok_or(QuoteError::MissingAccount("ticks"))?;
        let launch_key = launch_address(mint);
        let launch = decode_launch(&get(&launch_key).ok_or(QuoteError::MissingAccount("launch"))?)?;
        let zones = match get(&zones_address(&launch_key)).filter(|_| launch.has_fee_zones()) {
            Some(data) => Some(decode_zones(&data)?),
            None => None,
        };
        let hook = match get(&coin_hook_address(&launch_key)).filter(|_| launch.has_coin_hook()) {
            Some(data) => {
                let state = decode_coin_hook(&data)?;
                let paused = match get(&state.listing) {
                    Some(listing) => decode_listing(&listing)?.paused,
                    None => false,
                };
                Some(Hook { state, paused })
            }
            None => None,
        };
        Ok(Market { pool_key, pool, ticks, launch_key, launch, zones, hook })
    }

    /// The coin's mint (the pool's other currency is wrapped SOL).
    pub fn mint(&self) -> Pubkey {
        self.launch.mint
    }

    /// The pool's direction for a trade: a buy moves SOL in and the coin out.
    pub fn zero_for_one(&self, is_buy: bool) -> bool {
        is_buy != self.launch.token_is_0
    }

    /// The accounts of the PoolManager's `swap` for this coin, in the program's order, settling
    /// in tokens: the trader pays from one of their token accounts and is paid into the other.
    /// `hook_accounts` is the pool's published list (the account at [`hook_accounts_address`]),
    /// passed as it is.
    pub fn swap_accounts(&self, trader: &Pubkey, trader_sol: &Pubkey, trader_tokens: &Pubkey, hook_accounts: &[(Pubkey, bool)]) -> Vec<AccountMeta> {
        let (p, token_is_0) = (&self.pool, self.launch.token_is_0);
        let mut metas = meridian_pool_manager::accounts::Swap {
            sender: *trader,
            pool: self.pool_key,
            ticks: p.ticks,
            vault0: p.vault0,
            vault1: p.vault1,
            token0: Some(if token_is_0 { *trader_tokens } else { *trader_sol }),
            token1: Some(if token_is_0 { *trader_sol } else { *trader_tokens }),
            sender_claims: None,
            mint0: p.currency0,
            mint1: p.currency1,
            token_program0: p.token_program0,
            token_program1: p.token_program1,
            hooks: p.hooks,
            hook_caller: hook_caller_address(&self.pool_key),
            hook_claims: Some(hook_claims_address(&self.pool_key)),
            authority: manager_authority(),
        }
        .to_account_metas(None);
        metas.extend(hook_accounts.iter().map(|(key, writable)| if *writable { AccountMeta::new(*key, false) } else { AccountMeta::new_readonly(*key, false) }));
        metas
    }

    pub fn graduated(&self) -> bool {
        self.launch.status == Status::Graduated
    }

    /// The coin's market cap in lamports, as the launch hook measures it.
    pub fn mcap(&self) -> u64 {
        if self.graduated() {
            pool_mcap(self.pool.sqrt_price.get(), self.launch.token_is_0)
        } else {
            curve_mcap(self.launch.virtual_quote, self.launch.virtual_token)
        }
    }

    /// Whether this crate can quote the coin at all: its accounts are all here, and its hook, if
    /// it has one at work, is one of Stoke's.
    pub fn quotable(&self) -> Result<(), QuoteError> {
        if self.launch.pool != self.pool_key {
            return Err(QuoteError::BadAccount("launch"));
        }
        if self.launch.has_fee_zones() && self.zones.is_none() {
            return Err(QuoteError::MissingAccount("fee zones"));
        }
        match (&self.hook, self.launch.has_coin_hook()) {
            (None, true) => Err(QuoteError::MissingAccount("hook")),
            (Some(h), true) if !h.paused && h.state.program != HOOK_LAUNCH_TAX_ID && h.state.program != HOOK_MAX_BUY_ID => Err(QuoteError::UnknownHook(h.state.program)),
            _ => Ok(()),
        }
    }

    /// What the coin's hook does to this trade, as `CoinHookRef::on_trade` asks it: the fee it
    /// sets (at most 10%), or nothing. A paused hook isn't asked.
    fn hook_fee(&self, trader: &Pubkey, is_buy: bool, amount: u64, mcap: u64, clock: &Clock) -> Result<Option<u16>, QuoteError> {
        let Some(h) = self.hook.as_ref().filter(|_| self.launch.has_coin_hook()) else { return Ok(None) };
        if h.paused {
            return Ok(None);
        }
        let l = &self.launch;
        // Exact input: the side the trader fixed is exact, the other is valued at the price before the trade.
        let supply = TOTAL_SUPPLY as u128;
        let cap = |x: u128| x.min(u64::MAX as u128) as u64;
        let (sol, tokens) = if is_buy { (amount, cap(amount as u128 * supply / mcap.max(1) as u128)) } else { (cap(amount as u128 * mcap as u128 / supply), amount) };
        let args = TradeArgs {
            coin: CoinInfo { mint: l.mint, creator: l.creator, fee_bps: l.fee_bps, created_at: l.created_at, created_slot: l.created_slot },
            config: h.state.config.clone(),
            trader: *trader,
            is_buy,
            exact_in: true,
            sol,
            tokens,
            graduated: l.status == Status::Graduated,
            mcap,
            now: clock.unix_timestamp,
            slot: clock.slot,
        };
        let fee = if h.state.program == HOOK_LAUNCH_TAX_ID {
            launch_tax::Settings::decode(&args.config).and_then(|s| launch_tax::decide(&s, &args))
        } else if h.state.program == HOOK_MAX_BUY_ID {
            match max_buy::Settings::decode(&args.config).and_then(|s| max_buy::refuses(&s, &args)) {
                Some(code) => return Err(QuoteError::HookRefuses(code)),
                None => None,
            }
        } else {
            return Err(QuoteError::UnknownHook(h.state.program));
        };
        Ok(fee.map(|f| f.min(MAX_COIN_HOOK_FEE_BPS)))
    }

    /// The pool's own exact-input swap of `amount_in`, with the price left open. Returns (in, out).
    fn pool_swap(&self, st: &mut PoolState, ticks: &mut [u8], zero_for_one: bool, amount_in: u64) -> Result<(u64, u64), QuoteError> {
        if amount_in == 0 {
            return Ok((0, 0));
        }
        let mut tv = TicksView::load(ticks, &self.pool_key).map_err(|_| QuoteError::BadAccount("ticks"))?;
        let limit = if zero_for_one { MIN_SQRT_PRICE + 1 } else { MAX_SQRT_PRICE - 1 };
        let res = engine::swap(st, &mut tv, zero_for_one, -(amount_in as i128), limit, 0).map_err(|_| QuoteError::Math)?;
        let (d_in, d_out) = if zero_for_one { (res.delta.amount0, res.delta.amount1) } else { (res.delta.amount1, res.delta.amount0) };
        Ok((amount(d_in)?, amount(d_out)?))
    }

    /// What an exact-input trade returns right now: `amount` lamports in for a buy, `amount`
    /// token units in for a sell. `trader` matters for one wallet only, the coin's creator, whose
    /// sells are fee-free up to what they bought at launch; pass any other key for anyone else.
    ///
    /// The result is what the on-chain swap pays out with its price limit left open
    /// (`MIN_SQRT_PRICE + 1` / `MAX_SQRT_PRICE - 1`): slippage belongs in the swap's amount limit.
    pub fn quote(&self, is_buy: bool, amount: u64, trader: &Pubkey, clock: &Clock) -> Result<TradeQuote, QuoteError> {
        self.quotable()?;
        if amount == 0 {
            return Err(QuoteError::AmountTooSmall);
        }
        // The launch hook's `before_swap`, in its order.
        let mut l = self.launch.clone();
        let start_mcap = self.mcap();
        // Fee zones: track the hold above the top zone first (it may switch them off for this very swap).
        let mut zones = self.zones.clone().filter(|_| l.has_fee_zones());
        if let Some(z) = zones.as_mut() {
            z.track(start_mcap, clock.unix_timestamp);
        }
        let hook_bps = self.hook_fee(trader, is_buy, amount, start_mcap, clock)?;
        let zero_for_one = self.zero_for_one(is_buy);
        let mut st = self.pool;
        let mut ticks = self.ticks.clone();

        if l.status == Status::Graduated {
            if is_buy {
                // SOL is the specified side: the fee comes off the input before the pool.
                let bps = hook_bps.unwrap_or(l.fee_bps);
                let fee = fee_on_gross(amount, bps);
                let (_, out) = self.pool_swap(&mut st, &mut ticks, zero_for_one, amount - fee)?;
                // A trade that pays out nothing isn't one (the swap's minimum would refuse it).
                if out == 0 {
                    return Err(QuoteError::AmountTooSmall);
                }
                return Ok(TradeQuote { amount_in: amount, amount_out: out, fee, fee_bps: bps, graduates: false });
            }
            // A sell: the pool pays out, and `after_swap` takes the fee from that.
            let (_, quote) = self.pool_swap(&mut st, &mut ticks, zero_for_one, amount)?;
            let zone_bps = zones.as_ref().map_or(0, |z| z.sell_bps(start_mcap, pool_mcap(st.sqrt_price.get(), l.token_is_0)));
            let bps = sell_override(zone_bps, hook_bps).unwrap_or_else(|| l.fee_bps_for(trader, false, amount));
            let fee = fee_on_gross(quote, bps);
            if quote - fee == 0 {
                return Err(QuoteError::AmountTooSmall);
            }
            return Ok(TradeQuote { amount_in: amount, amount_out: quote - fee, fee, fee_bps: bps, graduates: false });
        }

        // On the curve.
        let mut curve = l.curve();
        if !is_buy {
            // The highest zone fee between where the sell starts and where it ends, if the zones are on.
            let zone_bps = match zones.as_ref() {
                Some(z) if !z.off => {
                    let mut after = curve;
                    after.apply_sell(&curve.sell_exact_in(amount, 0));
                    z.sell_bps(start_mcap, curve_mcap(after.virtual_quote, after.virtual_token))
                }
                _ => 0,
            };
            let bps = sell_override(zone_bps, hook_bps).unwrap_or_else(|| l.fee_bps_for(trader, false, amount));
            let fill = curve.sell_exact_in(amount, bps);
            if fill.tokens == 0 || fill.quote == 0 {
                return Err(QuoteError::AmountTooSmall);
            }
            return Ok(TradeQuote { amount_in: amount, amount_out: fill.quote, fee: fill.fee, fee_bps: bps, graduates: false });
        }

        // A buy: the coin's fee or its hook's, never so much that the anti-snipe tax leaves nothing.
        let dev_buy = l.is_dev_buy(trader, true, clock.slot);
        let snipe_bps = if dev_buy { 0 } else { l.snipe_bps(clock.slot) };
        let fee_bps = hook_bps.unwrap_or(l.fee_bps).min(MAX_BUY_TAX_BPS.saturating_sub(snipe_bps));
        let fill = curve.buy_exact_in(amount, fee_bps, snipe_bps);
        if fill.tokens == 0 || fill.quote == 0 {
            return Err(QuoteError::AmountTooSmall);
        }
        if !fill.completes {
            return Ok(TradeQuote { amount_in: amount, amount_out: fill.tokens, fee: fill.fee, fee_bps, graduates: false });
        }

        // This buy sells the curve's last token, so it graduates the coin and carries on into the
        // pool with the rest of its SOL, less the fee on that part.
        let pool_fee = fee_on_gross(amount - fill.quote, fee_bps);
        curve.apply_buy(&fill);
        l.set_curve(&curve);
        l.split_fee(fill.fee + pool_fee).map_err(|_| QuoteError::Math)?;
        // `graduate`: the curve's SOL and the reserved tokens become full-range liquidity at the
        // pool's price, one unit short of each so the rounded-up amounts always fit.
        let (lo, hi) = full_range(TICK_SPACING);
        let m = |r: Result<u128, meridian_core::math::MathError>| r.map_err(|_| QuoteError::Math);
        let (sa, sb, sp) = (m(get_sqrt_price_at_tick(lo))?, m(get_sqrt_price_at_tick(hi))?, st.sqrt_price.get());
        let (quote, tokens) = (l.real_quote as u128, l.lp_tokens as u128);
        let (a0, a1) = if l.token_is_0 { (tokens, quote) } else { (quote, tokens) };
        let liquidity = m(get_liquidity_for_amounts(sp, sa, sb, a0.saturating_sub(1), a1.saturating_sub(1)))?;
        let used0 = m(get_amount0_delta(sp, sb, liquidity, true))? as u64;
        let used1 = m(get_amount1_delta(sa, sp, liquidity, true))? as u64;
        let (used_token, used_quote) = if l.token_is_0 { (used0, used1) } else { (used1, used0) };
        if used_token > l.lp_tokens || used_quote > l.real_quote {
            return Err(QuoteError::Math);
        }
        {
            let mut tv = TicksView::load(&mut ticks, &self.pool_key).map_err(|_| QuoteError::BadAccount("ticks"))?;
            let mut position = PositionData::default();
            engine::modify_liquidity(&mut st, &mut tv, &mut position, lo, hi, liquidity as i128).map_err(|_| QuoteError::Math)?;
        }
        let (_, out) = self.pool_swap(&mut st, &mut ticks, zero_for_one, amount - fill.quote - pool_fee)?;
        Ok(TradeQuote { amount_in: amount, amount_out: fill.tokens + out, fee: fill.fee + pool_fee, fee_bps, graduates: true })
    }
}
