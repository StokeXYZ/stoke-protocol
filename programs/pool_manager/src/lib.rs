//! # Meridian PoolManager
//!
//! A concentrated-liquidity singleton with Uniswap v4's hook model, on Solana:
//! - all 14 hook permissions, encoded in the hook program's address;
//! - before/after callbacks for initialize, add/remove liquidity, swap and donate;
//! - return deltas (custom curves, swap fees taken by hooks, liquidity fees) and claims;
//! - dynamic LP fees, per-swap fee overrides, hookData, donate, protocol fees;
//! - noSelfCall: an operation the hook signs itself skips its callbacks.
//!
//! See `meridian_core::hook` for the interface and how it maps v4's semantics to Solana's
//! no-reentrancy rule.

use anchor_lang::prelude::*;

pub mod engine;
pub mod errors;
pub mod events;
pub mod hooks;
pub mod instructions;
pub mod settle;
pub mod state;
pub mod ticks;

use instructions::*;
use meridian_core::{ModifyLiquidityParams, Settle, SwapParams};
pub use state::*;

declare_id!("3EK8eA2473durhf2oKJzYk827GN56DHEZRMnSbuuBngm");

const _: () = assert!(meridian_core::POOL_MANAGER_ID.to_bytes()[0] == ID.to_bytes()[0]);

#[program]
pub mod meridian_pool_manager {
    use super::*;

    pub fn initialize_config(ctx: Context<InitializeConfig>, protocol_fee_controller: Pubkey) -> Result<()> {
        handle_initialize_config(ctx, protocol_fee_controller)
    }

    pub fn set_config(ctx: Context<SetConfig>, admin: Pubkey, protocol_fee_controller: Pubkey) -> Result<()> {
        handle_set_config(ctx, admin, protocol_fee_controller)
    }

    pub fn initialize<'info>(ctx: Context<'info, Initialize<'info>>, fee: u32, tick_spacing: i32, sqrt_price: u128) -> Result<()> {
        handle_initialize(ctx, fee, tick_spacing, sqrt_price)
    }

    pub fn modify_liquidity<'info>(
        ctx: Context<'info, ModifyLiquidity<'info>>,
        params: ModifyLiquidityParams,
        hook_data: Vec<u8>,
        settle: Settle,
    ) -> Result<()> {
        handle_modify_liquidity(ctx, params, hook_data, settle)
    }

    pub fn swap<'info>(
        ctx: Context<'info, Swap<'info>>,
        params: SwapParams,
        hook_data: Vec<u8>,
        settle: Settle,
        amount_limit: u64,
    ) -> Result<()> {
        handle_swap(ctx, params, hook_data, settle, amount_limit)
    }

    pub fn donate<'info>(ctx: Context<'info, Donate<'info>>, amount0: u64, amount1: u64, hook_data: Vec<u8>, settle: Settle) -> Result<()> {
        handle_donate(ctx, amount0, amount1, hook_data, settle)
    }

    pub fn open_position(ctx: Context<OpenPosition>, tick_lower: i32, tick_upper: i32, salt: [u8; 32]) -> Result<()> {
        handle_open_position(ctx, tick_lower, tick_upper, salt)
    }

    pub fn close_position(ctx: Context<ClosePosition>) -> Result<()> {
        handle_close_position(ctx)
    }

    pub fn open_claims(ctx: Context<OpenClaims>, owner: Pubkey) -> Result<()> {
        handle_open_claims(ctx, owner)
    }

    pub fn deposit(ctx: Context<Deposit>, amount0: u64, amount1: u64) -> Result<()> {
        handle_deposit(ctx, amount0, amount1)
    }

    pub fn withdraw(ctx: Context<Withdraw>, amount0: u64, amount1: u64) -> Result<()> {
        handle_withdraw(ctx, amount0, amount1)
    }

    pub fn transfer_claims(ctx: Context<TransferClaims>, amount0: u64, amount1: u64) -> Result<()> {
        handle_transfer_claims(ctx, amount0, amount1)
    }

    pub fn update_dynamic_lp_fee(ctx: Context<UpdateDynamicLpFee>, lp_fee: u32) -> Result<()> {
        handle_update_dynamic_lp_fee(ctx, lp_fee)
    }

    pub fn set_protocol_fee(ctx: Context<SetProtocolFee>, protocol_fee: u32) -> Result<()> {
        handle_set_protocol_fee(ctx, protocol_fee)
    }

    pub fn collect_protocol_fees(ctx: Context<CollectProtocolFees>, amount0: u64, amount1: u64) -> Result<()> {
        handle_collect_protocol_fees(ctx, amount0, amount1)
    }

    pub fn reserve_ticks(ctx: Context<ReserveTicks>, additional: u32) -> Result<()> {
        handle_reserve_ticks(ctx, additional)
    }
}
