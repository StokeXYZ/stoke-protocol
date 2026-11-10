#!/usr/bin/env bash
# Snapshots the mainnet coins in tests/mainnet.rs again, and dumps the four deployed programs,
# with Jupiter's test kit. Then runs every test offline against what it fetched.
#
#   RPC=<a mainnet RPC url> ./refresh-fixtures.sh
#
# Run it after a program upgrade, so the tests run against what's deployed. The local snapshots
# (tests/fixtures/local) aren't touched: see the README for rebuilding those.
set -euo pipefail
cd "$(dirname "$0")"
: "${RPC:?Set RPC to a mainnet RPC url}"
export PATH="$HOME/.cargo/bin:$PATH"

# The kit only dumps a program that's missing, and only re-snapshots accounts when REFRESH is set.
rm -f tests/fixtures/*.so
REFRESH=1 cargo test --test mainnet -- --test-threads=1
unset RPC
cargo test
shasum -a 256 tests/fixtures/*.so
