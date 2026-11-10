use anchor_lang::prelude::*;

use crate::errors::PmError;
use crate::state::Pool;
use crate::ticks;

#[derive(Accounts)]
pub struct ReserveTicks<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub pool: AccountLoader<'info, Pool>,
    /// CHECK: matched against the pool.
    #[account(mut)]
    pub ticks: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

/// Prepay tick capacity (hook actions can't grow the store mid-swap). At most 128 per call.
pub fn handle_reserve_ticks(ctx: Context<ReserveTicks>, additional: u32) -> Result<()> {
    require!(additional > 0 && additional <= 128, PmError::Unauthorized);
    require!(ctx.accounts.ticks.key() == ctx.accounts.pool.load()?.state.ticks, PmError::WrongPoolAccount);
    ticks::grow(&ctx.accounts.ticks.to_account_info(), &ctx.accounts.payer.to_account_info(), &ctx.accounts.system_program.to_account_info(), additional)
}
