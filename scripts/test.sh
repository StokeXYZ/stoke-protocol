#!/usr/bin/env bash
# Builds the programs, then runs every Rust test (math, curve, PoolManager, hooks, launch).
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$HOME/.local/share/solana/install/active_release/bin:$PATH"
./scripts/build.sh >/dev/null
cargo test --workspace 2>&1 | grep -E "^test result|^running|FAILED|panicked"
# The Jupiter adapter is its own workspace: its quotes against the deployed programs, on Jupiter's test kit.
(cd jupiter && cargo test 2>&1 | grep -E "^test result|^running|FAILED|panicked")
