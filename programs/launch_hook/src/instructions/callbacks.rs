//! The hook itself. Permissions (in the program id): beforeInitialize, beforeAddLiquidity,
//! beforeSwap, afterSwap, beforeSwapReturnDelta, afterSwapReturnDelta.
//!
//! The trading fee is always taken in SOL, as a hook delta:
//! - on the curve, inside the curve fill (beforeSwap);
//! - after graduation, from the specified amount when SOL is specified (exact-in buys,
//!   exact-out sells: beforeSwap) and from the unspecified amount otherwise (afterSwap).
//! The pool's own LP fee stays 0, so the whole fee goes to the creator, stakers and liquidity.
//!
//! Fee zones (see `zones`): a sell of a zoned coin pays the highest zone fee its price passes
//! through, instead of the coin's fee. On the curve that's all known in beforeSwap; in the pool,
//! beforeSwap records where the swap starts and afterSwap reads where it ended.
//!
//! Coin hooks (see `coin_hooks`): a coin with a hook asks it before every trade, in beforeSwap.
//! The hook can refuse the trade or set its fee (at most 10%), which then replaces the coin's.

use anchor_lang::prelude::*;
use meridian_core::math::*;
use meridian_core::*;

use super::coin_hooks::{sell_override, CoinHookRef};
use super::launch::TICK_SPACING;
use crate::errors::LaunchError;
use crate::events::{CurveTrade, Graduated, PoolFee};
use crate::state::*;
use super::zones::{curve_mcap, pool_mcap, ZonesRef};

#[derive(Accounts)]
pub struct HookCallBare<'info> {
    pub caller: Signer<'info>,
    /// CHECK: the pool being initialized.
    pub pool: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct HookCall<'info> {
    /// The PoolManager's hook-caller PDA for this pool (checked with `verify_call`).
    pub caller: Signer<'info>,
    /// CHECK: checked with `verify_call`.
    pub pool: UncheckedAccount<'info>,
    #[account(mut, constraint = launch.pool == pool.key() @ LaunchError::WrongLaunch)]
    pub launch: Box<Account<'info, Launch>>,
}

/// Only `launch` creates pools with this hook (and its own initialize skips this callback).
pub fn handle_before_initialize(_ctx: Context<HookCallBare>) -> Result<()> {
    err!(LaunchError::PoolsOnlyThroughLaunch)
}

/// Nobody but the curve can add liquidity before graduation, so the pool's price can't be set
/// or front-run before the curve's reserves arrive.
pub fn handle_before_add_liquidity(ctx: Context<HookCall>) -> Result<HookEffects> {
    verify_call(&crate::ID, &ctx.accounts.caller, &ctx.accounts.pool)?;
    require!(ctx.accounts.launch.status == Status::Graduated, LaunchError::LiquidityLockedUntilGraduation);
    Ok(HookEffects::default())
}

fn to_amount(x: i128) -> Result<u64> {
    u64::try_from(x.unsigned_abs()).map_err(|_| error!(LaunchError::MathError))
}

fn fee_on_gross(amount: u64, bps: u16) -> u64 {
    (amount as u128 * bps as u128 / BPS as u128) as u64
}

/// Fee on top of a net amount, so it's `bps` of the gross.
fn fee_on_net(amount: u64, bps: u16) -> u64 {
    if bps == 0 {
        return 0;
    }
    (amount as u128 * bps as u128).div_ceil(BPS as u128 - bps as u128) as u64
}

/// While the curve runs, every swap is filled by the curve (a NoOp on the pool, via
/// beforeSwapReturnDelta). The buy that sells the last token also graduates: the hook queues
/// adding the curve's reserves as full-range liquidity, and the rest of that buy continues into
/// the pool at the same price. After graduation it only takes the fee.
pub fn handle_before_swap(ctx: Context<HookCall>, sender: Pubkey, params: SwapParams) -> Result<BeforeSwapReturn> {
    let st = verify_call(&crate::ID, &ctx.accounts.caller, &ctx.accounts.pool)?;
    let launch_key = ctx.accounts.launch.key();
    let l = &mut ctx.accounts.launch;
    let clock = Clock::get()?;
    let is_buy = params.zero_for_one != l.token_is_0;
    let exact_in = params.amount_specified < 0;
    let amount = to_amount(params.amount_specified)?;
    // Fee zones: where this swap starts. (Exact-output sells are refused while the zones are on,
    // so the graduated path below never needs a zone fee in beforeSwap.)
    let start_mcap = if l.status == Status::Graduated { pool_mcap(st.sqrt_price.get(), l.token_is_0) } else { curve_mcap(l.virtual_quote, l.virtual_token) };
    let mut zones = ZonesRef::load(l, &launch_key, ctx.remaining_accounts)?;
    if let Some(z) = zones.as_mut() {
        z.begin_swap(l.mint, start_mcap, clock.unix_timestamp, is_buy, exact_in)?;
    }
    // The coin's hook, if it has one: its error refuses the trade, and a fee it returns is this trade's.
    let mut coin_hook = CoinHookRef::load(l, &launch_key, ctx.remaining_accounts)?;
    let hook_bps = match coin_hook.as_mut() {
        Some(h) => h.on_trade(l, sender, is_buy, exact_in, amount, start_mcap, &clock)?,
        None => None,
    };
    if l.status == Status::Graduated {
        let (ret, pool_quote) = graduated_before_swap(l, sender, is_buy, exact_in, amount, hook_bps)?;
        // A hooked coin: afterSwap checks the pool traded all the SOL the fee was taken on.
        if let (Some(h), Some(q)) = (coin_hook.as_mut(), pool_quote) {
            h.expect_pool_quote(q)?;
        }
        return Ok(ret);
    }

    let dev_buy = l.is_dev_buy(&sender, is_buy, clock.slot);
    let snipe_bps = if dev_buy { 0 } else { l.snipe_bps(clock.slot) };
    let mut curve = l.curve();
    // A buy pays the coin's fee or its hook's, never so much that the anti-snipe tax leaves nothing.
    let fee_bps = hook_bps.unwrap_or(l.fee_bps).min(MAX_BUY_TAX_BPS.saturating_sub(snipe_bps));
    // An exact-input sell moves the curve by its tokens whatever the fee: price it fee-free to
    // find where it ends, then charge the highest zone fee between there and where it started.
    let zone_bps = match zones.as_ref() {
        Some(z) if !is_buy && exact_in && !z.zones.off => {
            let mut after = curve;
            after.apply_sell(&curve.sell_exact_in(amount, 0));
            z.zones.sell_bps(start_mcap, curve_mcap(after.virtual_quote, after.virtual_token))
        }
        _ => 0,
    };
    // A sell in a fee zone or with a hook fee pays that (the higher of the two) instead of the coin's.
    let special = if is_buy { None } else { sell_override(zone_bps, hook_bps) };
    let fill = match (is_buy, exact_in) {
        (true, true) => curve.buy_exact_in(amount, fee_bps, snipe_bps),
        (true, false) => curve.buy_exact_out(amount, fee_bps, snipe_bps),
        (false, true) => curve.sell_exact_in(amount, special.unwrap_or_else(|| l.fee_bps_for(&sender, false, amount))),
        (false, false) => match special {
            Some(bps) => curve.sell_exact_out(amount, bps).ok_or(LaunchError::CurveCannotPay)?,
            None => {
                // The token amount isn't known until it's filled: size it fee-free, then price it
                // at the rate for that size.
                let probe = curve.sell_exact_out(amount, 0).ok_or(LaunchError::CurveCannotPay)?;
                curve.sell_exact_out(amount, l.fee_bps_for(&sender, false, probe.tokens)).ok_or(LaunchError::CurveCannotPay)?
            }
        },
    };
    require!(fill.tokens > 0 && fill.quote > 0, LaunchError::AmountTooSmall);
    // Specified currency: the input for exact input, the output for exact output.
    let (mut specified, unspecified) = match (is_buy, exact_in) {
        (true, true) => (fill.quote as i128, -(fill.tokens as i128)),
        (true, false) => (-(fill.tokens as i128), fill.quote as i128),
        (false, true) => (fill.tokens as i128, -(fill.quote as i128)),
        (false, false) => (-(fill.quote as i128), fill.tokens as i128),
    };
    // A graduating exact-in buy continues into the pool with the rest of its SOL: the fee on
    // that part is taken here too, since SOL is the specified currency.
    let mut pool_fee = 0;
    if fill.completes && exact_in {
        pool_fee = fee_on_gross(amount - fill.quote, fee_bps);
        specified += pool_fee as i128;
        if let Some(h) = coin_hook.as_mut() {
            h.expect_pool_quote(amount - fill.quote - pool_fee)?;
        }
    }
    if is_buy {
        curve.apply_buy(&fill);
    } else {
        curve.apply_sell(&fill);
    }
    l.set_curve(&curve);
    l.split_fee(fill.fee + pool_fee)?;
    l.burn_pending += fill.burned;
    if dev_buy {
        l.dev_free_tokens += fill.tokens;
    }
    // A zone or hook fee applies to the creator too, and leaves their fee-free allowance untouched.
    if special.is_none() {
        l.use_dev_allowance(&sender, is_buy, fill.tokens);
    }
    l.volume_quote += if is_buy { fill.quote } else { fill.curve_quote };
    l.trades += 1;
    emit!(CurveTrade {
        mint: l.mint,
        trader: sender,
        is_buy,
        quote: fill.quote,
        tokens: fill.tokens,
        fee: fill.fee,
        snipe: fill.snipe,
        burned: fill.burned,
        virtual_quote: l.virtual_quote,
        virtual_token: l.virtual_token,
        real_quote: l.real_quote,
        real_token: l.real_token,
    });
    if pool_fee > 0 {
        emit!(PoolFee { mint: l.mint, trader: sender, is_buy, fee: pool_fee });
    }

    let mut effects = HookEffects::default();
    if fill.completes {
        effects = graduate(l, &st, clock.unix_timestamp)?;
    }
    Ok(BeforeSwapReturn { delta: BeforeSwapDelta { specified, unspecified }, lp_fee_override: 0, effects })
}

/// After graduation: take the fee from the specified amount when it's SOL. `hook_bps` is the fee
/// the coin's hook set for this trade, if any. Also returns the SOL the pool then has to trade
/// (the amount less the fee on a buy, plus it on a sell), when SOL is the specified side.
fn graduated_before_swap(l: &mut Launch, sender: Pubkey, is_buy: bool, exact_in: bool, amount: u64, hook_bps: Option<u16>) -> Result<(BeforeSwapReturn, Option<u64>)> {
    l.trades += 1;
    let sol_specified = is_buy == exact_in;
    if !sol_specified {
        return Ok((BeforeSwapReturn::default(), None));
    }
    let fee = if is_buy {
        fee_on_gross(amount, hook_bps.unwrap_or(l.fee_bps))
    } else if let Some(bps) = hook_bps {
        // An exact-output sell with a hook fee: everyone pays it, the creator included.
        fee_on_net(amount, bps)
    } else if sender == l.creator && l.dev_free_tokens > 0 {
        // A creator's exact-output sell: the token amount isn't known yet, so afterSwap charges
        // any part above the allowance (in tokens, which are burned).
        0
    } else {
        fee_on_net(amount, l.fee_bps)
    };
    if fee > 0 {
        l.split_fee(fee)?;
        emit!(PoolFee { mint: l.mint, trader: sender, is_buy, fee });
    }
    let pool_quote = if is_buy { amount.checked_sub(fee) } else { amount.checked_add(fee) }.ok_or(LaunchError::MathError)?;
    Ok((BeforeSwapReturn { delta: BeforeSwapDelta { specified: fee as i128, unspecified: 0 }, lp_fee_override: 0, effects: HookEffects::default() }, Some(pool_quote)))
}

/// After graduation: take the fee from the unspecified amount when it's SOL. `delta` is the
/// pool's own delta (the swapper's view), without the hook's beforeSwap delta.
pub fn handle_after_swap(ctx: Context<HookCall>, sender: Pubkey, params: SwapParams, delta: BalanceDelta) -> Result<AfterSwapReturn> {
    let st = verify_call(&crate::ID, &ctx.accounts.caller, &ctx.accounts.pool)?;
    let launch_key = ctx.accounts.launch.key();
    let l = &mut ctx.accounts.launch;
    if l.status != Status::Graduated {
        return Ok(AfterSwapReturn::default());
    }
    let is_buy = params.zero_for_one != l.token_is_0;
    let exact_in = params.amount_specified < 0;
    let (d_token, d_quote) = if l.token_is_0 { (delta.amount0, delta.amount1) } else { (delta.amount1, delta.amount0) };
    let (tokens, quote) = (to_amount(d_token)?, to_amount(d_quote)?);
    // What beforeSwap recorded for a hooked coin: the fee its hook set, and the SOL the pool had
    // to trade. The fee was taken up front on all of it, so a swap cut short is refused.
    let (hook_bps, pool_quote) = CoinHookRef::recorded(l, &launch_key, ctx.remaining_accounts)?;
    if let Some(want) = pool_quote {
        require!(quote == want, LaunchError::SwapCutShort);
    }
    if tokens == 0 && quote == 0 {
        return Ok(AfterSwapReturn::default());
    }
    l.volume_quote += quote;
    let clock = Clock::get()?;
    if l.is_dev_buy(&sender, is_buy, clock.slot) {
        // A dev buy big enough to graduate the curve in the launch transaction.
        l.dev_free_tokens += tokens;
    }

    // Fee zones: an exact-input sell pays the highest zone fee between where beforeSwap saw it start
    // and where the pool's price is now (the pool state is written before this call).
    let zone_bps = match ZonesRef::load(l, &launch_key, ctx.remaining_accounts)? {
        Some(z) if !is_buy && exact_in => z.zones.sell_bps(z.zones.swap_start_mcap, pool_mcap(st.sqrt_price.get(), l.token_is_0)),
        _ => 0,
    };

    let mut unspecified: i128 = 0;
    if is_buy != exact_in {
        // SOL is unspecified: exact-output buys pay the fee on top, exact-input sells receive less.
        let special = if is_buy { hook_bps } else { sell_override(zone_bps, hook_bps) };
        let bps = special.unwrap_or_else(|| l.fee_bps_for(&sender, is_buy, tokens));
        let fee = if is_buy { fee_on_net(quote, bps) } else { fee_on_gross(quote, bps) };
        if special.is_none() {
            l.use_dev_allowance(&sender, is_buy, tokens);
        }
        if fee > 0 {
            l.split_fee(fee)?;
            emit!(PoolFee { mint: l.mint, trader: sender, is_buy, fee });
        }
        unspecified = fee as i128;
    } else if !is_buy && hook_bps.is_none() && sender == l.creator && l.dev_free_tokens > 0 {
        // The creator's exact-output sell skipped the SOL fee in beforeSwap. Charge the part above
        // the allowance in tokens (the unspecified currency) and burn them.
        let excess = tokens.saturating_sub(l.dev_free_tokens);
        let token_fee = fee_on_gross(excess, l.fee_bps);
        l.use_dev_allowance(&sender, false, tokens);
        l.burn_pending += token_fee;
        unspecified = token_fee as i128;
    }
    Ok(AfterSwapReturn { unspecified_delta: unspecified, effects: HookEffects::default() })
}

/// Fill the locked full-range position with the curve's reserves. The pool's LP fee stays 0.
fn graduate(l: &mut Launch, st: &PoolState, now: i64) -> Result<HookEffects> {
    let (lo, hi) = full_range(TICK_SPACING);
    let m = |r: core::result::Result<u128, MathError>| r.map_err(|_| error!(LaunchError::MathError));
    let (sa, sb, sp) = (m(get_sqrt_price_at_tick(lo))?, m(get_sqrt_price_at_tick(hi))?, st.sqrt_price.get());
    let (quote, tokens) = (l.real_quote as u128, l.lp_tokens as u128);
    let (a0, a1) = if l.token_is_0 { (tokens, quote) } else { (quote, tokens) };
    // One unit of margin per side, so the rounded-up amounts always fit what the hook holds.
    let liquidity = m(get_liquidity_for_amounts(sp, sa, sb, a0.saturating_sub(1), a1.saturating_sub(1)))?;
    let used0 = m(get_amount0_delta(sp, sb, liquidity, true))? as u64;
    let used1 = m(get_amount1_delta(sa, sp, liquidity, true))? as u64;
    let (used_token, used_quote) = if l.token_is_0 { (used0, used1) } else { (used1, used0) };
    require!(used_token <= l.lp_tokens && used_quote <= l.real_quote, LaunchError::MathError);
    // Rounding dust and the reserves fees put into the curve: tokens are burned, SOL waits for
    // `deepen`. The reserves were counted in `liquidity_added` on the way in; they're pending again
    // until `deepen` adds them, so they're never counted twice.
    l.burn_pending += l.lp_tokens - used_token;
    let leftover = l.real_quote - used_quote;
    l.liquidity_added -= leftover.min(l.liquidity_added);
    l.liquidity_pending += leftover;
    emit!(Graduated { mint: l.mint, pool: l.pool, liquidity, quote: used_quote, tokens: used_token });
    l.real_quote = 0;
    l.lp_tokens = 0;
    l.liquidity = liquidity;
    l.status = Status::Graduated;
    l.graduated_at = now;
    Ok(HookEffects { lp_fee: None, actions: vec![HookAction::ModifyLiquidity { position: l.position, liquidity_delta: liquidity as i128 }] })
}
