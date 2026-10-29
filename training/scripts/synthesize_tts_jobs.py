#!/usr/bin/env python3
"""Synthesize job JSONL using local Piper voices and macOS `say` voice Milena."""

from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import tempfile
import wave
from pathlib import Path
from typing import Any

_PIPER_VOICES: dict[str, Any] = {}


def piper_synthesize(job: dict[str, Any], model_dir: Path, output: Path) -> None:
    try:
        from piper import PiperVoice, SynthesisConfig
    except ImportError as exc:
        raise SystemExit("Install local dependency: training/.venv/bin/python -m pip install piper-tts") from exc

    voice_id = job["voice"].split(":", 1)[1]
    model_path = model_dir / f"{voice_id}.onnx"
    if not model_path.is_file():
        raise SystemExit(f"Piper voice model is missing: {model_path}; run download_piper_voices.sh")

    voice = _PIPER_VOICES.get(voice_id)
    if voice is None:
        voice = PiperVoice.load(str(model_path))
        _PIPER_VOICES[voice_id] = voice

    rate = float(job["speaking_rate"])
    variation = int(job["seed"]) % 3
    synth = SynthesisConfig(
        length_scale=1.0 / rate,
        noise_scale=(0.55, 0.667, 0.8)[variation],
        noise_w_scale=(0.6, 0.8, 1.0)[variation],
    )
    with tempfile.NamedTemporaryFile(suffix=".wav") as temp_audio:
        with wave.open(temp_audio.name, "wb") as wav_file:
            voice.synthesize_wav(job["text"], wav_file, syn_config=synth)
        subprocess.run(
            [
                "ffmpeg", "-hide_banner", "-loglevel", "error", "-y", "-i", temp_audio.name,
                "-ar", "16000", "-ac", "1", "-c:a", "pcm_s16le", str(output),
            ],
            check=True,
        )


def say_synthesize(job: dict[str, Any], output: Path) -> None:
    voice = job["voice"].split(":", 1)[1]
    words_per_minute = max(90, min(320, round(170 * float(job["speaking_rate"]))))
    with tempfile.NamedTemporaryFile(suffix=".aiff") as temp_audio:
        subprocess.run(
            ["say", "-v", voice, "-r", str(words_per_minute), "-o", temp_audio.name, job["text"]],
            check=True,
        )
        subprocess.run(
            [
                "ffmpeg", "-hide_banner", "-loglevel", "error", "-y", "-i", temp_audio.name,
                "-ar", "16000", "-ac", "1", "-c:a", "pcm_s16le", str(output),
            ],
            check=True,
        )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jobs", type=Path, default=Path("training/jobs/tts-jobs.jsonl"))
    parser.add_argument("--model-dir", type=Path, default=Path("training/models/piper"))
    parser.add_argument("--only-voice", help="Optional exact voice id, e.g. piper:ru_RU-denis-medium")
    parser.add_argument("--limit", type=int, help="Optional cap useful for a small preview batch")
    args = parser.parse_args()
    if shutil.which("ffmpeg") is None:
        raise SystemExit("ffmpeg is required for format normalization")

    jobs = [json.loads(line) for line in args.jobs.read_text(encoding="utf-8").splitlines() if line.strip()]
    if args.only_voice:
        jobs = [job for job in jobs if job["voice"] == args.only_voice]
    if args.limit is not None:
        jobs = jobs[: args.limit]

    completed = 0
    for job in jobs:
        # Job paths are relative to training/, so all generated samples stay gitignored.
        output = Path("training") / job["output_relative_path"]
        output.parent.mkdir(parents=True, exist_ok=True)
        if output.is_file():
            output.with_suffix(".json").write_text(
                json.dumps(job, ensure_ascii=False, sort_keys=True) + "\n", encoding="utf-8"
            )
            continue
        if job["voice"].startswith("piper:"):
            piper_synthesize(job, args.model_dir, output)
        elif job["voice"].startswith("say:"):
            say_synthesize(job, output)
        else:
            raise SystemExit(f"unsupported TTS provider in voice id: {job['voice']}")

        output.with_suffix(".json").write_text(
            json.dumps(job, ensure_ascii=False, sort_keys=True) + "\n", encoding="utf-8"
        )
        completed += 1
        if completed % 25 == 0:
            print(f"synthesized {completed}/{len(jobs)} new clips", flush=True)

    print(f"synthesized {completed} clips; skipped {len(jobs) - completed} existing clips")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
