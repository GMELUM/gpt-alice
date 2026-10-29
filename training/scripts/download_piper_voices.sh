#!/bin/sh
set -eu

TRAINING_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
MODEL_DIR="$TRAINING_DIR/models/piper"
PYTHON="$TRAINING_DIR/.venv/bin/python"

if [ ! -x "$PYTHON" ]; then
    echo "Create the local training environment first: python3 -m venv training/.venv && training/.venv/bin/python -m pip install piper-tts" >&2
    exit 1
fi

mkdir -p "$MODEL_DIR"
for voice in ru_RU-denis-medium ru_RU-dmitri-medium; do
    "$PYTHON" -m piper.download_voices "$voice" --download-dir "$MODEL_DIR"
done

printf 'Piper voices are in %s\n' "$MODEL_DIR"
