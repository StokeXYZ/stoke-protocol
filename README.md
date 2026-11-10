# Stoke protocol: Uniswap v4 hooks on Solana

The programs behind [usestoke.xyz](https://usestoke.xyz), live on Solana mainnet, written with Anchor 1.2:

| | What it is |
|---|---|
| `programs/pool_manager` | **PoolManager** (`3EK8eA2473durhf2oKJzYk827GN56DHEZRMnSbuuBngm`). Concentrated-liquidity pools with v4's full hook model: initialize, modify liquidity, swap, donate, claims, dynamic and protocol fees. |
| `programs/launch_hook` | **Stoke Launch** (`CAcC7pz43rLg14jAE8iR3w3sgXcd2GGtoeFWUBB1SGVq`, permissions `0x28cc`). The launchpad as one hook: one curve for every coin, graduation inside the swap that completes it, and a creator-chosen SOL fee (up to 2.5%) split between the creator, the coin's holders and the coin's own reserves. |
| `programs/hook_launch_tax`, `programs/hook_max_buy` | Stoke's two **coin hooks** (`3zbuMvvfy73QL6U8hsceeZxCpju2mDNed3YaQfTcdtjS`, `AXDdW1eY1YcxrxsrR6wGnj45hAFNtGtHrXCRsxZzKMqo`): small programs a creator can add to a coin, called before every trade of it. See "Coin hooks" below. |
| `crates/meridian-core` | Everything a hook or client needs without the PoolManager crate: v4's math in Q64.64, the 14 permission flags, fee encodings, the hook interface (arguments, returns, `verify_call`) and the pool/tick layouts. |
| `programs/mock_hook` | A test hook. It loads at any address, so tests can deploy it with any permission bits. It returns configured deltas/effects/actions, pays vaults, tries to misbehave, and logs every callback. |
| `crates/grind` | `meridian-grind <flags> <out.json>`: mines a program keypair whose address encodes a permission set. |
| `crates/stoke-quote` | What an exact-input trade of a coin returns, computed off-chain from its accounts with the programs' own code, and the swap instruction to send. For routers and bots. |
| `jupiter/` | `stoke-amm`: the [`jupiter-amm-interface`](https://github.com/jup-ag/jupiter-amm-interface) `Amm` for Stoke coins, with parity tests on Jupiter's test kit. Its own workspace; see [its README](jupiter/README.md). |

The programs haven't had a professional audit.

On mainnet as of 2026-11-10, the PoolManager and both coin hooks run this code. The launch hook is one upgrade behind it: there, only the admin can list a hook, and a hooked coin's swap that its price limit cuts short isn't refused yet (`SwapCutShort`). Everything else below is live.

## The hook model, and how it maps to v4

| Uniswap v4 | Stoke |
|---|---|
| Hook permissions in the low 14 bits of the hook address | The same bits, same positions, in the last two bytes of the hook **program id**. A program's id is its deploy keypair, so you mine one (`crates/grind`, or in the browser on `/hooks`), and a deployed hook can never gain or drop a permission. `isValidHookAddress` rules are identical. |
| `PoolKey { currency0, currency1, fee, tickSpacing, hooks }` → `PoolId` | Pools are PDAs `["pool", mint0, mint1, fee, tick_spacing, hooks]`, so one pair can have many pools with different hooks. |
| Singleton holding every pool's tokens | One program. Vaults are **per pool** so unrelated pools never share a write-locked account: Solana runs them in parallel. |
| 10 callbacks (before/after initialize, add, remove, swap, donate) | Same 10 callbacks, same arguments (`sender`, `key`, `params`, deltas, `hookData`), sent as Anchor-compatible instructions (`sha256("global:before_swap")`). An Anchor hook just declares `pub fn before_swap(...) -> Result<BeforeSwapReturn>`. |
| `beforeSwap` → `(BeforeSwapDelta, lpFeeOverride)`; `afterSwap` → `int128`; `afterAdd/RemoveLiquidity` → `BalanceDelta` | Identical semantics and signs, including `HookDeltaExceedsSwapAmount`, ignoring a delta whose `*ReturnDelta` bit isn't set, the specified/unspecified mapping, and the override flag (`0x400000`) on dynamic pools only. |
| `onlyPoolManager` | Each pool has a PoolManager PDA `["hook-caller", pool]` that signs only that pool's hook calls. `meridian_core::verify_call` checks it, the pool's owner and discriminator, and that the pool's hook is the caller's program. A hook can't impersonate the PoolManager to another hook or for another pool. |
| `take / settle / mint / burn` from inside a callback | Solana forbids re-entering a program that's still on the call stack. So a hook's net delta settles against its **claims** on the pool (v4's ERC-6909 mint/burn) when the operation ends, and tokens a hook moves into a vault during its callback are measured and credited to it (v4's `sync`/`settle`). The hook settles before the trader, so a hook that can't cover its delta fails the operation. |
| Calling `modifyLiquidity` / `swap` / `donate` / `updateDynamicLPFee` from a callback | Callbacks return `HookEffects { lp_fee, actions }`. The PoolManager applies them **as the hook, right after the callback returns and before it continues**, which is exactly where a v4 nested call would run. JIT liquidity, limit orders and the launch hook's in-swap graduation all use this. `update_dynamic_lp_fee` is also an instruction the hook can call at any time. |
| `noSelfCall` | Operations signed by the hook's PDA `["hook"]` skip its own callbacks. |
| `hookData` | Passed through byte for byte. |
| `unlock` + flash accounting across many calls | Each instruction settles its own deltas; several instructions in one transaction compose atomically. |
| Protocol fee (≤ 0.1% per direction, controller-set) | Same encoding (`calculateSwapFee`, two 12-bit halves), set and collected by the config's `protocol_fee_controller`. |

Safety rules a hook can't break:
- The trader's signature is never forwarded: every remaining account reaches the hook as a non-signer.
- Hooks get the pool read-only and can't touch vaults: the vault authority is a different PDA from the hook caller.
- Return data only counts if the hook program set it, since the runtime resets return data on each invocation.
- Actions can only modify positions the hook itself owns.
- Token-2022 mints with extensions that could move, freeze, tax or block vault tokens are refused at `initialize`. That covers transfer fees, transfer hooks, permanent delegates, non-transferable mints, default-frozen accounts, confidential transfers and pausable mints.

**Ticks** live in one growable account per pool, as a sorted array (binary search, neighbours for the next initialized tick), instead of v4's mapping and bitmap. `reserve_ticks` prepays capacity, since actions can't grow it mid-swap.

**Precision.** Prices are Q64.64 with ticks in ±443,636, so every sqrt price fits a u128. The tick math uses v4's exact constants, verified to the bit against a big-integer reference. Fee growth is Q64.64 (wrapping).

## Stoke Launch

Each launch is one pool (token / WSOL, dynamic fee kept at 0, tick spacing 200). The hook's permissions are `beforeInitialize | beforeAddLiquidity | beforeSwap | afterSwap | beforeSwapReturnDelta | afterSwapReturnDelta`.

**The same curve for every coin.** The config sets it, not the creator.
- The coin opens at about 206 SOL of market cap and graduates at about 1,467 SOL.
- Graduation comes once 400 SOL has been raised (`raise_target`; `curve_ratio_bps = 3750`).
- 72.7% of the supply sells on the curve and 27.3% goes to the pool.
- The start-to-graduation multiple is 1/a² ≈ 7.1×.

**Launch:**
- The creator pays the launch fee (0.03 SOL) to the config's `fee_recipient` and picks the coin's trading fee (`fee_bps`), within the config's `min_fee_bps..max_fee_bps`. The program refuses a config above 2.5% (`MAX_FEE_BPS`).
- The mint's whole 1B supply is minted into the hook's claims on the pool, and minting is revoked.
- The pool is initialized at the graduation price. The hook opens its full-range position and publishes its extra accounts at `["hook-accounts", pool]`.
- The optional dev buy goes in the same transaction. It's untaxed (same slot, creator) and becomes the creator's **fee-free sell allowance**: their sells pay no fee until they've sold that many tokens. A larger sell pays only on the part above the allowance.

**The fee: chosen by the creator, always in SOL.** On the curve it's part of the fill. After graduation the pool's LP fee stays 0 and the hook takes the fee through return deltas:
- `beforeSwap` takes it when SOL is the specified currency (exact-in buys, exact-out sells).
- `afterSwap` takes it when SOL is unspecified (exact-out buys, exact-in sells).

Each fee splits three ways (`creator_share_bps`, `staker_share_bps`, and the rest to the coin's reserves; 30/30/40% on mainnet). The protocol takes no share of trading fees: its income is the launch fee.
- **Creator.** Claimed by `claim_creator_fees`, to the creator's wrapped-SOL account only.
- **Holders.** `withdraw_holder_rewards` (permissionless) moves this share to one wallet only, the keeper (`HOLDER_PAYOUT`), which pays every holder in SOL each hour, weighted by their average balance over the hour. Holding counts 1×; tokens locked with `stake` for 7 or 30 days count 1.5× or 2×.
- **The coin's reserves.** Real SOL for the coin itself. On the curve it joins the curve's real reserves without moving the price, so the curve holds more than its sellers are owed. After graduation `deepen` (permissionless) adds it as locked SOL-only liquidity between the launch and graduation prices, so dips are absorbed by more SOL the more the coin trades. It only adds while the price is above that range; otherwise the SOL waits.

**Fee zones** are an option the creator picks in the launch transaction (`set_fee_zones`): up to five market cap bands, between 400 and 7,000 SOL, where sells pay more, up to 10%. A sell pays the highest zone its price passes through; buys never pay them. The zones switch off for good once the coin has held above the top one for five minutes.

**Curve phase:**
- Every swap is a NoOp on the pool, filled from the curve.
- Anti-snipe, the same for every coin (config `snipe_fee_bps`, `snipe_slots`): buys in the launch slot and the next two (≈ 1.2 s) pay 90%, and the tokens it buys are burned (`burn_pending`). The creator's dev buy in the launch transaction is exempt.
- Third-party liquidity, donations and other pools with this hook are refused until graduation.

**Graduation, in the swap:** the buy that sells the last curve token returns an action that adds the curve's SOL and the reserved tokens as full-range liquidity. The pool's price is already the curve's last price, so nothing jumps. The rest of that buy continues into the new liquidity and pays the same 1%.

**After graduation:** it's a normal pool, and anyone can add liquidity. Every position the hook owns, the full-range one and the floor, is locked: the hook has no instruction to remove one.

## Coin hooks (Stoke v3)

A creator can add one more program to their coin in the launch transaction: a **coin hook**. It can be one of Stoke's or any program of their own, once its code is locked. It isn't the pool's hook (that's always Stoke Launch); Stoke Launch calls it from `beforeSwap`, before every trade of the coin, on the curve and in the pool.

- **What it can do:** refuse the trade (an error), or set the trade's fee, 0 to 10% of its SOL side (`MAX_COIN_HOOK_FEE_BPS`). Nothing else: it's called with one account, its own listing, as a read-only signer.
- **The interface** is `crates/meridian-core/src/coin_hook.rs`: `on_attach(AttachArgs)` checks the creator's settings (up to 64 bytes) once; `on_trade(TradeArgs) -> TradeReturn` runs per trade. `verify_coin_hook_call` proves Stoke Launch is the caller.
- **Listing:** `list_hook` creates `["hook-listing", program]`. Anyone can call it, for a program nobody can change any more: immutable, or upgradeable by the config's admin only (Stoke's own hooks). `set_hook_paused` (the admin) pauses a hook: it can't be added, and its coins trade as if they had none.
- **Cut-short swaps:** when the trader fixes the SOL side of a pool swap, the fee is taken up front on all of it. For a hooked coin, `afterSwap` refuses a swap its price limit cut short (`SwapCutShort`), so the fee is never charged on SOL that didn't trade. Coins without a hook still pay it on the full amount.
- **Adding:** `attach_hook` (the creator, in the launch slot, before any trade, after `set_fee_zones` if both) creates `["coin-hook", launch]` and republishes the pool's hook accounts with three more: the coin hook account (writable), the listing, the hook program. It's never changed or removed.
- **Fees:** a hook's fee replaces the coin's for that trade, splits the same way, applies to the creator too (their fee-free allowance is untouched), and where a sell is also in a fee zone it pays the higher. On a buy, the anti-snipe tax plus the hook's fee stop at 99%.
- **The first two hooks:** `programs/hook_launch_tax` (a fee that starts high and falls to the coin's own) and `programs/hook_max_buy` (no single buy above a size for the first minutes). Both are raw-entrypoint programs of about 20 KB.

`tests/launch.rs` covers them from the "coin hooks" banner down (12 tests).

## Build and test

```bash
./scripts/build.sh     # cargo-build-sbf for each program → target/deploy
./scripts/test.sh      # build, then every Rust test
```

The tests (`programs/pool_manager/tests`, LiteSVM) are listed below.

**`pool.rs`** (12 tests):
- v4's initialize rules
- Token-2022 extension refusal
- range amounts
- exact-in and exact-out swaps equal the engine's quote across tick crossings
- price limits, slippage, fees, donate, claims and protocol fees
- a solvency run where every LP exits and only dust remains

**`hooks.rs`** (17 tests):
- each permission triggers exactly its callback, and callbacks get v4's arguments
- noSelfCall
- a constant-sum custom curve (CSMM) on an empty pool, and partial fills routed into the pool
- deltas without permission are ignored
- the afterSwap cut on exact in and exact out, and liquidity return deltas
- dynamic fee via afterInitialize, per-swap overrides, and override rules on static pools
- JIT liquidity as actions, queued swaps and donations, and direct vault payments
- safety:
  - the trader's signature is never forwarded
  - only the PoolManager can call a hook
  - reverts, garbage and silent hooks
  - positions owned by someone else

**`launch.rs`** (34 tests). The launchpad:
- the launch fee, the curve's start and graduation market caps, the pool and floor range
- creators choose a fee within 0.25–2.5%, and the config can't exceed 2.5% or a 20% protocol share
- third parties can't create pools or add liquidity
- the fee split (creator, holders, the coin's reserves), and that a round trip always loses
- anti-snipe: 90% in the launch slot and the window's last slot, nothing one slot later, dev buy exempt, burns only into the reserve
- the creator's sells are fee-free up to the dev buy, pro rata above it
- locked staking: 7 and 30 days at 1.5× and 2×, and the flexible tier taking no new stakes
- graduation inside the swap on SPL Token and Token-2022, then the 1% SOL fee on all four swap kinds, `deepen`, third-party liquidity and creator claims
- price continuity
- a 60-step random run of trades, stakes, unstakes and claims, with the books checked after every step

Fee zones (7 tests): set by the creator in the launch transaction only, the highest zone a sell passes through, no dodging by pushing the price up first, switching off for good, on the curve and in the pool.

Coin hooks (12 tests): listing and adding, the two Stoke hooks on the curve and in the pool, the 10% cap, pausing, hooks that revert or return garbage, what a hook is told and that it gets nothing but the call.

Off-chain quotes (3 tests): `stoke-quote` against the programs. Over a hundred random trades per coin, each quoted first and then run, from a coin's first slot through graduation and on in its pool, for plain coins, coins with fee zones and coins with Stoke's hooks. The amount received and the fee taken have to equal the quote exactly.

`jupiter_fixtures` (ignored by default) rebuilds the snapshots in `jupiter/tests/fixtures/local`.

## Deploying

The addresses above are the mainnet deployment. To deploy your own:

1. Deploy the programs with keypairs of your own (kept out of the repository: anyone holding one can deploy at that address) and update the `declare_id!`s and `meridian_core::POOL_MANAGER_ID`. A launch hook keypair must keep its flags: `cargo run -p meridian-grind -- 0x28cc <out.json>`.
2. From the upgrade-authority wallet, run `initialize_config` on the PoolManager and on the launch hook. Only the upgrade authority can initialize, so nobody can take the admin roles in between.
3. Leave the PoolManager's own protocol fee at 0 on launch pools, ideally by handing `protocol_fee_controller` to an address nobody controls.

Not done yet:
- A professional audit. Until there is one, the launch hook stays upgradeable; freezing its upgrade authority, so the locked liquidity and the fee split can never change, comes after.
- Address lookup tables for multi-hop routes.
- A general pool and liquidity UI for hooks other than the launch hook. The programs support any hook; the app only launches through this one so far.

## License

Apache-2.0. See [LICENSE](LICENSE).
