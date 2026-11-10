# stoke-amm: Stoke for Jupiter

The [`jupiter-amm-interface`](https://github.com/jup-ag/jupiter-amm-interface) `Amm` for Stoke coins, with parity tests on Jupiter's test kit.

```bash
cargo test        # offline: 15 tests, from the fixtures committed here
```

Stoke ([usestoke.xyz](https://usestoke.xyz)) is a launchpad on Solana. Every coin is one pool of Stoke's PoolManager, a concentrated-liquidity program with Uniswap v4's hook model, paired with wrapped SOL and with Stoke's launch hook on it. A coin trades on a bonding curve inside the hook until the curve has raised 400 SOL, and in its pool after that. The buy that sells the curve out also graduates the coin, inside the same swap.

| Program | Address | In the test kit |
|---|---|---|
| PoolManager (the one a swap calls) | `3EK8eA2473durhf2oKJzYk827GN56DHEZRMnSbuuBngm` | `stoke.so` |
| Launch hook | `CAcC7pz43rLg14jAE8iR3w3sgXcd2GGtoeFWUBB1SGVq` | `stoke_launch_hook.so` |
| Launch tax (a coin hook) | `3zbuMvvfy73QL6U8hsceeZxCpju2mDNed3YaQfTcdtjS` | `stoke_hook_launch_tax.so` |
| Max buy (a coin hook) | `AXDdW1eY1YcxrxsrR6wGnj45hAFNtGtHrXCRsxZzKMqo` | `stoke_hook_max_buy.so` |

## The `Amm`

`StokeAmm` in [`src/lib.rs`](src/lib.rs). One `Amm` covers a coin for its whole life, because a trade is the same instruction with the same accounts on the curve and in the pool.

| | |
|---|---|
| Label | `Stoke` |
| Keyed account | The coin's pool: an account of the PoolManager whose hook is the launch hook and whose other currency is wrapped SOL. No params. |
| Reserve mints | Wrapped SOL and the coin. |
| Accounts to update | Eight, the same for the coin's whole life (`has_dynamic_accounts` is false): the pool, its tick store, the coin's launch account, its fee zones, its coin hook, the pool's published hook accounts, and the listings of Stoke's two hooks. The fee zones and coin hook accounts only exist for coins launched with them; a missing one is expected. |
| Clock | From `AmmContext::clock_ref` (slot and unix time). It matters for the anti-snipe window in a coin's first three slots, for fee zones switching off, and for the Launch tax hook, which falls over time. |
| Exact out | Not supported. |
| Network calls | None. |

### Quoting

The quote is computed by [`stoke-quote`](../crates/stoke-quote), and that crate runs the programs' own code: the curve, the pool engine, the fee split, fee zones, and the two hook programs' decision functions are imported from the program crates, not rewritten. The only thing written twice is the order the launch hook's `before_swap` and `after_swap` call them in. That order is what the parity tests check.

Every fee is taken in SOL, whichever way the trade goes, so `fee_mint` is always wrapped SOL.

### The swap

One instruction, the PoolManager's `swap`, signed by the trader alone. It works as a top-level instruction and by CPI.

Accounts, in order: `sender` (signer), `pool` (w), `ticks` (w), `vault0` (w), `vault1` (w), `token0` (w), `token1` (w), `sender_claims` (none: the PoolManager's own id), `mint0`, `mint1`, `token_program0`, `token_program1`, `hooks` (the launch hook), `hook_caller`, `hook_claims` (w), `authority`. `token0` and `token1` are the trader's token accounts for the pool's two currencies. After those 16 come the hook's accounts, exactly as the launch hook publishes them for the pool at PDA `["hook-accounts", pool]`: the launch account and the hook's position, then the fee zones account for a coin with zones, then three more for a coin with a coin hook. That makes 18 to 22 accounts.

Data: Anchor's `swap` discriminator, then `zero_for_one: bool`, `amount_specified: i128` (negative for exact input), `sqrt_price_limit: u128`, `hook_data: Vec<u8>` (empty), `settle: { pay_with_claims: bool, take_as_claims: bool }` (both false: settle in tokens), `amount_limit: u64` (the least the trader accepts). `stoke_quote::swap_data(zero_for_one, amount_in, min_out)` builds it, with the price limit left open, which is what the quote assumes.

`Swap::Placeholder { data }` carries one byte, `zero_for_one`. A real `Swap` variant needs nothing else.

Call depth: the PoolManager calls the launch hook, and for a coin with a coin hook the launch hook calls that hook. Under Jupiter's program that is three CPIs deep at most (stack height 4).

Compute, measured in LiteSVM: about 63,000 units for a trade on the curve, 120,000 in the pool, and 236,000 for the buy that graduates a coin. A coin with fee zones and a hook adds 20,000 to 40,000.

### What isn't quoted

- **A coin with its creator's own hook program.** Creators can add a hook they wrote (it can set a trade's fee up to 10%, or refuse a trade). Only that program knows what it does, so `quote` returns an error and `is_active` is false for those coins. Coins with one of Stoke's two hooks are quoted exactly.
- **The creator's own sells.** A coin's creator sells fee-free up to what they bought at launch. A quote doesn't know who is trading, so it quotes the fee everyone else pays, and the creator receives more than quoted.
- **A buy the Max buy hook refuses.** For the first minutes of a coin launched with it, a buy above the creator's limit fails on-chain, so `quote` returns an error for that size.

## Tests

Both files use the kit's `assert_pool_parity`: for each trade it quotes with `StokeAmm`, runs the real `swap` in LiteSVM against the programs in `tests/fixtures/*.so`, and requires the tokens received to equal the quote to the unit.

**[`tests/mainnet.rs`](tests/mainnet.rs)**: five live mainnet coins, snapshotted by the kit, with fees of 1% and 2.5%. Buys from 1,000 lamports to 2,000 SOL, including buys that graduate the coin and carry on into its new pool, and sells from one coin to two million.

**[`tests/local.rs`](tests/local.rs)**: the states no mainnet coin is in yet. Nothing on mainnet has graduated, set fee zones or added a hook so far, so these snapshots are built by the programs' own test harness (`jupiter_fixtures` in `../programs/pool_manager/tests/launch.rs`), which launches each coin and trades it into the state:

| Snapshot | State |
|---|---|
| `first-second` | Launched one slot ago, inside the anti-snipe window |
| `graduated` | In its pool |
| `zones-on-the-curve` | Fee zones on the curve, the price inside the top one |
| `zones-in-the-pool` | Graduated, fee zones in the pool |
| `launch-tax` | Launch tax hook, part-way down, on the curve |
| `launch-tax-in-the-pool` | The same coin, graduated |
| `max-buy` | Max buy hook, inside its window |
| `zones-and-a-hook` | Fee zones and a hook: a sell pays the higher |
| `paused-hook` | A hook Stoke has paused |
| `own-hook` | A creator's own hook: not quoted |

The same file checks what the `Amm` reports about itself and what it refuses.

The programs in `tests/fixtures` are the ones deployed on mainnet, dumped by the kit. To snapshot the mainnet coins and programs again:

```bash
RPC=<a mainnet RPC url> ./refresh-fixtures.sh
```

To rebuild the local snapshots (needs the programs built first, `../scripts/build.sh`):

```bash
cd .. && cargo test -p meridian_pool_manager --test launch jupiter_fixtures -- --ignored
```

`stoke-quote` has its own parity tests in that harness too (`quotes_match_*` in `launch.rs`): over a hundred random trades per coin, from a coin's first slot through graduation and on in its pool, each quoted first and then run.

## Layout

This folder is its own Cargo workspace because the kit pins LiteSVM 0.14 and the programs' tests use a newer one. `Cargo.lock` here holds the kit's versions; don't `cargo update` it (see "Dependency notes" in the kit's README). It depends on `../crates/stoke-quote` and, through it, on the program crates in `../programs`, so fork the repository whole.
