#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

if ! command -v cross >/dev/null 2>&1; then
    echo "cross is required (install with: cargo install cross --locked --version 0.2.5)" >&2
    exit 1
fi
if ! command -v docker >/dev/null 2>&1; then
    echo "Docker is required by cross" >&2
    exit 1
fi

export CROSS_CONFIG="$ROOT/Cross.toml"
export CARGO_TARGET_DIR="$ROOT/target"

cross build --locked --release --target aarch64-unknown-linux-musl --manifest-path Cargo.toml
cross build --locked --release --target armv7-unknown-linux-gnueabihf --manifest-path native/gpt-live-host/Cargo.toml

mkdir -p dist
install -m 0755 target/aarch64-unknown-linux-musl/release/gpt-alice dist/gpt-alice
install -m 0755 target/armv7-unknown-linux-gnueabihf/release/gpt-live-host dist/gpt-live-host

echo "Built artifacts:"
file dist/gpt-alice dist/gpt-live-host
