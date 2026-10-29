#!/usr/bin/env python3
"""Create deterministic, provider-neutral TTS jobs for wake-word corpus work."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
from typing import Any


def read_phrases(path: Path) -> list[str]:
    return [
        line.strip()
        for line in path.read_text(encoding="utf-8").splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    ]


def stable_id(payload: dict[str, Any]) -> str:
    encoded = json.dumps(payload, ensure_ascii=False, sort_keys=True).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()[:20]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()

    config_path = args.config.resolve()
    config = json.loads(config_path.read_text(encoding="utf-8"))
    if config.get("schema_version") != 1:
        raise SystemExit("unsupported config schema_version")

    positive_file = (config_path.parent / config["positive_phrases_file"]).resolve()
    contexts_file = (config_path.parent / config["contexts_file"]).resolve()
    negative_file = (config_path.parent / config["hard_negative_phrases_file"]).resolve()
    stress_file = (config_path.parent / config["stress_only_phrases_file"]).resolve()
    voices = config.get("voices", [])
    rates = config.get("speaking_rates", [1.0])
    seeds = int(config.get("seeds_per_variant", 1))
    if not voices or not rates or seeds < 1:
        raise SystemExit("config must define voices, speaking_rates, and positive seeds_per_variant")

    jobs: list[dict[str, Any]] = []
    groups = (
        ("positive", read_phrases(positive_file) + read_phrases(contexts_file)),
        ("hard_negative", read_phrases(negative_file)),
        ("stress_only", read_phrases(stress_file)),
    )
    for label, phrases in groups:
        for phrase in phrases:
            for voice in voices:
                for rate in rates:
                    voice_seeds = int(config.get("seeds_per_voice", {}).get(voice, seeds))
                    for seed in range(voice_seeds):
                        item = {
                            "label": label,
                            "text": phrase,
                            "voice": voice,
                            "speaker_group": config.get("voice_groups", {}).get(voice, "unclassified_synthetic"),
                            "speaking_rate": rate,
                            "seed": seed,
                            "sample_rate_hz": config["sample_rate_hz"],
                            "channels": config["channels"],
                            "sample_format": config["sample_format"],
                        }
                        identity = {key: value for key, value in item.items() if key != "speaker_group"}
                        jobs.append(
                            {
                                "job_id": stable_id(identity),
                                **item,
                                "output_relative_path": f"{config['output_dir']}/{label}/{stable_id(identity)}.wav",
                            }
                        )

    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.output.open("w", encoding="utf-8", newline="\n") as output:
        for job in jobs:
            output.write(json.dumps(job, ensure_ascii=False, sort_keys=True) + "\n")
    print(f"created {len(jobs)} jobs: {args.output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
