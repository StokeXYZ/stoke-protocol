//! # Launch tax (a Stoke coin hook)
//!
//! A higher fee at launch that falls to the coin's own. The creator sets it for buys, sells or
//! both: where it starts (above the coin's fee, at most 10%) and how long it takes to fall
//! (1 minute to 7 days). It falls in a straight line, and after that the hook changes nothing.
//! The creator's dev buy, in the launch transaction, is exempt.
//!
//! The fee splits like every Stoke fee: creator, holders, the coin's reserves.
//!
//! Settings (12 bytes, little-endian): `buy_start_bps: u16 | buy_secs: u32 | sell_start_bps: u16 |
//! sell_secs: u32`. A side with `start_bps = 0` is off.

use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::msg;
use anchor_lang::solana_program::{account_info::AccountInfo, entrypoint::ProgramResult, program::set_return_data, program_error::ProgramError};
use meridian_core::coin_hook::{discriminator, verify_coin_hook_call, AttachArgs, TradeArgs, TradeReturn, MAX_COIN_HOOK_FEE_BPS};

pub const SETTINGS_LEN: usize = 12;
pub const MIN_SECS: u32 = 60;
pub const MAX_SECS: u32 = 7 * 86_400;

/// One side's tax: where the fee starts (bps) and how many seconds it takes to reach the coin's.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Side {
    pub start_bps: u16,
    pub secs: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    pub buy: Side,
    pub sell: Side,
}

impl Settings {
    pub fn decode(config: &[u8]) -> Option<Self> {
        if config.len() != SETTINGS_LEN {
            return None;
        }
        let u16_at = |o: usize| u16::from_le_bytes([config[o], config[o + 1]]);
        let u32_at = |o: usize| u32::from_le_bytes([config[o], config[o + 1], config[o + 2], config[o + 3]]);
        Some(Self { buy: Side { start_bps: u16_at(0), secs: u32_at(2) }, sell: Side { start_bps: u16_at(6), secs: u32_at(8) } })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(SETTINGS_LEN);
        v.extend_from_slice(&self.buy.start_bps.to_le_bytes());
        v.extend_from_slice(&self.buy.secs.to_le_bytes());
        v.extend_from_slice(&self.sell.start_bps.to_le_bytes());
        v.extend_from_slice(&self.sell.secs.to_le_bytes());
        v
    }

    /// At least one side on; each side that's on starts above the coin's fee, at most 10%, and
    /// lasts 1 minute to 7 days.
    pub fn valid(&self, fee_bps: u16) -> bool {
        let ok = |s: &Side| s.start_bps == 0 || (s.start_bps > fee_bps && s.start_bps <= MAX_COIN_HOOK_FEE_BPS && (MIN_SECS..=MAX_SECS).contains(&s.secs));
        (self.buy.start_bps > 0 || self.sell.start_bps > 0) && ok(&self.buy) && ok(&self.sell)
    }
}

/// The fee `age` seconds after launch for a side that starts at `start_bps` and falls to the
/// coin's `fee_bps` over `secs`. `None` once it's there (the hook then leaves the trade alone).
pub fn taxed_bps(side: Side, fee_bps: u16, age: i64) -> Option<u16> {
    if side.start_bps <= fee_bps || side.secs == 0 || age >= side.secs as i64 {
        return None;
    }
    let left = side.secs as u64 - age.max(0) as u64;
    let bps = fee_bps as u64 + (side.start_bps - fee_bps) as u64 * left / side.secs as u64;
    (bps > fee_bps as u64).then_some(bps.min(MAX_COIN_HOOK_FEE_BPS as u64) as u16)
}

/// The fee this trade pays instead of the coin's, if any.
pub fn decide(s: &Settings, t: &TradeArgs) -> Option<u16> {
    if t.is_dev_buy() {
        return None;
    }
    taxed_bps(if t.is_buy { s.buy } else { s.sell }, t.coin.fee_bps, t.age())
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
            Some(s) if s.valid(a.coin.fee_bps) => Ok(()),
            _ => {
                msg!("Launch tax: each side starts above the coin's fee, at most 10%, and lasts 1 minute to 7 days");
                Err(ProgramError::InvalidArgument)
            }
        }
    } else if disc == discriminator::ON_TRADE {
        let t = TradeArgs::parse(body).ok_or(ProgramError::InvalidInstructionData)?;
        // Settings were checked when the hook was added; anything else changes nothing.
        let fee_bps = Settings::decode(&t.config).and_then(|s| decide(&s, &t));
        let (ret, len) = TradeReturn { fee_bps }.encode();
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

    fn trade(is_buy: bool, age: i64) -> TradeArgs {
        TradeArgs { coin: CoinInfo { fee_bps: 100, created_at: 1_000, created_slot: 7, ..Default::default() }, is_buy, now: 1_000 + age, slot: 9, ..Default::default() }
    }

    #[test]
    fn settings_round_trip_and_rules() {
        let s = Settings { buy: Side { start_bps: 500, secs: 300 }, sell: Side { start_bps: 1_000, secs: 86_400 } };
        assert_eq!(Settings::decode(&s.encode()), Some(s));
        assert!(s.valid(100));
        assert_eq!(Settings::decode(&[0; 11]), None);
        // One side is enough; none isn't.
        assert!(Settings { buy: Side::default(), sell: s.sell }.valid(100));
        assert!(!Settings::default().valid(100));
        // Above the coin's fee, at most 10%, 1 minute to 7 days.
        assert!(!Settings { buy: Side { start_bps: 100, secs: 300 }, sell: Side::default() }.valid(100));
        assert!(!Settings { buy: Side { start_bps: 1_001, secs: 300 }, sell: Side::default() }.valid(100));
        assert!(!Settings { buy: Side { start_bps: 500, secs: 59 }, sell: Side::default() }.valid(100));
        assert!(!Settings { buy: Side { start_bps: 500, secs: MAX_SECS + 1 }, sell: Side::default() }.valid(100));
        assert!(Settings { buy: Side { start_bps: 1_000, secs: MAX_SECS }, sell: Side { start_bps: 101, secs: 60 } }.valid(100));
    }

    #[test]
    fn the_tax_falls_in_a_straight_line_to_the_coins_fee() {
        let s = Settings { buy: Side { start_bps: 500, secs: 400 }, sell: Side { start_bps: 1_000, secs: 900 } };
        assert_eq!(decide(&s, &trade(true, 0)), Some(500));
        assert_eq!(decide(&s, &trade(true, 100)), Some(400));
        assert_eq!(decide(&s, &trade(true, 399)), Some(101));
        assert_eq!(decide(&s, &trade(true, 400)), None);
        assert_eq!(decide(&s, &trade(false, 0)), Some(1_000));
        assert_eq!(decide(&s, &trade(false, 450)), Some(550));
        assert_eq!(decide(&s, &trade(false, 899)), Some(101));
        assert_eq!(decide(&s, &trade(false, 900)), None);
        assert_eq!(decide(&s, &trade(false, 1_000_000)), None);
        // A clock behind the launch counts as the launch.
        assert_eq!(decide(&s, &trade(false, -5)), Some(1_000));
        // A side that's off changes nothing.
        let sells_only = Settings { buy: Side::default(), sell: s.sell };
        assert_eq!(decide(&sells_only, &trade(true, 0)), None);
    }

    #[test]
    fn the_dev_buy_is_exempt() {
        let s = Settings { buy: Side { start_bps: 1_000, secs: 600 }, sell: Side::default() };
        let mut t = trade(true, 0);
        t.trader = t.coin.creator;
        t.slot = t.coin.created_slot;
        assert_eq!(decide(&s, &t), None);
        t.slot += 1; // the creator's later buys pay like anyone's
        assert_eq!(decide(&s, &t), Some(1_000));
    }
}
