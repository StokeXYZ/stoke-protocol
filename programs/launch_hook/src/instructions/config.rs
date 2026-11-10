use anchor_lang::prelude::*;
use anchor_lang::solana_program::bpf_loader_upgradeable;

use crate::errors::LaunchError;
use crate::state::*;

/// The highest trading fee any coin can have: 2.5%.
pub const MAX_FEE_BPS: u16 = 250;
/// The most the protocol can take of a coin's trading fee: 20%.
pub const MAX_PROTOCOL_SHARE_BPS: u16 = 2_000;

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug)]
pub struct ConfigArgs {
    pub fee_recipient: Pubkey,
    pub quote_mint: Pubkey,
    pub launch_fee: u64,
    pub raise_target: u64,
    pub curve_ratio_bps: u16,
    pub min_fee_bps: u16,
    pub max_fee_bps: u16,
    pub protocol_share_bps: u16,
    pub creator_share_bps: u16,
    pub staker_share_bps: u16,
    pub snipe_fee_bps: u16,
    pub snipe_slots: u32,
    pub launches_paused: bool,
}

impl ConfigArgs {
    fn apply(&self, c: &mut LaunchConfig) -> Result<()> {
        require!((1_000_000_000..=10_000_000_000_000).contains(&self.raise_target), LaunchError::BadConfig);
        require!((1_000..=5_000).contains(&self.curve_ratio_bps), LaunchError::BadConfig);
        require!(self.launch_fee <= 10_000_000_000, LaunchError::BadConfig);
        // A coin's fee can never exceed 2.5%, and the protocol's share can never exceed 20% of it.
        require!(self.min_fee_bps <= self.max_fee_bps && self.max_fee_bps <= MAX_FEE_BPS, LaunchError::BadConfig);
        require!(self.protocol_share_bps <= MAX_PROTOCOL_SHARE_BPS, LaunchError::BadConfig);
        require!(self.protocol_share_bps as u32 + self.creator_share_bps as u32 + self.staker_share_bps as u32 <= 10_000, LaunchError::BadConfig);
        // The anti-snipe tax plus the highest fee must leave the buyer something, and the window
        // stays short: it's there for the first blocks only (150 slots ≈ 1 minute at most).
        require!(self.snipe_fee_bps as u32 + MAX_FEE_BPS as u32 <= 9_900 && self.snipe_slots <= 150, LaunchError::BadConfig);
        c.fee_recipient = self.fee_recipient;
        c.quote_mint = self.quote_mint;
        c.launch_fee = self.launch_fee;
        c.raise_target = self.raise_target;
        c.curve_ratio_bps = self.curve_ratio_bps;
        c.min_fee_bps = self.min_fee_bps;
        c.max_fee_bps = self.max_fee_bps;
        c.protocol_share_bps = self.protocol_share_bps;
        c.creator_share_bps = self.creator_share_bps;
        c.staker_share_bps = self.staker_share_bps;
        c.snipe_fee_bps = self.snipe_fee_bps;
        c.snipe_slots = self.snipe_slots;
        c.launches_paused = self.launches_paused;
        Ok(())
    }
}

#[derive(Accounts)]
pub struct InitializeConfig<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(init, payer = admin, space = 8 + LaunchConfig::INIT_SPACE, seeds = [crate::state::CONFIG_SEED], bump)]
    pub config: Account<'info, LaunchConfig>,
    #[account(
        seeds = [crate::ID.as_ref()],
        bump,
        seeds::program = bpf_loader_upgradeable::ID,
        constraint = program_data.upgrade_authority_address == Some(admin.key()) @ LaunchError::NotUpgradeAuthority
    )]
    pub program_data: Box<Account<'info, ProgramData>>,
    pub system_program: Program<'info, System>,
}

pub fn handle_initialize_config(ctx: Context<InitializeConfig>, args: ConfigArgs) -> Result<()> {
    let c = &mut ctx.accounts.config;
    c.admin = ctx.accounts.admin.key();
    c.bump = ctx.bumps.config;
    args.apply(c)
}

#[derive(Accounts)]
pub struct UpdateConfig<'info> {
    pub admin: Signer<'info>,
    #[account(mut, seeds = [crate::state::CONFIG_SEED], bump = config.bump, has_one = admin @ LaunchError::Unauthorized)]
    pub config: Account<'info, LaunchConfig>,
}

/// Changes apply to future launches only: each launch snapshots its fees and curve.
pub fn handle_update_config(ctx: Context<UpdateConfig>, admin: Pubkey, args: ConfigArgs) -> Result<()> {
    let c = &mut ctx.accounts.config;
    c.admin = admin;
    args.apply(c)
}
