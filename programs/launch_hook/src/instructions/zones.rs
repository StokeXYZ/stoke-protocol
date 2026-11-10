//! Fee zones: an option a creator picks at launch. Inside a zone (a band of the coin's market
//! cap, about $100k–$350k), sells pay the zone's fee instead of the coin's, up to 10%; buys never
//! do. A sell pays the highest fee among the zones its price passes through, so pushing the price
//! out of a zone first doesn't dodge it. Once the coin holds above its top zone for
//! ZONES_OFF_AFTER_SECS, the zones switch off for good: if it dips back, sells pay the coin's own
//! fee, so nobody who bought higher is trapped. The fee splits like any other (creator, holders,
//! the coin's reserves).
//!
//! Zones are set by the creator in the launch transaction, before any trade, and never change.
//! A zoned coin is marked in its launch account, and its pool's published hook accounts gain the
//! zones account; every swap of it must pass that account (writable) or fails.

use anchor_lang::prelude::*;
use meridian_core::math::{mul_div, U256};
use meridian_core::HOOK_ACCOUNTS_SEED;

use super::coin_hooks::publish_hook_accounts;
use super::launch::TOTAL_SUPPLY;
use crate::errors::LaunchError;
use crate::events::{FeeZonesOff, FeeZonesSet};
use crate::state::*;

#[derive(Accounts)]
pub struct SetFeeZones<'info> {
    #[account(mut)]
    pub creator: Signer<'info>,
    #[account(mut, has_one = creator @ LaunchError::Unauthorized)]
    pub launch: Box<Account<'info, Launch>>,
    #[account(init, payer = creator, space = 8 + FeeZones::INIT_SPACE, seeds = [ZONES_SEED, launch.key().as_ref()], bump)]
    pub zones: Box<Account<'info, FeeZones>>,
    /// CHECK: the pool's published hook account list (this program's PDA), rewritten to add the zones.
    #[account(mut, seeds = [HOOK_ACCOUNTS_SEED, launch.pool.as_ref()], bump, owner = crate::ID)]
    pub hook_accounts: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// 1–5 zones, lowest first, not overlapping, within [ZONE_MIN_MCAP, ZONE_MAX_MCAP], each charging
/// more than the coin's own fee and at most MAX_ZONE_SELL_BPS.
pub fn validate_zones(zones: &[FeeZone], fee_bps: u16) -> Result<()> {
    require!(!zones.is_empty() && zones.len() <= MAX_FEE_ZONES, LaunchError::BadFeeZones);
    let mut floor = ZONE_MIN_MCAP;
    for z in zones {
        require!(z.from_mcap >= floor && z.to_mcap > z.from_mcap && z.to_mcap <= ZONE_MAX_MCAP, LaunchError::BadFeeZones);
        require!(z.sell_fee_bps > fee_bps && z.sell_fee_bps <= MAX_ZONE_SELL_BPS, LaunchError::BadFeeZones);
        floor = z.to_mcap;
    }
    Ok(())
}

pub fn handle_set_fee_zones(ctx: Context<SetFeeZones>, zones: Vec<FeeZone>) -> Result<()> {
    let clock = Clock::get()?;
    let a = &ctx.accounts;
    let l = &a.launch;
    // Only in the launch slot and before the first trade (the dev buy included): nobody ever buys
    // a coin whose zones could still change.
    require!(clock.slot == l.created_slot && l.trades == 0 && l.status == Status::Curve, LaunchError::ZonesOnlyAtLaunch);
    // A coin with both sets its zones first: `attach_hook` publishes the full account list.
    require!(!l.has_coin_hook(), LaunchError::ZonesBeforeHook);
    validate_zones(&zones, l.fee_bps)?;

    // Publish the zones account with the pool's other hook accounts, so every client passes it.
    publish_hook_accounts(&a.hook_accounts, &a.creator, &a.system_program, &[(l.key(), true), (l.position, true), (a.zones.key(), true)])?;

    let (launch_key, mint) = (l.key(), l.mint);
    let z = &mut ctx.accounts.zones;
    z.launch = launch_key;
    z.zones = zones.clone();
    z.bump = ctx.bumps.zones;
    ctx.accounts.launch.protocol_share_bps = FEE_ZONES_MARK;
    emit!(FeeZonesSet { mint, zones });
    Ok(())
}

// ------------------------------------------------------------------------------ swap time

/// The coin's market cap in SOL (lamports) on the curve: its price (virtual SOL per virtual token)
/// times the fixed supply.
pub fn curve_mcap(virtual_quote: u64, virtual_token: u64) -> u64 {
    let m = virtual_quote as u128 * TOTAL_SUPPLY as u128 / virtual_token.max(1) as u128;
    m.min(u64::MAX as u128) as u64
}

/// The coin's market cap in SOL (lamports) in the pool, from its Q64.64 sqrt price.
pub fn pool_mcap(sqrt_price: u128, token_is_0: bool) -> u64 {
    let (q64, sp, s) = (U256::from(1u128 << 64), U256::from(sqrt_price), U256::from(TOTAL_SUPPLY as u128));
    let m = if token_is_0 {
        // SOL per token = sp² / 2^128.
        mul_div(sp, sp, q64).and_then(|x| mul_div(x, s, q64))
    } else {
        // SOL per token = 2^128 / sp².
        mul_div(s, q64, sp).and_then(|x| mul_div(x, q64, sp))
    };
    m.and_then(|x| x.to_u128()).map_or(u64::MAX, |x| x.min(u64::MAX as u128) as u64)
}

/// A zoned coin's zones account, found among the hook's extra accounts: owned by this program,
/// a FeeZones for this launch, and writable. Only `set_fee_zones` creates one, once per launch.
pub struct ZonesRef<'a, 'info> {
    info: &'a AccountInfo<'info>,
    pub zones: FeeZones,
}

impl<'a, 'info> ZonesRef<'a, 'info> {
    /// `None` for a coin without zones; an error if the coin has zones and they weren't passed.
    pub fn load(l: &Launch, launch_key: &Pubkey, remaining: &'a [AccountInfo<'info>]) -> Result<Option<Self>> {
        if !l.has_fee_zones() {
            return Ok(None);
        }
        for info in remaining {
            if info.owner != &crate::ID || !info.is_writable {
                continue;
            }
            let Ok(zones) = FeeZones::try_deserialize(&mut &info.try_borrow_data()?[..]) else { continue };
            if zones.launch == *launch_key {
                return Ok(Some(ZonesRef { info, zones }));
            }
        }
        err!(LaunchError::FeeZonesMissing)
    }

    pub fn save(&self) -> Result<()> {
        self.zones.try_serialize(&mut &mut self.info.try_borrow_mut_data()?[..])
    }

    /// Before a swap: record its starting market cap, track the hold above the top zone (switching
    /// the zones off when it's long enough), and refuse exact-output sells while the zones are on
    /// (their end price depends on the fee). Saves the account.
    pub fn begin_swap(&mut self, mint: Pubkey, mcap: u64, now: i64, is_buy: bool, exact_in: bool) -> Result<()> {
        if self.zones.track(mcap, now) {
            emit!(FeeZonesOff { mint, mcap });
        }
        require!(self.zones.off || is_buy || exact_in, LaunchError::ExactOutSellInZones);
        self.zones.swap_start_mcap = mcap;
        self.save()
    }
}
