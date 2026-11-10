use anchor_lang::prelude::*;

/// Names follow Uniswap v4's custom errors where there is one.
#[error_code]
pub enum PmError {
    #[msg("currency0 must sort strictly before currency1")]
    CurrenciesOutOfOrderOrEqual,
    #[msg("Tick spacing too large")]
    TickSpacingTooLarge,
    #[msg("Tick spacing too small")]
    TickSpacingTooSmall,
    #[msg("Hook address not valid for these permissions and fee")]
    HookAddressNotValid,
    #[msg("LP fee too large")]
    LPFeeTooLarge,
    #[msg("Pool not initialized")]
    PoolNotInitialized,
    #[msg("Invalid sqrt price")]
    InvalidSqrtPrice,
    #[msg("tickLower must be below tickUpper")]
    TicksMisordered,
    #[msg("tickLower out of bounds")]
    TickLowerOutOfBounds,
    #[msg("tickUpper out of bounds")]
    TickUpperOutOfBounds,
    #[msg("Tick not a multiple of the tick spacing")]
    TickMisaligned,
    #[msg("Tick liquidity overflow")]
    TickLiquidityOverflow,
    #[msg("Cannot poke a position with no liquidity")]
    CannotUpdateEmptyPosition,
    #[msg("Swap amount cannot be zero")]
    SwapAmountCannotBeZero,
    #[msg("Price limit already exceeded")]
    PriceLimitAlreadyExceeded,
    #[msg("Price limit out of bounds")]
    PriceLimitOutOfBounds,
    #[msg("A 100% fee can't be used for an exact-output swap")]
    InvalidFeeForExactOut,
    #[msg("No in-range liquidity to receive the donation")]
    NoLiquidityToReceiveFees,
    #[msg("Hook delta exceeds the swap amount")]
    HookDeltaExceedsSwapAmount,
    #[msg("Hook returned an unreadable response")]
    InvalidHookResponse,
    #[msg("Only a dynamic-fee pool's hook can update its LP fee")]
    UnauthorizedDynamicLPFeeUpdate,
    #[msg("Protocol fee too large")]
    ProtocolFeeTooLarge,
    #[msg("Arithmetic overflow")]
    MathOverflow,
    #[msg("Amount does not fit a token amount")]
    AmountOverflow,
    #[msg("Received less than the minimum")]
    TooLittleReceived,
    #[msg("Would pay more than the maximum")]
    TooMuchRequested,
    #[msg("Not enough claims")]
    InsufficientClaims,
    #[msg("A claims account for this pool and owner is required")]
    ClaimsAccountRequired,
    #[msg("The hook's claims account is required to settle its delta")]
    HookClaimsRequired,
    #[msg("A token account is required")]
    TokenAccountRequired,
    #[msg("Account does not belong to this pool")]
    WrongPoolAccount,
    #[msg("Hook program doesn't match the pool")]
    WrongHookProgram,
    #[msg("Hook program isn't executable")]
    HookNotExecutable,
    #[msg("Mint uses a Token-2022 extension the pool can't hold safely")]
    UnsupportedMintExtension,
    #[msg("Mint is owned by a different token program")]
    WrongTokenProgram,
    #[msg("Tick store is full; reserve more capacity first")]
    TickStoreFull,
    #[msg("Position account not found among the remaining accounts")]
    PositionNotFound,
    #[msg("Position belongs to someone else")]
    NotPositionOwner,
    #[msg("A vault's balance went down during a hook call")]
    VaultBalanceDecreased,
    #[msg("Only the program's upgrade authority can do this")]
    NotUpgradeAuthority,
    #[msg("Unauthorized")]
    Unauthorized,
    #[msg("Hook actions aren't available in this callback")]
    HookActionsNotAllowed,
    #[msg("Math error")]
    MathError,
    #[msg("Not enough liquidity")]
    NotEnoughLiquidity,
    #[msg("Liquidity underflow")]
    LiquidityUnderflow,
    #[msg("Liquidity overflow")]
    LiquidityOverflow,
}

impl From<meridian_core::math::MathError> for PmError {
    fn from(e: meridian_core::math::MathError) -> Self {
        use meridian_core::math::MathError as M;
        match e {
            M::Overflow | M::PriceOverflow => PmError::MathOverflow,
            M::InvalidTick | M::InvalidSqrtPrice | M::InvalidPriceOrLiquidity => PmError::InvalidSqrtPrice,
            M::NotEnoughLiquidity => PmError::NotEnoughLiquidity,
            M::LiquidityOverflow => PmError::LiquidityOverflow,
            M::LiquidityUnderflow => PmError::LiquidityUnderflow,
        }
    }
}

pub fn m<T>(r: core::result::Result<T, meridian_core::math::MathError>) -> Result<T> {
    r.map_err(|e| error!(PmError::from(e)))
}
