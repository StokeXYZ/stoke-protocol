#!/usr/bin/env bash
# Builds the on-chain programs into protocol/target/deploy.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$HOME/.local/share/solana/install/active_release/bin:$PATH"
for p in pool_manager launch_hook mock_hook hook_launch_tax hook_max_buy; do
  cargo-build-sbf --manifest-path "programs/$p/Cargo.toml" --sbf-out-dir target/deploy 2>&1 | grep -E "^error|Finished" || true
done
# Deploy with the ground keypairs (the launch hook's address encodes its permissions).
cp keys/meridian_pool_manager-keypair.json target/deploy/ 2>/dev/null || true
cp keys/meridian_launch-keypair.json target/deploy/ 2>/dev/null || true
ls -la target/deploy/*.so
