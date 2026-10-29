#!/usr/bin/env python3
"""Create a small, same-text voice comparison from the public TeraTTSv2 demo."""

from __future__ import annotations

import argparse
import json
import subprocess
import tempfile
import time
import urllib.request
from pathlib import Path
from typing import Optional

SPACE = "https://teratts-tts.hf.space"
MODEL_ID = "TeraSpace/TeraTTSv2"
VOICES = ("ru_f1", "ru_f2", "ru_m1", "ru_m5")
RATES = (0.72, 1.0, 1.3)
TEXT = "Алиса!"


def request_json(url: str, payload: dict, timeout: int = 30) -> dict:
    request = urllib.request.Request(
        url,
        data=json.dumps(payload).encode("utf-8"),
        headers={"Content-Type": "application/json", "User-Agent": "gpt-alice-training-preview/1.0"},
        method="POST",
    )
    with urllib.request.urlopen(request, timeout=timeout) as response:
        return json.loads(response.read().decode("utf-8"))


def wait_for_result(event_id: str, timeout: int = 180) -> list:
    url = f"{SPACE}/gradio_api/call/synthesize/{event_id}"
    request = urllib.request.Request(url, headers={"User-Agent": "gpt-alice-training-preview/1.0"})
    with urllib.request.urlopen(request, timeout=timeout) as response:
        for raw_line in response:
            line = raw_line.decode("utf-8").strip()
            if line.startswith("data: "):
                payload = json.loads(line[6:])
                if isinstance(payload, dict) and payload.get("error"):
                    raise RuntimeError(f"TeraTTSv2 demo failed: {payload['error']}")
                return payload
    raise RuntimeError("TeraTTSv2 demo closed without returning audio")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output-dir", type=Path, default=Path("training/corpus/previews/teratts"))
    parser.add_argument("--text", default=TEXT, help="Same Russian text is synthesized for each style/rate")
    parser.add_argument("--voices", nargs="+", choices=VOICES, default=list(VOICES))
    parser.add_argument("--rates", nargs="+", type=float, default=list(RATES))
    args = parser.parse_args()
    if not shutil_which("ffmpeg"):
        raise SystemExit("ffmpeg is required for 16 kHz mono PCM normalization")
    args.output_dir.mkdir(parents=True, exist_ok=True)

    created = 0
    for voice in args.voices:
        for rate in args.rates:
            if rate <= 0 or not 0.5 <= 1.0 / rate <= 1.5:
                raise SystemExit(f"rate {rate} maps outside the demo duration_scale range 0.5–1.5")
            duration_scale = 1.0 / rate
            output = args.output_dir / f"{voice}-rate-{rate:.2f}.wav"
            if output.is_file() and output.with_suffix(".json").is_file():
                print(f"skip {output}", flush=True)
                continue

            queued = request_json(
                f"{SPACE}/gradio_api/call/synthesize",
                {"data": [f"<ru>{args.text}</ru>", voice, "distilled", "dictionary", duration_scale]},
            )
            result = wait_for_result(queued["event_id"])
            audio_data = result[0]
            audio_url = audio_data.get("url") if isinstance(audio_data, dict) else None
            if not audio_url:
                raise RuntimeError(f"TeraTTSv2 returned no audio URL for {voice} at rate {rate}")

            request = urllib.request.Request(audio_url, headers={"User-Agent": "gpt-alice-training-preview/1.0"})
            with urllib.request.urlopen(request, timeout=60) as response:
                raw_bytes = response.read()
            with tempfile.NamedTemporaryFile(suffix=".wav") as raw_audio:
                raw_audio.write(raw_bytes)
                raw_audio.flush()
                subprocess.run(
                    [
                        "ffmpeg", "-hide_banner", "-loglevel", "error", "-y", "-i", raw_audio.name,
                        "-ar", "16000", "-ac", "1", "-c:a", "pcm_s16le", str(output),
                    ],
                    check=True,
                )
            metadata = {
                "text": args.text,
                "voice": f"teratts:{voice}",
                "speaker_group": "adult_female_neural_tts" if "_f" in voice else "adult_male_neural_tts",
                "speaking_rate": rate,
                "duration_scale": duration_scale,
                "sample_rate_hz": 16000,
                "channels": 1,
                "sample_format": "pcm_s16le",
                "model_id": MODEL_ID,
                "model_license": "OpenRAIL-M with commercial attribution condition",
                "generation_source": SPACE,
                "generation_mode": "distilled; dictionary Russian stress",
            }
            output.with_suffix(".json").write_text(
                json.dumps(metadata, ensure_ascii=False, sort_keys=True) + "\n", encoding="utf-8"
            )
            created += 1
            print(f"created {output}", flush=True)
            time.sleep(0.4)

    print(f"created {created} preview clips in {args.output_dir}")
    return 0


def shutil_which(command: str) -> Optional[str]:
    from shutil import which

    return which(command)


if __name__ == "__main__":
    raise SystemExit(main())
