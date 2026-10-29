#!/usr/bin/env python3
"""Generate configured TeraTTSv2 Russian voice jobs into the local corpus."""

from __future__ import annotations

import argparse
import importlib
import json
import os
import subprocess
import sys
import tempfile
import wave
from pathlib import Path

MODEL_ID = "TeraSpace/TeraTTSv2"
MODEL_REVISION = "75983ac96062591d06ecd2ddb2aefb894104bc9e"
MODEL_FILES = [
    "config.json",
    "teratts.py",
    "teratts_ruaccent.py",
    "unicode_indexer.json",
    "models/duration_predictor.onnx",
    "models/sampler_distilled_cfg3_8step.onnx",
    "models/text_encoder.onnx",
    "models/vocoder.onnx",
    "styles/ru_f1/*",
    "styles/ru_f2/*",
    "styles/ru_m1/*",
    "styles/ru_m5/*",
    "ruaccent/dictionary/*",
    "LICENSE",
    "LICENSE-OPENRAIL",
    "RUACCENT_NOTICE.txt",
]


def load_runtime(cache_dir: Path, threads: int):
    try:
        from huggingface_hub import snapshot_download
        import numpy as np
    except ImportError as exc:
        raise SystemExit(
            "Install TeraTTS dependencies: training/.venv/bin/pip install "
            "-r training/requirements-teratts.txt"
        ) from exc

    release = Path(
        snapshot_download(
            repo_id=MODEL_ID,
            revision=MODEL_REVISION,
            cache_dir=str(cache_dir),
            allow_patterns=MODEL_FILES,
        )
    )
    sys.path.insert(0, str(release))
    runtime = importlib.import_module("teratts")
    loaded = runtime.load_model(
        release,
        model="distilled",
        provider="CPUExecutionProvider",
        threads=threads,
        russian_stress=True,
        ruaccent_mode="dictionary",
    )
    return runtime, loaded, np


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jobs", type=Path, default=Path("training/jobs/tts-jobs.jsonl"))
    parser.add_argument("--model-cache", type=Path, default=Path("training/models/huggingface"))
    parser.add_argument("--only-voice", help="Exact voice ID, e.g. teratts:ru_m5")
    parser.add_argument("--limit", type=int, help="Small preview batch limit")
    parser.add_argument("--threads", type=int, default=max(1, (os.cpu_count() or 2) // 2))
    args = parser.parse_args()
    if not shutil_which("ffmpeg"):
        raise SystemExit("ffmpeg is required for 16 kHz mono PCM normalization")

    jobs = [json.loads(line) for line in args.jobs.read_text(encoding="utf-8").splitlines() if line.strip()]
    jobs = [job for job in jobs if job["voice"].startswith("teratts:")]
    if args.only_voice:
        jobs = [job for job in jobs if job["voice"] == args.only_voice]
    if args.limit is not None:
        jobs = jobs[: args.limit]
    if not jobs:
        raise SystemExit("No TeraTTSv2 jobs found; regenerate the job manifest from config.example.json")

    runtime, loaded, _np = load_runtime(args.model_cache, args.threads)
    completed = 0
    skipped = 0
    for job in jobs:
        output = Path("training") / job["output_relative_path"]
        output.parent.mkdir(parents=True, exist_ok=True)
        sidecar = output.with_suffix(".json")
        if output.is_file() and sidecar.is_file():
            skipped += 1
            continue

        voice = job["voice"].split(":", 1)[1]
        phrase = f"<ru>{job['text']}</ru>"
        rate = float(job["speaking_rate"])
        audio = runtime.generate_speech(
            loaded,
            phrase,
            voice,
            duration_scale=1.0 / rate,
            seed=int(job["seed"]),
        )
        with tempfile.NamedTemporaryFile(suffix=".wav") as raw:
            runtime.write_wav(Path(raw.name), audio)
            subprocess.run(
                [
                    "ffmpeg", "-hide_banner", "-loglevel", "error", "-y", "-i", raw.name,
                    "-ar", "16000", "-ac", "1", "-c:a", "pcm_s16le", str(output),
                ],
                check=True,
            )
        metadata = {
            **job,
            "model_id": MODEL_ID,
            "model_revision": MODEL_REVISION,
            "model_license": "OpenRAIL-M with commercial attribution condition",
            "model_variant": "distilled",
            "accent_mode": "dictionary",
        }
        sidecar.write_text(json.dumps(metadata, ensure_ascii=False, sort_keys=True) + "\n", encoding="utf-8")
        completed += 1
        if completed % 10 == 0:
            print(f"synthesized {completed}/{len(jobs)} TeraTTSv2 clips", flush=True)

    print(f"synthesized {completed} TeraTTSv2 clips; skipped {skipped} existing clips")
    return 0


def shutil_which(command: str) -> str | None:
    from shutil import which

    return which(command)


if __name__ == "__main__":
    raise SystemExit(main())
