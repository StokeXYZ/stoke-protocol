use anchor_lang::prelude::*;

#[event]
pub struct Launched {
    pub launch: Pubkey,
    pub mint: Pubkey,
    pub creator: Pubkey,
    pub pool: Pubkey,
    pub raise_target: u64,
    pub snipe_fee_bps: u16,
    pub snipe_slots: u32,
    pub launch_fee: u64,
    pub fee_bps: u16,
}

#[event]
pub struct CurveTrade {
    pub mint: Pubkey,
    pub trader: Pubkey,
    pub is_buy: bool,
    /// Quote paid (buy, gross) or received (sell, net).
    pub quote: u64,
    pub tokens: u64,
    pub fee: u64,
    pub snipe: u64,
    pub burned: u64,
    pub virtual_quote: u64,
    pub virtual_token: u64,
    pub real_quote: u64,
    pub real_token: u64,
}

#[event]
pub struct Graduated {
    pub mint: Pubkey,
    pub pool: Pubkey,
    pub liquidity: u128,
    pub quote: u64,
    pub tokens: u64,
}

/// The trading fee on a trade after graduation (curve trades report theirs in `CurveTrade`).
#[event]
pub struct PoolFee {
    pub mint: Pubkey,
    pub trader: Pubkey,
    pub is_buy: bool,
    pub fee: u64,
}

#[event]
pub struct Deepened {
    pub mint: Pubkey,
    pub quote: u64,
    pub liquidity: u128,
}

#[event]
pub struct Staked {
    pub mint: Pubkey,
    pub owner: Pubkey,
    pub tier: u8,
    /// Positive to stake, negative to unstake.
    pub amount: i64,
    pub lock_until: i64,
    pub total_staked: u64,
}

#[event]
pub struct RewardsClaimed {
    pub mint: Pubkey,
    pub owner: Pubkey,
    pub amount: u64,
}

/// The holders' share left the coin for the keeper's epoch payout.
#[event]
pub struct HolderRewardsWithdrawn {
    pub mint: Pubkey,
    pub amount: u64,
}

/// A coin's fee zones, set in its launch transaction.
#[event]
pub struct FeeZonesSet {
    pub mint: Pubkey,
    pub zones: Vec<crate::state::FeeZone>,
}

/// A coin's fee zones switched off for good: it held above its top zone.
#[event]
pub struct FeeZonesOff {
    pub mint: Pubkey,
    pub mcap: u64,
}

/// A hook program became available to creators.
#[event]
pub struct HookListed {
    pub program: Pubkey,
}

/// A listed hook was paused (coins that have it trade without it) or unpaused.
#[event]
pub struct HookPaused {
    pub program: Pubkey,
    pub paused: bool,
}

/// A creator added a hook to their coin, in its launch transaction.
#[event]
pub struct HookAttached {
    pub mint: Pubkey,
    pub program: Pubkey,
    pub config: Vec<u8>,
}
