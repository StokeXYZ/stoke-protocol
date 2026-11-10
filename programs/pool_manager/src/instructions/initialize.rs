use anchor_lang::prelude::*;
use anchor_spl::token_2022::spl_token_2022;
use anchor_spl::token_interface::{Mint, TokenAccount, TokenInterface};
use meridian_core::fees::*;
use meridian_core::flags::*;
use meridian_core::hook::discriminator as disc;
use meridian_core::math::get_tick_at_sqrt_price;
use meridian_core::*;

use super::common::write_pool;
use crate::errors::{m, PmError};
use crate::events;
use crate::hooks::HookCtx;
use crate::state::Pool;
use crate::ticks;

#[derive(Accounts)]
#[instruction(fee: u32, tick_spacing: i32)]
pub struct Initialize<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    /// The `sender` hooks see.
    pub sender: Signer<'info>,
    #[account(mint::token_program = token_program0)]
    pub mint0: Box<InterfaceAccount<'info, Mint>>,
    #[account(mint::token_program = token_program1)]
    pub mint1: Box<InterfaceAccount<'info, Mint>>,
    /// CHECK: the hook program (the system program for none); validated in the handler.
    pub hooks: UncheckedAccount<'info>,
    #[account(
        init,
        payer = payer,
        space = 8 + core::mem::size_of::<Pool>(),
        seeds = [POOL_SEED, mint0.key().as_ref(), mint1.key().as_ref(), &fee.to_le_bytes(), &tick_spacing.to_le_bytes(), hooks.key().as_ref()],
        bump,
    )]
    pub pool: AccountLoader<'info, Pool>,
    /// CHECK: created here.
    #[account(mut, seeds = [TICKS_SEED, pool.key().as_ref()], bump)]
    pub ticks: UncheckedAccount<'info>,
    #[account(
        init,
        payer = payer,
        seeds = [VAULT_SEED, pool.key().as_ref(), mint0.key().as_ref()],
        bump,
        token::mint = mint0,
        token::authority = authority,
        token::token_program = token_program0,
    )]
    pub vault0: Box<InterfaceAccount<'info, TokenAccount>>,
    #[account(
        init,
        payer = payer,
        seeds = [VAULT_SEED, pool.key().as_ref(), mint1.key().as_ref()],
        bump,
        token::mint = mint1,
        token::authority = authority,
        token::token_program = token_program1,
    )]
    pub vault1: Box<InterfaceAccount<'info, TokenAccount>>,
    /// CHECK: PDA that owns every vault.
    #[account(seeds = [AUTHORITY_SEED], bump)]
    pub authority: UncheckedAccount<'info>,
    /// CHECK: PDA that signs this pool's hook calls.
    #[account(seeds = [HOOK_CALLER_SEED, pool.key().as_ref()], bump)]
    pub hook_caller: UncheckedAccount<'info>,
    pub token_program0: Interface<'info, TokenInterface>,
    pub token_program1: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

/// Token-2022 extensions that don't let anyone move, freeze, tax or block the vault's tokens.
fn check_mint_extensions(mint: &AccountInfo, token_program: &Pubkey) -> Result<()> {
    if *token_program != spl_token_2022::ID {
        return Ok(());
    }
    use spl_token_2022::extension::{BaseStateWithExtensions, ExtensionType as E, StateWithExtensions};
    let data = mint.try_borrow_data()?;
    let state = StateWithExtensions::<spl_token_2022::state::Mint>::unpack(&data)?;
    for ext in state.get_extension_types()? {
        match ext {
            E::MetadataPointer
            | E::TokenMetadata
            | E::GroupPointer
            | E::TokenGroup
            | E::GroupMemberPointer
            | E::TokenGroupMember
            | E::MintCloseAuthority
            | E::InterestBearingConfig
            | E::ScaledUiAmount => {}
            _ => return err!(PmError::UnsupportedMintExtension),
        }
    }
    Ok(())
}

/// v4's PoolManager.initialize.
pub fn handle_initialize<'info>(
    ctx: Context<'info, Initialize<'info>>,
    fee: u32,
    tick_spacing: i32,
    sqrt_price: u128,
) -> Result<()> {
    let a = &ctx.accounts;
    require!(tick_spacing <= MAX_TICK_SPACING, PmError::TickSpacingTooLarge);
    require!(tick_spacing >= MIN_TICK_SPACING, PmError::TickSpacingTooSmall);
    require!(a.mint0.key() < a.mint1.key(), PmError::CurrenciesOutOfOrderOrEqual);
    let hooks = a.hooks.key();
    require!(is_valid_hook_address(&hooks, fee), PmError::HookAddressNotValid);
    if hooks != Pubkey::default() {
        require!(a.hooks.executable, PmError::HookNotExecutable);
    }
    let lp_fee = if is_dynamic_fee(fee) {
        0
    } else {
        require!(is_valid_lp_fee(fee), PmError::LPFeeTooLarge);
        fee
    };
    check_mint_extensions(&a.mint0.to_account_info(), &a.token_program0.key())?;
    check_mint_extensions(&a.mint1.to_account_info(), &a.token_program1.key())?;
    let tick = m(get_tick_at_sqrt_price(sqrt_price))?;

    let pool_key = a.pool.key();
    ticks::create(
        &a.ticks.to_account_info(),
        &a.payer.to_account_info(),
        &a.system_program.to_account_info(),
        &pool_key,
        &[TICKS_SEED, pool_key.as_ref(), &[ctx.bumps.ticks]],
    )?;

    let (hook_auth, _) = hook_authority(&hooks);
    let mut st: PoolState = bytemuck::Zeroable::zeroed();
    st.currency0 = a.mint0.key();
    st.currency1 = a.mint1.key();
    st.hooks = hooks;
    st.hook_authority = hook_auth;
    st.vault0 = a.vault0.key();
    st.vault1 = a.vault1.key();
    st.ticks = a.ticks.key();
    st.token_program0 = a.token_program0.key();
    st.token_program1 = a.token_program1.key();
    st.fee = fee;
    st.lp_fee = lp_fee;
    st.tick_spacing = tick_spacing;
    st.hook_flags = hook_flags(&hooks);
    st.bump = ctx.bumps.pool;
    st.authority_bump = ctx.bumps.authority;
    st.hook_caller_bump = ctx.bumps.hook_caller;
    st.decimals0 = a.mint0.decimals;
    st.decimals1 = a.mint1.decimals;

    // Write the discriminator and key now, so a hook can decode the pool during beforeInitialize
    // (sqrt_price stays 0 until then: the pool isn't initialized yet).
    {
        let info = a.pool.to_account_info();
        let mut d = info.try_borrow_mut_data()?;
        d[..8].copy_from_slice(Pool::DISCRIMINATOR);
    }
    write_pool(&a.pool, &st)?;

    let sender = a.sender.key();
    let (pool_info, hooks_info, caller_info) = (a.pool.to_account_info(), a.hooks.to_account_info(), a.hook_caller.to_account_info());
    let hctx = HookCtx {
        program: &hooks_info,
        caller: &caller_info,
        pool: &pool_info,
        vaults: None,
        remaining: ctx.remaining_accounts,
        flags: st.hook_flags,
        active: hooks != Pubkey::default() && sender != hook_auth,
        caller_bump: st.hook_caller_bump,
    };
    let key = st.key();

    if hctx.on(BEFORE_INITIALIZE) {
        let _: ((), _) = hctx.call(encode_call(disc::BEFORE_INITIALIZE, &BeforeInitializeArgs { sender, key, sqrt_price }))?;
    }
    st.sqrt_price.set(sqrt_price);
    st.tick = tick;
    emit!(events::Initialize {
        pool: pool_key,
        currency0: st.currency0,
        currency1: st.currency1,
        fee,
        tick_spacing,
        hooks,
        sqrt_price,
        tick,
    });
    if hctx.on(AFTER_INITIALIZE) {
        write_pool(&a.pool, &st)?;
        let (effects, _): (HookEffects, _) =
            hctx.call(encode_call(disc::AFTER_INITIALIZE, &AfterInitializeArgs { sender, key, sqrt_price, tick }))?;
        require!(effects.actions.is_empty(), PmError::HookActionsNotAllowed);
        if let Some(f) = effects.lp_fee {
            require!(is_dynamic_fee(st.fee), PmError::UnauthorizedDynamicLPFeeUpdate);
            require!(is_valid_lp_fee(f), PmError::LPFeeTooLarge);
            st.lp_fee = f;
            emit!(events::LpFeeUpdated { pool: pool_key, lp_fee: f });
        }
    }
    write_pool(&a.pool, &st)
}
