use anchor_lang::prelude::*;
use meridian_core::POSITION_SEED;

use crate::engine::check_ticks;
use crate::errors::PmError;
use crate::state::{Pool, Position};

#[derive(Accounts)]
#[instruction(tick_lower: i32, tick_upper: i32, salt: [u8; 32])]
pub struct OpenPosition<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    pub owner: Signer<'info>,
    pub pool: AccountLoader<'info, Pool>,
    #[account(
        init,
        payer = payer,
        space = 8 + Position::INIT_SPACE,
        seeds = [POSITION_SEED, pool.key().as_ref(), owner.key().as_ref(), &tick_lower.to_le_bytes(), &tick_upper.to_le_bytes(), &salt],
        bump,
    )]
    pub position: Account<'info, Position>,
    pub system_program: Program<'info, System>,
}

/// Create an empty position, e.g. one a hook will fill through a queued ModifyLiquidity action.
pub fn handle_open_position(ctx: Context<OpenPosition>, tick_lower: i32, tick_upper: i32, salt: [u8; 32]) -> Result<()> {
    let st = ctx.accounts.pool.load()?.state;
    require!(st.sqrt_price.get() != 0, PmError::PoolNotInitialized);
    check_ticks(tick_lower, tick_upper, st.tick_spacing)?;
    let p = &mut ctx.accounts.position;
    p.pool = ctx.accounts.pool.key();
    p.owner = ctx.accounts.owner.key();
    p.tick_lower = tick_lower;
    p.tick_upper = tick_upper;
    p.salt = salt;
    p.bump = ctx.bumps.position;
    Ok(())
}

#[derive(Accounts)]
pub struct ClosePosition<'info> {
    pub owner: Signer<'info>,
    /// CHECK: receives the rent.
    #[account(mut)]
    pub recipient: UncheckedAccount<'info>,
    #[account(mut, close = recipient, has_one = owner @ PmError::NotPositionOwner, constraint = position.liquidity == 0 @ PmError::Unauthorized)]
    pub position: Account<'info, Position>,
}

/// Close an empty position and reclaim its rent. Collect fees first: a modify with zero liquidity
/// isn't possible once the position is empty.
pub fn handle_close_position(_ctx: Context<ClosePosition>) -> Result<()> {
    Ok(())
}
