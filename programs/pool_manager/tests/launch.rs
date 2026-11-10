//! The launch hook end to end: launch (flat fee) + dev buy, curve trading with the fee split three
//! ways, staking, fee-free dev sells, anti-snipe, graduation inside a swap, SOL fees through
//! return deltas after graduation, and the liquidity share deepening the coin's floor.

mod common;

use anchor_lang::solana_program::program_pack::Pack;
use anchor_lang::{
    prelude::Pubkey,
    solana_program::{
        bpf_loader_upgradeable,
        instruction::{AccountMeta, Instruction},
        system_instruction,
    },
    AccountDeserialize, InstructionData, ToAccountMetas,
};
use anchor_spl::token_2022::spl_token_2022;
use common::*;
use meridian_core::{fees::*, math::*, *};
use meridian_launch::{self as ml, curve, errors::LaunchError, instructions::*, state::{FeeZone, FeeZones, MIN_STAKE, ZONES_OFF_AFTER_SECS}, Launch, Stake, Status};
use meridian_pool_manager::{self as pm, errors::PmError};
use solana_keypair::Keypair;
use solana_signer::Signer;

const R: u64 = 400 * SOL;
const RATIO: u16 = 3_750;
const FEE: u16 = 100;
const PROTOCOL: u16 = 1_000;
const SHARE: u16 = 3_000;
const LAUNCH_FEE: u64 = 30_000_000; // 0.03 SOL
const SNIPE: u16 = 9_000; // 90%
const SNIPE_SLOTS: u32 = 3; // the launch slot and the next two, ≈ 1.2 s

/// Launch args with the default 1% fee.
fn args() -> LaunchArgs {
    LaunchArgs { fee_bps: FEE, ..Default::default() }
}

fn launch_err(e: LaunchError) -> String {
    format!("Custom({})", anchor_lang::error::ERROR_CODE_OFFSET + e as u32)
}

fn lpda(seeds: &[&[u8]]) -> Pubkey {
    Pubkey::find_program_address(seeds, &ml::ID).0
}

/// Everything the fee has ever paid out or set aside (graduation dust included).
fn fee_buckets(s: &Launch) -> u64 {
    s.protocol_earned + s.creator_earned + s.stakers_earned + s.liquidity_pending + s.liquidity_added
}

/// (protocol, creator, holders, reserves) for a fee. The protocol takes no trading fee: its old
/// share goes to the coin's reserves with the liquidity share.
fn split(fee: u64) -> (u64, u64, u64, u64) {
    let p = 0;
    let c = fee * SHARE as u64 / 10_000;
    let st = fee * SHARE as u64 / 10_000;
    (p, c, st, fee - p - c - st)
}

struct L {
    env: Env,
    quote: Pubkey,
    fee_recipient: Keypair,
}

fn setup() -> L {
    setup_quoted(false)
}

/// `wsol`: price coins in wrapped SOL itself, as mainnet does, instead of a mint standing in for it.
fn setup_quoted(wsol: bool) -> L {
    let mut env = Env::new();
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/deploy/");
    env.svm.add_program_from_file(ml::ID, format!("{dir}meridian_launch.so")).unwrap();
    let admin = env.admin.insecure_clone();
    env.set_upgrade_authority(&ml::ID, Some(admin.pubkey()));
    let quote = if wsol { env.wrapped_sol() } else { env.mint(9, TOKEN) }; // the mint stands in for wrapped SOL
    let fee_recipient = Keypair::new();
    let ix = Instruction {
        program_id: ml::ID,
        accounts: ml::accounts::InitializeConfig {
            admin: admin.pubkey(),
            config: lpda(&[ml::state::CONFIG_SEED]),
            program_data: Pubkey::find_program_address(&[ml::ID.as_ref()], &bpf_loader_upgradeable::ID).0,
            system_program: anchor_lang::system_program::ID,
        }
        .to_account_metas(None),
        data: ml::instruction::InitializeConfig {
            args: ConfigArgs {
                fee_recipient: fee_recipient.pubkey(),
                quote_mint: quote,
                launch_fee: LAUNCH_FEE,
                raise_target: R,
                curve_ratio_bps: RATIO,
                min_fee_bps: 25,
                max_fee_bps: 250,
                protocol_share_bps: PROTOCOL,
                creator_share_bps: SHARE,
                staker_share_bps: SHARE,
                snipe_fee_bps: SNIPE,
                snipe_slots: SNIPE_SLOTS,
                launches_paused: false,
            },
        }
        .data(),
    };
    ok(env.send(&[ix], &admin, &[]));
    L { env, quote, fee_recipient }
}

struct Launched {
    mint: Pubkey,
    launch: Pubkey,
    pk: PoolKeys,
    position: Pubkey,
    creator: Keypair,
}

impl L {
    /// A fresh token mint with the creator as mint authority (6 decimals).
    fn token_mint(&mut self, creator: &Keypair, program: Pubkey) -> Pubkey {
        let mint = Keypair::new();
        let lamports = self.env.svm.minimum_balance_for_rent_exemption(82);
        let ixs = [
            system_instruction::create_account(&creator.pubkey(), &mint.pubkey(), lamports, 82, &program),
            spl_token_2022::instruction::initialize_mint2(&program, &mint.pubkey(), &creator.pubkey(), None, 6).unwrap(),
        ];
        ok(self.env.send(&ixs, creator, &[&mint]));
        mint.pubkey()
    }

    fn launch_ix(&self, creator: &Pubkey, mint: &Pubkey, args: LaunchArgs) -> (Instruction, PoolKeys, Pubkey, Pubkey) {
        let (m0, m1) = if *mint < self.quote { (*mint, self.quote) } else { (self.quote, *mint) };
        let pk = self.env.keys(m0, m1, DYNAMIC_FEE_FLAG, TICK_SPACING, ml::ID);
        let launch = lpda(&[ml::state::LAUNCH_SEED, mint.as_ref()]);
        let (lo, hi) = full_range(TICK_SPACING);
        let position = Env::position_key(&pk, &pk.hook_authority, lo, hi, [0; 32]);
        let ix = Instruction {
            program_id: ml::ID,
            accounts: ml::accounts::LaunchToken {
                creator: *creator,
                config: lpda(&[ml::state::CONFIG_SEED]),
                mint: *mint,
                quote_mint: self.quote,
                fee_recipient: self.fee_recipient.pubkey(),
                launch,
                hook_authority: pk.hook_authority,
                reserve: lpda(&[ml::state::RESERVE_SEED, mint.as_ref()]),
                pool: pk.pool,
                ticks: pk.ticks,
                vault0: pk.vault0,
                vault1: pk.vault1,
                hook_claims: pk.hook_claims,
                position,
                hook_accounts: lpda(&[HOOK_ACCOUNTS_SEED, pk.pool.as_ref()]),
                pm_authority: pm_pda(&[AUTHORITY_SEED]),
                hook_caller: pk.hook_caller,
                pool_manager: pm::ID,
                hook_program: ml::ID,
                token_program: self.env.token_program_of(mint),
                quote_token_program: TOKEN,
                system_program: anchor_lang::system_program::ID,
            }
            .to_account_metas(None),
            data: ml::instruction::Launch { args }.data(),
        };
        (ix, pk, launch, position)
    }

    fn launch(&mut self, args: LaunchArgs, program: Pubkey) -> Launched {
        let creator = Keypair::new();
        self.env.svm.airdrop(&creator.pubkey(), 100 * SOL).unwrap();
        let mint = self.token_mint(&creator, program);
        let (ix, pk, launch, position) = self.launch_ix(&creator.pubkey(), &mint, args);
        ok(self.env.send(&[ix], &creator, &[]));
        // Trade after the anti-snipe window (the anti-snipe test trades inside it).
        self.warp_slots(SNIPE_SLOTS as u64);
        Launched { mint, launch, pk, position, creator }
    }

    /// Launch with the creator's dev buy (`dev_sol`, exact in) in the same transaction.
    fn launch_with_dev_buy(&mut self, args: LaunchArgs, dev_sol: u64) -> (Launched, User) {
        let creator = Keypair::new();
        self.env.svm.airdrop(&creator.pubkey(), 100 * SOL).unwrap();
        let mint = self.token_mint(&creator, TOKEN22);
        let (launch_ix, pk, launch, position) = self.launch_ix(&creator.pubkey(), &mint, args);
        let l = Launched { mint, launch, pk, position, creator: creator.insecure_clone() };
        let dev = User {
            kp: creator.insecure_clone(),
            t0: self.env.token_account(&pk.mint0, &creator.pubkey()),
            t1: self.env.token_account(&pk.mint1, &creator.pubkey()),
        };
        let dq = self.quote_acct(&l, &dev);
        self.env.mint_to(&self.quote.clone(), &dq, 50 * SOL);
        // The hook's account list isn't published until the launch runs: build it directly.
        let metas = vec![AccountMeta::new(launch, false), AccountMeta::new(position, false)];
        let buy = self.swap_ix_with(&l, &dev, true, -(dev_sol as i128), &metas);
        ok(self.env.send(&[launch_ix, buy], &creator, &[]));
        (l, dev)
    }

    fn state(&self, l: &Launched) -> Launch {
        Launch::try_deserialize(&mut self.env.svm.get_account(&l.launch).unwrap().data.as_slice()).unwrap()
    }

    fn stake_state(&self, l: &Launched, owner: &Pubkey, tier: u8) -> Stake {
        let key = lpda(&[ml::state::STAKE_SEED, l.launch.as_ref(), owner.as_ref(), &[tier]]);
        Stake::try_deserialize(&mut self.env.svm.get_account(&key).unwrap().data.as_slice()).unwrap()
    }

    /// Hook accounts, resolved the way any client does: from the hook's published list.
    fn hook_metas(&self, l: &Launched) -> Vec<AccountMeta> {
        let data = self.env.svm.get_account(&lpda(&[HOOK_ACCOUNTS_SEED, l.pk.pool.as_ref()])).unwrap().data;
        decode_hook_accounts(&data)
            .unwrap()
            .into_iter()
            .map(|(k, w)| if w { AccountMeta::new(k, false) } else { AccountMeta::new_readonly(k, false) })
            .collect()
    }

    fn trader(&mut self, l: &Launched, sol: u64) -> User {
        let u = self.env.user(&l.pk, 0);
        let qa = if l.pk.mint0 == self.quote { u.t0 } else { u.t1 };
        self.env.mint_to(&self.quote, &qa, sol);
        u
    }

    fn quote_acct(&self, l: &Launched, u: &User) -> Pubkey {
        if l.pk.mint0 == self.quote { u.t0 } else { u.t1 }
    }

    fn token_acct(&self, l: &Launched, u: &User) -> Pubkey {
        if l.pk.mint0 == self.quote { u.t1 } else { u.t0 }
    }

    /// A swap in v4 signs: `amount_specified < 0` is exact input.
    fn swap_ix_with(&self, l: &Launched, u: &User, is_buy: bool, amount_specified: i128, metas: &[AccountMeta]) -> Instruction {
        let zero_for_one = (l.pk.mint0 == self.quote) == is_buy;
        let limit = if zero_for_one { MIN_SQRT_PRICE + 1 } else { MAX_SQRT_PRICE - 1 };
        let p = SwapParams { zero_for_one, amount_specified, sqrt_price_limit: limit };
        let amount_limit = if amount_specified < 0 { 1 } else { u64::MAX };
        self.env.swap_ix(&l.pk, &u.key(), Some(u.t0), Some(u.t1), p, Settle::default(), amount_limit, false, true, metas, vec![])
    }

    fn swap(&mut self, l: &Launched, u: &User, is_buy: bool, amount_specified: i128) -> TxResult {
        let ix = self.swap_ix_with(l, u, is_buy, amount_specified, &self.hook_metas(l));
        let kp = u.kp.insecure_clone();
        self.env.pay(&[ix], &[&kp])
    }

    fn buy(&mut self, l: &Launched, u: &User, sol_in: u64) -> TxResult {
        self.swap(l, u, true, -(sol_in as i128))
    }

    fn sell(&mut self, l: &Launched, u: &User, tokens: u64) -> TxResult {
        self.swap(l, u, false, -(tokens as i128))
    }

    fn stake_ix(&self, l: &Launched, u: &User, tier: u8, amount: u64) -> Instruction {
        Instruction {
            program_id: ml::ID,
            accounts: ml::accounts::StakeTokens {
                owner: u.key(),
                launch: l.launch,
                stake: lpda(&[ml::state::STAKE_SEED, l.launch.as_ref(), u.key().as_ref(), &[tier]]),
                mint: l.mint,
                owner_token: self.token_acct(l, u),
                stake_vault: lpda(&[ml::state::STAKE_VAULT_SEED, l.mint.as_ref()]),
                hook_authority: l.pk.hook_authority,
                token_program: self.env.token_program_of(&l.mint),
                system_program: anchor_lang::system_program::ID,
            }
            .to_account_metas(None),
            data: ml::instruction::Stake { tier, amount }.data(),
        }
    }

    fn stake(&mut self, l: &Launched, u: &User, tier: u8, amount: u64) -> TxResult {
        let ix = self.stake_ix(l, u, tier, amount);
        let kp = u.kp.insecure_clone();
        self.env.pay(&[ix], &[&kp])
    }

    fn unstake(&mut self, l: &Launched, u: &User, tier: u8, amount: u64) -> TxResult {
        let ix = Instruction {
            program_id: ml::ID,
            accounts: ml::accounts::UnstakeTokens {
                owner: u.key(),
                launch: l.launch,
                stake: lpda(&[ml::state::STAKE_SEED, l.launch.as_ref(), u.key().as_ref(), &[tier]]),
                mint: l.mint,
                owner_token: self.token_acct(l, u),
                stake_vault: lpda(&[ml::state::STAKE_VAULT_SEED, l.mint.as_ref()]),
                hook_authority: l.pk.hook_authority,
                token_program: self.env.token_program_of(&l.mint),
            }
            .to_account_metas(None),
            data: ml::instruction::Unstake { amount }.data(),
        };
        let kp = u.kp.insecure_clone();
        self.env.pay(&[ix], &[&kp])
    }

    fn claim_rewards(&mut self, l: &Launched, u: &User, tier: u8, destination: Pubkey) -> TxResult {
        let ix = Instruction {
            program_id: ml::ID,
            accounts: ml::accounts::ClaimRewards {
                owner: u.key(),
                stake: lpda(&[ml::state::STAKE_SEED, l.launch.as_ref(), u.key().as_ref(), &[tier]]),
                launch: l.launch,
                hook_authority: l.pk.hook_authority,
                pool: l.pk.pool,
                hook_claims: l.pk.hook_claims,
                vault0: l.pk.vault0,
                vault1: l.pk.vault1,
                destination,
                mint0: l.pk.mint0,
                mint1: l.pk.mint1,
                token_program0: l.pk.tp0,
                token_program1: l.pk.tp1,
                pm_authority: pm_pda(&[AUTHORITY_SEED]),
                pool_manager: pm::ID,
            }
            .to_account_metas(None),
            data: ml::instruction::ClaimRewards {}.data(),
        };
        let kp = u.kp.insecure_clone();
        self.env.pay(&[ix], &[&kp])
    }

    fn treasury_ix(&self, l: &Launched, destination: Pubkey, data: Vec<u8>) -> Instruction {
        Instruction {
            program_id: ml::ID,
            accounts: ml::accounts::HookTreasury {
                launch: l.launch,
                hook_authority: l.pk.hook_authority,
                pool: l.pk.pool,
                hook_claims: l.pk.hook_claims,
                vault0: l.pk.vault0,
                vault1: l.pk.vault1,
                destination,
                mint0: l.pk.mint0,
                mint1: l.pk.mint1,
                token_program0: l.pk.tp0,
                token_program1: l.pk.tp1,
                pm_authority: pm_pda(&[AUTHORITY_SEED]),
                pool_manager: pm::ID,
            }
            .to_account_metas(None),
            data,
        }
    }

    fn protocol_ix(&self, l: &Launched, destination: Pubkey) -> Instruction {
        Instruction {
            program_id: ml::ID,
            accounts: ml::accounts::ProtocolTreasury {
                config: lpda(&[ml::state::CONFIG_SEED]),
                treasury: ml::accounts::HookTreasury {
                    launch: l.launch,
                    hook_authority: l.pk.hook_authority,
                    pool: l.pk.pool,
                    hook_claims: l.pk.hook_claims,
                    vault0: l.pk.vault0,
                    vault1: l.pk.vault1,
                    destination,
                    mint0: l.pk.mint0,
                    mint1: l.pk.mint1,
                    token_program0: l.pk.tp0,
                    token_program1: l.pk.tp1,
                    pm_authority: pm_pda(&[AUTHORITY_SEED]),
                    pool_manager: pm::ID,
                },
            }
            .to_account_metas(None),
            data: ml::instruction::WithdrawProtocolFees {}.data(),
        }
    }

    /// withdraw_holder_rewards: the holders' share to `destination` (must belong to HOLDER_PAYOUT).
    fn withdraw_holders(&mut self, l: &Launched, destination: Pubkey) -> TxResult {
        let ix = self.treasury_ix(l, destination, ml::instruction::WithdrawHolderRewards {}.data());
        self.env.pay(&[ix], &[])
    }

    fn floor_position(&self, l: &Launched) -> Pubkey {
        let s = self.state(l);
        Env::position_key(&l.pk, &l.pk.hook_authority, s.floor_tick_lower, s.floor_tick_upper, FLOOR_SALT)
    }

    fn deepen(&mut self, l: &Launched) -> TxResult {
        let payer = self.env.payer.pubkey();
        let ix = Instruction {
            program_id: ml::ID,
            accounts: ml::accounts::Deepen {
                payer,
                launch: l.launch,
                hook_authority: l.pk.hook_authority,
                pool: l.pk.pool,
                ticks: l.pk.ticks,
                position: self.floor_position(l),
                hook_claims: l.pk.hook_claims,
                vault0: l.pk.vault0,
                vault1: l.pk.vault1,
                mint0: l.pk.mint0,
                mint1: l.pk.mint1,
                token_program0: l.pk.tp0,
                token_program1: l.pk.tp1,
                hook_caller: l.pk.hook_caller,
                pm_authority: pm_pda(&[AUTHORITY_SEED]),
                pool_manager: pm::ID,
                hook_program: ml::ID,
                system_program: anchor_lang::system_program::ID,
            }
            .to_account_metas(None),
            data: ml::instruction::Deepen {}.data(),
        };
        self.env.pay(&[ix], &[])
    }

    /// The hook's claims always equal its buckets.
    // ------------------------------------------------------------------------ fee zones

    fn zones_key(&self, launch: &Pubkey) -> Pubkey {
        lpda(&[ml::state::ZONES_SEED, launch.as_ref()])
    }

    fn set_zones_ix(&self, creator: &Pubkey, launch: Pubkey, pool: Pubkey, zones: Vec<FeeZone>) -> Instruction {
        Instruction {
            program_id: ml::ID,
            accounts: ml::accounts::SetFeeZones {
                creator: *creator,
                launch,
                zones: self.zones_key(&launch),
                hook_accounts: lpda(&[HOOK_ACCOUNTS_SEED, pool.as_ref()]),
                system_program: anchor_lang::system_program::ID,
            }
            .to_account_metas(None),
            data: ml::instruction::SetFeeZones { zones }.data(),
        }
    }

    /// Launch with fee zones set in the same transaction, then (if `dev_sol` > 0) the dev buy.
    fn try_launch_with_zones(&mut self, args: LaunchArgs, zones: Vec<FeeZone>, dev_sol: u64) -> (Launched, User, TxResult) {
        let creator = Keypair::new();
        self.env.svm.airdrop(&creator.pubkey(), 100 * SOL).unwrap();
        let mint = self.token_mint(&creator, TOKEN22);
        let (launch_ix, pk, launch, position) = self.launch_ix(&creator.pubkey(), &mint, args);
        let l = Launched { mint, launch, pk, position, creator: creator.insecure_clone() };
        let dev = User {
            kp: creator.insecure_clone(),
            t0: self.env.token_account(&l.pk.mint0, &creator.pubkey()),
            t1: self.env.token_account(&l.pk.mint1, &creator.pubkey()),
        };
        let mut ixs = vec![launch_ix, self.set_zones_ix(&creator.pubkey(), launch, l.pk.pool, zones)];
        if dev_sol > 0 {
            let dq = self.quote_acct(&l, &dev);
            self.env.mint_to(&self.quote.clone(), &dq, 50 * SOL);
            let metas = vec![AccountMeta::new(launch, false), AccountMeta::new(position, false), AccountMeta::new(self.zones_key(&launch), false)];
            ixs.push(self.swap_ix_with(&l, &dev, true, -(dev_sol as i128), &metas));
        }
        let res = self.env.send(&ixs, &creator, &[]);
        (l, dev, res)
    }

    fn launch_with_zones(&mut self, zones: Vec<FeeZone>, dev_sol: u64) -> (Launched, User) {
        let (l, dev, res) = self.try_launch_with_zones(args(), zones, dev_sol);
        ok(res);
        self.warp_slots(SNIPE_SLOTS as u64);
        (l, dev)
    }

    fn zones(&self, l: &Launched) -> FeeZones {
        FeeZones::try_deserialize(&mut self.env.svm.get_account(&self.zones_key(&l.launch)).unwrap().data.as_slice()).unwrap()
    }

    /// The coin's market cap in SOL (lamports), on the curve or in the pool.
    fn mcap(&self, l: &Launched) -> u64 {
        let s = self.state(l);
        if s.status == Status::Graduated {
            pool_mcap(self.env.state(&l.pk.pool).sqrt_price.get(), s.token_is_0)
        } else {
            curve_mcap(s.virtual_quote, s.virtual_token)
        }
    }

    /// Buy on the curve until the market cap reaches about `target` SOL.
    fn push_curve_to(&mut self, l: &Launched, u: &User, target: u64) {
        let s = self.state(l);
        let now = curve_mcap(s.virtual_quote, s.virtual_token);
        let vq = (s.virtual_quote as f64 * ((target * SOL) as f64 / now as f64).sqrt()) as u64;
        let gross = ((vq - s.virtual_quote) as u128 * 10_000 / (10_000 - FEE as u128)) as u64 + 1_000;
        ok(self.buy(l, u, gross));
    }

    /// Tokens to sell on the curve to bring the market cap down to about `target` SOL.
    fn tokens_to_curve(&self, l: &Launched, target: u64) -> u64 {
        let s = self.state(l);
        let k = s.virtual_quote as u128 * s.virtual_token as u128;
        let vq = (s.virtual_quote as f64 * ((target * SOL) as f64 / curve_mcap(s.virtual_quote, s.virtual_token) as f64).sqrt()) as u128;
        (k / vq) as u64 - s.virtual_token
    }

    /// Sell `tokens` (exact input) on the curve and check it paid exactly `bps`.
    fn sell_paying(&mut self, l: &Launched, u: &User, tokens: u64, bps: u16) {
        let s = self.state(l);
        let want = s.curve().sell_exact_in(tokens, bps);
        ok(self.sell(l, u, tokens));
        assert_eq!(fee_buckets(&self.state(l)) - fee_buckets(&s), want.fee, "expected a {bps} bps sell fee");
    }

    /// Sell `tokens` (exact input) in the pool and check it paid `bps` of the SOL out.
    fn pool_sell_paying(&mut self, l: &Launched, u: &User, tokens: u64, bps: u16) {
        let (s, qa) = (self.state(l), self.quote_acct(l, u));
        let before = self.env.balance(&qa);
        ok(self.sell(l, u, tokens));
        let fee = fee_buckets(&self.state(l)) - fee_buckets(&s);
        let got = self.env.balance(&qa) - before;
        assert_eq!(fee, (got + fee) * bps as u64 / 10_000, "expected a {bps} bps sell fee");
    }

    /// Buy in the pool, 5 SOL at a time, until the market cap reaches `target` SOL.
    fn push_pool_to(&mut self, l: &Launched, u: &User, target: u64) {
        while self.mcap(l) < target * SOL {
            ok(self.buy(l, u, 5 * SOL));
        }
    }

    fn check_books(&self, l: &Launched) {
        let s = self.state(l);
        let c = self.env.claims(&l.pk.hook_claims).unwrap();
        let (cq, ct) = if l.pk.mint0 == self.quote { (c.amount0, c.amount1) } else { (c.amount1, c.amount0) };
        assert_eq!(cq, s.real_quote + s.protocol_quote + s.creator_quote + s.staker_owed + s.liquidity_pending, "quote books");
        assert_eq!(ct, s.real_token + s.lp_tokens + s.burn_pending, "token books");
        // The stake vault holds exactly what's staked.
        if let Some(v) = self.env.svm.get_account(&lpda(&[ml::state::STAKE_VAULT_SEED, l.mint.as_ref()])) {
            let amount = u64::from_le_bytes(v.data[64..72].try_into().unwrap());
            assert_eq!(amount, s.staked, "stake vault");
        }
    }

    fn now(&self) -> i64 {
        self.env.svm.get_sysvar::<anchor_lang::prelude::Clock>().unix_timestamp
    }

    fn warp_slots(&mut self, slots: u64) {
        let mut clock: anchor_lang::prelude::Clock = self.env.svm.get_sysvar();
        clock.slot += slots;
        self.env.svm.set_sysvar(&clock);
    }

    fn slot(&self) -> u64 {
        self.env.svm.get_sysvar::<anchor_lang::prelude::Clock>().slot
    }

    fn warp(&mut self, secs: i64) {
        let mut clock: anchor_lang::prelude::Clock = self.env.svm.get_sysvar();
        clock.unix_timestamp += secs;
        clock.slot += (secs as u64) * 2 + 1;
        self.env.svm.set_sysvar(&clock);
    }
}

#[test]
fn launch_takes_the_flat_fee_and_sets_up_the_pool() {
    let mut t = setup();
    let l = t.launch(args(), TOKEN22);
    assert_eq!(t.env.svm.get_account(&t.fee_recipient.pubkey()).unwrap().lamports, LAUNCH_FEE, "the flat launch fee");
    assert_eq!(t.state(&l).launch_fee_paid, LAUNCH_FEE, "and the launch records it");
    let s = t.state(&l);
    assert_eq!(s.status, Status::Curve);
    assert_eq!(s.real_token + s.lp_tokens, TOTAL_SUPPLY);
    assert_eq!((s.fee_bps, s.protocol_share_bps, s.creator_share_bps, s.staker_share_bps), (FEE, PROTOCOL, SHARE, SHARE));
    let mint = spl_token_2022::state::Mint::unpack_from_slice(&t.env.svm.get_account(&l.mint).unwrap().data[..82]).unwrap();
    assert_eq!(mint.supply, TOTAL_SUPPLY);
    assert!(mint.mint_authority.is_none());
    // Every coin opens at ~206 SOL of market cap and graduates at ~1,467 SOL.
    let open_mc = s.virtual_quote as f64 * TOTAL_SUPPLY as f64 / s.virtual_token as f64 / SOL as f64;
    let (vq, vt) = curve::final_reserves(&curve::params(TOTAL_SUPPLY, R, RATIO));
    let grad_mc = vq as f64 * TOTAL_SUPPLY as f64 / vt as f64 / SOL as f64;
    assert!((open_mc - 206.25).abs() < 0.5, "{open_mc}");
    assert!((grad_mc - 1466.7).abs() < 2.0, "{grad_mc}");
    // The pool sits at the graduation price with no liquidity, a dynamic LP fee of 0, the hook.
    let st = t.env.state(&l.pk.pool);
    assert_eq!(st.hooks, ml::ID);
    assert_eq!(st.hook_flags, ml::FLAGS);
    assert_eq!(st.liquidity.get(), 0);
    assert_eq!(st.fee, DYNAMIC_FEE_FLAG);
    assert_eq!(st.lp_fee, 0);
    // The floor range sits on the SOL side of the graduation price, aligned to the spacing.
    let (lo, hi) = (s.floor_tick_lower, s.floor_tick_upper);
    assert!(lo < hi && lo % TICK_SPACING == 0 && hi % TICK_SPACING == 0);
    if l.pk.mint0 == t.quote { assert!(st.tick < lo) } else { assert!(st.tick >= hi) }
    t.check_books(&l);
    assert_eq!(t.hook_metas(&l).iter().map(|m| m.pubkey).collect::<Vec<_>>(), vec![l.launch, l.position]);
}

#[test]
fn nobody_else_can_create_a_pool_with_the_launch_hook() {
    let mut t = setup();
    let (a, b) = t.env.pair(TOKEN, TOKEN);
    let pk = t.env.keys(a, b, DYNAMIC_FEE_FLAG, 200, ml::ID);
    assert_fails_with(t.env.try_init(&pk, P1, &[]), &launch_err(LaunchError::PoolsOnlyThroughLaunch));
}

#[test]
fn curve_fee_is_split_four_ways_and_round_trips_lose() {
    let mut t = setup();
    let l = t.launch(args(), TOKEN22);
    let u = t.trader(&l, 100 * SOL);
    let (qa, ta) = (t.quote_acct(&l, &u), t.token_acct(&l, &u));
    let want = t.state(&l).curve().buy_exact_in(SOL, FEE, 0);
    ok(t.buy(&l, &u, SOL));
    assert_eq!(t.env.balance(&ta), want.tokens);
    assert_eq!(t.env.balance(&qa), 99 * SOL);
    let s = t.state(&l);
    // 1% of 1 SOL: 30% to the creator, 30% to holders (no staking needed) and 40% into the
    // coin's own reserves: on the curve, real SOL in the curve that doesn't move the price.
    let (p, c, h, lq) = split(SOL / 100);
    assert_eq!((p, s.protocol_quote, s.protocol_earned), (0, 0, 0));
    assert_eq!(s.creator_quote, c);
    assert_eq!(s.staker_owed, h);
    assert_eq!((s.liquidity_pending, s.liquidity_added), (0, lq));
    assert_eq!(s.real_quote, SOL - SOL / 100 + lq);
    assert_eq!(s.virtual_quote, t.state(&l).curve().virtual_quote, "reserves don't move the price");
    t.check_books(&l);

    // The holders' share only goes to the keeper's wrapped-SOL account; anyone can send it there.
    let thief = t.env.token_account(&t.quote.clone(), &Pubkey::new_unique());
    assert_fails_with(t.withdraw_holders(&l, thief), &launch_err(LaunchError::Unauthorized));
    let keeper = t.env.token_account(&t.quote.clone(), &ml::state::HOLDER_PAYOUT);
    ok(t.withdraw_holders(&l, keeper));
    assert_eq!(t.env.balance(&keeper), h);
    let s = t.state(&l);
    assert_eq!((s.staker_owed, s.stakers_earned), (0, h));
    t.check_books(&l);
    // With nothing owed it's a no-op.
    ok(t.withdraw_holders(&l, keeper));
    assert_eq!(t.env.balance(&keeper), h);

    // Selling everything back returns less than was paid.
    ok(t.sell(&l, &u, want.tokens));
    assert_eq!(t.env.balance(&ta), 0);
    assert!(t.env.balance(&qa) < 100 * SOL);
    t.check_books(&l);
    // The pool itself never traded.
    assert_eq!(t.env.state(&l.pk.pool).liquidity.get(), 0);
}

#[test]
fn pre_graduation_liquidity_is_refused() {
    let mut t = setup();
    let l = t.launch(args(), TOKEN22);
    let u = t.trader(&l, 10 * SOL);
    ok(t.buy(&l, &u, SOL));
    let res = t.env.liquidity(&l.pk, &u, -20_000, 20_000, 1_000, &t.hook_metas(&l));
    assert_fails_with(res, &launch_err(LaunchError::LiquidityLockedUntilGraduation));
    let res = t.env.donate(&l.pk, &u, 0, 1, &t.hook_metas(&l));
    assert_err(res, PmError::NoLiquidityToReceiveFees);
}

#[test]
fn anti_snipe_taxes_the_first_slots_spares_the_dev_buy_and_burns() {
    let mut t = setup();
    let fresh = curve::params(TOTAL_SUPPLY, R, RATIO);
    let fresh_curve = curve::Curve { virtual_quote: fresh.virtual_quote, virtual_token: fresh.virtual_token, real_quote: 0, real_token: fresh.alloc };
    let (l, dev) = t.launch_with_dev_buy(args(), SOL);
    let s0 = t.state(&l);
    assert_eq!((s0.snipe_fee_bps, s0.snipe_slots), (SNIPE, SNIPE_SLOTS), "every coin gets the config's anti-snipe");
    let dev_tokens = fresh_curve.buy_exact_in(SOL, FEE, 0).tokens;
    assert_eq!(t.env.balance(&t.token_acct(&l, &dev)), dev_tokens, "no tax on the dev buy");
    assert_eq!(t.state(&l).burn_pending, 0);
    assert_eq!(t.state(&l).dev_free_tokens, dev_tokens);

    // A sniper bundled into the launch slot pays 90%, which buys tokens that will be burned.
    let s = t.trader(&l, 10 * SOL);
    let want = t.state(&l).curve().buy_exact_in(SOL, FEE, SNIPE);
    ok(t.buy(&l, &s, SOL));
    assert_eq!(t.env.balance(&t.token_acct(&l, &s)), want.tokens);
    assert_eq!(t.state(&l).burn_pending, want.burned);
    assert!(want.burned > want.tokens * 8, "the sniper keeps a tenth of what the SOL bought");
    assert_eq!(t.state(&l).dev_free_tokens, dev_tokens, "only the creator's launch-slot buy counts");

    // Still 90% in the window's last slot...
    t.warp_slots(SNIPE_SLOTS as u64 - 1);
    assert_eq!(t.state(&l).snipe_bps(t.slot()), SNIPE);
    let s2 = t.trader(&l, 10 * SOL);
    let want = t.state(&l).curve().buy_exact_in(SOL, FEE, SNIPE);
    ok(t.buy(&l, &s2, SOL));
    assert_eq!(t.env.balance(&t.token_acct(&l, &s2)), want.tokens);
    // ...and nothing one slot later.
    t.warp_slots(1);
    assert_eq!(t.state(&l).snipe_bps(t.slot()), 0);
    let want = t.state(&l).curve().buy_exact_in(SOL, FEE, 0);
    let late = t.trader(&l, 10 * SOL);
    ok(t.buy(&l, &late, SOL));
    assert_eq!(t.env.balance(&t.token_acct(&l, &late)), want.tokens);
    t.check_books(&l);

    // Nobody can point the burn at another account...
    let other = t.env.token_account(&l.mint, &Pubkey::new_unique());
    let ix = t.treasury_ix(&l, other, ml::instruction::BurnPending {}.data());
    assert_fails_with(t.env.pay(&[ix], &[]), &launch_err(LaunchError::Unauthorized));
    // ...and the real one is permissionless and really burns.
    let pending = t.state(&l).burn_pending;
    let ix = t.treasury_ix(&l, lpda(&[ml::state::RESERVE_SEED, l.mint.as_ref()]), ml::instruction::BurnPending {}.data());
    ok(t.env.pay(&[ix], &[]));
    let mint_acct = spl_token_2022::state::Mint::unpack_from_slice(&t.env.svm.get_account(&l.mint).unwrap().data[..82]).unwrap();
    assert_eq!(mint_acct.supply, TOTAL_SUPPLY - pending);
    assert_eq!(t.state(&l).burned, pending);
    t.check_books(&l);
}

#[test]
fn dev_sells_are_fee_free_up_to_the_dev_buy() {
    let mut t = setup();
    let (l, dev) = t.launch_with_dev_buy(args(), 2 * SOL);
    let (dq, dt) = (t.quote_acct(&l, &dev), t.token_acct(&l, &dev));
    let bought = t.env.balance(&dt);
    assert_eq!(t.state(&l).dev_free_tokens, bought);
    t.warp(5);

    // Selling half the dev buy: no fee at all.
    let before = t.state(&l);
    let want = before.curve().sell_exact_in(bought / 2, 0);
    let q0 = t.env.balance(&dq);
    ok(t.sell(&l, &dev, bought / 2));
    assert_eq!(t.env.balance(&dq) - q0, want.quote);
    assert_eq!(fee_buckets(&t.state(&l)), fee_buckets(&before), "fee-free");
    assert_eq!(t.state(&l).dev_free_tokens, bought - bought / 2);
    t.check_books(&l);

    // A later buy pays the fee and isn't fee-free to sell: selling everything pays the fee on
    // exactly the part above the allowance.
    ok(t.buy(&l, &dev, SOL));
    let held = t.env.balance(&dt);
    let left = t.state(&l).dev_free_tokens;
    let bps = ((FEE as u128 * (held - left) as u128).div_ceil(held as u128)) as u16;
    let before = t.state(&l);
    let want = before.curve().sell_exact_in(held, bps);
    let q0 = t.env.balance(&dq);
    ok(t.sell(&l, &dev, held));
    assert_eq!(t.env.balance(&dq) - q0, want.quote);
    assert_eq!(fee_buckets(&t.state(&l)) - fee_buckets(&before), want.fee);
    assert_eq!(t.state(&l).dev_free_tokens, 0);
    t.check_books(&l);

    // Anyone else pays the full fee.
    let u = t.trader(&l, 10 * SOL);
    ok(t.buy(&l, &u, SOL));
    let held = t.env.balance(&t.token_acct(&l, &u));
    let want = t.state(&l).curve().sell_exact_in(held, FEE);
    let before = t.state(&l);
    ok(t.sell(&l, &u, held));
    assert_eq!(fee_buckets(&t.state(&l)) - fee_buckets(&before), want.fee);
}

#[test]
fn locked_staking_weighs_more_and_flexible_is_retired() {
    let mut t = setup();
    let l = t.launch(args(), TOKEN22);
    let a = t.trader(&l, 20 * SOL);
    let b = t.trader(&l, 20 * SOL);
    ok(t.buy(&l, &a, 5 * SOL));
    ok(t.buy(&l, &b, 5 * SOL));
    let x = t.env.balance(&t.token_acct(&l, &a)).min(t.env.balance(&t.token_acct(&l, &b))) / 2;

    assert_fails_with(t.stake(&l, &a, 3, x), &launch_err(LaunchError::BadTier));
    // Holding already earns 1×, so the flexible tier takes no new stakes.
    assert_fails_with(t.stake(&l, &a, 0, x), &launch_err(LaunchError::BadTier));
    assert_fails_with(t.stake(&l, &a, 1, MIN_STAKE - 1), &launch_err(LaunchError::StakeTooSmall));
    ok(t.stake(&l, &a, 1, x)); // 7 days, 1.5×
    ok(t.stake(&l, &b, 2, x)); // 30 days, 2×
    let s = t.state(&l);
    assert_eq!((s.staked, s.stakers), (2 * x, 2));
    assert_eq!(s.total_weight, x as u128 * 3 / 2 + x as u128 * 2);
    assert_eq!(t.stake_state(&l, &a.key(), 1).lock_until, t.now() + 7 * 86_400);
    assert_eq!(t.stake_state(&l, &b.key(), 2).lock_until, t.now() + 30 * 86_400);
    t.check_books(&l);

    // Fees go to the holders' share (the keeper pays it out by weight), not to on-chain claims.
    let before = t.state(&l).staker_owed;
    let c = t.trader(&l, 20 * SOL);
    ok(t.buy(&l, &c, 10 * SOL));
    let (_, _, holders, _) = split(10 * SOL / 100);
    assert_eq!(t.state(&l).staker_owed - before, holders);
    let qa = t.quote_acct(&l, &a);
    let a0 = t.env.balance(&qa);
    ok(t.claim_rewards(&l, &a, 1, qa));
    assert_eq!(t.env.balance(&qa), a0, "the reward index no longer grows");
    t.check_books(&l);

    // Locks still hold: 7 days for tier 1, 30 for tier 2.
    assert_fails_with(t.unstake(&l, &a, 1, x), &launch_err(LaunchError::StillLocked));
    t.warp(7 * 86_400);
    ok(t.unstake(&l, &a, 1, x));
    assert_fails_with(t.unstake(&l, &b, 2, x), &launch_err(LaunchError::StillLocked));
    t.warp(23 * 86_400);
    ok(t.unstake(&l, &b, 2, x));
    let s = t.state(&l);
    assert_eq!((s.staked, s.total_weight, s.stakers), (0, 0, 0));
    t.check_books(&l);

    // With nobody staked, holders still get their share and liquidity keeps its own.
    let before = t.state(&l);
    ok(t.buy(&l, &c, SOL));
    let after = t.state(&l);
    let (_, _, h, lq) = split(SOL / 100);
    assert_eq!(after.staker_owed - before.staker_owed, h);
    assert_eq!(after.liquidity_added - before.liquidity_added, lq);
    assert_eq!(after.real_quote - before.real_quote, SOL - SOL / 100 + lq);
    t.check_books(&l);
}

#[test]
fn the_buy_that_completes_the_curve_graduates_inside_the_swap() {
    for program in [TOKEN22, TOKEN] {
        let mut t = setup();
        let l = t.launch(args(), program);
        let whale = t.trader(&l, 700 * SOL);
        let (qa, ta) = (t.quote_acct(&l, &whale), t.token_acct(&l, &whale));
        let price_before = t.env.state(&l.pk.pool).sqrt_price.get();

        // 500 SOL: ~404 completes the curve, the rest continues into the brand-new pool, and
        // pays the same 1% on the way in.
        let before = t.state(&l);
        let fill = before.curve().buy_exact_in(500 * SOL, FEE, 0);
        assert!(fill.completes);
        ok(t.buy(&l, &whale, 500 * SOL));
        let s = t.state(&l);
        assert_eq!(s.status, Status::Graduated);
        assert_eq!(s.real_token, 0);
        let st = t.env.state(&l.pk.pool);
        assert_eq!(st.lp_fee, 0, "the pool's LP fee stays 0: the hook takes the fee in SOL");
        assert_ne!(st.sqrt_price.get(), price_before);
        assert_eq!(t.env.position(&l.position).liquidity, s.liquidity);
        assert_eq!(t.env.balance(&qa), 200 * SOL, "the whole 500 SOL was spent");
        assert!(t.env.balance(&ta) > curve::params(TOTAL_SUPPLY, R, RATIO).alloc);
        let pool_fee = (500 * SOL - fill.quote) / 100;
        let charged = fee_buckets(&s) - fee_buckets(&before);
        assert!(charged >= fill.fee + pool_fee && charged - (fill.fee + pool_fee) < 10_000, "{charged} (graduation rounding dust)");
        t.check_books(&l);

        // The liquidity share deepens the floor: SOL-only liquidity under the graduation price.
        let pending = t.state(&l).liquidity_pending;
        assert!(pending > ml::instructions::MIN_DEEPEN);
        ok(t.deepen(&l));
        let s = t.state(&l);
        assert!(s.liquidity_pending < 10, "{}", s.liquidity_pending);
        assert_eq!(s.liquidity_added, pending - s.liquidity_pending);
        assert_eq!(t.env.position(&t.floor_position(&l)).liquidity, s.floor_liquidity);
        assert!(s.floor_liquidity > 0);
        t.check_books(&l);

        // After graduation every trade still pays 1% in SOL, whichever side is specified.
        // Exact-input buy: 1% of the SOL in.
        let before = t.state(&l);
        ok(t.buy(&l, &whale, SOL));
        assert_eq!(fee_buckets(&t.state(&l)) - fee_buckets(&before), SOL / 100);
        // Exact-input sell: 1% of the gross SOL out.
        let before = (t.state(&l), t.env.balance(&qa));
        let tokens = t.env.balance(&ta);
        ok(t.sell(&l, &whale, tokens / 20));
        let fee = fee_buckets(&t.state(&l)) - fee_buckets(&before.0);
        let got = t.env.balance(&qa) - before.1;
        assert_eq!(fee, (got + fee) / 100);
        // Exact-output buy: the fee is on top of the pool's SOL input.
        let before = (t.state(&l), t.env.balance(&qa));
        ok(t.swap(&l, &whale, true, 1_000_000_000_000));
        let fee = fee_buckets(&t.state(&l)) - fee_buckets(&before.0);
        let paid = before.1 - t.env.balance(&qa);
        assert_eq!(fee as u128, ((paid - fee) as u128 * 100).div_ceil(9_900));
        // Exact-output sell: exactly the SOL asked for, fee on top.
        let before = (t.state(&l), t.env.balance(&qa));
        ok(t.swap(&l, &whale, false, SOL as i128));
        assert_eq!(t.env.balance(&qa) - before.1, SOL);
        let fee = fee_buckets(&t.state(&l)) - fee_buckets(&before.0);
        assert_eq!(fee as u128, (SOL as u128 * 100).div_ceil(9_900));
        t.check_books(&l);

        // Anyone can add liquidity now.
        let lp = t.trader(&l, 10 * SOL);
        let lpt = t.token_acct(&l, &lp);
        let wt = whale.kp.insecure_clone();
        let ix = spl_token_2022::instruction::transfer_checked(&program, &ta, &l.mint, &lpt, &wt.pubkey(), &[], 1_000_000_000_000, 6).unwrap();
        ok(t.env.pay(&[ix], &[&wt]));
        let (lo, hi) = full_range(200);
        ok(t.env.liquidity(&l.pk, &lp, lo, hi, 1_000_000, &t.hook_metas(&l)));

        // The creator's third goes to the creator only.
        let cq = t.env.token_account(&t.quote.clone(), &l.creator.pubkey());
        let owed = t.state(&l).creator_quote;
        assert!(owed > 0);
        let ix = t.treasury_ix(&l, cq, ml::instruction::ClaimCreatorFees {}.data());
        ok(t.env.pay(&[ix], &[]));
        assert_eq!(t.env.balance(&cq), owed);
        assert_eq!(t.state(&l).creator_quote, 0);
        let thief = t.env.token_account(&t.quote.clone(), &Pubkey::new_unique());
        let ix = t.treasury_ix(&l, thief, ml::instruction::ClaimCreatorFees {}.data());
        assert_fails_with(t.env.pay(&[ix], &[]), &launch_err(LaunchError::Unauthorized));
        t.check_books(&l);

        // The protocol takes no trading fee (its share went to the coin's reserves); withdrawing
        // what older coins owed it still only pays the fee recipient.
        assert_eq!((t.state(&l).protocol_quote, t.state(&l).protocol_earned), (0, 0));
        let ix = t.protocol_ix(&l, thief);
        assert_fails_with(t.env.pay(&[ix], &[]), &launch_err(LaunchError::Unauthorized));
        t.check_books(&l);
    }
}

#[test]
fn creators_choose_the_fee_up_to_two_and_a_half_percent() {
    let mut t = setup();
    for bad in [0u16, 24, 251, 1_000] {
        let creator = Keypair::new();
        t.env.svm.airdrop(&creator.pubkey(), 10 * SOL).unwrap();
        let mint = t.token_mint(&creator, TOKEN22);
        let (ix, ..) = t.launch_ix(&creator.pubkey(), &mint, LaunchArgs { fee_bps: bad, ..Default::default() });
        assert_fails_with(t.env.send(&[ix], &creator, &[]), &launch_err(LaunchError::FeeOutOfRange));
    }
    // A 2.5% coin charges 2.5%, split the same way.
    let l = t.launch(LaunchArgs { fee_bps: 250, ..Default::default() }, TOKEN22);
    assert_eq!(t.state(&l).fee_bps, 250);
    let u = t.trader(&l, 10 * SOL);
    let want = t.state(&l).curve().buy_exact_in(SOL, 250, 0);
    ok(t.buy(&l, &u, SOL));
    assert_eq!(want.fee, SOL / 40);
    assert_eq!(t.env.balance(&t.token_acct(&l, &u)), want.tokens);
    let s = t.state(&l);
    assert_eq!(fee_buckets(&s), SOL / 40);
    assert_eq!(s.protocol_earned, 0);
    assert_eq!(s.liquidity_added, SOL / 40 - 2 * (SOL / 40 * 3 / 10));
    // The config can't allow more than 2.5%, or give the protocol more than 20% of a fee.
    let admin = t.env.admin.insecure_clone();
    for (max_fee, share, snipe) in [(251u16, 1_000u16, SNIPE), (250, 2_001, SNIPE), (250, 1_000, 9_700)] {
        let ix = Instruction {
            program_id: ml::ID,
            accounts: ml::accounts::UpdateConfig { admin: admin.pubkey(), config: lpda(&[ml::state::CONFIG_SEED]) }.to_account_metas(None),
            data: ml::instruction::UpdateConfig {
                admin: admin.pubkey(),
                args: ConfigArgs {
                    fee_recipient: t.fee_recipient.pubkey(),
                    quote_mint: t.quote,
                    launch_fee: LAUNCH_FEE,
                    raise_target: R,
                    curve_ratio_bps: RATIO,
                    min_fee_bps: 25,
                    max_fee_bps: max_fee,
                    protocol_share_bps: share,
                    creator_share_bps: SHARE,
                    staker_share_bps: SHARE,
                    snipe_fee_bps: snipe,
                    snipe_slots: SNIPE_SLOTS,
                    launches_paused: false,
                },
            }
            .data(),
        };
        assert_fails_with(t.env.send(&[ix], &admin, &[]), &launch_err(LaunchError::BadConfig));
    }
}

#[test]
fn graduation_price_is_continuous() {
    let mut t = setup();
    let l = t.launch(args(), TOKEN22);
    let st = t.env.state(&l.pk.pool);
    let p = curve::params(TOTAL_SUPPLY, R, RATIO);
    let (vq, vt) = curve::final_reserves(&p);
    let px = price_x64(st.sqrt_price.get()).unwrap() as f64 / Q64 as f64;
    let curve_px = if l.pk.mint0 == t.quote { vt as f64 / vq as f64 } else { vq as f64 / vt as f64 };
    assert!((px / curve_px - 1.0).abs() < 1e-9, "{px} vs {curve_px}");
    // Buying exactly the rest of the curve leaves the pool at that price, now with liquidity.
    let u = t.trader(&l, 600 * SOL);
    let need = t.state(&l).curve().buy_exact_out(t.state(&l).real_token, FEE, 0).quote;
    ok(t.buy(&l, &u, need));
    let s = t.state(&l);
    assert_eq!(s.status, Status::Graduated);
    let st2 = t.env.state(&l.pk.pool);
    assert_eq!(st2.sqrt_price.get(), st.sqrt_price.get(), "no price jump at graduation");
    assert!(st2.liquidity.get() > 0);
    t.check_books(&l);
}

#[test]
fn many_traders_and_stakers_then_everyone_exits() {
    use rand::{Rng, SeedableRng};
    let mut rng = rand::rngs::StdRng::seed_from_u64(42);
    let mut t = setup();
    let l = t.launch(args(), TOKEN22);
    let keeper = t.env.token_account(&t.quote.clone(), &ml::state::HOLDER_PAYOUT);
    let traders: Vec<User> = (0..5).map(|_| t.trader(&l, 60 * SOL)).collect();
    let stake_of = |t: &L, u: &User| t.env.svm.get_account(&lpda(&[ml::state::STAKE_SEED, l.launch.as_ref(), u.key().as_ref(), &[1]])).map(|_| t.stake_state(&l, &u.key(), 1));
    for i in 0..60 {
        let k = rng.gen_range(0..traders.len());
        let u = &traders[k];
        let held = t.env.balance(&t.token_acct(&l, u));
        match rng.gen_range(0..10) {
            0..=4 => {
                let _ = t.buy(&l, u, rng.gen_range(10_000_000..6 * SOL));
            }
            5..=6 if held > 0 => ok(t.sell(&l, u, rng.gen_range(1..=held))).pipe(drop),
            7 if held >= MIN_STAKE => ok(t.stake(&l, u, 1, rng.gen_range(MIN_STAKE..=held))).pipe(drop),
            8 => {
                if let Some(st) = stake_of(&t, u) {
                    if st.amount > 0 && t.now() >= st.lock_until {
                        ok(t.unstake(&l, u, 1, rng.gen_range(1..=st.amount)));
                    }
                }
            }
            // Anyone can move the holders' share to the keeper at any time.
            _ => ok(t.withdraw_holders(&l, keeper)).pipe(drop),
        }
        if i % 10 == 0 {
            t.warp(2 * 86_400);
        }
        t.check_books(&l);
    }
    assert_eq!(t.state(&l).status, Status::Curve);
    t.warp(7 * 86_400);
    for u in &traders {
        if let Some(st) = stake_of(&t, u) {
            if st.amount > 0 {
                ok(t.unstake(&l, u, 1, st.amount));
            }
        }
        let held = t.env.balance(&t.token_acct(&l, u));
        if held > 0 {
            ok(t.sell(&l, u, held));
        }
        t.check_books(&l);
    }
    ok(t.withdraw_holders(&l, keeper));
    let s = t.state(&l);
    // Everyone is out, and everything holders earned reached the keeper for payout.
    assert_eq!((s.staked, s.staker_owed), (0, 0));
    assert_eq!(t.env.balance(&keeper), s.stakers_earned);
    t.check_books(&l);
}

/// The curve's virtual liquidity prices trades exactly like a plain constant-product pool (Raydium CPMM-style)
/// holding the same reserves, while only ever paying sellers out of the real SOL buyers put in.
#[test]
fn virtual_liquidity_trades_like_a_constant_product_pool() {
    let mut t = setup();
    let l = t.launch(args(), TOKEN22);
    t.warp_slots(5); // past the anti-snipe window: normal 1% fee
    let s0 = t.state(&l);
    // The comparison: a constant-product pool with the curve's opening reserves and the same 1% fee.
    struct Cp {
        x: f64,
        y: f64,
    }
    impl Cp {
        fn buy(&mut self, sol: f64) -> f64 {
            let eff = sol * 0.99;
            let out = self.y - self.x * self.y / (self.x + eff);
            self.x += eff;
            self.y -= out;
            out
        }
        fn sell(&mut self, tok: f64) -> f64 {
            let gross = self.x - self.x * self.y / (self.y + tok);
            self.y += tok;
            self.x -= gross;
            gross * 0.99
        }
    }
    let mut cp = Cp { x: s0.virtual_quote as f64, y: s0.virtual_token as f64 };
    let sol = |v: f64| v / 1e9;
    let mtok = |v: f64| v / 1e12; // raw (6 decimals) → millions of tokens
    // SOL per whole token (lamports per raw unit ÷ 1,000) × 1B supply = SOL of market cap.
    let mcap = |s: &Launch| s.virtual_quote as f64 / s.virtual_token as f64 / 1_000.0 * 1e9;
    println!("\nopening reserves: {:.1} SOL virtual, {:.1}M tokens · market cap {:.1} SOL · real SOL in the curve {}", sol(s0.virtual_quote as f64), mtok(s0.virtual_token as f64), mcap(&s0), sol(s0.real_quote as f64));

    let a = t.trader(&l, 50 * SOL);
    let b = t.trader(&l, 50 * SOL);
    let (qa, qb, ta, tb) = (t.quote_acct(&l, &a), t.quote_acct(&l, &b), t.token_acct(&l, &a), t.token_acct(&l, &b));
    let close = |chain: f64, model: f64| (chain - model).abs() / model < 0.0005; // within 0.05%

    ok(t.buy(&l, &a, 10 * SOL));
    let a_tok = t.env.balance(&ta) as f64;
    let m = cp.buy(10e9);
    let s = t.state(&l);
    println!("A buys 10 SOL  → {:.3}M tokens (pool model {:.3}M) · market cap {:.1} SOL · real SOL in curve {:.3}", mtok(a_tok), mtok(m), mcap(&s), sol(s.real_quote as f64));
    assert!(close(a_tok, m));

    ok(t.buy(&l, &b, 10 * SOL));
    let b_tok = t.env.balance(&tb) as f64;
    let m = cp.buy(10e9);
    let s = t.state(&l);
    println!("B buys 10 SOL  → {:.3}M tokens (pool model {:.3}M) · market cap {:.1} SOL · real SOL in curve {:.3}", mtok(b_tok), mtok(m), mcap(&s), sol(s.real_quote as f64));
    assert!(close(b_tok, m));

    let before = t.env.balance(&qa) as f64;
    ok(t.sell(&l, &a, a_tok as u64));
    let a_back = t.env.balance(&qa) as f64 - before;
    let m = cp.sell(a_tok);
    let s = t.state(&l);
    println!("A sells all    → {:.4} SOL back (pool model {:.4}) · A's profit {:+.4} SOL · market cap {:.1} SOL · real SOL in curve {:.3}", sol(a_back), sol(m), sol(a_back) - 10.0, mcap(&s), sol(s.real_quote as f64));
    assert!(close(a_back, m));

    let before = t.env.balance(&qb) as f64;
    ok(t.sell(&l, &b, b_tok as u64));
    let b_back = t.env.balance(&qb) as f64 - before;
    let m = cp.sell(b_tok);
    let s = t.state(&l);
    println!("B sells all    → {:.4} SOL back (pool model {:.4}) · B's result  {:+.4} SOL · market cap {:.1} SOL · real SOL in curve {:.6}", sol(b_back), sol(m), sol(b_back) - 10.0, mcap(&s), sol(s.real_quote as f64));
    assert!(close(b_back, m));
    println!(
        "fees: {:.4} SOL to the creator, holders and the coin's reserves · paid out to A and B: {:.4} of the 20 SOL they put in\n",
        sol((s.creator_earned + s.stakers_earned + s.liquidity_added + s.liquidity_pending) as f64),
        sol(a_back + b_back)
    );
    // Everyone's out: the curve is back at its opening price, and the real SOL it still holds is the
    // reserves fees put in (rounding dust at most): real backing that replaced virtual liquidity.
    assert!(s.real_quote.abs_diff(s.liquidity_added) < 10, "{} vs {}", s.real_quote, s.liquidity_added);
    assert!(s.virtual_quote.abs_diff(s0.virtual_quote) < 10, "back at the opening price (rounding dust at most)");
    t.check_books(&l);
}

// ---------------------------------------------------------------------------------- fee zones

fn zone(from: u64, to: u64, bps: u16) -> FeeZone {
    FeeZone { from_mcap: from * SOL, to_mcap: to * SOL, sell_fee_bps: bps }
}

/// Zones on the curve (it opens at ≈206 SOL and graduates at ≈1,467): 4% · 7% · 10%.
fn curve_zones() -> Vec<FeeZone> {
    vec![zone(600, 800, 400), zone(800, 1_000, 700), zone(1_000, 1_200, 1_000)]
}

#[test]
fn fee_zones_are_set_by_the_creator_in_the_launch_transaction_only() {
    let mut t = setup();
    let (l, _, res) = t.try_launch_with_zones(args(), curve_zones(), 0);
    ok(res);
    assert!(t.state(&l).has_fee_zones());
    let z = t.zones(&l);
    assert_eq!((z.zones.clone(), z.off, z.launch), (curve_zones(), false, l.launch));
    // The pool's published hook accounts now carry the zones account, writable, so every client passes it.
    let metas = t.hook_metas(&l);
    assert_eq!(metas.iter().map(|m| (m.pubkey, m.is_writable)).collect::<Vec<_>>(), vec![(l.launch, true), (l.position, true), (t.zones_key(&l.launch), true)]);

    // A coin without zones can't get them later.
    let plain = t.launch(args(), TOKEN22);
    let creator = plain.creator.insecure_clone();
    let ix = t.set_zones_ix(&creator.pubkey(), plain.launch, plain.pk.pool, curve_zones());
    assert_fails_with(t.env.send(&[ix], &creator, &[]), &launch_err(LaunchError::ZonesOnlyAtLaunch));
    assert!(!t.state(&plain).has_fee_zones());

    // Nor after the first trade, even in the launch transaction (here, after the dev buy).
    let creator = Keypair::new();
    t.env.svm.airdrop(&creator.pubkey(), 100 * SOL).unwrap();
    let mint = t.token_mint(&creator, TOKEN22);
    let (launch_ix, pk, launch, position) = t.launch_ix(&creator.pubkey(), &mint, args());
    let l2 = Launched { mint, launch, pk, position, creator: creator.insecure_clone() };
    let dev = User { kp: creator.insecure_clone(), t0: t.env.token_account(&pk.mint0, &creator.pubkey()), t1: t.env.token_account(&pk.mint1, &creator.pubkey()) };
    let dq = t.quote_acct(&l2, &dev);
    t.env.mint_to(&t.quote.clone(), &dq, 5 * SOL);
    let buy = t.swap_ix_with(&l2, &dev, true, -(SOL as i128), &[AccountMeta::new(launch, false), AccountMeta::new(position, false)]);
    let set = t.set_zones_ix(&creator.pubkey(), launch, pk.pool, curve_zones());
    assert_fails_with(t.env.send(&[launch_ix, buy, set], &creator, &[]), &launch_err(LaunchError::ZonesOnlyAtLaunch));

    // Only the creator sets them.
    let creator = Keypair::new();
    let other = Keypair::new();
    t.env.svm.airdrop(&creator.pubkey(), 100 * SOL).unwrap();
    t.env.svm.airdrop(&other.pubkey(), 10 * SOL).unwrap();
    let mint = t.token_mint(&creator, TOKEN22);
    let (launch_ix, pk, launch, _) = t.launch_ix(&creator.pubkey(), &mint, args());
    let set = t.set_zones_ix(&other.pubkey(), launch, pk.pool, curve_zones());
    assert_fails_with(t.env.send(&[launch_ix, set], &creator, &[&other]), &launch_err(LaunchError::Unauthorized));

    // Every rule on the zones themselves.
    let bad = [
        vec![],                                                        // none
        vec![zone(600, 800, FEE)],                                     // no more than the coin's fee
        vec![zone(600, 800, 1_001)],                                   // above 10%
        vec![zone(300, 800, 400)],                                     // below the lowest market cap
        vec![zone(6_000, 7_001, 400)],                                 // above the highest
        vec![zone(600, 600, 400)],                                     // empty
        vec![zone(600, 800, 400), zone(700, 900, 500)],                // overlapping
        vec![zone(800, 1_000, 500), zone(600, 800, 400)],              // not lowest first
        (0..6).map(|i| zone(600 + i * 100, 700 + i * 100, 400)).collect(), // more than 5
    ];
    for zones in bad {
        let (_, _, res) = t.try_launch_with_zones(args(), zones.clone(), 0);
        assert_fails_with(res, &launch_err(LaunchError::BadFeeZones));
    }
    // 10% exactly, at the edges of the allowed range, 5 zones with gaps: fine.
    let (_, _, res) = t.try_launch_with_zones(args(), vec![zone(400, 500, 1_000), zone(600, 700, 200), zone(800, 900, 300), zone(1_000, 2_000, 400), zone(6_000, 7_000, 500)], 0);
    ok(res);
}

#[test]
fn zone_sells_pay_the_highest_zone_fee_their_price_passes_through() {
    let mut t = setup();
    let (l, _) = t.launch_with_zones(curve_zones(), 0);
    let u = t.trader(&l, 2_000 * SOL);
    let ta = t.token_acct(&l, &u);
    assert!(t.mcap(&l).abs_diff(206_250_000_000) < 1_000_000_000, "{}", t.mcap(&l));

    // Below the zones: the coin's own 1%, both ways.
    ok(t.buy(&l, &u, 50 * SOL));
    let tokens = t.env.balance(&ta) / 10;
    t.sell_paying(&l, &u, tokens, FEE);

    // In zone 1 (600–800): buys still pay 1%, sells 4%.
    t.push_curve_to(&l, &u, 700);
    assert!((600 * SOL..800 * SOL).contains(&t.mcap(&l)), "{}", t.mcap(&l));
    let s = t.state(&l);
    let want = s.curve().buy_exact_in(SOL, FEE, 0);
    ok(t.buy(&l, &u, SOL));
    assert_eq!(fee_buckets(&t.state(&l)) - fee_buckets(&s), want.fee, "buys never pay a zone fee");
    t.sell_paying(&l, &u, 1_000_000_000, 400);

    // In zone 2 (800–1,000): 7%; a sell from there down into zone 1 still pays 7%.
    t.push_curve_to(&l, &u, 900);
    t.sell_paying(&l, &u, 1_000_000_000, 700);
    let tokens = t.tokens_to_curve(&l, 700);
    t.sell_paying(&l, &u, tokens, 700);
    assert!((600 * SOL..800 * SOL).contains(&t.mcap(&l)));

    // From zone 3 (1,000–1,200) all the way below the zones: 10%, the highest it passed through.
    t.push_curve_to(&l, &u, 1_100);
    let tokens = t.tokens_to_curve(&l, 500);
    t.sell_paying(&l, &u, tokens, 1_000);
    assert!(t.mcap(&l) < 600 * SOL);
    t.check_books(&l);
}

#[test]
fn pushing_the_price_above_the_zones_first_doesnt_dodge_them() {
    let mut t = setup();
    let (l, _) = t.launch_with_zones(curve_zones(), 0);
    let whale = t.trader(&l, 2_000 * SOL);
    t.push_curve_to(&l, &whale, 900);
    // Pump above the top zone, then dump into the zones: the dump starts above them but passes
    // through zone 3, so it pays 10%.
    t.push_curve_to(&l, &whale, 1_300);
    let tokens = t.tokens_to_curve(&l, 850);
    t.sell_paying(&l, &whale, tokens, 1_000);
    // A sell that stays above the top zone pays the coin's fee.
    t.push_curve_to(&l, &whale, 1_400);
    let tokens = t.tokens_to_curve(&l, 1_250);
    t.sell_paying(&l, &whale, tokens, FEE);
    assert!(!t.zones(&l).off, "a pump alone doesn't switch the zones off");
}

#[test]
fn zones_switch_off_for_good_once_the_coin_holds_above_its_top_zone() {
    let mut t = setup();
    let (l, _) = t.launch_with_zones(curve_zones(), 0);
    let u = t.trader(&l, 2_000 * SOL);
    t.push_curve_to(&l, &u, 1_300);
    ok(t.buy(&l, &u, SOL / 10)); // starts above the top: the hold begins
    let since = t.zones(&l).above_since;
    assert!(since > 0);
    t.warp(ZONES_OFF_AFTER_SECS - 1);
    ok(t.buy(&l, &u, SOL / 11));
    assert!(!t.zones(&l).off, "not held long enough");

    // Dipping back into the zones resets the hold (and that dip pays the zone fee).
    let tokens = t.tokens_to_curve(&l, 1_100);
    t.sell_paying(&l, &u, tokens, 1_000);
    t.push_curve_to(&l, &u, 1_300); // starts below the top: resets
    assert_eq!(t.zones(&l).above_since, 0);
    ok(t.buy(&l, &u, SOL / 12));
    t.warp(ZONES_OFF_AFTER_SECS / 2);
    ok(t.buy(&l, &u, SOL / 13));
    assert!(!t.zones(&l).off);
    t.warp(ZONES_OFF_AFTER_SECS / 2);
    ok(t.buy(&l, &u, SOL / 14));
    let z = t.zones(&l);
    assert!(z.off && z.off_at > 0, "held above the top for the whole time: off for good");

    // Off: sells back through the zones pay the coin's own fee, and exact-output sells work again.
    let tokens = t.tokens_to_curve(&l, 900);
    t.sell_paying(&l, &u, tokens, FEE);
    ok(t.swap(&l, &u, false, SOL as i128));
    t.push_curve_to(&l, &u, 1_100);
    let tokens = t.tokens_to_curve(&l, 700);
    t.sell_paying(&l, &u, tokens, FEE);
    t.check_books(&l);
}

#[test]
fn zoned_coins_refuse_swaps_without_their_zones_account_and_exact_output_sells() {
    let mut t = setup();
    let (l, _) = t.launch_with_zones(curve_zones(), 0);
    let u = t.trader(&l, 100 * SOL);
    ok(t.buy(&l, &u, 10 * SOL));
    let missing = launch_err(LaunchError::FeeZonesMissing);
    // Without the zones account (the old two-account list), both ways.
    let old = [AccountMeta::new(l.launch, false), AccountMeta::new(l.position, false)];
    let kp = u.kp.insecure_clone();
    let sell = t.swap_ix_with(&l, &u, false, -1_000_000_000, &old);
    assert_fails_with(t.env.pay(&[sell], &[&kp]), &missing);
    let buy = t.swap_ix_with(&l, &u, true, -(SOL as i128), &old);
    assert_fails_with(t.env.pay(&[buy], &[&kp]), &missing);
    // Read-only: it couldn't be updated, so it doesn't count.
    let ro = [old[0].clone(), old[1].clone(), AccountMeta::new_readonly(t.zones_key(&l.launch), false)];
    let sell = t.swap_ix_with(&l, &u, false, -1_000_000_000, &ro);
    assert_fails_with(t.env.pay(&[sell], &[&kp]), &missing);
    // Another coin's zones don't count either.
    let (other, _) = t.launch_with_zones(curve_zones(), 0);
    let wrong = [old[0].clone(), old[1].clone(), AccountMeta::new(t.zones_key(&other.launch), false)];
    let sell = t.swap_ix_with(&l, &u, false, -1_000_000_000, &wrong);
    assert_fails_with(t.env.pay(&[sell], &[&kp]), &missing);

    // Exact-output sells are refused while the zones are on; exact-output buys are fine.
    assert_fails_with(t.swap(&l, &u, false, (SOL / 10) as i128), &launch_err(LaunchError::ExactOutSellInZones));
    ok(t.swap(&l, &u, true, 1_000_000_000));

    // A coin without zones ignores the extra account and trades as before.
    let plain = t.launch(args(), TOKEN22);
    let p = t.trader(&plain, 10 * SOL);
    ok(t.buy(&plain, &p, SOL));
    ok(t.swap(&plain, &p, false, (SOL / 10) as i128));
}

#[test]
fn the_creator_pays_zone_fees_too_and_keeps_the_fee_free_allowance() {
    let mut t = setup();
    let (l, dev) = t.launch_with_zones(curve_zones(), 20 * SOL);
    let allowance = t.state(&l).dev_free_tokens;
    assert!(allowance > 0);
    let u = t.trader(&l, 2_000 * SOL);
    t.push_curve_to(&l, &u, 700);
    // In zone 1 the creator's sell pays 4%, and their allowance is untouched.
    t.sell_paying(&l, &dev, allowance / 4, 400);
    assert_eq!(t.state(&l).dev_free_tokens, allowance);
    // Below the zones, the allowance works as before: fee-free.
    let tokens = t.tokens_to_curve(&l, 500);
    t.sell_paying(&l, &u, tokens, 400); // from zone 1 down below the zones: 4%
    t.sell_paying(&l, &dev, allowance / 4, 0);
    assert_eq!(t.state(&l).dev_free_tokens, allowance - allowance / 4);
    t.check_books(&l);
}

#[test]
fn zones_apply_in_the_pool_after_graduation() {
    let mut t = setup();
    // Above graduation (≈1,467 SOL): 5% from 1,600, 8% from 2,000 to 2,400.
    let zones = vec![zone(1_600, 2_000, 500), zone(2_000, 2_400, 800)];
    let (l, _) = t.launch_with_zones(zones, 0);
    let whale = t.trader(&l, 3_000 * SOL);
    let (qa, ta) = (t.quote_acct(&l, &whale), t.token_acct(&l, &whale));
    ok(t.buy(&l, &whale, 410 * SOL));
    assert_eq!(t.state(&l).status, Status::Graduated);
    let m = t.mcap(&l);
    assert!((1_460 * SOL..1_600 * SOL).contains(&m), "the pool picks up where the curve left off: {m}");

    // Below the zones: 1%.
    t.pool_sell_paying(&l, &whale, t.env.balance(&ta) / 200, FEE);
    // Zone 1: buys pay 1%, sells 5%.
    t.push_pool_to(&l, &whale, 1_700);
    let before = (t.state(&l), t.env.balance(&qa));
    ok(t.buy(&l, &whale, SOL));
    assert_eq!(fee_buckets(&t.state(&l)) - fee_buckets(&before.0), SOL / 100);
    t.pool_sell_paying(&l, &whale, t.env.balance(&ta) / 500, 500);
    // From zone 2 down below the zones: 8%.
    t.push_pool_to(&l, &whale, 2_100);
    t.pool_sell_paying(&l, &whale, t.env.balance(&ta) / 3, 800);
    assert!(t.mcap(&l) < 1_600 * SOL, "{}", t.mcap(&l));
    // Exact-output sells are refused while the zones are on.
    assert_fails_with(t.swap(&l, &whale, false, SOL as i128), &launch_err(LaunchError::ExactOutSellInZones));

    // Held above 2,400 long enough: off for good, and a dump back through the zones pays 1%.
    t.warp(1); // the test clock starts at 0, which the hold treats as "not started"
    t.push_pool_to(&l, &whale, 2_500);
    ok(t.buy(&l, &whale, SOL / 10));
    t.warp(ZONES_OFF_AFTER_SECS - 1);
    ok(t.buy(&l, &whale, SOL / 11));
    assert!(!t.zones(&l).off, "a second short");
    t.warp(1);
    ok(t.buy(&l, &whale, SOL / 12));
    assert!(t.zones(&l).off);
    t.pool_sell_paying(&l, &whale, t.env.balance(&ta) / 3, FEE);
    assert!(t.mcap(&l) < 2_000 * SOL);
    t.check_books(&l);
}

trait Pipe: Sized {
    fn pipe<R>(self, f: impl FnOnce(Self) -> R) -> R {
        f(self)
    }
}
impl<T> Pipe for T {}

// ---------------------------------------------------------------------------------- coin hooks (Stoke v3)

use meridian_launch::state::{CoinHook, HookListing, COIN_HOOK_SEED, MAX_COIN_HOOK_FEE_BPS};
use meridian_mock_hook::{coin_mode, COIN_LOG_TAG};
use stoke_hook_launch_tax as tax;
use stoke_hook_max_buy as max_buy;

const DEPLOY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/deploy/");
/// What the mock coin hook fails with (`ProgramError::Custom(0xbad)`).
const MOCK_REVERT: &str = "Custom(2989)";

fn anchor_err(code: u32) -> String {
    format!("Custom({code})")
}

/// The mock coin hook's settings: a mode, then (for `coin_mode::FEE`) the fee it returns.
fn mock_cfg(mode: u8, bps: u16) -> Vec<u8> {
    let mut v = vec![mode];
    v.extend_from_slice(&bps.to_le_bytes());
    v
}

fn tax_cfg(buy: (u16, u32), sell: (u16, u32)) -> Vec<u8> {
    tax::Settings { buy: tax::Side { start_bps: buy.0, secs: buy.1 }, sell: tax::Side { start_bps: sell.0, secs: sell.1 } }.encode()
}

fn b64(s: &str) -> Vec<u8> {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let (mut out, mut acc, mut bits) = (Vec::new(), 0u32, 0);
    for c in s.bytes().filter(|c| *c != b'=') {
        acc = (acc << 6) | A.iter().position(|x| *x == c).unwrap() as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

/// What the mock coin hook logged in a transaction: (its argument's bytes, [accounts, signer, writable]) per call.
fn mock_calls(logs: &[String]) -> Vec<(Vec<u8>, Vec<u8>)> {
    logs.iter()
        .filter_map(|l| l.strip_prefix("Program data: "))
        .filter_map(|l| {
            let parts: Vec<Vec<u8>> = l.split(' ').map(b64).collect();
            (parts.len() == 3 && parts[0] == COIN_LOG_TAG).then(|| (parts[1].clone(), parts[2].clone()))
        })
        .collect()
}

impl L {
    fn listing_key(&self, program: &Pubkey) -> Pubkey {
        lpda(&[HOOK_LISTING_SEED, program.as_ref()])
    }

    fn coin_hook_key(&self, launch: &Pubkey) -> Pubkey {
        lpda(&[COIN_HOOK_SEED, launch.as_ref()])
    }

    fn list_hook_ix(&self, payer: &Pubkey, program: &Pubkey) -> Instruction {
        Instruction {
            program_id: ml::ID,
            accounts: ml::accounts::ListHook {
                payer: *payer,
                config: lpda(&[ml::state::CONFIG_SEED]),
                hook_program: *program,
                program_data: Pubkey::find_program_address(&[program.as_ref()], &bpf_loader_upgradeable::ID).0,
                listing: self.listing_key(program),
                system_program: anchor_lang::system_program::ID,
            }
            .to_account_metas(None),
            data: ml::instruction::ListHook {}.data(),
        }
    }

    /// Load a hook program at a fresh address, upgradeable by the admin only, and list it, as the admin.
    fn add_hook(&mut self, so: &str) -> Pubkey {
        let program = Pubkey::new_unique();
        self.env.svm.add_program_from_file(program, format!("{DEPLOY}{so}.so")).unwrap();
        let admin = self.env.admin.insecure_clone();
        self.env.set_upgrade_authority(&program, Some(admin.pubkey()));
        let ix = self.list_hook_ix(&admin.pubkey(), &program);
        ok(self.env.send(&[ix], &admin, &[]));
        program
    }

    fn pause_ix(&self, admin: &Pubkey, program: &Pubkey, paused: bool) -> Instruction {
        Instruction {
            program_id: ml::ID,
            accounts: ml::accounts::SetHookPaused { admin: *admin, config: lpda(&[ml::state::CONFIG_SEED]), listing: self.listing_key(program) }.to_account_metas(None),
            data: ml::instruction::SetHookPaused { paused }.data(),
        }
    }

    fn set_hook_paused(&mut self, program: &Pubkey, paused: bool) {
        let admin = self.env.admin.insecure_clone();
        let ix = self.pause_ix(&admin.pubkey(), program, paused);
        ok(self.env.send(&[ix], &admin, &[]));
    }

    fn attach_ix(&self, creator: &Pubkey, launch: Pubkey, pool: Pubkey, program: Pubkey, config: Vec<u8>) -> Instruction {
        Instruction {
            program_id: ml::ID,
            accounts: ml::accounts::AttachHook {
                creator: *creator,
                launch,
                listing: self.listing_key(&program),
                hook_program: program,
                coin_hook: self.coin_hook_key(&launch),
                hook_accounts: lpda(&[HOOK_ACCOUNTS_SEED, pool.as_ref()]),
                system_program: anchor_lang::system_program::ID,
            }
            .to_account_metas(None),
            data: ml::instruction::AttachHook { config }.data(),
        }
    }

    /// The accounts a hooked coin's swaps pass, as the pool's published list will have them.
    fn hooked_metas(&self, launch: Pubkey, position: Pubkey, program: Pubkey, zoned: bool) -> Vec<AccountMeta> {
        let mut m = vec![AccountMeta::new(launch, false), AccountMeta::new(position, false)];
        if zoned {
            m.push(AccountMeta::new(self.zones_key(&launch), false));
        }
        m.extend([AccountMeta::new(self.coin_hook_key(&launch), false), AccountMeta::new_readonly(self.listing_key(&program), false), AccountMeta::new_readonly(program, false)]);
        m
    }

    /// Launch with a hook added in the same transaction (after any fee zones), then the dev buy.
    fn try_launch_with_hook(&mut self, program: Pubkey, config: Vec<u8>, zones: Option<Vec<FeeZone>>, dev_sol: u64) -> (Launched, User, TxResult) {
        let creator = Keypair::new();
        self.env.svm.airdrop(&creator.pubkey(), 100 * SOL).unwrap();
        let mint = self.token_mint(&creator, TOKEN22);
        let (launch_ix, pk, launch, position) = self.launch_ix(&creator.pubkey(), &mint, args());
        let l = Launched { mint, launch, pk, position, creator: creator.insecure_clone() };
        let dev = User {
            kp: creator.insecure_clone(),
            t0: self.env.token_account(&l.pk.mint0, &creator.pubkey()),
            t1: self.env.token_account(&l.pk.mint1, &creator.pubkey()),
        };
        let zoned = zones.is_some();
        let mut ixs = vec![launch_ix];
        if let Some(z) = zones {
            ixs.push(self.set_zones_ix(&creator.pubkey(), launch, l.pk.pool, z));
        }
        ixs.push(self.attach_ix(&creator.pubkey(), launch, l.pk.pool, program, config));
        if dev_sol > 0 {
            let dq = self.quote_acct(&l, &dev);
            self.env.mint_to(&self.quote.clone(), &dq, 50 * SOL);
            let metas = self.hooked_metas(launch, position, program, zoned);
            ixs.push(self.swap_ix_with(&l, &dev, true, -(dev_sol as i128), &metas));
        }
        let res = self.env.send(&ixs, &creator, &[]);
        (l, dev, res)
    }

    fn launch_with_hook(&mut self, program: Pubkey, config: Vec<u8>, dev_sol: u64) -> (Launched, User) {
        let (l, dev, res) = self.try_launch_with_hook(program, config, None, dev_sol);
        ok(res);
        self.warp_slots(SNIPE_SLOTS as u64);
        (l, dev)
    }

    fn coin_hook(&self, l: &Launched) -> CoinHook {
        CoinHook::try_deserialize(&mut self.env.svm.get_account(&self.coin_hook_key(&l.launch)).unwrap().data.as_slice()).unwrap()
    }

    fn listing(&self, program: &Pubkey) -> HookListing {
        HookListing::try_deserialize(&mut self.env.svm.get_account(&self.listing_key(program)).unwrap().data.as_slice()).unwrap()
    }

    /// Buy `sol` (exact input) on the curve, outside the anti-snipe window, and check it paid exactly `bps`.
    fn buy_paying(&mut self, l: &Launched, u: &User, sol: u64, bps: u16) {
        let s = self.state(l);
        let want = s.curve().buy_exact_in(sol, bps, 0);
        ok(self.buy(l, u, sol));
        assert_eq!(fee_buckets(&self.state(l)) - fee_buckets(&s), want.fee, "expected a {bps} bps buy fee");
    }
}

#[test]
fn hooks_are_listed_by_the_admin_and_added_by_the_creator_in_the_launch_transaction_only() {
    let mut t = setup();
    let admin = t.env.admin.insecure_clone();

    // Anyone lists a hook, once its code is locked. While someone can still change it, nobody can,
    // its owner and the admin included.
    let program = Pubkey::new_unique();
    t.env.svm.add_program_from_file(program, format!("{DEPLOY}stoke_hook_launch_tax.so")).unwrap();
    let stranger = Keypair::new();
    t.env.svm.airdrop(&stranger.pubkey(), 10 * SOL).unwrap();
    t.env.set_upgrade_authority(&program, Some(stranger.pubkey()));
    for who in [&stranger, &admin] {
        let ix = t.list_hook_ix(&who.pubkey(), &program);
        assert_fails_with(t.env.send(&[ix], who, &[]), &launch_err(LaunchError::HookUpgradeable));
    }
    // Only a program.
    let not_a_program = t.list_hook_ix(&admin.pubkey(), &stranger.pubkey());
    assert!(t.env.send(&[not_a_program], &admin, &[]).is_err());
    // Immutable: a stranger lists it, pays for the listing, and anyone's coin can then have it.
    let frozen = Pubkey::new_unique();
    t.env.svm.add_program_from_file(frozen, format!("{DEPLOY}stoke_hook_max_buy.so")).unwrap();
    t.env.set_upgrade_authority(&frozen, None);
    let paid = t.env.svm.get_balance(&stranger.pubkey()).unwrap();
    let ix = t.list_hook_ix(&stranger.pubkey(), &frozen);
    ok(t.env.send(&[ix], &stranger, &[]));
    assert!(t.env.svm.get_balance(&stranger.pubkey()).unwrap() < paid);
    let (_, _, res) = t.try_launch_with_hook(frozen, max_buy::Settings { max_lamports: SOL, secs: 600 }.encode(), None, SOL);
    ok(res);
    // The stranger can't pause it, or anything else: only the admin pauses.
    let ix = t.pause_ix(&stranger.pubkey(), &frozen, true);
    assert_fails_with(t.env.send(&[ix], &stranger, &[]), &launch_err(LaunchError::Unauthorized));
    // Upgradeable by the admin only (Stoke's own hooks): locked too, whoever lists it.
    t.env.set_upgrade_authority(&program, Some(admin.pubkey()));
    for own in [ml::ID, pm::ID] {
        let ix = t.list_hook_ix(&admin.pubkey(), &own);
        assert_fails_with(t.env.send(&[ix], &admin, &[]), &launch_err(LaunchError::HookNotListed));
    }
    // A hook that isn't listed can't be added.
    let cfg = tax_cfg((500, 300), (1_000, 3_600));
    let (_, _, res) = t.try_launch_with_hook(program, cfg.clone(), None, 0);
    assert!(res.is_err());
    let ix = t.list_hook_ix(&stranger.pubkey(), &program);
    ok(t.env.send(&[ix], &stranger, &[]));
    let listing = t.listing(&program);
    assert_eq!((listing.program, listing.paused, listing.coins), (program, false, 0));
    // Once listed, it's listed: there's no second listing.
    let ix = t.list_hook_ix(&admin.pubkey(), &program);
    assert!(t.env.send(&[ix], &admin, &[]).is_err());

    // The creator adds it in the launch transaction.
    let (l, _, res) = t.try_launch_with_hook(program, cfg.clone(), None, 0);
    ok(res);
    let s = t.state(&l);
    assert!(s.has_coin_hook() && !s.has_fee_zones());
    let h = t.coin_hook(&l);
    assert_eq!((h.launch, h.program, h.listing, h.config.clone()), (l.launch, program, t.listing_key(&program), cfg.clone()));
    assert_eq!(t.listing(&program).coins, 1);
    // The pool's published hook accounts carry the hook's accounts, so every client passes them.
    let metas = t.hook_metas(&l);
    assert_eq!(
        metas.iter().map(|m| (m.pubkey, m.is_writable)).collect::<Vec<_>>(),
        vec![(l.launch, true), (l.position, true), (t.coin_hook_key(&l.launch), true), (t.listing_key(&program), false), (program, false)]
    );
    assert_eq!(metas, t.hooked_metas(l.launch, l.position, program, false));

    // A coin without a hook can't get one later.
    let plain = t.launch(args(), TOKEN22);
    let creator = plain.creator.insecure_clone();
    let ix = t.attach_ix(&creator.pubkey(), plain.launch, plain.pk.pool, program, cfg.clone());
    assert_fails_with(t.env.send(&[ix], &creator, &[]), &launch_err(LaunchError::HookOnlyAtLaunch));
    assert!(!t.state(&plain).has_coin_hook());

    // Nor after the first trade, even in the launch transaction (here, after the dev buy).
    let creator = Keypair::new();
    t.env.svm.airdrop(&creator.pubkey(), 100 * SOL).unwrap();
    let mint = t.token_mint(&creator, TOKEN22);
    let (launch_ix, pk, launch, position) = t.launch_ix(&creator.pubkey(), &mint, args());
    let l2 = Launched { mint, launch, pk, position, creator: creator.insecure_clone() };
    let dev = User { kp: creator.insecure_clone(), t0: t.env.token_account(&pk.mint0, &creator.pubkey()), t1: t.env.token_account(&pk.mint1, &creator.pubkey()) };
    let dq = t.quote_acct(&l2, &dev);
    t.env.mint_to(&t.quote.clone(), &dq, 5 * SOL);
    let buy = t.swap_ix_with(&l2, &dev, true, -(SOL as i128), &[AccountMeta::new(launch, false), AccountMeta::new(position, false)]);
    let attach = t.attach_ix(&creator.pubkey(), launch, pk.pool, program, cfg.clone());
    assert_fails_with(t.env.send(&[launch_ix.clone(), buy, attach.clone()], &creator, &[]), &launch_err(LaunchError::HookOnlyAtLaunch));
    // And a coin has one hook: a second one in the same transaction fails.
    assert!(t.env.send(&[launch_ix, attach.clone(), attach], &creator, &[]).is_err());

    // Only the creator adds it.
    let creator = Keypair::new();
    let other = Keypair::new();
    t.env.svm.airdrop(&creator.pubkey(), 100 * SOL).unwrap();
    t.env.svm.airdrop(&other.pubkey(), 10 * SOL).unwrap();
    let mint = t.token_mint(&creator, TOKEN22);
    let (launch_ix, pk, launch, _) = t.launch_ix(&creator.pubkey(), &mint, args());
    let attach = t.attach_ix(&other.pubkey(), launch, pk.pool, program, cfg.clone());
    assert_fails_with(t.env.send(&[launch_ix, attach], &creator, &[&other]), &launch_err(LaunchError::Unauthorized));

    // The hook checks its settings (here: a sell tax at the coin's own fee), and settings stop at 64 bytes.
    let (_, _, res) = t.try_launch_with_hook(program, tax_cfg((0, 0), (FEE, 3_600)), None, 0);
    assert_fails_with(res, "InvalidArgument");
    let mock = t.add_hook("meridian_mock_hook");
    let (_, _, res) = t.try_launch_with_hook(mock, mock_cfg(coin_mode::REFUSE_ATTACH, 0), None, 0);
    assert_fails_with(res, MOCK_REVERT);
    let (_, _, res) = t.try_launch_with_hook(mock, vec![0; 65], None, 0);
    assert_fails_with(res, &launch_err(LaunchError::HookConfigTooLong));
    let (_, _, res) = t.try_launch_with_hook(mock, vec![0; 64], None, 0);
    ok(res);

    // A paused hook can't be added; only the admin pauses.
    let ix = t.pause_ix(&stranger.pubkey(), &program, true);
    assert_fails_with(t.env.send(&[ix], &stranger, &[]), &launch_err(LaunchError::Unauthorized));
    t.set_hook_paused(&program, true);
    let (_, _, res) = t.try_launch_with_hook(program, cfg.clone(), None, 0);
    assert_fails_with(res, &launch_err(LaunchError::HookNotListed));
    t.set_hook_paused(&program, false);

    // With fee zones too: the zones go first, and the published list carries both.
    let (z, _, res) = t.try_launch_with_hook(program, cfg.clone(), Some(curve_zones()), SOL);
    ok(res);
    let s = t.state(&z);
    assert!(s.has_coin_hook() && s.has_fee_zones());
    assert_eq!(t.hook_metas(&z), t.hooked_metas(z.launch, z.position, program, true));
    assert_eq!(t.listing(&program).coins, 2);
    let creator = Keypair::new();
    t.env.svm.airdrop(&creator.pubkey(), 100 * SOL).unwrap();
    let mint = t.token_mint(&creator, TOKEN22);
    let (launch_ix, pk, launch, _) = t.launch_ix(&creator.pubkey(), &mint, args());
    let attach = t.attach_ix(&creator.pubkey(), launch, pk.pool, program, cfg);
    let zones = t.set_zones_ix(&creator.pubkey(), launch, pk.pool, curve_zones());
    assert_fails_with(t.env.send(&[launch_ix, attach, zones], &creator, &[]), &launch_err(LaunchError::ZonesBeforeHook));
}

#[test]
fn launch_tax_starts_high_and_falls_to_the_coins_fee_on_the_curve() {
    let mut t = setup();
    let program = t.add_hook("stoke_hook_launch_tax");
    // Buys: 5% falling over 400 s. Sells: 10% falling over 900 s. The coin's own fee is 1%.
    let (l, dev) = t.launch_with_hook(program, tax_cfg((500, 400), (1_000, 900)), 20 * SOL);
    // The dev buy, in the launch transaction, paid the coin's fee.
    let s = t.state(&l);
    let allowance = s.dev_free_tokens;
    assert!(allowance > 0);
    assert_eq!(fee_buckets(&s), 20 * SOL / 100);

    let u = t.trader(&l, 2_000 * SOL);
    let ta = t.token_acct(&l, &u);
    // At launch: buys pay 5%, sells 10%, the creator included (their fee-free allowance stays).
    t.buy_paying(&l, &u, 10 * SOL, 500);
    t.sell_paying(&l, &u, t.env.balance(&ta) / 10, 1_000);
    t.sell_paying(&l, &dev, allowance / 4, 1_000);
    assert_eq!(t.state(&l).dev_free_tokens, allowance);
    // The creator's later buys pay like anyone's.
    t.buy_paying(&l, &dev, SOL, 500);

    // 100 s in: buys 4%, sells 9%.
    t.warp(100);
    t.buy_paying(&l, &u, SOL, 400);
    t.sell_paying(&l, &u, 1_000_000_000, 900);
    // 450 s in: the buy tax is over; sells are at 5.5%, exact-output sells included.
    t.warp(350);
    t.buy_paying(&l, &u, SOL, FEE);
    t.sell_paying(&l, &u, 1_000_000_000, 550);
    let s = t.state(&l);
    let want = s.curve().sell_exact_out(SOL / 10, 550).unwrap();
    ok(t.swap(&l, &u, false, (SOL / 10) as i128));
    assert_eq!(fee_buckets(&t.state(&l)) - fee_buckets(&s), want.fee);

    // 900 s in: the hook changes nothing, and the creator's allowance works as before.
    t.warp(450);
    t.buy_paying(&l, &u, SOL, FEE);
    t.sell_paying(&l, &u, 1_000_000_000, FEE);
    t.sell_paying(&l, &dev, allowance / 4, 0);
    assert_eq!(t.state(&l).dev_free_tokens, allowance - allowance / 4);
    t.check_books(&l);
}

#[test]
fn launch_tax_applies_in_the_pool_after_graduation() {
    let mut t = setup();
    let program = t.add_hook("stoke_hook_launch_tax");
    let week = 7 * 86_400;
    // Buys 8% and sells 10%, both falling over a week.
    let (l, _) = t.launch_with_hook(program, tax_cfg((800, week), (1_000, week)), 0);
    let whale = t.trader(&l, 3_000 * SOL);
    let (qa, ta) = (t.quote_acct(&l, &whale), t.token_acct(&l, &whale));
    // The buy that graduates the coin pays the 8% on both parts: the curve's and the pool's.
    let before = t.state(&l);
    ok(t.buy(&l, &whale, 500 * SOL));
    let s = t.state(&l);
    assert_eq!(s.status, Status::Graduated);
    // (The buckets also pick up graduation's rounding dust: a few thousand lamports.)
    let fee = fee_buckets(&s) - fee_buckets(&before);
    assert!((40 * SOL..40 * SOL + 10_000).contains(&fee), "{fee}");

    // At launch: sells 10%.
    t.pool_sell_paying(&l, &whale, t.env.balance(&ta) / 200, 1_000);

    // Half a week in: buys 4.5%, sells 5.5%, exact input and exact output.
    t.warp(week as i64 / 2);
    let s = t.state(&l);
    ok(t.buy(&l, &whale, SOL));
    assert_eq!(fee_buckets(&t.state(&l)) - fee_buckets(&s), SOL * 450 / 10_000);
    t.pool_sell_paying(&l, &whale, t.env.balance(&ta) / 500, 550);
    // Exact-output buy: the fee comes on top of what the pool takes.
    let (s, q0) = (t.state(&l), t.env.balance(&qa));
    ok(t.swap(&l, &whale, true, 1_000_000_000_000));
    let (fee, paid) = (fee_buckets(&t.state(&l)) - fee_buckets(&s), q0 - t.env.balance(&qa));
    assert_eq!(fee as u128, ((paid - fee) as u128 * 450).div_ceil(10_000 - 450));
    // Exact-output sell: the fee comes on top of the SOL asked for.
    let s = t.state(&l);
    ok(t.swap(&l, &whale, false, SOL as i128));
    assert_eq!((fee_buckets(&t.state(&l)) - fee_buckets(&s)) as u128, (SOL as u128 * 550).div_ceil(10_000 - 550));

    // A week in: the coin's own 1%.
    t.warp(week as i64 / 2);
    let s = t.state(&l);
    ok(t.buy(&l, &whale, SOL));
    assert_eq!(fee_buckets(&t.state(&l)) - fee_buckets(&s), SOL / 100);
    t.pool_sell_paying(&l, &whale, t.env.balance(&ta) / 500, FEE);
    t.check_books(&l);
}

#[test]
fn max_buy_refuses_bigger_buys_for_the_coins_first_minutes_only() {
    let mut t = setup();
    let program = t.add_hook("stoke_hook_max_buy");
    let cfg = max_buy::Settings { max_lamports: SOL, secs: 600 }.encode();
    let refused = anchor_err(max_buy::BUY_ABOVE_MAX);
    // Its settings are checked when it's added.
    let (_, _, res) = t.try_launch_with_hook(program, max_buy::Settings { max_lamports: SOL / 100, secs: 600 }.encode(), None, 0);
    assert_fails_with(res, "InvalidArgument");
    // It binds the creator's dev buy too: a bigger one fails the launch.
    let (_, _, res) = t.try_launch_with_hook(program, cfg.clone(), None, 2 * SOL);
    assert_fails_with(res, &refused);
    let (l, dev) = t.launch_with_hook(program, cfg, SOL);
    assert!(t.state(&l).dev_free_tokens > 0);

    let u = t.trader(&l, 500 * SOL);
    let ta = t.token_acct(&l, &u);
    // Up to the cap is fine, at the coin's own fee; a lamport more isn't.
    t.buy_paying(&l, &u, SOL, FEE);
    assert_fails_with(t.buy(&l, &u, SOL + 1), &refused);
    assert_fails_with(t.buy(&l, &dev, 3 * SOL), &refused);
    // While the cap lasts a buy spends an exact SOL amount: one that fixes the tokens instead
    // would pay whatever they cost, so it's refused, however small.
    let exact_out = anchor_err(max_buy::EXACT_OUT_BUY);
    assert_fails_with(t.swap(&l, &u, true, 1_000_000_000), &exact_out);
    assert_fails_with(t.swap(&l, &u, true, 50_000_000_000_000), &exact_out);
    // Buying again is fine: it caps one buy, not a wallet. Sells are never limited.
    t.buy_paying(&l, &u, SOL, FEE);
    t.sell_paying(&l, &u, t.env.balance(&ta), FEE);

    // Until the window ends, and not a second longer.
    t.warp(599);
    assert_fails_with(t.buy(&l, &u, 50 * SOL), &refused);
    t.warp(1);
    t.buy_paying(&l, &u, 50 * SOL, FEE);
    ok(t.swap(&l, &u, true, 1_000_000_000));
    t.check_books(&l);
}

#[test]
fn coins_hooked_on_the_first_release_keep_working() {
    // The first release's coin hook account had no `swap_pool_quote`: it was 8 bytes smaller and
    // ended at `bump`. Put a coin's account back in that shape and trade it everywhere.
    let mut t = setup();
    let program = t.add_hook("stoke_hook_launch_tax");
    let week = 7 * 86_400;
    let cfg = tax_cfg((800, week), (1_000, week));
    let (l, dev) = t.launch_with_hook(program, cfg.clone(), SOL);
    let key = t.coin_hook_key(&l.launch);
    let before = t.coin_hook(&l);
    let mut account = t.env.svm.get_account(&key).unwrap();
    let old_len = account.data.len() - 8;
    let used = 8 + 32 * 3 + 4 + cfg.len() + 2 + 8 + 1;
    account.data[used..].fill(0);
    account.data.truncate(old_len);
    t.env.svm.set_account(key, account).unwrap();

    let whale = t.trader(&l, 3_000 * SOL);
    let (qa, ta) = (t.quote_acct(&l, &whale), t.token_acct(&l, &whale));
    // On the curve: the hook's fee, both ways, and the creator pays it too.
    t.buy_paying(&l, &whale, 10 * SOL, 800);
    t.sell_paying(&l, &whale, 1_000_000_000, 1_000);
    t.sell_paying(&l, &dev, 1_000_000_000, 1_000);
    // Through graduation and in the pool, every kind of swap.
    ok(t.buy(&l, &whale, 500 * SOL));
    assert_eq!(t.state(&l).status, Status::Graduated);
    t.pool_sell_paying(&l, &whale, t.env.balance(&ta) / 200, 1_000);
    let s = t.state(&l);
    ok(t.buy(&l, &whale, SOL));
    assert_eq!(fee_buckets(&t.state(&l)) - fee_buckets(&s), SOL * 8 / 100);
    ok(t.swap(&l, &whale, true, 1_000_000_000_000));
    let (s, q0) = (t.state(&l), t.env.balance(&qa));
    ok(t.swap(&l, &whale, false, SOL as i128));
    assert_eq!(t.env.balance(&qa) - q0, SOL);
    assert_eq!((fee_buckets(&t.state(&l)) - fee_buckets(&s)) as u128, (SOL as u128 * 1_000).div_ceil(9_000));
    // A swap its price limit cuts short is refused for it too.
    let sp = t.env.state(&l.pk.pool).sqrt_price.get();
    let zero_for_one = l.pk.mint0 == t.quote;
    let p = SwapParams { zero_for_one, amount_specified: -(100 * SOL as i128), sqrt_price_limit: if zero_for_one { sp - sp / 2_000 } else { sp + sp / 2_000 } };
    let ix = t.env.swap_ix(&l.pk, &whale.key(), Some(whale.t0), Some(whale.t1), p, Settle::default(), 1, false, true, &t.hook_metas(&l), vec![]);
    let kp = whale.kp.insecure_clone();
    assert_fails_with(t.env.pay(&[ix], &[&kp]), &launch_err(LaunchError::SwapCutShort));
    // The account is still its old size, and nothing it held was disturbed.
    let after = t.coin_hook(&l);
    assert_eq!(t.env.svm.get_account(&key).unwrap().data.len(), old_len);
    assert_eq!((after.launch, after.program, after.listing, after.config, after.attached_at, after.bump), (before.launch, before.program, before.listing, before.config, before.attached_at, before.bump));
    t.check_books(&l);
}

#[test]
fn a_hooked_coins_fee_is_never_charged_on_sol_the_pool_didnt_trade() {
    let mut t = setup();
    let mock = t.add_hook("meridian_mock_hook");
    let (l, _) = t.launch_with_hook(mock, mock_cfg(coin_mode::FEE, 1_000), 0);
    let whale = t.trader(&l, 5_000 * SOL);
    ok(t.buy(&l, &whale, 600 * SOL));
    assert_eq!(t.state(&l).status, Status::Graduated);
    let cut_short = launch_err(LaunchError::SwapCutShort);
    let kp = whale.kp.insecure_clone();
    let sp = t.env.state(&l.pk.pool).sqrt_price.get();
    // A price limit 0.05% away: the pool can only trade a sliver of 100 SOL either way.
    for (is_buy, amount_specified, amount_limit) in [(true, -(100 * SOL as i128), 1), (false, 100 * SOL as i128, u64::MAX)] {
        let zero_for_one = (l.pk.mint0 == t.quote) == is_buy;
        let limit = if zero_for_one { sp - sp / 2_000 } else { sp + sp / 2_000 };
        let p = SwapParams { zero_for_one, amount_specified, sqrt_price_limit: limit };
        let ix = t.env.swap_ix(&l.pk, &whale.key(), Some(whale.t0), Some(whale.t1), p, Settle::default(), amount_limit, false, true, &t.hook_metas(&l), vec![]);
        assert_fails_with(t.env.pay(&[ix], &[&kp]), &cut_short);
    }
    // With the price left open, the same trades go through and pay the hook's 10% on what traded.
    let (s, qa) = (t.state(&l), t.quote_acct(&l, &whale));
    ok(t.buy(&l, &whale, 100 * SOL));
    assert_eq!(fee_buckets(&t.state(&l)) - fee_buckets(&s), 10 * SOL);
    let (s, q0) = (t.state(&l), t.env.balance(&qa));
    ok(t.swap(&l, &whale, false, 100 * SOL as i128));
    assert_eq!(t.env.balance(&qa) - q0, 100 * SOL);
    assert_eq!((fee_buckets(&t.state(&l)) - fee_buckets(&s)) as u128, (100 * SOL as u128 * 1_000).div_ceil(9_000));
    // Paused, the check still holds for the coin: nothing is charged on an unfilled amount.
    t.set_hook_paused(&mock, true);
    let zero_for_one = l.pk.mint0 == t.quote;
    let sp = t.env.state(&l.pk.pool).sqrt_price.get();
    let limit = if zero_for_one { sp - sp / 2_000 } else { sp + sp / 2_000 };
    let p = SwapParams { zero_for_one, amount_specified: -(100 * SOL as i128), sqrt_price_limit: limit };
    let ix = t.env.swap_ix(&l.pk, &whale.key(), Some(whale.t0), Some(whale.t1), p, Settle::default(), 1, false, true, &t.hook_metas(&l), vec![]);
    assert_fails_with(t.env.pay(&[ix], &[&kp]), &cut_short);
    t.check_books(&l);
}

#[test]
fn a_hook_sets_fees_up_to_ten_percent_and_a_broken_one_can_be_paused() {
    let mut t = setup();
    let mock = t.add_hook("meridian_mock_hook");

    // A hook asking for 25% gets 10%, on buys and sells.
    let (l, _) = t.launch_with_hook(mock, mock_cfg(coin_mode::FEE, 2_500), 0);
    let u = t.trader(&l, 500 * SOL);
    t.buy_paying(&l, &u, 10 * SOL, MAX_COIN_HOOK_FEE_BPS);
    t.sell_paying(&l, &u, 1_000_000_000, MAX_COIN_HOOK_FEE_BPS);
    t.check_books(&l);

    // A hook can also make trades free.
    let (l, _) = t.launch_with_hook(mock, mock_cfg(coin_mode::FEE, 0), 0);
    let u = t.trader(&l, 500 * SOL);
    t.buy_paying(&l, &u, 10 * SOL, 0);
    t.sell_paying(&l, &u, 1_000_000_000, 0);

    // One that returns nothing leaves trades as they are.
    let (l, _) = t.launch_with_hook(mock, mock_cfg(coin_mode::SILENT, 0), 0);
    let u = t.trader(&l, 500 * SOL);
    t.buy_paying(&l, &u, 10 * SOL, FEE);
    t.sell_paying(&l, &u, 1_000_000_000, FEE);

    // In the anti-snipe window, a hook's buy fee stops where the buyer would be left nothing.
    let (l, _, res) = t.try_launch_with_hook(mock, mock_cfg(coin_mode::FEE, 1_000), None, 0);
    ok(res);
    let u = t.trader(&l, 500 * SOL);
    let s = t.state(&l);
    let want = s.curve().buy_exact_in(SOL, 9_900 - SNIPE, SNIPE);
    assert!(want.tokens > 0);
    ok(t.buy(&l, &u, SOL));
    assert_eq!(fee_buckets(&t.state(&l)) - fee_buckets(&s), want.fee);
    assert_eq!(t.env.balance(&t.token_acct(&l, &u)), want.tokens);

    // A hook that fails refuses every trade...
    let broken = t.add_hook("meridian_mock_hook");
    let (l, _) = t.launch_with_hook(broken, mock_cfg(coin_mode::REVERT, 0), 0);
    let u = t.trader(&l, 500 * SOL);
    assert_fails_with(t.buy(&l, &u, SOL), MOCK_REVERT);
    // ...and one that returns nonsense fails them too.
    let (g, _) = t.launch_with_hook(broken, mock_cfg(coin_mode::GARBAGE, 0), 0);
    let gu = t.trader(&g, 500 * SOL);
    assert_fails_with(t.buy(&g, &gu, SOL), &launch_err(LaunchError::BadHookReturn));
    // Paused, its coins trade as if they had no hook: nobody is locked in.
    t.set_hook_paused(&broken, true);
    t.buy_paying(&l, &u, 10 * SOL, FEE);
    t.sell_paying(&l, &u, 1_000_000_000, FEE);
    t.buy_paying(&g, &gu, 10 * SOL, FEE);
    // The program account isn't needed while it's paused.
    let metas = t.hook_metas(&l);
    let kp = u.kp.insecure_clone();
    let sell = t.swap_ix_with(&l, &u, false, -1_000_000_000, &metas[..metas.len() - 1]);
    ok(t.env.pay(&[sell], &[&kp]));
    // Unpaused, the hook is back.
    t.set_hook_paused(&broken, false);
    assert_fails_with(t.sell(&l, &u, 1_000_000_000), MOCK_REVERT);
    // Other hooks were never affected.
    let (l, _) = t.launch_with_hook(mock, mock_cfg(coin_mode::NONE, 0), 0);
    let u = t.trader(&l, 10 * SOL);
    t.buy_paying(&l, &u, SOL, FEE);
}

#[test]
fn a_sell_in_a_fee_zone_pays_the_higher_of_the_zone_and_the_hook() {
    let mut t = setup();
    let mock = t.add_hook("meridian_mock_hook");
    // The hook sets 5% on everything; zones are 4% · 7% · 10%.
    let (l, _, res) = t.try_launch_with_hook(mock, mock_cfg(coin_mode::FEE, 500), Some(curve_zones()), 0);
    ok(res);
    t.warp_slots(SNIPE_SLOTS as u64);
    let u = t.trader(&l, 2_000 * SOL);
    // Below the zones: the hook's 5%, both ways.
    t.buy_paying(&l, &u, 50 * SOL, 500);
    t.sell_paying(&l, &u, 1_000_000_000, 500);
    // Zone 1 (4%): the hook's 5% is higher.
    t.push_curve_to(&l, &u, 720);
    assert!((600 * SOL..800 * SOL).contains(&t.mcap(&l)), "{}", t.mcap(&l));
    t.sell_paying(&l, &u, 1_000_000_000, 500);
    // Zone 2 (7%) and zone 3 (10%): the zone's.
    t.push_curve_to(&l, &u, 920);
    t.sell_paying(&l, &u, 1_000_000_000, 700);
    t.push_curve_to(&l, &u, 1_120);
    t.sell_paying(&l, &u, 1_000_000_000, 1_000);
    // Buys only ever pay the hook's.
    t.buy_paying(&l, &u, SOL, 500);
    // Exact-output sells stay refused while the zones are on.
    assert_fails_with(t.swap(&l, &u, false, (SOL / 10) as i128), &launch_err(LaunchError::ExactOutSellInZones));
    t.check_books(&l);
}

#[test]
fn hooked_coins_refuse_swaps_without_their_hook_accounts() {
    let mut t = setup();
    let mock = t.add_hook("meridian_mock_hook");
    let (l, _) = t.launch_with_hook(mock, mock_cfg(coin_mode::FEE, 300), 0);
    let (other, _) = t.launch_with_hook(mock, mock_cfg(coin_mode::FEE, 300), 0);
    let u = t.trader(&l, 100 * SOL);
    t.buy_paying(&l, &u, 10 * SOL, 300);
    let missing = launch_err(LaunchError::HookAccountsMissing);
    let kp = u.kp.insecure_clone();
    let full = t.hook_metas(&l);
    let (base, hook, listing, program) = (full[..2].to_vec(), full[2].clone(), full[3].clone(), full[4].clone());
    let cases: Vec<Vec<AccountMeta>> = vec![
        base.clone(),                                                                                       // the old two-account list
        [base.clone(), vec![listing.clone(), program.clone()]].concat(),                                    // no coin hook account
        [base.clone(), vec![AccountMeta::new_readonly(hook.pubkey, false), listing.clone(), program.clone()]].concat(), // read-only: it couldn't record the fee
        [base.clone(), vec![AccountMeta::new(t.coin_hook_key(&other.launch), false), listing.clone(), program.clone()]].concat(), // another coin's
        [base.clone(), vec![hook.clone(), program.clone()]].concat(),                                       // no listing
        [base.clone(), vec![hook.clone(), listing.clone()]].concat(),                                       // no program
    ];
    for metas in cases {
        for (is_buy, amount) in [(true, -(SOL as i128)), (false, -1_000_000_000)] {
            let ix = t.swap_ix_with(&l, &u, is_buy, amount, &metas);
            assert_fails_with(t.env.pay(&[ix], &[&kp]), &missing);
        }
    }
    // With all of them, the hook's in any order after the launch account: fine.
    let shuffled = vec![base[0].clone(), program, listing, hook, base[1].clone()];
    let ix = t.swap_ix_with(&l, &u, false, -1_000_000_000, &shuffled);
    ok(t.env.pay(&[ix], &[&kp]));
    // A coin without a hook ignores the extra accounts and trades as before.
    let plain = t.launch(args(), TOKEN22);
    let p = t.trader(&plain, 10 * SOL);
    let pkp = p.kp.insecure_clone();
    let mut metas = t.hook_metas(&plain);
    metas.extend(full[2..].iter().cloned());
    let s = t.state(&plain);
    let ix = t.swap_ix_with(&plain, &p, true, -(SOL as i128), &metas);
    ok(t.env.pay(&[ix], &[&pkp]));
    assert_eq!(fee_buckets(&t.state(&plain)) - fee_buckets(&s), SOL / 100);
}

#[test]
fn the_hook_is_told_about_each_trade_and_gets_nothing_but_the_call() {
    let mut t = setup();
    let mock = t.add_hook("meridian_mock_hook");
    let cfg = [mock_cfg(coin_mode::NONE, 0), vec![9, 8, 7]].concat();

    // When it's added: the coin and the creator's settings.
    let (l, dev, res) = t.try_launch_with_hook(mock, cfg.clone(), None, SOL);
    let meta = ok(res);
    let calls = mock_calls(&meta.logs);
    assert_eq!(calls.len(), 2, "on_attach, then on_trade for the dev buy");
    let s = t.state(&l);
    let coin = CoinInfo { mint: l.mint, creator: dev.key(), fee_bps: FEE, created_at: s.created_at, created_slot: s.created_slot };
    assert_eq!(AttachArgs::parse(&calls[0].0), Some(AttachArgs { coin: coin.clone(), config: cfg.clone() }));
    // One account, the listing: a signer it can't write to.
    assert_eq!(calls[0].1, vec![1, 1, 0]);
    let dev_buy = TradeArgs::parse(&calls[1].0).unwrap();
    assert!(dev_buy.is_dev_buy() && dev_buy.sol == SOL);
    t.warp(30);

    // A buy of SOL: the SOL is exact, the tokens are valued at the price before the trade.
    let u = t.trader(&l, 100 * SOL);
    let (m, clock_now, slot) = (t.mcap(&l), t.now(), t.slot());
    let meta = ok(t.buy(&l, &u, 3 * SOL));
    let calls = mock_calls(&meta.logs);
    assert_eq!(calls.len(), 1);
    let want = TradeArgs {
        coin: coin.clone(),
        config: cfg.clone(),
        trader: u.key(),
        is_buy: true,
        exact_in: true,
        sol: 3 * SOL,
        tokens: (3 * SOL as u128 * TOTAL_SUPPLY as u128 / m as u128) as u64,
        graduated: false,
        mcap: m,
        now: clock_now,
        slot,
    };
    assert_eq!(TradeArgs::parse(&calls[0].0), Some(want.clone()));
    assert_eq!(borsh::from_slice::<TradeArgs>(&calls[0].0).unwrap(), want);
    assert!(want.sol_is_exact() && want.age() == 30 && !want.is_dev_buy());
    assert_eq!(calls[0].1, vec![1, 1, 0]);

    // A sell of tokens: the tokens are exact, the SOL is their value at the price before.
    let m = t.mcap(&l);
    let meta = ok(t.sell(&l, &u, 5_000_000_000));
    let got = TradeArgs::parse(&mock_calls(&meta.logs)[0].0).unwrap();
    assert_eq!((got.is_buy, got.exact_in, got.tokens, got.sol, got.mcap), (false, true, 5_000_000_000, (5_000_000_000u128 * m as u128 / TOTAL_SUPPLY as u128) as u64, m));
    assert!(!got.sol_is_exact());

    // In the pool after graduation: the same, with `graduated` set.
    let whale = t.trader(&l, 1_000 * SOL);
    ok(t.buy(&l, &whale, 450 * SOL));
    assert_eq!(t.state(&l).status, Status::Graduated);
    let m = t.mcap(&l);
    let meta = ok(t.swap(&l, &whale, true, 2_000_000_000_000));
    let got = TradeArgs::parse(&mock_calls(&meta.logs)[0].0).unwrap();
    assert_eq!((got.graduated, got.is_buy, got.exact_in, got.tokens, got.mcap), (true, true, false, 2_000_000_000_000, m));
    assert_eq!(got.sol, (2_000_000_000_000u128 * m as u128 / TOTAL_SUPPLY as u128) as u64);
}

#[test]
fn only_stoke_launch_can_call_a_coin_hook() {
    let mut t = setup();
    for so in ["stoke_hook_max_buy", "stoke_hook_launch_tax", "meridian_mock_hook"] {
        let program = t.add_hook(so);
        let data = encode_coin_hook_call(meridian_core::coin_hook::discriminator::ON_TRADE, &TradeArgs::default());
        // Any signer that isn't the hook's listing.
        let stranger = Keypair::new();
        t.env.svm.airdrop(&stranger.pubkey(), SOL).unwrap();
        let ix = Instruction { program_id: program, accounts: vec![AccountMeta::new_readonly(stranger.pubkey(), true)], data: data.clone() };
        assert_fails_with(t.env.pay(&[ix], &[&stranger]), "IllegalOwner");
        // The listing itself, which nobody but Stoke Launch can sign with.
        let ix = Instruction { program_id: program, accounts: vec![AccountMeta::new_readonly(t.listing_key(&program), false)], data: data.clone() };
        assert_fails_with(t.env.pay(&[ix], &[]), "MissingRequiredSignature");
        // Another hook's listing, even signed for: a keypair account made to look owned by Stoke Launch.
        let other = t.add_hook(so);
        let forged = Keypair::new();
        let mut account = t.env.svm.get_account(&t.listing_key(&other)).unwrap();
        account.lamports = SOL;
        t.env.svm.set_account(forged.pubkey(), account).unwrap();
        let ix = Instruction { program_id: program, accounts: vec![AccountMeta::new_readonly(forged.pubkey(), true)], data };
        assert_fails_with(t.env.pay(&[ix], &[&forged]), "InvalidAccountData");
    }
}

#[test]
fn a_hook_costs_a_trade_little_compute() {
    let mut t = setup();
    let program = t.add_hook("stoke_hook_launch_tax");
    let (hooked, _) = t.launch_with_hook(program, tax_cfg((500, 400), (1_000, 900)), 0);
    let plain = t.launch(args(), TOKEN22);
    let (hu, pu) = (t.trader(&hooked, 10 * SOL), t.trader(&plain, 10 * SOL));
    let with = ok(t.buy(&hooked, &hu, SOL)).compute_units_consumed;
    let without = ok(t.buy(&plain, &pu, SOL)).compute_units_consumed;
    println!("buy: {without} CU without a hook, {with} with one");
    assert!(with < without + 40_000, "{with} vs {without}");
    let with = ok(t.sell(&hooked, &hu, 1_000_000_000)).compute_units_consumed;
    let without = ok(t.sell(&plain, &pu, 1_000_000_000)).compute_units_consumed;
    println!("sell: {without} CU without a hook, {with} with one");
    assert!(with < without + 40_000, "{with} vs {without}");
}

// ---------------------------------------------------------------------------------- off-chain quotes

/// A tiny deterministic generator, so a failing run can be replayed.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

impl L {
    /// Stoke's two hooks at the addresses the quote crate knows them by, listed.
    fn add_stoke_hooks(&mut self) {
        let admin = self.env.admin.insecure_clone();
        for (id, so) in [(stoke_quote::HOOK_LAUNCH_TAX_ID, "stoke_hook_launch_tax"), (stoke_quote::HOOK_MAX_BUY_ID, "stoke_hook_max_buy")] {
            self.env.svm.add_program_from_file(id, format!("{DEPLOY}{so}.so")).unwrap();
            self.env.set_upgrade_authority(&id, Some(admin.pubkey()));
            let ix = self.list_hook_ix(&admin.pubkey(), &id);
            ok(self.env.send(&[ix], &admin, &[]));
        }
    }

    /// The coin as the quote crate reads it: straight from its accounts.
    fn market(&self, l: &Launched) -> stoke_quote::Market {
        let data = |k: &Pubkey| self.env.svm.get_account(k).unwrap().data;
        let launch = stoke_quote::decode_launch(&data(&l.launch)).unwrap();
        let zones = launch.has_fee_zones().then(|| stoke_quote::decode_zones(&data(&stoke_quote::zones_address(&l.launch))).unwrap());
        let hook = launch.has_coin_hook().then(|| {
            let state = stoke_quote::decode_coin_hook(&data(&stoke_quote::coin_hook_address(&l.launch))).unwrap();
            let paused = stoke_quote::decode_listing(&data(&state.listing)).unwrap().paused;
            stoke_quote::Hook { state, paused }
        });
        stoke_quote::Market { pool_key: l.pk.pool, pool: stoke_quote::decode_pool(&data(&l.pk.pool)).unwrap(), ticks: data(&l.pk.ticks), launch_key: l.launch, launch, zones, hook }
    }

    fn quote(&self, l: &Launched, u: &User, is_buy: bool, amount: u64) -> Result<stoke_quote::TradeQuote, stoke_quote::QuoteError> {
        let clock: anchor_lang::prelude::Clock = self.env.svm.get_sysvar();
        self.market(l).quote(is_buy, amount, &u.key(), &stoke_quote::Clock { slot: clock.slot, unix_timestamp: clock.unix_timestamp })
    }

    /// Quote an exact-input trade, run it, and check the trader received exactly the quote, and the
    /// coin took exactly the fee quoted. A trade the quote refuses has to fail on-chain too.
    /// Returns what was received (0 when the trade was refused).
    #[track_caller]
    fn assert_quoted(&mut self, l: &Launched, u: &User, is_buy: bool, amount: u64) -> u64 {
        let out_acct = if is_buy { self.token_acct(l, u) } else { self.quote_acct(l, u) };
        let (before, state) = (self.env.balance(&out_acct), self.state(l));
        let quote = self.quote(l, u, is_buy, amount);
        let res = self.swap(l, u, is_buy, -(amount as i128));
        match quote {
            Ok(q) => {
                ok(res);
                let got = self.env.balance(&out_acct) - before;
                assert_eq!(got, q.amount_out, "{} of {amount}: quoted {} out", if is_buy { "buy" } else { "sell" }, q.amount_out);
                let after = self.state(l);
                // The fee buckets also pick up graduation's rounding dust, in the swap that graduates.
                if state.status == after.status {
                    assert_eq!(fee_buckets(&after) - fee_buckets(&state), q.fee, "fee of a {} of {amount}", if is_buy { "buy" } else { "sell" });
                }
                got
            }
            Err(e) => {
                assert!(res.is_err(), "the quote refused a {} of {amount} ({e}), but it went through", if is_buy { "buy" } else { "sell" });
                0
            }
        }
    }

    /// Random buys and sells of all sizes, each one quoted first. Buys lean bigger so the coin runs.
    #[track_caller]
    fn trade_at_random(&mut self, l: &Launched, u: &User, rng: &mut Rng, trades: usize) {
        let ta = self.token_acct(l, u);
        for _ in 0..trades {
            let held = self.env.balance(&ta);
            if held > 0 && rng.below(5) < 2 {
                // From a single token unit to the whole bag.
                let tokens = match rng.below(4) {
                    0 => 1 + rng.below(1_000),
                    1 => held,
                    _ => 1 + rng.below(held),
                };
                self.assert_quoted(l, u, false, tokens.min(held));
            } else {
                let sol = match rng.below(6) {
                    0 => 1 + rng.below(5_000),          // dust
                    1 => SOL / 100 + rng.below(SOL),
                    5 => 20 * SOL + rng.below(60 * SOL), // a whale
                    _ => SOL / 10 + rng.below(8 * SOL),
                };
                self.assert_quoted(l, u, true, sol);
            }
            if rng.below(7) == 0 {
                self.warp(rng.below(240) as i64);
            }
        }
    }
}

#[test]
fn quotes_match_a_plain_coin_from_its_first_second_to_its_pool() {
    let mut t = setup();
    // Inside the anti-snipe window: the tax is part of the quote.
    let creator = Keypair::new();
    t.env.svm.airdrop(&creator.pubkey(), 100 * SOL).unwrap();
    let mint = t.token_mint(&creator, TOKEN22);
    let (ix, pk, launch, position) = t.launch_ix(&creator.pubkey(), &mint, args());
    ok(t.env.send(&[ix], &creator, &[]));
    let l = Launched { mint, launch, pk, position, creator: creator.insecure_clone() };
    let sniper = t.trader(&l, 20 * SOL);
    let got = t.assert_quoted(&l, &sniper, true, SOL);
    assert!(got > 0 && t.state(&l).burn_pending > got * 8, "the 90% tax bought tokens to burn");
    t.warp_slots(SNIPE_SLOTS as u64);

    // The creator's fee-free sells, up to and past their allowance.
    let (l, dev) = t.launch_with_dev_buy(args(), 5 * SOL);
    t.warp_slots(SNIPE_SLOTS as u64);
    let allowance = t.state(&l).dev_free_tokens;
    assert_eq!(t.quote(&l, &dev, false, allowance / 3).unwrap().fee, 0);
    t.assert_quoted(&l, &dev, false, allowance / 3);
    // They buy more, then sell everything: more than the allowance left, so only the part above it pays.
    t.assert_quoted(&l, &dev, true, 3 * SOL);
    let held = t.env.balance(&t.token_acct(&l, &dev));
    let q = t.quote(&l, &dev, false, held).unwrap();
    assert!(held > allowance && q.fee > 0 && q.fee_bps < FEE, "{} bps", q.fee_bps);
    t.assert_quoted(&l, &dev, false, held);

    // Everything else, at random, through graduation and on in the pool.
    let u = t.trader(&l, 5_000 * SOL);
    let mut rng = Rng(7);
    t.trade_at_random(&l, &u, &mut rng, 60);
    assert_eq!(t.state(&l).status, Status::Curve, "still on the curve");
    // The buy that graduates the coin: the curve's part, the new liquidity, and the pool's part.
    let left = R - t.state(&l).real_quote;
    let got = t.assert_quoted(&l, &u, true, left + 30 * SOL);
    assert!(got > 0);
    assert_eq!(t.state(&l).status, Status::Graduated);
    t.trade_at_random(&l, &u, &mut rng, 80);
    t.check_books(&l);

    // A buy that lands exactly on the curve's end, give or take a lamport.
    for extra in [0u64, 1, 2, 1_000] {
        let l = t.launch(args(), TOKEN22);
        let u = t.trader(&l, 1_000 * SOL);
        let s = t.state(&l);
        let fill = s.curve().buy_exact_in(1_000 * SOL, FEE, 0);
        assert!(fill.completes);
        t.assert_quoted(&l, &u, true, fill.quote - 1);
        t.assert_quoted(&l, &u, true, 1 + extra);
        t.assert_quoted(&l, &u, true, SOL);
    }
}

#[test]
fn quotes_match_coins_with_fee_zones() {
    let mut t = setup();
    // Zones on the curve, and above graduation.
    for zones in [curve_zones(), vec![zone(1_600, 2_000, 500), zone(2_000, 2_400, 800)]] {
        let (l, _) = t.launch_with_zones(zones, 0);
        let u = t.trader(&l, 6_000 * SOL);
        let mut rng = Rng(21);
        t.warp(1);
        t.trade_at_random(&l, &u, &mut rng, 90);
        // Run it up through the zones and into the pool, selling on the way.
        while t.state(&l).status == Status::Curve {
            t.assert_quoted(&l, &u, true, 60 * SOL);
            let held = t.env.balance(&t.token_acct(&l, &u));
            t.assert_quoted(&l, &u, false, held / 40);
        }
        t.trade_at_random(&l, &u, &mut rng, 60);
        // Hold above the top zone until they switch off, then sell back through them.
        t.push_pool_to(&l, &u, 2_600);
        t.assert_quoted(&l, &u, true, SOL);
        t.warp(ZONES_OFF_AFTER_SECS + 1);
        t.assert_quoted(&l, &u, true, SOL);
        let held = t.env.balance(&t.token_acct(&l, &u));
        t.assert_quoted(&l, &u, false, held / 2);
        t.trade_at_random(&l, &u, &mut rng, 30);
        t.check_books(&l);
    }
}

#[test]
fn quotes_match_coins_with_stokes_hooks_and_refuse_the_rest() {
    let mut t = setup();
    t.add_stoke_hooks();

    // Launch tax, falling as the coin ages, on the curve and in the pool.
    let (l, _) = t.launch_with_hook(stoke_quote::HOOK_LAUNCH_TAX_ID, tax_cfg((800, 1_800), (1_000, 7_200)), SOL);
    let u = t.trader(&l, 6_000 * SOL);
    let mut rng = Rng(99);
    assert_eq!(t.quote(&l, &u, true, SOL).unwrap().fee_bps, 800);
    t.trade_at_random(&l, &u, &mut rng, 80);
    while t.state(&l).status == Status::Curve {
        t.assert_quoted(&l, &u, true, 70 * SOL);
    }
    t.trade_at_random(&l, &u, &mut rng, 60);
    t.warp(7_200);
    assert_eq!(t.quote(&l, &u, true, SOL).unwrap().fee_bps, FEE, "the tax has run its course");
    t.trade_at_random(&l, &u, &mut rng, 20);
    t.check_books(&l);

    // Max buy: what the hook refuses, the quote refuses.
    let (l, _) = t.launch_with_hook(stoke_quote::HOOK_MAX_BUY_ID, max_buy::Settings { max_lamports: 2 * SOL, secs: 600 }.encode(), SOL);
    let u = t.trader(&l, 500 * SOL);
    assert_eq!(t.quote(&l, &u, true, 2 * SOL + 1), Err(stoke_quote::QuoteError::HookRefuses(max_buy::BUY_ABOVE_MAX)));
    t.trade_at_random(&l, &u, &mut rng, 60);
    t.warp(600);
    t.assert_quoted(&l, &u, true, 40 * SOL);

    // A paused hook isn't asked.
    let (l, _) = t.launch_with_hook(stoke_quote::HOOK_LAUNCH_TAX_ID, tax_cfg((1_000, 86_400), (1_000, 86_400)), 0);
    let u = t.trader(&l, 100 * SOL);
    assert_eq!(t.quote(&l, &u, true, SOL).unwrap().fee_bps, 1_000);
    t.set_hook_paused(&stoke_quote::HOOK_LAUNCH_TAX_ID, true);
    assert_eq!(t.quote(&l, &u, true, SOL).unwrap().fee_bps, FEE);
    t.trade_at_random(&l, &u, &mut rng, 20);
    t.set_hook_paused(&stoke_quote::HOOK_LAUNCH_TAX_ID, false);

    // A zoned coin with a hook: the higher of the two on a sell.
    let (l, _, res) = t.try_launch_with_hook(stoke_quote::HOOK_LAUNCH_TAX_ID, tax_cfg((0, 0), (500, 86_400)), Some(curve_zones()), 0);
    ok(res);
    t.warp_slots(SNIPE_SLOTS as u64);
    let u = t.trader(&l, 3_000 * SOL);
    t.trade_at_random(&l, &u, &mut rng, 40);
    t.push_curve_to(&l, &u, 1_100);
    let held = t.env.balance(&t.token_acct(&l, &u));
    assert_eq!(t.quote(&l, &u, false, held / 50).unwrap().fee_bps, 1_000);
    t.trade_at_random(&l, &u, &mut rng, 40);

    // Somebody else's hook: only its own program knows what it does, so there's no quote.
    let mock = t.add_hook("meridian_mock_hook");
    let (l, _) = t.launch_with_hook(mock, mock_cfg(coin_mode::FEE, 300), 0);
    let u = t.trader(&l, 10 * SOL);
    assert_eq!(t.quote(&l, &u, true, SOL), Err(stoke_quote::QuoteError::UnknownHook(mock)));
    assert_eq!(t.market(&l).quotable(), Err(stoke_quote::QuoteError::UnknownHook(mock)));
    // Paused, the coin is a plain coin again, and quotable.
    t.set_hook_paused(&mock, true);
    t.assert_quoted(&l, &u, true, SOL);
}

// ---------------------------------------------------------------------------------- Jupiter's test kit

impl L {
    /// Snapshot a coin the way Jupiter's test kit keeps one (a folder of `<address>.bin`, each a
    /// bincode `Account`), with every account its quote and its swap read, the two mints and the
    /// clock, plus `coin.json` saying which pool and mint it is.
    fn snapshot_for_jupiter(&self, l: &Launched, name: &str, about: &str) {
        use std::{collections::BTreeSet, fs, path::PathBuf};
        let dir = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../jupiter/tests/fixtures/local")).join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let m = self.market(l);
        let published = decode_hook_accounts(&self.env.svm.get_account(&stoke_quote::hook_accounts_address(&l.pk.pool)).unwrap().data).unwrap();
        let nobody = Pubkey::new_unique();
        let mut keys: BTreeSet<Pubkey> = stoke_quote::Market::accounts(&l.pk.pool, &m.pool, &l.mint).into_iter().collect();
        keys.extend(m.swap_accounts(&nobody, &nobody, &nobody, &published).iter().map(|a| a.pubkey));
        keys.extend([l.mint, self.quote, anchor_lang::pubkey!("SysvarC1ock11111111111111111111111111111111")]);
        keys.remove(&nobody);
        for key in keys {
            let Some(a) = self.env.svm.get_account(&key).filter(|a| !a.executable) else { continue };
            let mut bin = Vec::with_capacity(57 + a.data.len());
            bin.extend_from_slice(&a.lamports.to_le_bytes());
            bin.extend_from_slice(&(a.data.len() as u64).to_le_bytes());
            bin.extend_from_slice(&a.data);
            bin.extend_from_slice(a.owner.as_ref());
            bin.push(a.executable as u8);
            bin.extend_from_slice(&a.rent_epoch.to_le_bytes());
            fs::write(dir.join(format!("{key}.bin")), bin).unwrap();
        }
        fs::write(dir.join("coin.json"), format!("{{\n  \"pool\": \"{}\",\n  \"mint\": \"{}\",\n  \"about\": \"{about}\"\n}}\n", l.pk.pool, l.mint)).unwrap();
    }
}

/// Builds the coins mainnet doesn't have yet (graduated, with fee zones, with a hook) and
/// snapshots each for Jupiter's test kit, which then checks `stoke-amm`'s quotes against them
/// (`protocol/jupiter/tests/local.rs`). Priced in wrapped SOL itself, like mainnet.
///
///   cargo test -p meridian_pool_manager --test launch jupiter_fixtures -- --ignored
#[test]
#[ignore = "rewrites protocol/jupiter/tests/fixtures/local"]
fn jupiter_fixtures() {
    let whale = 4_000 * SOL;

    // Inside the anti-snipe window: launched one slot ago.
    let mut t = setup_quoted(true);
    assert_eq!(t.quote, stoke_quote::WSOL_MINT);
    let creator = Keypair::new();
    t.env.svm.airdrop(&creator.pubkey(), 100 * SOL).unwrap();
    let mint = t.token_mint(&creator, TOKEN22);
    let (ix, pk, launch, position) = t.launch_ix(&creator.pubkey(), &mint, args());
    ok(t.env.send(&[ix], &creator, &[]));
    let l = Launched { mint, launch, pk, position, creator };
    t.warp_slots(1);
    assert!(t.state(&l).snipe_bps(t.slot()) > 0);
    t.snapshot_for_jupiter(&l, "first-second", "Launched one slot ago: buys still pay the anti-snipe tax (90% of the buy goes to tokens that are burned).");

    // In its pool.
    let l = t.launch(args(), TOKEN22);
    let u = t.trader(&l, whale);
    ok(t.buy(&l, &u, 460 * SOL));
    assert_eq!(t.state(&l).status, Status::Graduated);
    ok(t.buy(&l, &u, 35 * SOL));
    let held = t.env.balance(&t.token_acct(&l, &u));
    ok(t.sell(&l, &u, held / 7));
    t.check_books(&l);
    t.snapshot_for_jupiter(&l, "graduated", "Sold out its curve and trades in its pool, at its 1% fee.");

    // Fee zones on the curve, the price inside the top one.
    let (l, _) = t.launch_with_zones(curve_zones(), 0);
    let u = t.trader(&l, whale);
    t.warp(1);
    t.push_curve_to(&l, &u, 1_100);
    t.snapshot_for_jupiter(&l, "zones-on-the-curve", "Fee zones on the curve (sells pay 4% at 600-800 SOL of market cap, 7% at 800-1,000, 10% at 1,000-1,200), the price at about 1,100.");

    // Fee zones above graduation, the price inside the top one, in the pool.
    let (l, _) = t.launch_with_zones(vec![zone(1_600, 2_000, 500), zone(2_000, 2_400, 800)], 0);
    let u = t.trader(&l, whale);
    t.warp(1);
    ok(t.buy(&l, &u, 460 * SOL));
    assert_eq!(t.state(&l).status, Status::Graduated);
    t.push_pool_to(&l, &u, 2_200);
    t.snapshot_for_jupiter(&l, "zones-in-the-pool", "Graduated, with fee zones in the pool (sells pay 5% at 1,600-2,000 SOL of market cap, 8% at 2,000-2,400), the price at about 2,200.");

    // Stoke's Launch tax, part-way down, on the curve and then in the pool.
    let mut t = setup_quoted(true);
    t.add_stoke_hooks();
    let (l, _) = t.launch_with_hook(stoke_quote::HOOK_LAUNCH_TAX_ID, tax_cfg((800, 1_800), (1_000, 7_200)), SOL);
    let u = t.trader(&l, whale);
    ok(t.buy(&l, &u, 12 * SOL));
    t.warp(600);
    let bps = t.quote(&l, &u, true, SOL).unwrap().fee_bps;
    assert!(bps > FEE && bps < 800, "{bps}");
    t.snapshot_for_jupiter(&l, "launch-tax", "Stoke's Launch tax hook, 10 minutes in: buys started at 8% and fall to the coin's 1% over 30 minutes, sells started at 10% and fall over 2 hours.");
    ok(t.buy(&l, &u, 460 * SOL));
    assert_eq!(t.state(&l).status, Status::Graduated);
    t.warp(300);
    t.snapshot_for_jupiter(&l, "launch-tax-in-the-pool", "The same coin 5 minutes later, graduated: the Launch tax carries on in the pool.");

    // Stoke's Max buy, inside its window.
    let (l, _) = t.launch_with_hook(stoke_quote::HOOK_MAX_BUY_ID, max_buy::Settings { max_lamports: 2 * SOL, secs: 600 }.encode(), SOL);
    let u = t.trader(&l, whale);
    ok(t.buy(&l, &u, 2 * SOL));
    t.warp(60);
    t.snapshot_for_jupiter(&l, "max-buy", "Stoke's Max buy hook, 1 minute in: no single buy above 2 SOL for the coin's first 10 minutes.");

    // Fee zones and a hook: a sell pays the higher of the two.
    let (l, _, res) = t.try_launch_with_hook(stoke_quote::HOOK_LAUNCH_TAX_ID, tax_cfg((0, 0), (500, 86_400)), Some(curve_zones()), 0);
    ok(res);
    t.warp_slots(SNIPE_SLOTS as u64);
    let u = t.trader(&l, whale);
    t.push_curve_to(&l, &u, 1_100);
    t.snapshot_for_jupiter(&l, "zones-and-a-hook", "Fee zones on the curve and the Launch tax hook on sells (5% falling over a day), the price at about 1,100 SOL of market cap: a sell pays the higher of the zone and the hook.");

    // A hook Stoke has paused: the coin trades as a plain coin.
    let mut t = setup_quoted(true);
    t.add_stoke_hooks();
    let (l, _) = t.launch_with_hook(stoke_quote::HOOK_LAUNCH_TAX_ID, tax_cfg((1_000, 86_400), (1_000, 86_400)), SOL);
    let u = t.trader(&l, whale);
    ok(t.buy(&l, &u, 20 * SOL));
    t.set_hook_paused(&stoke_quote::HOOK_LAUNCH_TAX_ID, true);
    assert_eq!(t.quote(&l, &u, true, SOL).unwrap().fee_bps, FEE);
    t.snapshot_for_jupiter(&l, "paused-hook", "Launched with a 10% Launch tax for a day, and Stoke has paused the hook: every trade pays the coin's 1%.");

    // A creator's own hook: nothing but its own program knows what it does to a trade.
    let mock = t.add_hook("meridian_mock_hook");
    let (l, _) = t.launch_with_hook(mock, mock_cfg(coin_mode::FEE, 300), 0);
    let u = t.trader(&l, whale);
    ok(t.buy(&l, &u, 5 * SOL));
    assert_eq!(t.market(&l).quotable(), Err(stoke_quote::QuoteError::UnknownHook(mock)));
    t.snapshot_for_jupiter(&l, "own-hook", "A hook program its creator wrote: stoke-amm doesn't quote it.");
}
