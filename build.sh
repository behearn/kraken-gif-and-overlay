#!/bin/sh
set -eu
cd "$(CDPATH= cd -- "$(dirname "$0")" && pwd)"
export CARGO_TARGET_DIR="$PWD/target"
cargo build --release
echo "Built $CARGO_TARGET_DIR/release/kraken-gif-and-overlay"
