#!/usr/bin/env python3
"""Make deterministic clean/noisy positive samples at configured SNR levels."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import random
import wave
from collections import OrderedDict
from array import array
from pathlib import Path
from typing import Any

RATE = 16000


def read_pcm(path: Path) -> list[int]:
    with wave.open(str(path), "rb") as wav:
        if wav.getnchannels() != 1 or wav.getsampwidth() != 2 or wav.getframerate() != RATE:
            raise ValueError(f"expected 16 kHz mono PCM16 WAV: {path}")
        raw = wav.readframes(wav.getnframes())
    samples = array("h")
    samples.frombytes(raw)
    if __import__("sys").byteorder != "little":
        samples.byteswap()
    return samples.tolist()


def write_pcm(path: Path, samples: list[int]) -> None:
    encoded = array("h", samples)
    if __import__("sys").byteorder != "little":
        encoded.byteswap()
    path.parent.mkdir(parents=True, exist_ok=True)
    with wave.open(str(path), "wb") as wav:
        wav.setnchannels(1)
        wav.setsampwidth(2)
        wav.setframerate(RATE)
        wav.writeframes(encoded.tobytes())


def rms(samples: list[int]) -> float:
    if not samples:
        return 0.0
    return math.sqrt(sum(sample * sample for sample in samples) / len(samples))


def fit_background(samples: list[int], length: int, rng: random.Random) -> list[int]:
    if not samples:
        raise ValueError("background recording is empty")
    start = rng.randrange(len(samples))
    return [samples[(start + i) % len(samples)] for i in range(length)]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jobs", type=Path, default=Path("training/jobs/tts-jobs.jsonl"))
    parser.add_argument("--pitch-jobs", type=Path, default=Path("training/jobs/pitch-jobs.jsonl"))
    parser.add_argument("--synthetic-root", type=Path, default=Path("training"))
    parser.add_argument("--background-root", type=Path, default=Path("training/corpus/background"))
    parser.add_argument("--output-root", type=Path, default=Path("training/corpus/mixed"))
    parser.add_argument("--snr-db", type=float, nargs="+", default=[-5, 0, 5, 10, 20])
    parser.add_argument("--max-leading-silence", type=float, default=0.5)
    parser.add_argument("--max-trailing-silence", type=float, default=0.5)
    parser.add_argument("--seed", type=int, default=20261029)
    args = parser.parse_args()

    backgrounds = sorted(args.background_root.rglob("*.wav"))
    backgrounds += sorted(Path("training/assets/ambience").glob("*.wav"))
    backgrounds = list(dict.fromkeys(path.resolve() for path in backgrounds))
    if not backgrounds:
        raise SystemExit(f"no background WAVs found below {args.background_root}")
    background_cache: OrderedDict[Path, list[int]] = OrderedDict()
    background_source_ids: dict[str, str] = {}
    music_manifest = args.background_root / "music" / "music-backgrounds.jsonl"
    if music_manifest.is_file():
        for line in music_manifest.read_text(encoding="utf-8").splitlines():
            record = json.loads(line)
            background_source_ids[record["clip"]] = record["source_id"]
    jobs = [json.loads(line) for line in args.jobs.read_text(encoding="utf-8").splitlines() if line.strip()]
    if args.pitch_jobs.is_file():
        jobs.extend(
            json.loads(line)
            for line in args.pitch_jobs.read_text(encoding="utf-8").splitlines()
            if line.strip()
        )
    positives = [job for job in jobs if job["label"] == "positive"]

    manifest_path = args.output_root / "mixed-manifest.jsonl"
    args.output_root.mkdir(parents=True, exist_ok=True)
    written = 0
    with manifest_path.open("w", encoding="utf-8", newline="\n") as manifest:
        for job in positives:
            source = args.synthetic_root / job["output_relative_path"]
            if not source.is_file():
                continue
            speech = read_pcm(source)
            speech_rms = rms(speech)
            if speech_rms < 1:
                continue
            for snr_db in args.snr_db:
                digest = hashlib.sha256(
                    f"{args.seed}:{job['job_id']}:{snr_db}".encode("utf-8")
                ).digest()
                rng = random.Random(int.from_bytes(digest[:8], "big"))
                background_path = backgrounds[rng.randrange(len(backgrounds))]
                if background_path not in background_cache:
                    background_cache[background_path] = read_pcm(background_path)
                    if len(background_cache) > 16:
                        background_cache.popitem(last=False)
                else:
                    background_cache.move_to_end(background_path)
                background = fit_background(background_cache[background_path], len(speech), rng)
                background_rms = rms(background)
                if background_rms < 1:
                    continue

                scale = speech_rms / (10 ** (snr_db / 20) * background_rms)
                mixed_float = [s + b * scale for s, b in zip(speech, background)]
                peak = max(abs(sample) for sample in mixed_float) or 1.0
                peak_scale = min(1.0, 32767.0 / peak)
                mixed = [max(-32768, min(32767, round(sample * peak_scale))) for sample in mixed_float]
                snr_tag = f"{'m' if snr_db < 0 else 'p'}{abs(snr_db):g}".replace(".", "_")
                output = args.output_root / f"{job['job_id']}-snr{snr_tag}.wav"
                leading = rng.uniform(0, max(0, args.max_leading_silence))
                trailing = rng.uniform(0, max(0, args.max_trailing_silence))
                lead_samples = round(leading * RATE)
                tail_samples = round(trailing * RATE)
                mixed = [0] * lead_samples + mixed + [0] * tail_samples
                write_pcm(output, mixed)
                record: dict[str, Any] = {
                    "clip": output.name,
                    "label": "positive_mixed",
                    "job_id": job["job_id"],
                    "voice": job["voice"],
                    "speaker_group": job.get("speaker_group", job["voice"]),
                    "text": job["text"],
                    "speaking_rate": job["speaking_rate"],
                    "background_clip": background_path.name,
                    "background_recording_id": background_source_ids.get(
                        background_path.name, background_path.stem
                    ),
                    "snr_db": snr_db,
                    "leading_silence_seconds": round(leading, 3),
                    "trailing_silence_seconds": round(trailing, 3),
                    "sample_rate_hz": RATE,
                    "channels": 1,
                    "sample_format": "pcm_s16le",
                    "seed": args.seed,
                }
                manifest.write(json.dumps(record, ensure_ascii=False, sort_keys=True) + "\n")
                written += 1

    print(f"created {written} mixed positive clips from {len(positives)} TTS jobs")
    print(f"manifest: {manifest_path}; source positive WAVs remain unchanged")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
