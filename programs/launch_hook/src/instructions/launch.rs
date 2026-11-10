use anchor_lang::prelude::*;
use anchor_lang::solana_program::{program::invoke, program::invoke_signed, program_option::COption, system_instruction};
use anchor_lang::{InstructionData, ToAccountMetas};
use anchor_spl::token_2022::spl_token_2022;
use anchor_spl::token_interface::{Mint, TokenAccount, TokenInterface};
use meridian_core::fees::DYNAMIC_FEE_FLAG;
use meridian_core::math::{get_tick_at_sqrt_price, sqrt_price_from_ratio};
use meridian_core::*;
use meridian_pool_manager as pm;

use crate::curve;
use crate::errors::LaunchError;
use crate::events::Launched;
use crate::pm_cpi;
use crate::state::*;

pub const TOTAL_SUPPLY: u64 = 1_000_000_000 * 1_000_000;
pub const DECIMALS: u8 = 6;
pub const TICK_SPACING: i32 = 200;
pub const POSITION_SALT: [u8; 32] = [0; 32];
/// Salt of the buy-side "floor" position `deepen` fills.
pub const FLOOR_SALT: [u8; 32] = [1; 32];

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, Default)]
pub struct LaunchArgs {
    /// The coin's trading fee (bps of the SOL side of every trade), within the config's range.
    pub fee_bps: u16,
}

#[derive(Accounts)]
pub struct LaunchToken<'info> {
    #[account(mut)]
    pub creator: Signer<'info>,
    #[account(seeds = [crate::state::CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, LaunchConfig>>,
    /// Created earlier in the same transaction (or the one before) with the creator as mint
    /// authority. `launch` mints the whole supply and revokes the authority.
    #[account(mut, mint::token_program = token_program)]
    pub mint: Box<InterfaceAccount<'info, Mint>>,
    #[account(address = config.quote_mint, mint::token_program = quote_token_program)]
    pub quote_mint: Box<InterfaceAccount<'info, Mint>>,
    /// CHECK: receives the flat launch fee (address-checked).
    #[account(mut, address = config.fee_recipient)]
    pub fee_recipient: UncheckedAccount<'info>,
    #[account(init, payer = creator, space = 8 + Launch::INIT_SPACE, seeds = [LAUNCH_SEED, mint.key().as_ref()], bump)]
    pub launch: Box<Account<'info, Launch>>,
    /// CHECK: this program's signing PDA.
    #[account(seeds = [HOOK_AUTHORITY_SEED], bump)]
    pub hook_authority: UncheckedAccount<'info>,
    #[account(
        init,
        payer = creator,
        seeds = [RESERVE_SEED, mint.key().as_ref()],
        bump,
        token::mint = mint,
        token::authority = hook_authority,
        token::token_program = token_program,
    )]
    pub reserve: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: created by the PoolManager (it checks the address).
    #[account(mut)]
    pub pool: UncheckedAccount<'info>,
    /// CHECK: created by the PoolManager.
    #[account(mut)]
    pub ticks: UncheckedAccount<'info>,
    /// CHECK: created by the PoolManager.
    #[account(mut)]
    pub vault0: UncheckedAccount<'info>,
    /// CHECK: created by the PoolManager.
    #[account(mut)]
    pub vault1: UncheckedAccount<'info>,
    /// CHECK: created by the PoolManager (this hook's claims on the pool).
    #[account(mut)]
    pub hook_claims: UncheckedAccount<'info>,
    /// CHECK: created by the PoolManager (this hook's full-range position).
    #[account(mut)]
    pub position: UncheckedAccount<'info>,
    /// CHECK: created here: the accounts clients append to this pool's operations.
    #[account(mut, seeds = [HOOK_ACCOUNTS_SEED, pool.key().as_ref()], bump)]
    pub hook_accounts: UncheckedAccount<'info>,
    /// CHECK: PoolManager PDA.
    pub pm_authority: UncheckedAccount<'info>,
    /// CHECK: PoolManager PDA.
    pub hook_caller: UncheckedAccount<'info>,
    /// CHECK: address-checked.
    #[account(address = POOL_MANAGER_ID)]
    pub pool_manager: UncheckedAccount<'info>,
    /// CHECK: address-checked: the pool's hook is this program.
    #[account(address = crate::ID)]
    pub hook_program: UncheckedAccount<'info>,
    pub token_program: Interface<'info, TokenInterface>,
    pub quote_token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

pub fn handle_launch(ctx: Context<LaunchToken>, args: LaunchArgs) -> Result<()> {
    let a = &ctx.accounts;
    let cfg = &a.config;
    require!(!cfg.launches_paused, LaunchError::LaunchesPaused);
    require!((cfg.min_fee_bps..=cfg.max_fee_bps).contains(&args.fee_bps), LaunchError::FeeOutOfRange);
    let creator = a.creator.key();
    let m = &a.mint;
    require!(
        m.decimals == DECIMALS && m.supply == 0 && m.mint_authority == COption::Some(creator) && m.freeze_authority.is_none(),
        LaunchError::BadMint
    );
    let auth_bump = ctx.bumps.hook_authority;
    let tp = a.token_program.key();

    // The flat launch fee, to the protocol.
    if cfg.launch_fee > 0 {
        invoke(
            &system_instruction::transfer(&creator, &a.fee_recipient.key(), cfg.launch_fee),
            &[a.creator.to_account_info(), a.fee_recipient.to_account_info(), a.system_program.to_account_info()],
        )?;
    }

    // Mint the whole supply to the reserve, then revoke minting for good.
    invoke(
        &spl_token_2022::instruction::mint_to(&tp, &m.key(), &a.reserve.key(), &creator, &[], TOTAL_SUPPLY)?,
        &[a.mint.to_account_info(), a.reserve.to_account_info(), a.creator.to_account_info(), a.token_program.to_account_info()],
    )?;
    invoke(
        &spl_token_2022::instruction::set_authority(&tp, &m.key(), None, spl_token_2022::instruction::AuthorityType::MintTokens, &creator, &[])?,
        &[a.mint.to_account_info(), a.creator.to_account_info(), a.token_program.to_account_info()],
    )?;

    // Curve, and the pool price it will graduate at.
    let p = curve::params(TOTAL_SUPPLY, cfg.raise_target, cfg.curve_ratio_bps);
    let (vq_f, vt_f) = curve::final_reserves(&p);
    let token_is_0 = m.key() < a.quote_mint.key();
    let sqrt_price = if token_is_0 { sqrt_price_from_ratio(vq_f, vt_f) } else { sqrt_price_from_ratio(vt_f, vq_f) }.ok_or(LaunchError::MathError)?;
    // The buy-side floor range `deepen` fills: between the launch and graduation prices, entirely
    // on the SOL side of the graduation price.
    let start_sqrt = if token_is_0 {
        sqrt_price_from_ratio(p.virtual_quote as u128, p.virtual_token as u128)
    } else {
        sqrt_price_from_ratio(p.virtual_token as u128, p.virtual_quote as u128)
    }
    .ok_or(LaunchError::MathError)?;
    let tick_of = |s: u128| get_tick_at_sqrt_price(s).map_err(|_| error!(LaunchError::MathError));
    let (t_start, t_grad) = (tick_of(start_sqrt)?, tick_of(sqrt_price)?);
    let floor = |t: i32| t.div_euclid(TICK_SPACING) * TICK_SPACING;
    let (floor_lo, floor_hi) = if token_is_0 {
        (floor(t_start) + TICK_SPACING, floor(t_grad))
    } else {
        (floor(t_grad) + TICK_SPACING, floor(t_start))
    };
    require!(floor_lo < floor_hi, LaunchError::BadConfig);
    let (mint0, mint1) = if token_is_0 { (m.key(), a.quote_mint.key()) } else { (a.quote_mint.key(), m.key()) };
    let (tp0, tp1) = if token_is_0 { (tp, a.quote_token_program.key()) } else { (a.quote_token_program.key(), tp) };
    let (mi0, mi1) = if token_is_0 {
        (a.mint.to_account_info(), a.quote_mint.to_account_info())
    } else {
        (a.quote_mint.to_account_info(), a.mint.to_account_info())
    };
    let (ti0, ti1) = if token_is_0 {
        (a.token_program.to_account_info(), a.quote_token_program.to_account_info())
    } else {
        (a.quote_token_program.to_account_info(), a.token_program.to_account_info())
    };
    let infos = vec![
        a.creator.to_account_info(),
        a.hook_authority.to_account_info(),
        mi0,
        mi1,
        a.hook_program.to_account_info(),
        a.pool.to_account_info(),
        a.ticks.to_account_info(),
        a.vault0.to_account_info(),
        a.vault1.to_account_info(),
        a.pm_authority.to_account_info(),
        a.hook_caller.to_account_info(),
        ti0,
        ti1,
        a.system_program.to_account_info(),
        a.hook_claims.to_account_info(),
        a.position.to_account_info(),
        a.reserve.to_account_info(),
        a.pool_manager.to_account_info(),
    ];

    // 1. The pool: dynamic fee, initialized at the graduation price. The hook is the sender,
    //    so its own beforeInitialize (which refuses everyone else) is skipped.
    pm_cpi::call(
        pm::instruction::Initialize { fee: DYNAMIC_FEE_FLAG, tick_spacing: TICK_SPACING, sqrt_price }.data(),
        pm::accounts::Initialize {
            payer: creator,
            sender: a.hook_authority.key(),
            mint0,
            mint1,
            hooks: crate::ID,
            pool: a.pool.key(),
            ticks: a.ticks.key(),
            vault0: a.vault0.key(),
            vault1: a.vault1.key(),
            authority: a.pm_authority.key(),
            hook_caller: a.hook_caller.key(),
            token_program0: tp0,
            token_program1: tp1,
            system_program: a.system_program.key(),
        }
        .to_account_metas(None),
        &infos,
        auth_bump,
    )?;

    // 2. The whole supply becomes the hook's claims on the pool (the curve sells from them).
    let (amount0, amount1) = if token_is_0 { (TOTAL_SUPPLY, 0) } else { (0, TOTAL_SUPPLY) };
    let (tok0, tok1) = if token_is_0 { (Some(a.reserve.key()), None) } else { (None, Some(a.reserve.key())) };
    pm_cpi::call(
        pm::instruction::Deposit { amount0, amount1 }.data(),
        pm::accounts::Deposit {
            payer: creator,
            owner: a.hook_authority.key(),
            pool: a.pool.key(),
            claims: a.hook_claims.key(),
            vault0: a.vault0.key(),
            vault1: a.vault1.key(),
            token0: tok0,
            token1: tok1,
            mint0,
            mint1,
            token_program0: tp0,
            token_program1: tp1,
            system_program: a.system_program.key(),
        }
        .to_account_metas(None),
        &infos,
        auth_bump,
    )?;

    // 3. The full-range position graduation will fill (and nothing can ever remove).
    let (lo, hi) = full_range(TICK_SPACING);
    pm_cpi::call(
        pm::instruction::OpenPosition { tick_lower: lo, tick_upper: hi, salt: POSITION_SALT }.data(),
        pm::accounts::OpenPosition {
            payer: creator,
            owner: a.hook_authority.key(),
            pool: a.pool.key(),
            position: a.position.key(),
            system_program: a.system_program.key(),
        }
        .to_account_metas(None),
        &infos,
        auth_bump,
    )?;

    // 4. Publish the accounts every operation on this pool must pass to the hook.
    let pool_key = a.pool.key();
    let list = encode_hook_accounts(&[(a.launch.key(), true), (a.position.key(), true)]);
    let space = list.len();
    invoke_signed(
        &system_instruction::create_account(&creator, &a.hook_accounts.key(), Rent::get()?.minimum_balance(space), space as u64, &crate::ID),
        &[a.creator.to_account_info(), a.hook_accounts.to_account_info(), a.system_program.to_account_info()],
        &[&[HOOK_ACCOUNTS_SEED, pool_key.as_ref(), &[ctx.bumps.hook_accounts]]],
    )?;
    a.hook_accounts.try_borrow_mut_data()?.copy_from_slice(&list);

    let clock = Clock::get()?;
    let launch_key = a.launch.key();
    let (mint_key, position_key) = (m.key(), a.position.key());
    let cfg = (*a.config).clone();
    let l = &mut ctx.accounts.launch;
    l.mint = mint_key;
    l.creator = creator;
    l.pool = pool_key;
    l.position = position_key;
    l.token_is_0 = token_is_0;
    l.status = Status::Curve;
    l.created_at = clock.unix_timestamp;
    l.created_slot = clock.slot;
    l.snipe_fee_bps = cfg.snipe_fee_bps;
    l.snipe_slots = cfg.snipe_slots;
    l.fee_bps = args.fee_bps;
    l.launch_fee_paid = cfg.launch_fee;
    l.protocol_share_bps = cfg.protocol_share_bps;
    l.creator_share_bps = cfg.creator_share_bps;
    l.staker_share_bps = cfg.staker_share_bps;
    l.raise_target = cfg.raise_target;
    l.floor_tick_lower = floor_lo;
    l.floor_tick_upper = floor_hi;
    l.virtual_quote = p.virtual_quote;
    l.virtual_token = p.virtual_token;
    l.real_token = p.alloc;
    l.lp_tokens = p.lp_tokens;
    l.bump = ctx.bumps.launch;

    emit!(Launched {
        launch: launch_key,
        mint: mint_key,
        creator,
        pool: pool_key,
        raise_target: cfg.raise_target,
        snipe_fee_bps: cfg.snipe_fee_bps,
        snipe_slots: cfg.snipe_slots,
        launch_fee: cfg.launch_fee,
        fee_bps: args.fee_bps,
    });
    Ok(())
}
