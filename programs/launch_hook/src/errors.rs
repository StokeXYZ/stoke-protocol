use anchor_lang::prelude::*;

#[error_code]
pub enum LaunchError {
    #[msg("Pools with this hook are created by `launch` only")]
    PoolsOnlyThroughLaunch,
    #[msg("Liquidity opens when the curve graduates")]
    LiquidityLockedUntilGraduation,
    #[msg("Launches are paused")]
    LaunchesPaused,
    #[msg("Sniper tax or window above the maximum")]
    SnipeTooHigh,
    #[msg("The mint must have 6 decimals, no supply, no freeze authority, and the creator as mint authority")]
    BadMint,
    #[msg("This pool isn't this launch's")]
    WrongLaunch,
    #[msg("Amount too small to trade")]
    AmountTooSmall,
    #[msg("The curve doesn't hold that much SOL")]
    CurveCannotPay,
    #[msg("Not graduated yet")]
    NotGraduated,
    #[msg("Only the upgrade authority can do this")]
    NotUpgradeAuthority,
    #[msg("Unauthorized")]
    Unauthorized,
    #[msg("Bad config value")]
    BadConfig,
    #[msg("Math error")]
    MathError,
    #[msg("Staking tier must be 0 (flexible), 1 (7 days) or 2 (30 days)")]
    BadTier,
    #[msg("Stake at least one whole token")]
    StakeTooSmall,
    #[msg("This stake is still locked")]
    StillLocked,
    #[msg("Not that much staked")]
    NotEnoughStaked,
    #[msg("Trading fee outside the allowed range")]
    FeeOutOfRange,
    #[msg("Fee zones can only be set by the creator in the launch transaction, before any trade")]
    ZonesOnlyAtLaunch,
    #[msg("Fee zones: 1 to 5 zones, lowest first, not overlapping, within the allowed market caps, each charging more than the coin's fee and at most 10%")]
    BadFeeZones,
    #[msg("This coin has fee zones: pass its zones account (from the pool's published hook accounts)")]
    FeeZonesMissing,
    #[msg("While a coin's fee zones are on, sells must be exact-input (sell a token amount)")]
    ExactOutSellInZones,
    #[msg("A hook can only be added by the creator in the launch transaction, before any trade")]
    HookOnlyAtLaunch,
    #[msg("This hook isn't listed, or it's paused")]
    HookNotListed,
    #[msg("A hook's settings can be at most 64 bytes")]
    HookConfigTooLong,
    #[msg("This coin has a hook: pass its hook accounts (from the pool's published hook accounts)")]
    HookAccountsMissing,
    #[msg("The coin's hook returned something that isn't a trade decision")]
    BadHookReturn,
    #[msg("Set fee zones before adding a hook")]
    ZonesBeforeHook,
    #[msg("A hook can be listed once its code is locked: make the program immutable first (solana program set-upgrade-authority <program> --final)")]
    HookUpgradeable,
    #[msg("The price limit stopped this swap short of the amount asked for: use the amount limit for slippage instead")]
    SwapCutShort,
}
