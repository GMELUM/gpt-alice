#!/usr/bin/env python3
"""Train an explicitly experimental openWakeWord classification head on local WAVs."""

from __future__ import annotations

import argparse
import hashlib
import json
import random
import wave
from collections import defaultdict
from pathlib import Path
from typing import Any

import numpy as np
import torch
from scipy.io import wavfile
from sklearn.metrics import roc_auc_score

from openwakeword.utils import AudioFeatures

ROOT = Path(__file__).resolve().parents[2]
RATE = 16_000
WINDOW = RATE * 2
FRAMES = 16
EMBEDDING_DIM = 96
TRAIN_VOICES = {
    "piper:ru_RU-denis-medium",
    "piper:ru_RU-dmitri-medium",
    "say:Milena",
    "teratts:ru_f1",
    "teratts:ru_f2",
}
VALIDATION_VOICE = "teratts:ru_m1"
TEST_VOICE = "teratts:ru_m5"


class WakeWordHead(torch.nn.Module):
    """Small openWakeWord-style DNN over the frozen 16x96 embedding window."""

    def __init__(self) -> None:
        super().__init__()
        self.flatten = torch.nn.Flatten()
        self.layer1 = torch.nn.Linear(FRAMES * EMBEDDING_DIM, 32)
        self.relu1 = torch.nn.ReLU()
        self.layernorm1 = torch.nn.LayerNorm(32)
        self.block_linear = torch.nn.Linear(32, 32)
        self.block_relu = torch.nn.ReLU()
        self.block_norm = torch.nn.LayerNorm(32)
        self.last_layer = torch.nn.Linear(32, 1)
        self.last_act = torch.nn.Sigmoid()

    def forward(self, features: torch.Tensor) -> torch.Tensor:
        x = self.flatten(features)
        x = self.relu1(self.layernorm1(self.layer1(x)))
        x = self.block_relu(self.block_norm(self.block_linear(x)))
        return self.last_act(self.last_layer(x))


def read_wav(path: Path) -> np.ndarray:
    rate, audio = wavfile.read(path)
    if rate != RATE or audio.ndim != 1 or audio.dtype != np.int16:
        raise ValueError(f"Expected 16 kHz mono PCM16 WAV: {path}")
    return audio


def crop_window(
    audio: np.ndarray,
    *,
    text: str | None = None,
    leading_silence: float = 0.0,
    trailing_silence: float = 0.0,
    center_fraction: float | None = None,
) -> np.ndarray:
    duration = len(audio) / RATE
    if center_fraction is None and text and "алиса" in text.lower():
        start = text.lower().find("алиса")
        center_fraction = (start + len("алиса") / 2) / max(1, len(text))
    if center_fraction is None:
        center_fraction = 0.5
    speech_duration = max(0.0, duration - leading_silence - trailing_silence)
    center = leading_silence + speech_duration * min(1.0, max(0.0, center_fraction))
    start_sample = round(center * RATE - WINDOW / 2)
    source_start = max(0, start_sample)
    source_end = min(len(audio), start_sample + WINDOW)
    result = np.zeros(WINDOW, dtype=np.int16)
    if source_end > source_start:
        target_start = source_start - start_sample
        result[target_start : target_start + source_end - source_start] = audio[source_start:source_end]
    return result


def embed_windows(features: AudioFeatures, windows: list[np.ndarray], batch_size: int = 32) -> np.ndarray:
    if not windows:
        return np.empty((0, FRAMES, EMBEDDING_DIM), dtype=np.float32)
    output: list[np.ndarray] = []
    for offset in range(0, len(windows), batch_size):
        batch = np.stack(windows[offset : offset + batch_size]).astype(np.int16, copy=False)
        embedded = features.embed_clips(batch, batch_size=batch_size, ncpu=2)
        if embedded.shape[1:] != (FRAMES, EMBEDDING_DIM):
            raise ValueError(f"Unexpected feature shape {embedded.shape}; expected (*, 16, 96)")
        output.append(embedded.astype(np.float32, copy=False))
    return np.concatenate(output, axis=0)


def load_jobs() -> list[dict[str, Any]]:
    path = ROOT / "training/jobs/tts-jobs.jsonl"
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]


def prepare_data(features: AudioFeatures, report_dir: Path, seed: int) -> dict[str, Any]:
    rng = random.Random(seed)
    jobs = load_jobs()
    mixed_rows = [
        json.loads(line)
        for line in (ROOT / "training/corpus/mixed/mixed-manifest.jsonl").read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]
    mixed_by_job: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for row in mixed_rows:
        if row["snr_db"] == 5:
            mixed_by_job[row["job_id"]].append(row)

    windows: dict[str, list[np.ndarray]] = defaultdict(list)
    records: dict[str, list[str]] = defaultdict(list)
    target_jobs: dict[str, list[dict[str, Any]]] = defaultdict(list)
    split_for_voice = {
        **{voice: "train" for voice in TRAIN_VOICES},
        VALIDATION_VOICE: "validation",
        TEST_VOICE: "test",
    }
    for job in jobs:
        voice = job["voice"]
        split = split_for_voice.get(voice)
        if not split:
            continue
        source = ROOT / "training" / job["output_relative_path"]
        if not source.is_file():
            continue
        target_jobs[split].append(job)
        wav = read_wav(source)
        label = job["label"]
        if label == "positive":
            windows[f"{split}_positive"].append(crop_window(wav, text=job["text"]))
            records[f"{split}_positive"].append(job["job_id"])
            if split in {"train", "validation"}:
                for row in mixed_by_job.get(job["job_id"], [])[:1]:
                    mixed_path = ROOT / "training/corpus/mixed" / row["clip"]
                    mixed_audio = read_wav(mixed_path)
                    windows[f"{split}_positive"].append(
                        crop_window(
                            mixed_audio,
                            text=row["text"],
                            leading_silence=row["leading_silence_seconds"],
                            trailing_silence=row["trailing_silence_seconds"],
                        )
                    )
                    records[f"{split}_positive"].append(job["job_id"] + ":snr+5")
        elif label == "hard_negative":
            windows[f"{split}_negative"].append(crop_window(wav, center_fraction=rng.uniform(0.2, 0.8)))
            records[f"{split}_negative"].append(job["job_id"])
        elif label == "stress_only" and split == "test":
            windows["test_stress_negative"].append(crop_window(wav, center_fraction=rng.uniform(0.2, 0.8)))
            records["test_stress_negative"].append(job["job_id"])

    # Split music by the original track ID so excerpts from one track never cross splits.
    background_dir = ROOT / "training/corpus/background/music"
    background_rows = [
        json.loads(line)
        for line in (background_dir / "music-backgrounds.jsonl").read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]
    source_ids = sorted({row["source_id"] for row in background_rows})
    source_split: dict[str, str] = {}
    for index, source_id in enumerate(source_ids):
        bucket = index % 10
        source_split[source_id] = "train" if bucket < 8 else ("validation" if bucket == 8 else "test")

    background_windows: dict[str, list[np.ndarray]] = defaultdict(list)
    background_streams: dict[str, list[tuple[str, np.ndarray, float]]] = defaultdict(list)
    row_by_clip = {row["clip"]: row for row in background_rows}
    for path in sorted(background_dir.glob("*.wav")):
        row = row_by_clip[path.name]
        split = source_split[row["source_id"]]
        audio = read_wav(path)
        background_streams[split].append((path.name, audio, len(audio) / RATE))
        if len(audio) >= WINDOW:
            background_windows[split].append(crop_window(audio, center_fraction=0.25))
            background_windows[split].append(crop_window(audio, center_fraction=0.75))

    report_dir.mkdir(parents=True, exist_ok=True)
    embedded: dict[str, np.ndarray] = {}
    for key, values in windows.items():
        embedded[key] = embed_windows(features, values)
    for split, values in background_windows.items():
        embedded[f"{split}_background_negative"] = embed_windows(features, values)
    for split, streams in background_streams.items():
        feature_streams: list[tuple[str, np.ndarray, float]] = []
        for name, audio, duration in streams:
            result = features.embed_clips(audio[None, :].astype(np.int16, copy=False), batch_size=1, ncpu=2)[0]
            feature_streams.append((name, result.astype(np.float32, copy=False), duration))
        background_streams[split] = feature_streams

    for key, value in embedded.items():
        np.save(report_dir / f"{key}.npy", value)

    # Real recordings stay entirely out of fitting and threshold selection.
    real_windows: list[np.ndarray] = []
    real_names: list[str] = []
    recorded_dir = ROOT / "training/corpus/recorded"
    for metadata_path in sorted(recorded_dir.glob("*.json")):
        metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
        wav_path = metadata_path.with_suffix(".wav")
        if not wav_path.is_file() or metadata.get("label") != "positive":
            continue
        audio = read_wav(wav_path)
        real_windows.append(crop_window(audio, text=metadata.get("text", "")))
        real_names.append(wav_path.name)
    embedded["real_positive"] = embed_windows(features, real_windows)
    np.save(report_dir / "real_positive.npy", embedded["real_positive"])

    split_report = {
        "seed": seed,
        "train_voices": sorted(TRAIN_VOICES),
        "validation_voice": VALIDATION_VOICE,
        "test_voice": TEST_VOICE,
        "background_source_counts": {
            split: sum(1 for value in source_split.values() if value == split)
            for split in ("train", "validation", "test")
        },
        "sample_counts": {key: len(value) for key, value in windows.items()},
        "background_window_counts": {key: len(value) for key, value in background_windows.items()},
        "background_stream_count": {key: len(value) for key, value in background_streams.items()},
        "real_recording_test_count": len(real_windows),
        "real_recording_files": real_names,
    }
    (report_dir / "split.json").write_text(json.dumps(split_report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    embedded["_background_streams"] = background_streams  # type: ignore[assignment]
    embedded["_split_report"] = split_report  # type: ignore[assignment]
    return embedded


def scores_for_windows(model: torch.nn.Module, features: np.ndarray, batch_size: int = 256) -> np.ndarray:
    if len(features) == 0:
        return np.empty((0,), dtype=np.float32)
    model.eval()
    output: list[np.ndarray] = []
    with torch.no_grad():
        for offset in range(0, len(features), batch_size):
            batch = torch.from_numpy(features[offset : offset + batch_size]).float()
            output.append(model(batch).squeeze(-1).cpu().numpy())
    return np.concatenate(output)


def stream_false_positives(model: torch.nn.Module, streams: list[tuple[str, np.ndarray, float]], threshold: float) -> tuple[int, float]:
    false_positives = 0
    duration_hours = sum(duration for _, _, duration in streams) / 3600
    cooldown_frames = round(2.0 / 0.08)
    for _name, feature_stream, _duration in streams:
        windows = np.stack([feature_stream[i : i + FRAMES] for i in range(max(0, len(feature_stream) - FRAMES + 1))])
        scores = scores_for_windows(model, windows)
        last_counted = -cooldown_frames - 1
        for index, score in enumerate(scores):
            if score >= threshold and index - last_counted >= cooldown_frames:
                false_positives += 1
                last_counted = index
    return false_positives, duration_hours


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--steps", type=int, default=600, help="Small CPU-only pilot; this is not production training")
    parser.add_argument("--batch-size", type=int, default=64)
    parser.add_argument("--seed", type=int, default=20261029)
    parser.add_argument("--output", type=Path, default=ROOT / "training/reports/alice-openwakeword-pilot")
    args = parser.parse_args()
    random.seed(args.seed)
    np.random.seed(args.seed)
    torch.manual_seed(args.seed)

    feature_root = ROOT / "training/models/openwakeword"
    model_features = AudioFeatures(
        melspec_model_path=str(feature_root / "melspectrogram.onnx"),
        embedding_model_path=str(feature_root / "embedding_model.onnx"),
        device="cpu",
        ncpu=2,
    )
    print("Extracting openWakeWord features from clean/noisy WAVs…", flush=True)
    data = prepare_data(model_features, args.output, args.seed)
    train_positive = data["train_positive"]
    train_negative = np.concatenate([data["train_negative"], data["train_background_negative"]])
    val_positive = data["validation_positive"]
    val_negative = np.concatenate([data["validation_negative"], data["validation_background_negative"]])
    if min(len(train_positive), len(train_negative), len(val_positive), len(val_negative)) == 0:
        raise SystemExit("A train or validation class has no samples; refusing to train an invalid model")

    model = WakeWordHead()
    optimizer = torch.optim.Adam(model.parameters(), lr=1e-4)
    loss_fn = torch.nn.BCELoss()
    best_auc = -1.0
    best_state: dict[str, torch.Tensor] | None = None
    batch_size = args.batch_size
    positive_batch = batch_size // 2
    negative_batch = batch_size - positive_batch
    val_y = np.concatenate([np.ones(len(val_positive)), np.zeros(len(val_negative))])
    val_x = np.concatenate([val_positive, val_negative])
    checkpoints: list[dict[str, float]] = []
    validation_streams = data["_background_streams"]["validation"]
    for step in range(1, args.steps + 1):
        pos_indices = np.random.randint(0, len(train_positive), size=positive_batch)
        neg_indices = np.random.randint(0, len(train_negative), size=negative_batch)
        batch_x = np.concatenate([train_positive[pos_indices], train_negative[neg_indices]])
        batch_y = torch.from_numpy(np.concatenate([np.ones(positive_batch), np.zeros(negative_batch)])).float().unsqueeze(1)
        permutation = torch.randperm(len(batch_x))
        x = torch.from_numpy(batch_x[permutation.numpy()]).float()
        y = batch_y[permutation]
        model.train()
        optimizer.zero_grad(set_to_none=True)
        predictions = model(x)
        loss = loss_fn(predictions, y)
        loss.backward()
        optimizer.step()

        if step == 1 or step % 50 == 0 or step == args.steps:
            val_scores = scores_for_windows(model, val_x)
            auc = float(roc_auc_score(val_y, val_scores))
            val_accuracy = float(((val_scores >= 0.5) == val_y).mean())
            fp_count, val_hours = stream_false_positives(model, validation_streams, 0.5)
            faph = fp_count / val_hours if val_hours else None
            checkpoints.append({"step": step, "loss": float(loss.item()), "validation_auc": auc, "validation_accuracy_at_0_5": val_accuracy, "validation_faph_at_0_5": faph or 0.0})
            print(f"step {step}/{args.steps}: loss={loss.item():.4f}, val_auc={auc:.3f}, val_acc={val_accuracy:.3f}, val_FA/h@0.5={faph}", flush=True)
            if auc > best_auc:
                best_auc = auc
                best_state = {key: value.detach().clone() for key, value in model.state_dict().items()}

    assert best_state is not None
    model.load_state_dict(best_state)
    model.eval()

    validation_scores = scores_for_windows(model, val_x)
    candidate_thresholds = np.linspace(0.05, 0.99, 95)
    threshold = float(candidate_thresholds[-1])
    for candidate in candidate_thresholds:
        count, hours = stream_false_positives(model, validation_streams, float(candidate))
        faph = count / hours if hours else float("inf")
        recall = float((scores_for_windows(model, val_positive) >= candidate).mean())
        if faph <= 1.0:
            threshold = float(candidate)
            if recall >= 0.8:
                break

    def binary_metrics(positive: np.ndarray, negative: np.ndarray, cutoff: float) -> dict[str, float]:
        pos_scores = scores_for_windows(model, positive)
        neg_scores = scores_for_windows(model, negative)
        return {
            "positive_recall": float((pos_scores >= cutoff).mean()) if len(pos_scores) else 0.0,
            "negative_false_accept_rate": float((neg_scores >= cutoff).mean()) if len(neg_scores) else 0.0,
            "positive_count": int(len(pos_scores)),
            "negative_count": int(len(neg_scores)),
        }

    test_negative = data.get("test_negative", np.empty((0, FRAMES, EMBEDDING_DIM), dtype=np.float32))
    stress_negative = data.get("test_stress_negative", np.empty((0, FRAMES, EMBEDDING_DIM), dtype=np.float32))
    test_streams = data["_background_streams"]["test"]
    test_fp_count, test_hours = stream_false_positives(model, test_streams, threshold)
    test_metrics = binary_metrics(data["test_positive"], test_negative, threshold)
    test_metrics["stress_only_false_accept_rate"] = float((scores_for_windows(model, stress_negative) >= threshold).mean()) if len(stress_negative) else 0.0
    test_metrics["real_recorded_positive_recall"] = float((scores_for_windows(model, data["real_positive"]) >= threshold).mean()) if len(data["real_positive"]) else 0.0
    test_metrics["background_false_positives"] = int(test_fp_count)
    test_metrics["background_hours"] = float(test_hours)
    test_metrics["background_false_positives_per_hour"] = float(test_fp_count / test_hours) if test_hours else None

    model_path = args.output / "alice_openwakeword_head.onnx"
    checkpoint_path = args.output / "alice_openwakeword_head.pt"
    torch.save(model.state_dict(), checkpoint_path)
    torch.onnx.export(
        model.eval(),
        torch.zeros(1, FRAMES, EMBEDDING_DIM),
        str(model_path),
        input_names=["features"],
        output_names=["probability"],
        dynamic_axes={"features": {0: "batch"}, "probability": {0: "batch"}},
        opset_version=13,
        dynamo=False,
    )
    report = {
        "status": "experimental_pilot",
        "model": model_path.name,
        "format": "ONNX classification head; requires openWakeWord-compatible 16x96 embeddings",
        "feature_backbone": "openWakeWord 0.6.0 embedding and melspectrogram ONNX models",
        "warning": "Russian is outside openWakeWord's officially supported language set. Metrics are exploratory and do not establish Mini 3 performance.",
        "training": {
            "seed": args.seed,
            "steps": args.steps,
            "batch_size": batch_size,
            "train_positive_windows": int(len(train_positive)),
            "train_negative_windows": int(len(train_negative)),
            "best_validation_auc": float(best_auc),
            "selected_threshold": threshold,
            "checkpoint_history": checkpoints,
        },
        "validation": binary_metrics(val_positive, val_negative, threshold),
        "test": test_metrics,
        "split": data["_split_report"],
    }
    (args.output / "metrics.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(f"Saved experimental model: {model_path}", flush=True)
    print(f"Saved metrics: {args.output / 'metrics.json'}", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
