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
# tract compilation is memory intensive; callers may override this limit.
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"

cross build --locked --release --target aarch64-unknown-linux-musl --manifest-path Cargo.toml --bin gpt-alice
cross build --locked --release --target armv7-unknown-linux-gnueabihf --manifest-path native/gpt-live-host/Cargo.toml
cross build --locked --release --target armv7-unknown-linux-gnueabihf --manifest-path native/audio-sink/Cargo.toml

mkdir -p dist
install -m 0755 target/aarch64-unknown-linux-musl/release/gpt-alice dist/gpt-alice
install -m 0755 target/armv7-unknown-linux-gnueabihf/release/gpt-live-host dist/gpt-live-host
install -m 0755 target/armv7-unknown-linux-gnueabihf/release/gpt-alice-audio-sink dist/audio-sink
install -d -m 0755 dist/wakeword
install -m 0644 assets/wakeword/*.onnx assets/wakeword/settings.json dist/wakeword/

echo "Built artifacts:"
file dist/gpt-alice dist/gpt-live-host dist/audio-sink dist/wakeword/*.onnx
