use anchor_lang::prelude::*;
use anchor_lang::solana_program::bpf_loader_upgradeable;
use meridian_core::CONFIG_SEED;

use crate::errors::PmError;
use crate::state::Config;

#[derive(Accounts)]
pub struct InitializeConfig<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(init, payer = admin, space = 8 + Config::INIT_SPACE, seeds = [CONFIG_SEED], bump)]
    pub config: Account<'info, Config>,
    /// Only the upgrade authority can initialize, so nobody can take the admin role between
    /// deploy and initialize.
    #[account(
        seeds = [crate::ID.as_ref()],
        bump,
        seeds::program = bpf_loader_upgradeable::ID,
        constraint = program_data.upgrade_authority_address == Some(admin.key()) @ PmError::NotUpgradeAuthority
    )]
    pub program_data: Box<Account<'info, ProgramData>>,
    pub system_program: Program<'info, System>,
}

pub fn handle_initialize_config(ctx: Context<InitializeConfig>, protocol_fee_controller: Pubkey) -> Result<()> {
    let c = &mut ctx.accounts.config;
    c.admin = ctx.accounts.admin.key();
    c.protocol_fee_controller = protocol_fee_controller;
    c.bump = ctx.bumps.config;
    Ok(())
}

#[derive(Accounts)]
pub struct SetConfig<'info> {
    pub admin: Signer<'info>,
    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump, has_one = admin @ PmError::Unauthorized)]
    pub config: Account<'info, Config>,
}

pub fn handle_set_config(ctx: Context<SetConfig>, admin: Pubkey, protocol_fee_controller: Pubkey) -> Result<()> {
    let c = &mut ctx.accounts.config;
    c.admin = admin;
    c.protocol_fee_controller = protocol_fee_controller;
    Ok(())
}
