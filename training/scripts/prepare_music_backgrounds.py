#!/usr/bin/env python3
"""Create local-only noise clips from a deterministic sample of a music library."""

from __future__ import annotations

import argparse
import hashlib
import json
import random
import shutil
import subprocess
from pathlib import Path

AUDIO_EXTENSIONS = {".aac", ".aif", ".aiff", ".alac", ".flac", ".m4a", ".mp3", ".ogg", ".opus", ".wav", ".wma"}


def duration_seconds(path: Path) -> float:
    result = subprocess.run(
        [
            "ffprobe", "-v", "error", "-show_entries", "format=duration",
            "-of", "default=noprint_wrappers=1:nokey=1", str(path),
        ],
        check=True,
        capture_output=True,
        text=True,
    )
    return float(result.stdout.strip())


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--music-root", required=True, type=Path)
    parser.add_argument("--output-dir", type=Path, default=Path("training/corpus/background/music"))
    parser.add_argument("--track-count", type=int, default=100)
    parser.add_argument("--clips-per-track", type=int, default=3)
    parser.add_argument("--clip-seconds", type=float, default=12.0)
    parser.add_argument("--seed", type=int, default=20261029)
    args = parser.parse_args()

    root = args.music_root.expanduser().resolve()
    if not root.is_dir():
        raise SystemExit(f"music directory not found: {root}")
    if shutil.which("ffmpeg") is None or shutil.which("ffprobe") is None:
        raise SystemExit("ffmpeg and ffprobe must be installed and available in PATH")
    if args.track_count < 1 or args.clips_per_track < 1 or args.clip_seconds <= 0:
        raise SystemExit("track-count, clips-per-track, and clip-seconds must be positive")

    candidates = sorted(
        path for path in root.rglob("*")
        if path.is_file() and path.suffix.lower() in AUDIO_EXTENSIONS
    )
    rng = random.Random(args.seed)
    if len(candidates) < args.track_count:
        raise SystemExit(f"found {len(candidates)} audio files; need {args.track_count} distinct tracks")
    selected = sorted(rng.sample(candidates, args.track_count))

    output_dir = args.output_dir.resolve()
    output_dir.mkdir(parents=True, exist_ok=True)
    manifest_path = output_dir / "music-backgrounds.jsonl"
    with manifest_path.open("w", encoding="utf-8", newline="\n") as manifest:
        clip_index = 0
        for track_index, source in enumerate(selected, start=1):
            duration = duration_seconds(source)
            if duration <= args.clip_seconds:
                raise SystemExit(f"track is too short for requested clip: {source}")
            for part in range(1, args.clips_per_track + 1):
                start = rng.uniform(0, duration - args.clip_seconds)
                output = output_dir / f"music-{track_index:03d}-{part:02d}.wav"
                subprocess.run(
                    [
                        "ffmpeg", "-hide_banner", "-loglevel", "error", "-y",
                        "-ss", f"{start:.3f}", "-i", str(source),
                        "-t", f"{args.clip_seconds:.3f}", "-vn",
                        "-ar", "16000", "-ac", "1", "-c:a", "pcm_s16le", str(output),
                    ],
                    check=True,
                )
                clip_index += 1
                relative = source.relative_to(root).as_posix()
                source_id = hashlib.sha256(relative.encode("utf-8")).hexdigest()[:16]
                record = {
                    "clip": output.name,
                    "label": "background_music",
                    "source_id": source_id,
                    "source_relative_path": relative,
                    "source_duration_seconds": round(duration, 3),
                    "clip_start_seconds": round(start, 3),
                    "clip_duration_seconds": args.clip_seconds,
                    "sample_rate_hz": 16000,
                    "channels": 1,
                    "sample_format": "pcm_s16le",
                    "seed": args.seed,
                }
                manifest.write(json.dumps(record, ensure_ascii=False, sort_keys=True) + "\n")

    print(f"created {clip_index} clips from {len(selected)} distinct tracks in {output_dir}")
    print("Music stays in the user-specified source directory; output is gitignored local training data.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
