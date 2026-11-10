use anchor_lang::prelude::*;

pub const CONFIG_SEED: &[u8] = b"config";
pub const LAUNCH_SEED: &[u8] = b"launch";
pub const RESERVE_SEED: &[u8] = b"reserve";
pub const STAKE_SEED: &[u8] = b"stake";
pub const STAKE_VAULT_SEED: &[u8] = b"stake-vault";

pub const BPS: u64 = 10_000;
/// Rewards per unit of stake weight are kept in Q64.64.
pub const ACC_SHIFT: u32 = 64;
/// Smallest stake (1 whole token), so the reward index can't be blown up by dust stakes.
pub const MIN_STAKE: u64 = 1_000_000;

/// Staking tiers: (lock seconds, weight multiplier in bps). Holding the coin earns 1× without
/// staking, so tier 0 (flexible, 1×) takes no new stakes; it stays so old flexible stakes can leave.
pub const STAKE_TIERS: [(i64, u64); 3] = [(0, 10_000), (7 * 86_400, 15_000), (30 * 86_400, 20_000)];

/// The only wallet `withdraw_holder_rewards` can pay: the keeper, which pays every holder (1×)
/// and locked staker (1.5× / 2×) their share each hourly epoch, weighted by average balance.
pub const HOLDER_PAYOUT: Pubkey = pubkey!("EyMvXkRrrBqt7nBdwS3XzaJULA6tAAW4Ar5myLJ5z6Eq");

#[account]
#[derive(InitSpace)]
pub struct LaunchConfig {
    pub admin: Pubkey,
    /// Receives the launch fee and the protocol's share of trading fees.
    pub fee_recipient: Pubkey,
    /// What launches are priced in (wrapped SOL).
    pub quote_mint: Pubkey,
    /// Flat fee per launch, in lamports.
    pub launch_fee: u64,
    /// Quote raised by the curve before it graduates.
    pub raise_target: u64,
    /// The curve's shape (a, in bps). The start→graduation market cap multiple is 1/a².
    pub curve_ratio_bps: u16,
    /// The trading fee each creator may choose, in bps of the SOL side of every trade.
    pub min_fee_bps: u16,
    pub max_fee_bps: u16,
    /// Shares of the trading fee (bps of the fee). The coin's liquidity gets the rest.
    pub protocol_share_bps: u16,
    pub creator_share_bps: u16,
    pub staker_share_bps: u16,
    /// Anti-snipe, the same for every coin: buys in the launch slot and the next
    /// `snipe_slots - 1` slots (≈ 400 ms each) pay this tax, which buys tokens that are burned.
    pub snipe_fee_bps: u16,
    pub snipe_slots: u32,
    pub launches_paused: bool,
    pub bump: u8,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq, InitSpace)]
pub enum Status {
    /// Trading on the hook's curve; the pool has no liquidity.
    Curve,
    /// The curve sold out and its reserves were added to the pool, locked.
    Graduated,
}

#[account]
#[derive(InitSpace)]
pub struct Launch {
    pub mint: Pubkey,
    pub creator: Pubkey,
    pub pool: Pubkey,
    /// The hook's full-range position, funded at graduation and never removable.
    pub position: Pubkey,
    pub token_is_0: bool,
    pub status: Status,
    pub created_at: i64,
    pub created_slot: u64,
    pub graduated_at: i64,
    /// Anti-snipe tax (from the config at launch): `snipe_fee_bps` on buys in the first
    /// `snipe_slots` slots, counting the launch slot. The creator's dev buy is exempt.
    pub snipe_fee_bps: u16,
    pub snipe_slots: u32,
    /// The trading fee the creator chose at launch (bps of the SOL side of every trade).
    pub fee_bps: u16,
    pub protocol_share_bps: u16,
    pub creator_share_bps: u16,
    pub staker_share_bps: u16,
    pub raise_target: u64,
    pub virtual_quote: u64,
    pub virtual_token: u64,
    pub real_quote: u64,
    /// Tokens left for sale on the curve.
    pub real_token: u64,
    /// Tokens reserved for the pool.
    pub lp_tokens: u64,
    pub liquidity: u128,
    /// Tokens bought by the sniper tax (and graduation dust), burned by `burn_pending`.
    pub burn_pending: u64,
    pub burned: u64,
    /// Tokens the creator bought at launch; their sells are fee-free until this runs out.
    pub dev_free_tokens: u64,
    // ---- the trading fee's shares (all SOL, held as the hook's claims on the pool)
    /// Protocol's unwithdrawn share.
    pub protocol_quote: u64,
    pub protocol_earned: u64,
    /// The flat fee the creator paid to launch (lamports, to the fee recipient).
    pub launch_fee_paid: u64,
    /// Creator's unclaimed share.
    pub creator_quote: u64,
    pub creator_earned: u64,
    /// Holders' share (the old stakers' share), waiting for `withdraw_holder_rewards` and the
    /// keeper's next epoch payout. Also covers any pending rewards of stakes from before holders
    /// earned without staking (the reward index no longer grows).
    pub staker_owed: u64,
    pub stakers_earned: u64,
    /// Reserves share waiting to be added to the pool (`deepen` adds it).
    pub liquidity_pending: u64,
    /// The coin's reserves share: on the curve, what fees put into its real reserves; after
    /// graduation, what `deepen` has added to the pool.
    pub liquidity_added: u64,
    /// The buy-side position between the launch and graduation prices that `deepen` fills.
    pub floor_tick_lower: i32,
    pub floor_tick_upper: i32,
    pub floor_liquidity: u128,
    // ---- staking
    pub staked: u64,
    pub total_weight: u128,
    /// Rewards per unit of weight, Q64.64, only ever growing.
    pub reward_index: u128,
    pub stakers: u32,
    pub volume_quote: u64,
    pub trades: u64,
    pub bump: u8,
}

impl Launch {
    pub fn curve(&self) -> crate::curve::Curve {
        crate::curve::Curve {
            virtual_quote: self.virtual_quote,
            virtual_token: self.virtual_token,
            real_quote: self.real_quote,
            real_token: self.real_token,
        }
    }

    pub fn set_curve(&mut self, c: &crate::curve::Curve) {
        self.virtual_quote = c.virtual_quote;
        self.virtual_token = c.virtual_token;
        self.real_quote = c.real_quote;
        self.real_token = c.real_token;
    }

    /// Anti-snipe tax in bps for a buy landing in `slot`.
    pub fn snipe_bps(&self, slot: u64) -> u16 {
        if slot < self.created_slot.saturating_add(self.snipe_slots as u64) {
            self.snipe_fee_bps
        } else {
            0
        }
    }

    /// The fee rate for a trade: the creator's sells are free up to what they bought at launch,
    /// and a sell larger than what's left pays only on the part above it.
    pub fn fee_bps_for(&self, sender: &Pubkey, is_buy: bool, tokens: u64) -> u16 {
        if is_buy || *sender != self.creator || self.dev_free_tokens == 0 || tokens == 0 {
            return self.fee_bps;
        }
        if tokens <= self.dev_free_tokens {
            return 0;
        }
        let charged = (tokens - self.dev_free_tokens) as u128;
        (self.fee_bps as u128 * charged).div_ceil(tokens as u128) as u16
    }

    /// Record a creator sell against their fee-free allowance.
    pub fn use_dev_allowance(&mut self, sender: &Pubkey, is_buy: bool, tokens: u64) {
        if !is_buy && *sender == self.creator {
            self.dev_free_tokens = self.dev_free_tokens.saturating_sub(tokens);
        }
    }

    /// A buy by the creator in the launch slot is the dev buy.
    pub fn is_dev_buy(&self, sender: &Pubkey, is_buy: bool, slot: u64) -> bool {
        is_buy && *sender == self.creator && slot == self.created_slot
    }

    /// Split a fee between the creator, holders and the coin's own reserves. The protocol takes no
    /// share of trading fees (its income is the launch fee; `protocol_share_bps` stays for the account
    /// layout). The holders' share is paid out off-chain by epoch (see HOLDER_PAYOUT). The reserves
    /// share is real SOL for the coin: on the curve it joins the curve's real reserves without moving
    /// the price (real SOL replacing virtual liquidity, counted in `liquidity_added`), and after
    /// graduation it waits for `deepen` to add it to the pool. Returns (protocol, creator, holders, reserves).
    pub fn split_fee(&mut self, fee: u64) -> Result<(u64, u64, u64, u64)> {
        if fee == 0 {
            return Ok((0, 0, 0, 0));
        }
        let creator = (fee as u128 * self.creator_share_bps as u128 / BPS as u128) as u64;
        let stakers = (fee as u128 * self.staker_share_bps as u128 / BPS as u128) as u64;
        let reserves = fee - creator - stakers;
        self.creator_quote = self.creator_quote.checked_add(creator).ok_or(crate::errors::LaunchError::MathError)?;
        self.creator_earned = self.creator_earned.saturating_add(creator);
        self.staker_owed = self.staker_owed.checked_add(stakers).ok_or(crate::errors::LaunchError::MathError)?;
        self.stakers_earned = self.stakers_earned.saturating_add(stakers);
        if self.status == Status::Curve {
            self.real_quote = self.real_quote.checked_add(reserves).ok_or(crate::errors::LaunchError::MathError)?;
            self.liquidity_added = self.liquidity_added.saturating_add(reserves);
        } else {
            self.liquidity_pending = self.liquidity_pending.checked_add(reserves).ok_or(crate::errors::LaunchError::MathError)?;
        }
        Ok((0, creator, stakers, reserves))
    }
}

/// One holder's stake in one coin, per tier.
#[account]
#[derive(InitSpace)]
pub struct Stake {
    pub launch: Pubkey,
    pub owner: Pubkey,
    pub tier: u8,
    pub amount: u64,
    pub weight: u128,
    /// weight × reward_index at the last update (Q64.64).
    pub reward_debt: u128,
    /// Rewards earned and not yet claimed (lamports).
    pub pending: u64,
    pub claimed: u64,
    pub lock_until: i64,
    pub bump: u8,
}

/// weight × index >> 64, in 256-bit so neither side can overflow.
pub fn accrued(weight: u128, index: u128) -> Result<u128> {
    use meridian_core::math::U256;
    let v = (U256::from(weight) * U256::from(index)) >> ACC_SHIFT;
    require!(v.bits() <= 128, crate::errors::LaunchError::MathError);
    Ok(v.as_u128())
}

impl Stake {
    /// Move everything earned since the last update into `pending`.
    pub fn settle(&mut self, index: u128) -> Result<()> {
        let now = accrued(self.weight, index)?;
        let earned = now.checked_sub(self.reward_debt).ok_or(crate::errors::LaunchError::MathError)?;
        self.pending = self.pending.checked_add(u64::try_from(earned).map_err(|_| crate::errors::LaunchError::MathError)?).ok_or(crate::errors::LaunchError::MathError)?;
        self.reward_debt = now;
        Ok(())
    }
}

// ---------------------------------------------------------------------------- fee zones

pub const ZONES_SEED: &[u8] = b"zones";
/// Fee zones are an option a creator picks at launch: up to this many market cap bands.
pub const MAX_FEE_ZONES: usize = 5;
/// The most a zone can charge on sells (10%).
pub const MAX_ZONE_SELL_BPS: u16 = 1_000;
/// Where zones may sit, in SOL market cap (lamports). The launch page holds creators to
/// $100k–$350k at the day's SOL price; these bounds keep that true on-chain for SOL between
/// roughly $50 and $250, and keep zones well clear of the launch price (≈206 SOL).
pub const ZONE_MIN_MCAP: u64 = 400 * 1_000_000_000;
pub const ZONE_MAX_MCAP: u64 = 7_000 * 1_000_000_000;
/// The zones switch off for good once the coin has held above its top zone this long, so one
/// buy can't switch them off for a dump right after it.
pub const ZONES_OFF_AFTER_SECS: i64 = 300;
/// A coin's options are marked in its `protocol_share_bps` (unused since the protocol stopped taking
/// a trading fee), so every swap of it requires the accounts they need: fee zones, a coin hook, or both.
pub const FEE_ZONES_MARK: u16 = u16::MAX;
pub const COIN_HOOK_MARK: u16 = u16::MAX - 1;
pub const ZONES_AND_HOOK_MARK: u16 = u16::MAX - 2;

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, Default, PartialEq, Eq, InitSpace)]
pub struct FeeZone {
    /// The band, in SOL market cap (lamports): from (inclusive) to (exclusive).
    pub from_mcap: u64,
    pub to_mcap: u64,
    /// The sell fee inside the band (bps): above the coin's fee, at most 10%.
    pub sell_fee_bps: u16,
}

/// A coin's fee zones (seeds: ZONES_SEED, launch). Only sells pay them, and only while the zones
/// are on: a sell pays the highest fee among the zones its price passes through.
#[account]
#[derive(InitSpace)]
pub struct FeeZones {
    pub launch: Pubkey,
    /// Lowest first, not overlapping.
    #[max_len(5)]
    pub zones: Vec<FeeZone>,
    /// Off for good: the coin held above its top zone for ZONES_OFF_AFTER_SECS.
    pub off: bool,
    pub off_at: i64,
    /// When the coin was first seen above its top zone in its current stay there (0: it isn't).
    pub above_since: i64,
    /// The market cap at the start of the swap in progress: beforeSwap records it for afterSwap.
    pub swap_start_mcap: u64,
    pub bump: u8,
}

impl FeeZones {
    /// The top of the highest zone.
    pub fn top(&self) -> u64 {
        self.zones.last().map_or(0, |z| z.to_mcap)
    }

    /// Before each swap: track how long the coin has held above its top zone, and switch the
    /// zones off once it's held there long enough. Returns true when they just switched off.
    pub fn track(&mut self, mcap: u64, now: i64) -> bool {
        if self.off {
            return false;
        }
        if mcap < self.top() {
            self.above_since = 0;
        } else if self.above_since == 0 {
            self.above_since = now.max(1);
        } else if now - self.above_since >= ZONES_OFF_AFTER_SECS {
            self.off = true;
            self.off_at = now;
            return true;
        }
        false
    }

    /// The sell fee for a sell moving the market cap from `start` down to `end`: the highest fee
    /// among the zones it passes through, or 0 (the coin's own fee) if none or the zones are off.
    pub fn sell_bps(&self, start: u64, end: u64) -> u16 {
        if self.off {
            return 0;
        }
        let (hi, lo) = (start.max(end), start.min(end));
        self.zones.iter().filter(|z| z.from_mcap <= hi && z.to_mcap > lo).map(|z| z.sell_fee_bps).max().unwrap_or(0)
    }
}

impl Launch {
    pub fn has_fee_zones(&self) -> bool {
        matches!(self.protocol_share_bps, FEE_ZONES_MARK | ZONES_AND_HOOK_MARK)
    }

    pub fn has_coin_hook(&self) -> bool {
        matches!(self.protocol_share_bps, COIN_HOOK_MARK | ZONES_AND_HOOK_MARK)
    }
}

// ---------------------------------------------------------------------------- coin hooks (Stoke v3)

pub use meridian_core::coin_hook::{HOOK_LISTING_SEED, MAX_COIN_HOOK_CONFIG, MAX_COIN_HOOK_FEE_BPS};

pub const COIN_HOOK_SEED: &[u8] = b"coin-hook";
/// `CoinHook::swap_fee_bps` when the hook left the swap in progress as it is.
pub const NO_HOOK_FEE: u16 = u16::MAX;
/// `CoinHook::swap_pool_quote` when the swap in progress has nothing to check.
pub const NO_POOL_CHECK: u64 = u64::MAX;
/// Buys always leave the buyer something: the anti-snipe tax plus a hook's fee stop here.
pub const MAX_BUY_TAX_BPS: u16 = 9_900;

/// A hook program creators can add to their coins (seeds: HOOK_LISTING_SEED, program). Anyone
/// lists one, once its code is locked; only the config's admin can pause it. The account also signs this program's calls into the hook, which is
/// how the hook knows a call is real (`meridian_core::verify_coin_hook_call` reads the first two
/// fields, so they stay first).
#[account]
#[derive(InitSpace)]
pub struct HookListing {
    pub program: Pubkey,
    /// Paused: it can't be added to new coins, and coins that have it trade as if they had none.
    pub paused: bool,
    pub listed_at: i64,
    /// Coins launched with it.
    pub coins: u32,
    pub bump: u8,
}

/// A coin's hook (seeds: COIN_HOOK_SEED, launch): set by the creator in the launch transaction,
/// before any trade, and never changed or removed.
#[account]
#[derive(InitSpace)]
pub struct CoinHook {
    pub launch: Pubkey,
    pub program: Pubkey,
    /// The hook's listing account, which signs every call into it.
    pub listing: Pubkey,
    /// The creator's settings, passed to the hook on every trade.
    #[max_len(MAX_COIN_HOOK_CONFIG)]
    pub config: Vec<u8>,
    /// The fee the hook set for the swap in progress (NO_HOOK_FEE: none): beforeSwap records it
    /// for afterSwap.
    pub swap_fee_bps: u16,
    pub attached_at: i64,
    pub bump: u8,
    /// The SOL the pool has to trade in the swap in progress (NO_POOL_CHECK: nothing to check).
    /// When the trader fixes the SOL side, the fee is taken up front on all of it: afterSwap
    /// refuses a swap the pool cut short, so the fee is never charged on SOL that didn't trade.
    ///
    /// Added after the first release, at the end: an account from before it is 8 bytes shorter
    /// on paper, but every account is allocated for 64 bytes of settings and both first hooks
    /// use 12, so this field reads as the zeros after them and is written in the same room.
    pub swap_pool_quote: u64,
}
