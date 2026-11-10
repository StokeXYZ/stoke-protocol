//! # Max buy (a Stoke coin hook)
//!
//! For a coin's first minutes, no single buy can be larger than the size its creator set. It
//! binds everyone, the creator's dev buy included. Sells are never limited, and once the window
//! ends the hook changes nothing.
//!
//! It caps the size of one buy, not what a wallet can hold: someone can still buy more than once.
//! While the cap lasts, a buy has to spend an exact SOL amount (exact input), which is how every
//! app buys: a buy that fixes the tokens instead would pay whatever they cost, cap or not.
//!
//! Settings (12 bytes, little-endian): `max_lamports: u64 | secs: u32`.

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::msg;
use anchor_lang::solana_program::{account_info::AccountInfo, entrypoint::ProgramResult, program::set_return_data, program_error::ProgramError};
use meridian_core::coin_hook::{discriminator, verify_coin_hook_call, AttachArgs, TradeArgs, TradeReturn};

pub const SETTINGS_LEN: usize = 12;
/// The smallest cap a creator can set: 0.05 SOL, so a coin can always be bought.
pub const MIN_MAX_LAMPORTS: u64 = 50_000_000;
pub const MIN_SECS: u32 = 60;
pub const MAX_SECS: u32 = 86_400;
/// The error a buy above the cap fails with (`custom program error: 0x1b59`).
pub const BUY_ABOVE_MAX: u32 = 7_001;
/// The error an exact-output buy fails with while the cap lasts (`0x1b5a`).
pub const EXACT_OUT_BUY: u32 = 7_002;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    /// The largest single buy, in lamports.
    pub max_lamports: u64,
    /// How long the cap lasts after launch, in seconds.
    pub secs: u32,
}

impl Settings {
    pub fn decode(config: &[u8]) -> Option<Self> {
        if config.len() != SETTINGS_LEN {
            return None;
        }
        Some(Self { max_lamports: u64::from_le_bytes(config[..8].try_into().ok()?), secs: u32::from_le_bytes(config[8..12].try_into().ok()?) })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(SETTINGS_LEN);
        v.extend_from_slice(&self.max_lamports.to_le_bytes());
        v.extend_from_slice(&self.secs.to_le_bytes());
        v
    }

    /// At least 0.05 SOL, for 1 minute to 24 hours.
    pub fn valid(&self) -> bool {
        self.max_lamports >= MIN_MAX_LAMPORTS && (MIN_SECS..=MAX_SECS).contains(&self.secs)
    }
}

/// Why this trade is refused, if it is: inside the window, a buy above the cap, or one that
/// doesn't spend an exact SOL amount.
pub fn refuses(s: &Settings, t: &TradeArgs) -> Option<u32> {
    if !t.is_buy || t.age() >= s.secs as i64 {
        return None;
    }
    if !t.exact_in {
        return Some(EXACT_OUT_BUY);
    }
    (t.sol > s.max_lamports).then_some(BUY_ABOVE_MAX)
}

#[cfg(not(feature = "no-entrypoint"))]
anchor_lang::solana_program::entrypoint!(process);

/// A panic just fails the call: no message, so no formatting code in the binary.
#[cfg(all(feature = "custom-panic", target_os = "solana", not(feature = "no-entrypoint")))]
#[no_mangle]
fn custom_panic(_info: &core::panic::PanicInfo<'_>) {}

pub fn process(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    if data.len() < 8 || accounts.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }
    // Only Stoke Launch calls a coin hook.
    verify_coin_hook_call(program_id, &accounts[0])?;
    let (disc, body) = data.split_at(8);
    if disc == discriminator::ON_ATTACH {
        let a = AttachArgs::parse(body).ok_or(ProgramError::InvalidInstructionData)?;
        match Settings::decode(&a.config) {
            Some(s) if s.valid() => Ok(()),
            _ => {
                msg!("Max buy: at least 0.05 SOL, for 1 minute to 24 hours");
                Err(ProgramError::InvalidArgument)
            }
        }
    } else if disc == discriminator::ON_TRADE {
        let t = TradeArgs::parse(body).ok_or(ProgramError::InvalidInstructionData)?;
        // Settings were checked when the hook was added; anything else changes nothing.
        if let Some(code) = Settings::decode(&t.config).and_then(|s| refuses(&s, &t)) {
            msg!("Max buy: for its first minutes, each buy of this coin spends an exact SOL amount, up to its cap");
            return Err(ProgramError::Custom(code));
        }
        let (ret, len) = TradeReturn::default().encode();
        set_return_data(&ret[..len]);
        Ok(())
    } else {
        Err(ProgramError::InvalidInstructionData)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use meridian_core::coin_hook::CoinInfo;

    fn buy(sol: u64, age: i64) -> TradeArgs {
        TradeArgs { coin: CoinInfo { created_at: 500, ..Default::default() }, is_buy: true, exact_in: true, sol, now: 500 + age, ..Default::default() }
    }

    #[test]
    fn settings_round_trip_and_rules() {
        let s = Settings { max_lamports: 1_000_000_000, secs: 600 };
        assert_eq!(Settings::decode(&s.encode()), Some(s));
        assert!(s.valid());
        assert_eq!(Settings::decode(&[0; 8]), None);
        assert!(!Settings { max_lamports: MIN_MAX_LAMPORTS - 1, secs: 600 }.valid());
        assert!(!Settings { max_lamports: 1_000_000_000, secs: 59 }.valid());
        assert!(!Settings { max_lamports: 1_000_000_000, secs: MAX_SECS + 1 }.valid());
        assert!(Settings { max_lamports: MIN_MAX_LAMPORTS, secs: MAX_SECS }.valid());
    }

    #[test]
    fn caps_buys_inside_the_window_only() {
        let s = Settings { max_lamports: 1_000_000_000, secs: 600 };
        assert_eq!(refuses(&s, &buy(1_000_000_000, 0)), None, "the cap itself is fine");
        assert_eq!(refuses(&s, &buy(1_000_000_001, 0)), Some(BUY_ABOVE_MAX));
        assert_eq!(refuses(&s, &buy(5_000_000_000, 599)), Some(BUY_ABOVE_MAX));
        assert_eq!(refuses(&s, &buy(5_000_000_000, 600)), None, "the window is over");
        // A buy that fixes the tokens would pay whatever they cost: refused while the cap lasts.
        let mut exact_out = buy(1, 0);
        exact_out.exact_in = false;
        assert_eq!(refuses(&s, &exact_out), Some(EXACT_OUT_BUY));
        exact_out.now += 600;
        assert_eq!(refuses(&s, &exact_out), None);
        // Sells are never limited.
        let mut sell = buy(50_000_000_000, 0);
        sell.is_buy = false;
        assert_eq!(refuses(&s, &sell), None);
        sell.exact_in = false;
        assert_eq!(refuses(&s, &sell), None);
        // It binds the creator's dev buy too.
        let mut dev = buy(2_000_000_000, 0);
        dev.trader = dev.coin.creator;
        dev.slot = dev.coin.created_slot;
        assert_eq!(refuses(&s, &dev), Some(BUY_ABOVE_MAX));
    }
}
