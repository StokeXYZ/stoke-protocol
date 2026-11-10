//! # Meridian Launch
//!
//! The non-extraction launchpad as a single hook on the Meridian PoolManager. Each launch is one
//! pool (token / SOL, dynamic fee kept at 0) with this hook:
//! - **Curve phase.** beforeSwap + beforeSwapReturnDelta fill every trade from one fixed bonding
//!   curve held as the hook's claims on the pool; the pool itself never trades.
//! - **Graduation, inside the swap.** The buy that sells the last curve token queues a
//!   ModifyLiquidity action: the curve's SOL and the reserved tokens become locked full-range
//!   liquidity at exactly the curve's last price, and the rest of that buy fills from the pool.
//! - **One fee, all kept in the coin.** The creator picks the coin's fee (up to 2.5%). Every trade
//!   pays it in SOL (on the curve, then through beforeSwap/afterSwap return deltas): 30% to the
//!   creator, 30% to holders and 40% into the coin's own reserves: the curve's real SOL while it
//!   runs, then the locked pool (`deepen`). The protocol takes no trading fee, only the launch fee.
//! - **Holders earn.** The holders' share goes to the keeper (`withdraw_holder_rewards`), which
//!   pays it out each hour by average balance: 1× just for holding, 1.5× / 2× for 7- / 30-day locks.
//! - **Dev buy.** Optional; the creator's sells are fee-free up to what they bought at launch.
//! - **Anti-snipe.** Always on: buys in the first few slots (about a second) pay a steep tax
//!   that buys tokens which are burned. The creator's dev buy, in the launch transaction, is exempt.
//! - **Fee zones.** Optional, set by the creator in the launch transaction: market cap bands
//!   (about $100k–$350k) where sells pay a higher fee, up to 10%. A sell pays the highest zone fee
//!   its price passes through; the zones switch off for good once the coin holds above the top one.
//! - **Coin hooks (Stoke v3).** Optional, added by the creator in the launch transaction: any
//!   program whose code is locked (immutable), theirs or someone else's, called before every trade
//!   of the coin. It can refuse the trade or set its fee (at most 10%), nothing else; Stoke can
//!   pause a hook, and coins that have it then trade without it. See `meridian_core::coin_hook`.
//! - **Locked liquidity.** Every position belongs to the hook, which has no instruction to remove
//!   one. beforeInitialize refuses pools not created by `launch`; beforeAddLiquidity keeps
//!   everyone else's liquidity out until graduation.

use anchor_lang::prelude::*;
use meridian_core::{BalanceDelta, ModifyLiquidityParams, PoolKey, SwapParams};

pub mod curve;
pub mod errors;
pub mod events;
pub mod instructions;
pub mod pm_cpi;
pub mod state;

use instructions::*;
pub use state::*;

declare_id!("CAcC7pz43rLg14jAE8iR3w3sgXcd2GGtoeFWUBB1SGVq");

/// beforeInitialize | beforeAddLiquidity | beforeSwap | afterSwap | beforeSwapReturnDelta |
/// afterSwapReturnDelta
pub const FLAGS: u16 = 0x28cc;
const _: () = assert!(((ID.to_bytes()[30] as u16) << 8 | ID.to_bytes()[31] as u16) & 0x3fff == FLAGS);

#[program]
pub mod meridian_launch {
    use super::*;

    pub fn initialize_config(ctx: Context<InitializeConfig>, args: ConfigArgs) -> Result<()> {
        handle_initialize_config(ctx, args)
    }

    pub fn update_config(ctx: Context<UpdateConfig>, admin: Pubkey, args: ConfigArgs) -> Result<()> {
        handle_update_config(ctx, admin, args)
    }

    pub fn launch(ctx: Context<LaunchToken>, args: LaunchArgs) -> Result<()> {
        handle_launch(ctx, args)
    }

    pub fn claim_creator_fees(ctx: Context<HookTreasury>) -> Result<()> {
        handle_claim_creator_fees(ctx)
    }

    pub fn withdraw_protocol_fees(ctx: Context<ProtocolTreasury>) -> Result<()> {
        handle_withdraw_protocol_fees(ctx)
    }

    pub fn deepen(ctx: Context<Deepen>) -> Result<()> {
        handle_deepen(ctx)
    }

    pub fn stake(ctx: Context<StakeTokens>, tier: u8, amount: u64) -> Result<()> {
        handle_stake(ctx, tier, amount)
    }

    pub fn unstake(ctx: Context<UnstakeTokens>, amount: u64) -> Result<()> {
        handle_unstake(ctx, amount)
    }

    pub fn claim_rewards(ctx: Context<ClaimRewards>) -> Result<()> {
        handle_claim_rewards(ctx)
    }

    pub fn withdraw_holder_rewards(ctx: Context<HookTreasury>) -> Result<()> {
        handle_withdraw_holder_rewards(ctx)
    }

    pub fn burn_pending(ctx: Context<HookTreasury>) -> Result<()> {
        handle_burn_pending(ctx)
    }

    pub fn set_fee_zones(ctx: Context<SetFeeZones>, zones: Vec<FeeZone>) -> Result<()> {
        handle_set_fee_zones(ctx, zones)
    }

    pub fn list_hook(ctx: Context<ListHook>) -> Result<()> {
        handle_list_hook(ctx)
    }

    pub fn set_hook_paused(ctx: Context<SetHookPaused>, paused: bool) -> Result<()> {
        handle_set_hook_paused(ctx, paused)
    }

    pub fn attach_hook(ctx: Context<AttachHook>, config: Vec<u8>) -> Result<()> {
        handle_attach_hook(ctx, config)
    }

    // ---------------------------------------------------- hook callbacks (PoolManager only)

    pub fn before_initialize(ctx: Context<HookCallBare>, _sender: Pubkey, _key: PoolKey, _sqrt_price: u128) -> Result<()> {
        handle_before_initialize(ctx)
    }

    pub fn before_add_liquidity(
        ctx: Context<HookCall>,
        _sender: Pubkey,
        _key: PoolKey,
        _params: ModifyLiquidityParams,
        _hook_data: Vec<u8>,
    ) -> Result<meridian_core::HookEffects> {
        handle_before_add_liquidity(ctx)
    }

    pub fn before_swap(
        ctx: Context<HookCall>,
        sender: Pubkey,
        _key: PoolKey,
        params: SwapParams,
        _hook_data: Vec<u8>,
    ) -> Result<meridian_core::BeforeSwapReturn> {
        handle_before_swap(ctx, sender, params)
    }

    pub fn after_swap(
        ctx: Context<HookCall>,
        sender: Pubkey,
        _key: PoolKey,
        params: SwapParams,
        delta: BalanceDelta,
        _hook_data: Vec<u8>,
    ) -> Result<meridian_core::AfterSwapReturn> {
        handle_after_swap(ctx, sender, params, delta)
    }
}
