//! Coin hooks (Stoke v3): a program a creator adds to their coin, called before every trade of it.
//! The interface, and what a hook can and can't do, is in `meridian_core::coin_hook`.
//!
//! - **Listed by anyone, once its code is locked.** `list_hook` makes a hook program available
//!   to creators. Anyone can call it, for a program nobody can change any more: it has to be
//!   immutable (or upgradeable by the config's admin only, as Stoke's own hooks are). So a hook
//!   does tomorrow what it did when a coin launched with it.
//! - **Paused by Stoke.** `set_hook_paused` (the config's admin) pauses or unpauses a listed
//!   hook. A paused hook can't be added to new coins, and coins that have it trade as if they
//!   had none: a hook that traps buyers can't keep them in a coin.
//! - **Added by the creator, once.** `attach_hook` runs in the launch transaction, before any trade
//!   (the dev buy included), with the creator's settings for it. The hook checks them
//!   (`on_attach`). Nobody ever buys a coin whose hook could still change: it's never replaced or
//!   removed.
//! - **Called on every trade.** beforeSwap calls the hook's `on_trade`. An error refuses the
//!   trade. A fee it returns (at most 10%) is the trade's fee instead of the coin's, splits like
//!   any other, applies to the creator too and leaves their fee-free allowance untouched; where a
//!   sell is also in a fee zone, it pays the higher of the two.
//!
//! A hooked coin is marked in its launch account, and its pool's published hook accounts gain the
//! coin hook account (writable), the hook's listing and the hook program; every swap of it must
//! pass them or fails.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::bpf_loader_upgradeable;
use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::solana_program::program::{get_return_data, invoke, invoke_signed};
use anchor_lang::solana_program::system_instruction;
use meridian_core::coin_hook::{discriminator, encode_coin_hook_call, AttachArgs, CoinInfo, TradeArgs, TradeReturn};
use meridian_core::{encode_hook_accounts, HOOK_ACCOUNTS_SEED, POOL_MANAGER_ID};

use super::launch::TOTAL_SUPPLY;
use crate::errors::LaunchError;
use crate::events::{HookAttached, HookListed, HookPaused};
use crate::state::*;

// ------------------------------------------------------------------------------ listing

#[derive(Accounts)]
pub struct ListHook<'info> {
    /// Whoever lists the hook: anyone. They pay for its listing account.
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, LaunchConfig>,
    /// CHECK: any deployed program but this one and the PoolManager.
    #[account(executable, constraint = hook_program.key() != crate::ID && hook_program.key() != POOL_MANAGER_ID @ LaunchError::HookNotListed)]
    pub hook_program: UncheckedAccount<'info>,
    /// The hook's code has to be locked: immutable, or upgradeable by the config's admin only.
    #[account(
        seeds = [hook_program.key().as_ref()],
        bump,
        seeds::program = bpf_loader_upgradeable::ID,
        constraint = program_data.upgrade_authority_address.is_none() || program_data.upgrade_authority_address == Some(config.admin) @ LaunchError::HookUpgradeable
    )]
    pub program_data: Box<Account<'info, ProgramData>>,
    #[account(init, payer = payer, space = 8 + HookListing::INIT_SPACE, seeds = [HOOK_LISTING_SEED, hook_program.key().as_ref()], bump)]
    pub listing: Account<'info, HookListing>,
    pub system_program: Program<'info, System>,
}

pub fn handle_list_hook(ctx: Context<ListHook>) -> Result<()> {
    let program = ctx.accounts.hook_program.key();
    let l = &mut ctx.accounts.listing;
    l.program = program;
    l.paused = false;
    l.listed_at = Clock::get()?.unix_timestamp;
    l.coins = 0;
    l.bump = ctx.bumps.listing;
    emit!(HookListed { program });
    Ok(())
}

#[derive(Accounts)]
pub struct SetHookPaused<'info> {
    pub admin: Signer<'info>,
    #[account(seeds = [CONFIG_SEED], bump = config.bump, has_one = admin @ LaunchError::Unauthorized)]
    pub config: Account<'info, LaunchConfig>,
    #[account(mut, seeds = [HOOK_LISTING_SEED, listing.program.as_ref()], bump = listing.bump)]
    pub listing: Account<'info, HookListing>,
}

pub fn handle_set_hook_paused(ctx: Context<SetHookPaused>, paused: bool) -> Result<()> {
    let l = &mut ctx.accounts.listing;
    l.paused = paused;
    emit!(HookPaused { program: l.program, paused });
    Ok(())
}

// ------------------------------------------------------------------------------ attaching

#[derive(Accounts)]
pub struct AttachHook<'info> {
    #[account(mut)]
    pub creator: Signer<'info>,
    #[account(mut, has_one = creator @ LaunchError::Unauthorized)]
    pub launch: Box<Account<'info, Launch>>,
    #[account(mut, seeds = [HOOK_LISTING_SEED, hook_program.key().as_ref()], bump = listing.bump)]
    pub listing: Box<Account<'info, HookListing>>,
    /// CHECK: the listed program (the listing's address is derived from it).
    #[account(executable)]
    pub hook_program: UncheckedAccount<'info>,
    #[account(init, payer = creator, space = 8 + CoinHook::INIT_SPACE, seeds = [COIN_HOOK_SEED, launch.key().as_ref()], bump)]
    pub coin_hook: Box<Account<'info, CoinHook>>,
    /// CHECK: the pool's published hook account list (this program's PDA), rewritten to add the hook's accounts.
    #[account(mut, seeds = [HOOK_ACCOUNTS_SEED, launch.pool.as_ref()], bump, owner = crate::ID)]
    pub hook_accounts: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

pub fn handle_attach_hook(ctx: Context<AttachHook>, config: Vec<u8>) -> Result<()> {
    let clock = Clock::get()?;
    let a = &ctx.accounts;
    let l = &a.launch;
    // Only in the launch slot and before the first trade (the dev buy included).
    require!(clock.slot == l.created_slot && l.trades == 0 && l.status == Status::Curve, LaunchError::HookOnlyAtLaunch);
    require!(!a.listing.paused, LaunchError::HookNotListed);
    require!(config.len() <= MAX_COIN_HOOK_CONFIG, LaunchError::HookConfigTooLong);

    // The hook checks the creator's settings: one that refuses them fails the launch.
    let listing_info = a.listing.to_account_info();
    call_hook(
        &a.hook_program,
        &listing_info,
        a.listing.bump,
        encode_coin_hook_call(discriminator::ON_ATTACH, &AttachArgs { coin: coin_info(l), config: config.clone() }),
    )?;

    // Publish the hook's accounts with the pool's others, so every client passes them.
    let launch_key = l.key();
    let mut list = vec![(launch_key, true), (l.position, true)];
    if l.has_fee_zones() {
        list.push((Pubkey::find_program_address(&[ZONES_SEED, launch_key.as_ref()], &crate::ID).0, true));
    }
    list.extend([(a.coin_hook.key(), true), (a.listing.key(), false), (a.hook_program.key(), false)]);
    publish_hook_accounts(&a.hook_accounts, &a.creator, &a.system_program, &list)?;

    let (mint, program, listing, zoned) = (l.mint, a.hook_program.key(), a.listing.key(), l.has_fee_zones());
    let h = &mut ctx.accounts.coin_hook;
    h.launch = launch_key;
    h.program = program;
    h.listing = listing;
    h.config = config.clone();
    h.swap_fee_bps = NO_HOOK_FEE;
    h.swap_pool_quote = NO_POOL_CHECK;
    h.attached_at = clock.unix_timestamp;
    h.bump = ctx.bumps.coin_hook;
    ctx.accounts.launch.protocol_share_bps = if zoned { ZONES_AND_HOOK_MARK } else { COIN_HOOK_MARK };
    ctx.accounts.listing.coins = ctx.accounts.listing.coins.saturating_add(1);
    emit!(HookAttached { mint, program, config });
    Ok(())
}

/// Rewrite a pool's published hook account list (growing it, paid by `payer`).
pub fn publish_hook_accounts<'info>(
    hook_accounts: &AccountInfo<'info>,
    payer: &AccountInfo<'info>,
    system_program: &AccountInfo<'info>,
    accounts: &[(Pubkey, bool)],
) -> Result<()> {
    let list = encode_hook_accounts(accounts);
    let need = Rent::get()?.minimum_balance(list.len()).saturating_sub(hook_accounts.lamports());
    if need > 0 {
        invoke(
            &system_instruction::transfer(payer.key, hook_accounts.key, need),
            &[payer.clone(), hook_accounts.clone(), system_program.clone()],
        )?;
    }
    hook_accounts.resize(list.len())?;
    hook_accounts.try_borrow_mut_data()?.copy_from_slice(&list);
    Ok(())
}

// ------------------------------------------------------------------------------ swap time

fn coin_info(l: &Launch) -> CoinInfo {
    CoinInfo { mint: l.mint, creator: l.creator, fee_bps: l.fee_bps, created_at: l.created_at, created_slot: l.created_slot }
}

/// Call a hook, signed by its listing account (this program's PDA): the hook sees one account,
/// the listing, as a read-only signer. Nothing else is forwarded.
fn call_hook<'info>(program: &AccountInfo<'info>, listing: &AccountInfo<'info>, listing_bump: u8, data: Vec<u8>) -> Result<()> {
    let ix = Instruction { program_id: *program.key, accounts: vec![AccountMeta::new_readonly(*listing.key, true)], data };
    invoke_signed(&ix, &[listing.clone(), program.clone()], &[&[HOOK_LISTING_SEED, program.key.as_ref(), &[listing_bump]]])?;
    Ok(())
}

/// A sell's fee when a fee zone or the coin's hook sets one: the higher of the two. `None`: neither does.
pub fn sell_override(zone_bps: u16, hook_bps: Option<u16>) -> Option<u16> {
    match hook_bps {
        Some(h) => Some(h.max(zone_bps)),
        None if zone_bps > 0 => Some(zone_bps),
        None => None,
    }
}

/// A hooked coin's hook, found among the swap's extra accounts: the coin hook account (owned by
/// this program, for this launch, writable), the listing it names, and the hook program.
pub struct CoinHookRef<'a, 'info> {
    info: &'a AccountInfo<'info>,
    hook: CoinHook,
    listing_info: &'a AccountInfo<'info>,
    listing: HookListing,
    program: Option<&'a AccountInfo<'info>>,
}

/// The launch's coin hook account among `remaining`.
fn find_coin_hook<'a, 'info>(launch_key: &Pubkey, remaining: &'a [AccountInfo<'info>]) -> Result<(&'a AccountInfo<'info>, CoinHook)> {
    for info in remaining {
        if info.owner != &crate::ID || !info.is_writable {
            continue;
        }
        let Ok(hook) = CoinHook::try_deserialize(&mut &info.try_borrow_data()?[..]) else { continue };
        if hook.launch == *launch_key {
            return Ok((info, hook));
        }
    }
    err!(LaunchError::HookAccountsMissing)
}

impl<'a, 'info> CoinHookRef<'a, 'info> {
    /// `None` for a coin without a hook; an error if the coin has one and its accounts weren't passed.
    pub fn load(l: &Launch, launch_key: &Pubkey, remaining: &'a [AccountInfo<'info>]) -> Result<Option<Self>> {
        if !l.has_coin_hook() {
            return Ok(None);
        }
        let (info, hook) = find_coin_hook(launch_key, remaining)?;
        let listing_info = remaining.iter().find(|a| *a.key == hook.listing && a.owner == &crate::ID).ok_or(LaunchError::HookAccountsMissing)?;
        let listing = HookListing::try_deserialize(&mut &listing_info.try_borrow_data()?[..]).map_err(|_| error!(LaunchError::HookAccountsMissing))?;
        let program = remaining.iter().find(|a| *a.key == hook.program);
        Ok(Some(Self { info, hook, listing_info, listing, program }))
    }

    /// Before a swap: ask the hook. An error from it refuses the trade. Returns the fee it set for
    /// this trade, if any (at most MAX_COIN_HOOK_FEE_BPS), and records it for afterSwap. A paused
    /// hook isn't called.
    #[allow(clippy::too_many_arguments)]
    pub fn on_trade(&mut self, l: &Launch, trader: Pubkey, is_buy: bool, exact_in: bool, amount: u64, mcap: u64, clock: &Clock) -> Result<Option<u16>> {
        let fee = if self.listing.paused { None } else { self.ask(l, trader, is_buy, exact_in, amount, mcap, clock)? };
        self.hook.swap_fee_bps = fee.unwrap_or(NO_HOOK_FEE);
        self.hook.swap_pool_quote = NO_POOL_CHECK;
        self.save()?;
        Ok(fee)
    }

    /// The SOL the pool has to trade in this swap, for afterSwap to check (see `CoinHook::swap_pool_quote`).
    pub fn expect_pool_quote(&mut self, quote: u64) -> Result<()> {
        self.hook.swap_pool_quote = quote;
        self.save()
    }

    fn save(&self) -> Result<()> {
        self.hook.try_serialize(&mut &mut self.info.try_borrow_mut_data()?[..])
    }

    #[allow(clippy::too_many_arguments)]
    fn ask(&self, l: &Launch, trader: Pubkey, is_buy: bool, exact_in: bool, amount: u64, mcap: u64, clock: &Clock) -> Result<Option<u16>> {
        let program = self.program.ok_or(LaunchError::HookAccountsMissing)?;
        // The side the trader fixed is exact; the other is valued at the price before the trade
        // (market cap over the fixed supply).
        let supply = TOTAL_SUPPLY as u128;
        let cap = |x: u128| x.min(u64::MAX as u128) as u64;
        let (sol, tokens) = if is_buy == exact_in {
            (amount, cap(amount as u128 * supply / mcap.max(1) as u128))
        } else {
            (cap(amount as u128 * mcap as u128 / supply), amount)
        };
        let args = TradeArgs {
            coin: coin_info(l),
            config: self.hook.config.clone(),
            trader,
            is_buy,
            exact_in,
            sol,
            tokens,
            graduated: l.status == Status::Graduated,
            mcap,
            now: clock.unix_timestamp,
            slot: clock.slot,
        };
        call_hook(program, self.listing_info, self.listing.bump, encode_coin_hook_call(discriminator::ON_TRADE, &args))?;
        // Only what the hook itself returned counts (the runtime clears return data on each call).
        let ret = match get_return_data() {
            Some((from, bytes)) if from == *program.key => TradeReturn::deserialize(&mut bytes.as_slice()).map_err(|_| error!(LaunchError::BadHookReturn))?,
            _ => TradeReturn::default(),
        };
        Ok(ret.fee_bps.map(|f| f.min(MAX_COIN_HOOK_FEE_BPS)))
    }

    /// After a swap: what beforeSwap recorded for it. The fee the hook set, if any, and the SOL
    /// the pool has to have traded, if that's to be checked.
    pub fn recorded(l: &Launch, launch_key: &Pubkey, remaining: &[AccountInfo<'info>]) -> Result<(Option<u16>, Option<u64>)> {
        if !l.has_coin_hook() {
            return Ok((None, None));
        }
        let (_, hook) = find_coin_hook(launch_key, remaining)?;
        Ok(((hook.swap_fee_bps != NO_HOOK_FEE).then_some(hook.swap_fee_bps), (hook.swap_pool_quote != NO_POOL_CHECK).then_some(hook.swap_pool_quote)))
    }
}
