#!/usr/bin/env python3
"""Create clearly labeled high-pitch stress proxies from available positive TTS clips."""

from __future__ import annotations

import argparse
import hashlib
import json
import shutil
import subprocess
from pathlib import Path


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jobs", type=Path, default=Path("training/jobs/tts-jobs.jsonl"))
    parser.add_argument("--output-jobs", type=Path, default=Path("training/jobs/pitch-jobs.jsonl"))
    parser.add_argument("--synthetic-root", type=Path, default=Path("training"))
    parser.add_argument("--output-dir", type=Path, default=Path("training/corpus/pitch_augmented/positive"))
    parser.add_argument("--semitones", type=float, nargs="+", default=[3, 5])
    args = parser.parse_args()
    if shutil.which("ffmpeg") is None:
        raise SystemExit("ffmpeg is required for pitch augmentation")

    jobs = [json.loads(line) for line in args.jobs.read_text(encoding="utf-8").splitlines() if line.strip()]
    generated: list[dict[str, object]] = []
    written = 0
    for source_job in jobs:
        if source_job["label"] != "positive":
            continue
        source = args.synthetic_root / source_job["output_relative_path"]
        if not source.is_file():
            continue
        for semitones in args.semitones:
            if semitones <= 0 or semitones > 7:
                raise SystemExit("semitones must be in the range (0, 7]")
            identity = f"{source_job['job_id']}:pitch-up:{semitones:g}"
            job_id = hashlib.sha256(identity.encode("utf-8")).hexdigest()[:20]
            filename = f"{job_id}-pitchup{semitones:g}.wav"
            output = args.output_dir / filename
            output.parent.mkdir(parents=True, exist_ok=True)
            ratio = 2 ** (semitones / 12)
            filter_chain = f"asetrate=16000*{ratio:.8f},aresample=16000,atempo={1 / ratio:.8f}"
            if not output.is_file():
                subprocess.run(
                    [
                        "ffmpeg", "-hide_banner", "-loglevel", "error", "-y", "-i", str(source),
                        "-af", filter_chain, "-ar", "16000", "-ac", "1", "-c:a", "pcm_s16le", str(output),
                    ],
                    check=True,
                )
                written += 1
            job = {
                **source_job,
                "job_id": job_id,
                "voice": f"pitch_aug:{source_job['voice']}",
                "speaker_group": "pitch_shifted_proxy_not_child",
                "pitch_shift_semitones": semitones,
                "augmentation_source_job_id": source_job["job_id"],
                "output_relative_path": output.relative_to(args.synthetic_root).as_posix(),
            }
            output.with_suffix(".json").write_text(
                json.dumps(job, ensure_ascii=False, sort_keys=True) + "\n", encoding="utf-8"
            )
            generated.append(job)

    args.output_jobs.parent.mkdir(parents=True, exist_ok=True)
    with args.output_jobs.open("w", encoding="utf-8", newline="\n") as manifest:
        for job in generated:
            manifest.write(json.dumps(job, ensure_ascii=False, sort_keys=True) + "\n")
    print(f"created {written} high-pitch stress proxies; jobs: {len(generated)}")
    print("These are pitch-shifted adult TTS, not real child speech.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
