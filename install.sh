#!/usr/bin/env bash
# Build codex-launcher, then install it or update the installed copy.
#
# Cargo puts the binary in ~/.cargo/bin unless its install root is set elsewhere.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")"

cargo build --release --locked
# The install reuses the release build above instead of compiling again.
cargo install --locked --path .
