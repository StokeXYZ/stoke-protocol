#![allow(dead_code)]

use anchor_lang::prelude::borsh;
use anchor_lang::{
    prelude::Pubkey,
    solana_program::{bpf_loader_upgradeable, instruction::{AccountMeta, Instruction}, system_instruction},
    AccountDeserialize, InstructionData, ToAccountMetas,
};
use anchor_spl::token_2022::spl_token_2022;
use litesvm::{types::{FailedTransactionMetadata, TransactionMetadata}, LiteSVM};
use meridian_core::*;
use meridian_mock_hook::{MockConfig, LOG_SIZE, LOG_SLOT};
use meridian_pool_manager as pm;
use solana_account::Account;
use solana_keypair::Keypair;
use solana_message::{Message, VersionedMessage};
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;

pub type TxResult = Result<TransactionMetadata, FailedTransactionMetadata>;

pub const SOL: u64 = 1_000_000_000;
pub const TOKEN: Pubkey = anchor_spl::token::ID;
pub const TOKEN22: Pubkey = spl_token_2022::ID;
pub const NO_HOOK: Pubkey = Pubkey::new_from_array([0; 32]);

pub fn pm_pda(seeds: &[&[u8]]) -> Pubkey {
    Pubkey::find_program_address(seeds, &pm::ID).0
}

pub fn code(e: pm::errors::PmError) -> u32 {
    anchor_lang::error::ERROR_CODE_OFFSET + e as u32
}

#[track_caller]
pub fn ok(res: TxResult) -> TransactionMetadata {
    match res {
        Ok(m) => m,
        Err(f) => panic!("transaction failed: {:?}\nlogs: {:#?}", f.err, f.meta.logs),
    }
}

#[track_caller]
pub fn assert_err(res: TxResult, e: pm::errors::PmError) {
    let want = format!("Custom({})", code(e));
    match res {
        Ok(m) => panic!("expected {e:?}, transaction succeeded\nlogs: {:#?}", m.logs),
        Err(f) => {
            let got = format!("{:?}", f.err);
            assert!(got.contains(&want), "expected {e:?} ({want}), got {got}\nlogs: {:#?}", f.meta.logs);
        }
    }
}

#[track_caller]
pub fn assert_fails_with(res: TxResult, needle: &str) {
    match res {
        Ok(m) => panic!("expected failure containing {needle:?}, transaction succeeded\nlogs: {:#?}", m.logs),
        Err(f) => {
            let all = format!("{:?} {:?}", f.err, f.meta.logs);
            assert!(all.contains(needle), "expected {needle:?} in {all}");
        }
    }
}

/// A hook address with the given permission bits (and a distinguishing first byte).
pub fn hook_address(flags: u16, tag: u8) -> Pubkey {
    let mut b = [0u8; 32];
    b[0] = 0x4d;
    b[1] = tag;
    b[2] = 0x11;
    b[30] = (flags >> 8) as u8;
    b[31] = flags as u8;
    Pubkey::new_from_array(b)
}

#[derive(Clone, Copy, Debug)]
pub struct PoolKeys {
    pub pool: Pubkey,
    pub ticks: Pubkey,
    pub vault0: Pubkey,
    pub vault1: Pubkey,
    pub mint0: Pubkey,
    pub mint1: Pubkey,
    pub tp0: Pubkey,
    pub tp1: Pubkey,
    pub hooks: Pubkey,
    pub hook_caller: Pubkey,
    pub hook_authority: Pubkey,
    pub hook_claims: Pubkey,
    pub fee: u32,
    pub spacing: i32,
}

pub struct User {
    pub kp: Keypair,
    pub t0: Pubkey,
    pub t1: Pubkey,
}

impl User {
    pub fn key(&self) -> Pubkey {
        self.kp.pubkey()
    }
}

/// Mock hook state accounts for one hook.
#[derive(Clone, Copy)]
pub struct Mock {
    pub program: Pubkey,
    pub config: Pubkey,
    pub log: Pubkey,
}

impl Mock {
    pub fn metas(&self) -> Vec<AccountMeta> {
        vec![AccountMeta::new_readonly(self.config, false), AccountMeta::new(self.log, false)]
    }
}

pub struct Env {
    pub svm: LiteSVM,
    pub payer: Keypair,
    pub admin: Keypair,
}

impl Env {
    pub fn new() -> Self {
        let mut svm = LiteSVM::new();
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/deploy/");
        svm.add_program_from_file(pm::ID, format!("{dir}meridian_pool_manager.so")).unwrap();
        let payer = Keypair::new();
        let admin = Keypair::new();
        svm.airdrop(&payer.pubkey(), 10_000 * SOL).unwrap();
        svm.airdrop(&admin.pubkey(), 100 * SOL).unwrap();
        let mut env = Self { svm, payer, admin };
        let admin_key = env.admin.pubkey();
        env.set_upgrade_authority(&pm::ID, Some(admin_key));
        let ix = Instruction {
            program_id: pm::ID,
            accounts: pm::accounts::InitializeConfig {
                admin: admin_key,
                config: pm_pda(&[CONFIG_SEED]),
                program_data: Pubkey::find_program_address(&[pm::ID.as_ref()], &bpf_loader_upgradeable::ID).0,
                system_program: anchor_lang::system_program::ID,
            }
            .to_account_metas(None),
            data: pm::instruction::InitializeConfig { protocol_fee_controller: admin_key }.data(),
        };
        let admin = env.admin.insecure_clone();
        ok(env.send(&[ix], &admin, &[]));
        env
    }

    pub fn set_upgrade_authority(&mut self, program: &Pubkey, authority: Option<Pubkey>) {
        let key = Pubkey::find_program_address(&[program.as_ref()], &bpf_loader_upgradeable::ID).0;
        let mut account = self.svm.get_account(&key).unwrap();
        match authority {
            Some(a) => {
                account.data[12] = 1;
                account.data[13..45].copy_from_slice(a.as_ref());
            }
            None => {
                account.data[12] = 0;
                account.data[13..45].fill(0);
            }
        }
        self.svm.set_account(key, account).unwrap();
    }

    pub fn send(&mut self, ixs: &[Instruction], payer: &Keypair, extra: &[&Keypair]) -> TxResult {
        let mut signers: Vec<&Keypair> = vec![payer];
        for e in extra {
            if e.pubkey() != payer.pubkey() {
                signers.push(e);
            }
        }
        let mut all = vec![solana_compute_budget(1_400_000)];
        all.extend_from_slice(ixs);
        let msg = Message::new_with_blockhash(&all, Some(&payer.pubkey()), &self.svm.latest_blockhash());
        let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &signers).unwrap();
        let res = self.svm.send_transaction(tx);
        self.svm.expire_blockhash();
        res
    }

    pub fn pay(&mut self, ixs: &[Instruction], extra: &[&Keypair]) -> TxResult {
        let payer = self.payer.insecure_clone();
        self.send(ixs, &payer, extra)
    }

    // ------------------------------------------------------------- tokens

    pub fn mint(&mut self, decimals: u8, program: Pubkey) -> Pubkey {
        let mint = Keypair::new();
        let space = 82;
        let lamports = self.svm.minimum_balance_for_rent_exemption(space);
        let ixs = [
            system_instruction::create_account(&self.payer.pubkey(), &mint.pubkey(), lamports, space as u64, &program),
            spl_token_2022::instruction::initialize_mint2(&program, &mint.pubkey(), &self.payer.pubkey(), None, decimals).unwrap(),
        ];
        ok(self.pay(&ixs, &[&mint]));
        mint.pubkey()
    }

    /// A Token-2022 mint with a transfer fee (a pool must refuse it).
    pub fn mint_with_transfer_fee(&mut self) -> Pubkey {
        use spl_token_2022::extension::ExtensionType;
        let mint = Keypair::new();
        let space = ExtensionType::try_calculate_account_len::<spl_token_2022::state::Mint>(&[ExtensionType::TransferFeeConfig]).unwrap();
        let lamports = self.svm.minimum_balance_for_rent_exemption(space);
        let ixs = [
            system_instruction::create_account(&self.payer.pubkey(), &mint.pubkey(), lamports, space as u64, &TOKEN22),
            spl_token_2022::extension::transfer_fee::instruction::initialize_transfer_fee_config(&TOKEN22, &mint.pubkey(), None, None, 100, 1_000_000).unwrap(),
            spl_token_2022::instruction::initialize_mint2(&TOKEN22, &mint.pubkey(), &self.payer.pubkey(), None, 6).unwrap(),
        ];
        ok(self.pay(&ixs, &[&mint]));
        mint.pubkey()
    }

    pub fn token_program_of(&self, mint: &Pubkey) -> Pubkey {
        self.svm.get_account(mint).unwrap().owner
    }

    pub fn token_account(&mut self, mint: &Pubkey, owner: &Pubkey) -> Pubkey {
        let program = self.token_program_of(mint);
        let acct = Keypair::new();
        let space = 165;
        let lamports = self.svm.minimum_balance_for_rent_exemption(space);
        let ixs = [
            system_instruction::create_account(&self.payer.pubkey(), &acct.pubkey(), lamports, space as u64, &program),
            spl_token_2022::instruction::initialize_account3(&program, &acct.pubkey(), mint, owner).unwrap(),
        ];
        ok(self.pay(&ixs, &[&acct]));
        acct.pubkey()
    }

    /// Wrapped SOL itself, as mainnet has it: a mint nobody can mint from.
    pub fn wrapped_sol(&mut self) -> Pubkey {
        use anchor_lang::solana_program::program_pack::Pack;
        let mint = anchor_spl::token::spl_token::native_mint::ID;
        if self.svm.get_account(&mint).is_none() {
            let mut data = vec![0; 82];
            spl_token_2022::state::Mint { decimals: 9, is_initialized: true, ..Default::default() }.pack_into_slice(&mut data);
            let lamports = self.svm.minimum_balance_for_rent_exemption(data.len());
            self.svm.set_account(mint, Account { lamports, data, owner: TOKEN, executable: false, rent_epoch: 0 }).unwrap();
        }
        mint
    }

    pub fn mint_to(&mut self, mint: &Pubkey, to: &Pubkey, amount: u64) {
        let program = self.token_program_of(mint);
        if *mint == anchor_spl::token::spl_token::native_mint::ID {
            // Wrapped SOL is wrapped, not minted: send the lamports and have the token program count them.
            self.svm.airdrop(to, amount).unwrap();
            ok(self.pay(&[spl_token_2022::instruction::sync_native(&program, to).unwrap()], &[]));
            return;
        }
        let ix = spl_token_2022::instruction::mint_to(&program, mint, to, &self.payer.pubkey(), &[], amount).unwrap();
        ok(self.pay(&[ix], &[]));
    }

    pub fn balance(&self, account: &Pubkey) -> u64 {
        let a = self.svm.get_account(account).unwrap();
        u64::from_le_bytes(a.data[64..72].try_into().unwrap())
    }

    /// Two mints sorted as currency0 < currency1.
    pub fn pair(&mut self, p0: Pubkey, p1: Pubkey) -> (Pubkey, Pubkey) {
        let a = self.mint(6, p0);
        let b = self.mint(9, p1);
        if a < b { (a, b) } else { (b, a) }
    }

    pub fn user(&mut self, pk: &PoolKeys, amount: u64) -> User {
        let kp = Keypair::new();
        self.svm.airdrop(&kp.pubkey(), 10 * SOL).unwrap();
        let t0 = self.token_account(&pk.mint0, &kp.pubkey());
        let t1 = self.token_account(&pk.mint1, &kp.pubkey());
        if amount > 0 {
            self.mint_to(&pk.mint0, &t0, amount);
            self.mint_to(&pk.mint1, &t1, amount);
        }
        User { kp, t0, t1 }
    }

    // ------------------------------------------------------------- mock hooks

    pub fn add_mock(&mut self, flags: u16, tag: u8, cfg: &MockConfig) -> Mock {
        let program = hook_address(flags, tag);
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/deploy/");
        self.svm.add_program_from_file(program, format!("{dir}meridian_mock_hook.so")).unwrap();
        let config = Pubkey::new_unique();
        let log = Pubkey::new_unique();
        let mock = Mock { program, config, log };
        self.set_mock(&mock, cfg);
        self.svm
            .set_account(log, Account { lamports: SOL, data: vec![0; LOG_SIZE], owner: program, executable: false, rent_epoch: 0 })
            .unwrap();
        mock
    }

    pub fn set_mock(&mut self, mock: &Mock, cfg: &MockConfig) {
        let mut data = borsh::to_vec(cfg).unwrap();
        data.resize(8192, 0);
        self.svm
            .set_account(mock.config, Account { lamports: SOL, data, owner: mock.program, executable: false, rent_epoch: 0 })
            .unwrap();
    }

    pub fn calls(&self, mock: &Mock, slot: usize) -> u32 {
        let l = self.svm.get_account(&mock.log).unwrap().data;
        u32::from_le_bytes(l[slot * 4..slot * 4 + 4].try_into().unwrap())
    }

    pub fn all_calls(&self, mock: &Mock) -> [u32; 10] {
        core::array::from_fn(|i| self.calls(mock, i))
    }

    pub fn last_call<T: borsh::BorshDeserialize>(&self, mock: &Mock, slot: usize) -> T {
        let l = self.svm.get_account(&mock.log).unwrap().data;
        let off = 40 + slot * (4 + LOG_SLOT);
        let n = u32::from_le_bytes(l[off..off + 4].try_into().unwrap()) as usize;
        T::deserialize(&mut &l[off + 4..off + 4 + n]).unwrap()
    }

    // ------------------------------------------------------------- pools

    pub fn keys(&self, mint0: Pubkey, mint1: Pubkey, fee: u32, spacing: i32, hooks: Pubkey) -> PoolKeys {
        let pool = pm_pda(&[POOL_SEED, mint0.as_ref(), mint1.as_ref(), &fee.to_le_bytes(), &spacing.to_le_bytes(), hooks.as_ref()]);
        let hook_authority = hook_authority(&hooks).0;
        PoolKeys {
            pool,
            ticks: pm_pda(&[TICKS_SEED, pool.as_ref()]),
            vault0: pm_pda(&[VAULT_SEED, pool.as_ref(), mint0.as_ref()]),
            vault1: pm_pda(&[VAULT_SEED, pool.as_ref(), mint1.as_ref()]),
            mint0,
            mint1,
            tp0: self.token_program_of(&mint0),
            tp1: self.token_program_of(&mint1),
            hooks,
            hook_caller: pm_pda(&[HOOK_CALLER_SEED, pool.as_ref()]),
            hook_authority,
            hook_claims: pm_pda(&[CLAIMS_SEED, pool.as_ref(), hook_authority.as_ref()]),
            fee,
            spacing,
        }
    }

    pub fn initialize_ix(&self, pk: &PoolKeys, sender: &Pubkey, sqrt_price: u128, remaining: &[AccountMeta]) -> Instruction {
        let mut accounts = pm::accounts::Initialize {
            payer: self.payer.pubkey(),
            sender: *sender,
            mint0: pk.mint0,
            mint1: pk.mint1,
            hooks: pk.hooks,
            pool: pk.pool,
            ticks: pk.ticks,
            vault0: pk.vault0,
            vault1: pk.vault1,
            authority: pm_pda(&[AUTHORITY_SEED]),
            hook_caller: pk.hook_caller,
            token_program0: pk.tp0,
            token_program1: pk.tp1,
            system_program: anchor_lang::system_program::ID,
        }
        .to_account_metas(None);
        accounts.extend_from_slice(remaining);
        Instruction { program_id: pm::ID, accounts, data: pm::instruction::Initialize { fee: pk.fee, tick_spacing: pk.spacing, sqrt_price }.data() }
    }

    pub fn try_init(&mut self, pk: &PoolKeys, sqrt_price: u128, remaining: &[AccountMeta]) -> TxResult {
        let ix = self.initialize_ix(pk, &self.payer.pubkey(), sqrt_price, remaining);
        self.pay(&[ix], &[])
    }

    pub fn init(&mut self, mint0: Pubkey, mint1: Pubkey, fee: u32, spacing: i32, hooks: Pubkey, sqrt_price: u128, remaining: &[AccountMeta]) -> PoolKeys {
        let pk = self.keys(mint0, mint1, fee, spacing, hooks);
        ok(self.try_init(&pk, sqrt_price, remaining));
        pk
    }

    pub fn state(&self, pool: &Pubkey) -> PoolState {
        PoolState::from_account_data(&self.svm.get_account(pool).unwrap().data).unwrap()
    }

    pub fn ticks(&self, pk: &PoolKeys) -> Vec<TickEntry> {
        let d = self.svm.get_account(&pk.ticks).unwrap().data;
        let h: TicksHeader = bytemuck::pod_read_unaligned(&d[..TICKS_HEADER_SIZE]);
        (0..h.len as usize)
            .map(|i| bytemuck::pod_read_unaligned(&d[TICKS_HEADER_SIZE + i * TICK_ENTRY_SIZE..TICKS_HEADER_SIZE + (i + 1) * TICK_ENTRY_SIZE]))
            .collect()
    }

    pub fn ticks_data(&self, pk: &PoolKeys) -> Vec<u8> {
        self.svm.get_account(&pk.ticks).unwrap().data
    }

    pub fn position_key(pk: &PoolKeys, owner: &Pubkey, lower: i32, upper: i32, salt: [u8; 32]) -> Pubkey {
        pm_pda(&[POSITION_SEED, pk.pool.as_ref(), owner.as_ref(), &lower.to_le_bytes(), &upper.to_le_bytes(), &salt])
    }

    pub fn position(&self, key: &Pubkey) -> pm::Position {
        let a = self.svm.get_account(key).unwrap();
        pm::Position::try_deserialize(&mut a.data.as_slice()).unwrap()
    }

    pub fn claims_key(pk: &PoolKeys, owner: &Pubkey) -> Pubkey {
        pm_pda(&[CLAIMS_SEED, pk.pool.as_ref(), owner.as_ref()])
    }

    pub fn claims(&self, key: &Pubkey) -> Option<pm::Claims> {
        let a = self.svm.get_account(key)?;
        if a.data.is_empty() {
            return None;
        }
        Some(pm::Claims::try_deserialize(&mut a.data.as_slice()).unwrap())
    }

    pub fn open_claims(&mut self, pk: &PoolKeys, owner: &Pubkey) {
        let ix = Instruction {
            program_id: pm::ID,
            accounts: pm::accounts::OpenClaims {
                payer: self.payer.pubkey(),
                pool: pk.pool,
                claims: Self::claims_key(pk, owner),
                system_program: anchor_lang::system_program::ID,
            }
            .to_account_metas(None),
            data: pm::instruction::OpenClaims { owner: *owner }.data(),
        };
        ok(self.pay(&[ix], &[]));
    }

    #[allow(clippy::too_many_arguments)]
    pub fn modify_ix(
        &self,
        pk: &PoolKeys,
        owner: &Pubkey,
        t0: Option<Pubkey>,
        t1: Option<Pubkey>,
        params: ModifyLiquidityParams,
        settle: Settle,
        owner_claims: bool,
        hook_claims: bool,
        remaining: &[AccountMeta],
        hook_data: Vec<u8>,
    ) -> Instruction {
        let mut accounts = pm::accounts::ModifyLiquidity {
            payer: self.payer.pubkey(),
            owner: *owner,
            pool: pk.pool,
            ticks: pk.ticks,
            position: Self::position_key(pk, owner, params.tick_lower, params.tick_upper, params.salt),
            vault0: pk.vault0,
            vault1: pk.vault1,
            token0: t0,
            token1: t1,
            owner_claims: owner_claims.then(|| Self::claims_key(pk, owner)),
            mint0: pk.mint0,
            mint1: pk.mint1,
            token_program0: pk.tp0,
            token_program1: pk.tp1,
            hooks: pk.hooks,
            hook_caller: pk.hook_caller,
            hook_claims: hook_claims.then_some(pk.hook_claims),
            authority: pm_pda(&[AUTHORITY_SEED]),
            system_program: anchor_lang::system_program::ID,
        }
        .to_account_metas(None);
        accounts.extend_from_slice(remaining);
        Instruction { program_id: pm::ID, accounts, data: pm::instruction::ModifyLiquidity { params, hook_data, settle }.data() }
    }

    pub fn liquidity(&mut self, pk: &PoolKeys, u: &User, lower: i32, upper: i32, delta: i128, remaining: &[AccountMeta]) -> TxResult {
        let params = ModifyLiquidityParams { tick_lower: lower, tick_upper: upper, liquidity_delta: delta, salt: [0; 32] };
        let hook_claims = self.claims(&pk.hook_claims).is_some();
        let ix = self.modify_ix(pk, &u.key(), Some(u.t0), Some(u.t1), params, Settle::default(), false, hook_claims, remaining, vec![]);
        let kp = u.kp.insecure_clone();
        self.pay(&[ix], &[&kp])
    }

    #[allow(clippy::too_many_arguments)]
    pub fn swap_ix(
        &self,
        pk: &PoolKeys,
        sender: &Pubkey,
        t0: Option<Pubkey>,
        t1: Option<Pubkey>,
        params: SwapParams,
        settle: Settle,
        amount_limit: u64,
        sender_claims: bool,
        hook_claims: bool,
        remaining: &[AccountMeta],
        hook_data: Vec<u8>,
    ) -> Instruction {
        let mut accounts = pm::accounts::Swap {
            sender: *sender,
            pool: pk.pool,
            ticks: pk.ticks,
            vault0: pk.vault0,
            vault1: pk.vault1,
            token0: t0,
            token1: t1,
            sender_claims: sender_claims.then(|| Self::claims_key(pk, sender)),
            mint0: pk.mint0,
            mint1: pk.mint1,
            token_program0: pk.tp0,
            token_program1: pk.tp1,
            hooks: pk.hooks,
            hook_caller: pk.hook_caller,
            hook_claims: hook_claims.then_some(pk.hook_claims),
            authority: pm_pda(&[AUTHORITY_SEED]),
        }
        .to_account_metas(None);
        accounts.extend_from_slice(remaining);
        Instruction { program_id: pm::ID, accounts, data: pm::instruction::Swap { params, hook_data, settle, amount_limit }.data() }
    }

    /// Exact input (amount > 0 here, sent as negative) or exact output (amount < 0 → positive).
    pub fn swap(&mut self, pk: &PoolKeys, u: &User, zero_for_one: bool, amount_specified: i128, remaining: &[AccountMeta]) -> TxResult {
        let limit = if zero_for_one { math::MIN_SQRT_PRICE + 1 } else { math::MAX_SQRT_PRICE - 1 };
        let params = SwapParams { zero_for_one, amount_specified, sqrt_price_limit: limit };
        let hook_claims = self.claims(&pk.hook_claims).is_some();
        let max = if amount_specified > 0 { u64::MAX } else { 0 };
        let ix = self.swap_ix(pk, &u.key(), Some(u.t0), Some(u.t1), params, Settle::default(), max, false, hook_claims, remaining, vec![]);
        let kp = u.kp.insecure_clone();
        self.pay(&[ix], &[&kp])
    }

    pub fn donate_ix(&self, pk: &PoolKeys, u: &User, amount0: u64, amount1: u64, remaining: &[AccountMeta]) -> Instruction {
        let hook_claims = self.claims(&pk.hook_claims).is_some();
        let mut accounts = pm::accounts::Donate {
            sender: u.key(),
            pool: pk.pool,
            ticks: pk.ticks,
            vault0: pk.vault0,
            vault1: pk.vault1,
            token0: Some(u.t0),
            token1: Some(u.t1),
            sender_claims: None,
            mint0: pk.mint0,
            mint1: pk.mint1,
            token_program0: pk.tp0,
            token_program1: pk.tp1,
            hooks: pk.hooks,
            hook_caller: pk.hook_caller,
            hook_claims: hook_claims.then_some(pk.hook_claims),
            authority: pm_pda(&[AUTHORITY_SEED]),
        }
        .to_account_metas(None);
        accounts.extend_from_slice(remaining);
        Instruction { program_id: pm::ID, accounts, data: pm::instruction::Donate { amount0, amount1, hook_data: vec![], settle: Settle::default() }.data() }
    }

    pub fn donate(&mut self, pk: &PoolKeys, u: &User, amount0: u64, amount1: u64, remaining: &[AccountMeta]) -> TxResult {
        let ix = self.donate_ix(pk, u, amount0, amount1, remaining);
        let kp = u.kp.insecure_clone();
        self.pay(&[ix], &[&kp])
    }

    /// What the program's own engine says a swap would do right now (a native quote).
    pub fn quote(&self, pk: &PoolKeys, zero_for_one: bool, amount_specified: i128, lp_fee_override: u32) -> BalanceDelta {
        let mut st = self.state(&pk.pool);
        let mut data = self.ticks_data(pk);
        let mut tv = pm::ticks::TicksView::load(&mut data, &pk.pool).unwrap();
        let limit = if zero_for_one { math::MIN_SQRT_PRICE + 1 } else { math::MAX_SQRT_PRICE - 1 };
        pm::engine::swap(&mut st, &mut tv, zero_for_one, amount_specified, limit, lp_fee_override).unwrap().delta
    }

    /// Run a PoolManager instruction through a mock hook, signed by that hook's authority.
    pub fn act(&mut self, mock: &Mock, inner: Instruction, extra: &[&Keypair]) -> TxResult {
        let mut accounts = vec![AccountMeta::new_readonly(pm::ID, false)];
        accounts.extend(inner.accounts.iter().map(|m| {
            let mut m = m.clone();
            if m.pubkey == hook_authority(&mock.program).0 {
                m.is_signer = false;
            }
            m
        }));
        let mut data = meridian_mock_hook::ACT.to_vec();
        data.extend_from_slice(&inner.data);
        self.pay(&[Instruction { program_id: mock.program, accounts, data }], extra)
    }
}

pub fn solana_compute_budget(units: u32) -> Instruction {
    // ComputeBudgetInstruction::SetComputeUnitLimit (discriminator 2).
    let mut data = vec![2u8];
    data.extend_from_slice(&units.to_le_bytes());
    Instruction { program_id: anchor_lang::pubkey!("ComputeBudget111111111111111111111111111111"), accounts: vec![], data }
}

/// sqrt price for price 1.
pub const P1: u128 = math::Q64;
