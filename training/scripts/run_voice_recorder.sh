#!/bin/sh
set -eu

PROJECT_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
cd "$PROJECT_ROOT"
export GPT_ALICE_TRAINING_DIR="$PROJECT_ROOT/training/corpus/recorded"
exec cargo run --bin gpt-alice-training-recorder
