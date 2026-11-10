//! Coin hooks (Stoke v3): a small program a creator adds to their coin when they launch it.
//!
//! Every coin's pool has one PoolManager hook, Stoke Launch. A coin hook plugs into *that*: Stoke
//! Launch calls it before every trade of the coin, on the curve and in the pool, and the hook can
//! - **refuse the trade**, by returning an error, or
//! - **set the trade's fee**, from 0 to [`MAX_COIN_HOOK_FEE_BPS`] (10%) of its SOL side. The fee
//!   splits like every Stoke fee: creator, holders, the coin's reserves.
//!
//! That's all it can do. It never holds or moves a coin's funds, can't touch liquidity, and gets
//! no signature but the caller's.
//!
//! Anyone can write one and add it to a coin once its code is locked (Stoke Launch's `list_hook`
//! takes any immutable program), in the launch transaction only, before the coin's first trade.
//! Stoke can pause a hook: coins that have it then trade as if they had none, until it's unpaused.
//!
//! ## Interface
//!
//! Two Anchor-compatible instructions (discriminator = sha256("global:<name>")[..8], then the
//! Borsh-encoded argument):
//!
//! ```ignore
//! pub fn on_attach(ctx: Context<CoinHookCall>, args: AttachArgs) -> Result<()>
//! pub fn on_trade(ctx: Context<CoinHookCall>, args: TradeArgs) -> Result<TradeReturn>
//! ```
//!
//! - `on_attach` runs once, when a creator adds the hook: check `args.config` (the creator's
//!   settings for this coin, up to [`MAX_COIN_HOOK_CONFIG`] bytes) and fail if it's not valid.
//! - `on_trade` runs before every trade. Return `TradeReturn::default()` to leave the trade as it
//!   is, a fee to set it, or an error to refuse it.
//!
//! Accounts: `[caller (signer, read-only)]`. The caller is the hook's own listing account, a PDA
//! of Stoke Launch that only it can sign with: [`verify_coin_hook_call`] proves the call is real.
//! Hooks are stateless in this version: everything they need arrives in the arguments.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::account_info::AccountInfo;

/// Stoke Launch: the program that calls coin hooks.
pub const LAUNCH_HOOK_ID: Pubkey = anchor_lang::pubkey!("CAcC7pz43rLg14jAE8iR3w3sgXcd2GGtoeFWUBB1SGVq");

/// Under Stoke Launch: a listed hook's account, seeds [HOOK_LISTING_SEED, hook program id].
pub const HOOK_LISTING_SEED: &[u8] = b"hook-listing";
/// sha256("account:HookListing")[..8].
pub const HOOK_LISTING_DISCRIMINATOR: [u8; 8] = [45, 203, 219, 69, 233, 251, 221, 192];

/// The most a hook can set a trade's fee to: 10% of its SOL side.
pub const MAX_COIN_HOOK_FEE_BPS: u16 = 1_000;
/// The most settings a creator can give a hook for their coin, in bytes.
pub const MAX_COIN_HOOK_CONFIG: usize = 64;

pub mod discriminator {
    /// sha256("global:on_attach")[..8].
    pub const ON_ATTACH: [u8; 8] = [126, 98, 66, 138, 165, 201, 150, 131];
    /// sha256("global:on_trade")[..8].
    pub const ON_TRADE: [u8; 8] = [241, 76, 162, 39, 190, 225, 138, 106];
}

/// The coin a hook is being called for.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, Default, PartialEq)]
pub struct CoinInfo {
    pub mint: Pubkey,
    pub creator: Pubkey,
    /// The coin's own trading fee, in bps of the SOL side of every trade.
    pub fee_bps: u16,
    /// When the coin launched (unix seconds) and in which slot.
    pub created_at: i64,
    pub created_slot: u64,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, Default, PartialEq)]
pub struct AttachArgs {
    pub coin: CoinInfo,
    /// The creator's settings for this coin. Stored as given and passed to every `on_trade`.
    pub config: Vec<u8>,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, Default, PartialEq)]
pub struct TradeArgs {
    pub coin: CoinInfo,
    pub config: Vec<u8>,
    pub trader: Pubkey,
    pub is_buy: bool,
    /// The trader fixed what goes in (true) or what comes out (false).
    pub exact_in: bool,
    /// The trade's size, in lamports and in token units (6 decimals), before fees. The side the
    /// trader fixed is exact (see [`TradeArgs::sol_is_exact`]); the other is valued at the price
    /// before the trade.
    pub sol: u64,
    pub tokens: u64,
    /// False while the coin is on its curve, true once it trades in its pool.
    pub graduated: bool,
    /// The coin's market cap in lamports, before the trade.
    pub mcap: u64,
    /// The cluster's clock.
    pub now: i64,
    pub slot: u64,
}

impl TradeArgs {
    /// Whether `sol` is the amount the trader fixed (otherwise `tokens` is).
    pub fn sol_is_exact(&self) -> bool {
        self.is_buy == self.exact_in
    }

    /// Seconds since the coin launched.
    pub fn age(&self) -> i64 {
        self.now.saturating_sub(self.coin.created_at).max(0)
    }

    /// The creator's buy in the launch transaction.
    pub fn is_dev_buy(&self) -> bool {
        self.is_buy && self.trader == self.coin.creator && self.slot == self.coin.created_slot
    }
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct TradeReturn {
    /// This trade's fee, in bps of its SOL side, instead of the coin's own. Anything above
    /// [`MAX_COIN_HOOK_FEE_BPS`] is cut to it. `None` leaves the trade as it is.
    pub fee_bps: Option<u16>,
}

// ------------------------------------------------------------------ small hooks
//
// The same encodings as the Borsh derives above, read and written by hand, so a hook that isn't
// an Anchor program stays a few kilobytes (rent is paid per byte of program). They read the
// fields they know and ignore anything after them, so a later version of the interface can add
// fields at the end without breaking the hooks already deployed.

struct Cursor<'a>(&'a [u8]);

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Some(head)
    }
    fn key(&mut self) -> Option<Pubkey> {
        Some(Pubkey::new_from_array(self.take(32)?.try_into().ok()?))
    }
    fn bool(&mut self) -> Option<bool> {
        match self.take(1)?[0] {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn i64(&mut self) -> Option<i64> {
        Some(i64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn bytes(&mut self) -> Option<Vec<u8>> {
        let n = u32::from_le_bytes(self.take(4)?.try_into().ok()?) as usize;
        Some(self.take(n)?.to_vec())
    }
    fn coin(&mut self) -> Option<CoinInfo> {
        Some(CoinInfo { mint: self.key()?, creator: self.key()?, fee_bps: self.u16()?, created_at: self.i64()?, created_slot: self.u64()? })
    }
}

impl AttachArgs {
    /// Decode `on_attach`'s argument (the instruction data after its discriminator).
    pub fn parse(data: &[u8]) -> Option<Self> {
        let mut c = Cursor(data);
        Some(Self { coin: c.coin()?, config: c.bytes()? })
    }
}

impl TradeArgs {
    /// Decode `on_trade`'s argument (the instruction data after its discriminator).
    pub fn parse(data: &[u8]) -> Option<Self> {
        let mut c = Cursor(data);
        Some(Self {
            coin: c.coin()?,
            config: c.bytes()?,
            trader: c.key()?,
            is_buy: c.bool()?,
            exact_in: c.bool()?,
            sol: c.u64()?,
            tokens: c.u64()?,
            graduated: c.bool()?,
            mcap: c.u64()?,
            now: c.i64()?,
            slot: c.u64()?,
        })
    }
}

impl TradeReturn {
    /// The return data for `on_trade`: pass `&bytes[..len]` to `set_return_data`.
    pub fn encode(&self) -> ([u8; 3], usize) {
        match self.fee_bps {
            None => ([0, 0, 0], 1),
            Some(f) => {
                let b = f.to_le_bytes();
                ([1, b[0], b[1]], 3)
            }
        }
    }
}

/// Instruction data for a coin hook call.
pub fn encode_coin_hook_call<T: AnchorSerialize>(discriminator: [u8; 8], args: &T) -> Vec<u8> {
    let mut data = Vec::with_capacity(8 + 200);
    data.extend_from_slice(&discriminator);
    args.serialize(&mut data).expect("serialize coin hook call");
    data
}

/// For coin hooks: checks that `caller` is this hook's listing account and that it signed.
///
/// The listing is a PDA of Stoke Launch, so only Stoke Launch can sign with it, and only Stoke
/// Launch can write its data: a signing account it owns that holds this program's listing can't
/// be forged.
pub fn verify_coin_hook_call(hook_program_id: &Pubkey, caller: &AccountInfo) -> core::result::Result<(), ProgramError> {
    if !caller.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if caller.owner != &LAUNCH_HOOK_ID {
        return Err(ProgramError::IllegalOwner);
    }
    let data = caller.try_borrow_data()?;
    if data.len() < 40 || data[..8] != HOOK_LISTING_DISCRIMINATOR || data[8..40] != hook_program_id.to_bytes() {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hand_written_encodings_match_borsh() {
        let coin = CoinInfo { mint: Pubkey::new_unique(), creator: Pubkey::new_unique(), fee_bps: 150, created_at: 1_762_700_000, created_slot: 377_000_123 };
        for config in [vec![], vec![7u8], (0..64).collect::<Vec<u8>>()] {
            let attach = AttachArgs { coin: coin.clone(), config: config.clone() };
            let bytes = borsh::to_vec(&attach).unwrap();
            assert_eq!(AttachArgs::parse(&bytes), Some(attach));
            assert_eq!(AttachArgs::parse(&bytes[..bytes.len() - 1]), None, "truncated");
            let trade = TradeArgs {
                coin: coin.clone(),
                config,
                trader: Pubkey::new_unique(),
                is_buy: true,
                exact_in: false,
                sol: 1_234_567_890,
                tokens: u64::MAX,
                graduated: true,
                mcap: 206_250_000_000,
                now: -5,
                slot: 9,
            };
            let mut bytes = borsh::to_vec(&trade).unwrap();
            assert_eq!(TradeArgs::parse(&bytes), Some(trade.clone()));
            assert_eq!(TradeArgs::parse(&bytes[..bytes.len() - 1]), None, "truncated");
            // Fields a later version adds at the end are ignored.
            bytes.extend_from_slice(&[7; 40]);
            assert_eq!(TradeArgs::parse(&bytes), Some(trade));
        }
        for ret in [TradeReturn { fee_bps: None }, TradeReturn { fee_bps: Some(0) }, TradeReturn { fee_bps: Some(1_000) }, TradeReturn { fee_bps: Some(u16::MAX) }] {
            let (bytes, len) = ret.encode();
            assert_eq!(&bytes[..len], borsh::to_vec(&ret).unwrap().as_slice());
        }
    }
}
