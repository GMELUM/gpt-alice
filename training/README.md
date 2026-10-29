# Wake-word training groundwork

This directory contains the reproducible data-preparation scaffold and experimental training recipe for a Russian wake-word detector. The first target phrase is `Алиса`. A Rust streaming inference path exists in `native/gpt-live-host`; its auto-call setting stays disabled until the model achieves a usable false-accept/recall tradeoff.

## Training approach

Use synthetic speech as a way to expand pronunciation and speaker variation, not as the entire dataset. Generate the positive phrase with multiple Russian voices, speaking rates, intonations, and phrase positions. Add real recordings through the Mini 3 microphones before treating a candidate as usable; TTS alone will not capture the Station's microphone, enclosure, room, playback, and distance characteristics.

Use two distinct negative pools:

- **Hard speech negatives:** confusable names and phrases, ordinary Russian speech, and phrases that contain similar syllables. Keep the wake phrase itself out of this pool unless it is spoken as part of a longer, non-addressed utterance and is labeled deliberately.
- **Stress-only phrases:** genuinely ambiguous near-homophones such as `а лиса`. Score these separately first. The acoustics may be indistinguishable from `Алиса`; training them as negatives can reduce recall for the real trigger, and no acoustic model can infer intent from identical sound alone.
- **Ambient negatives:** music, television/radio speech, city and household sounds, silence, and the speaker's own playback. Mix backgrounds into positive speech at varied signal-to-noise ratios, and also test long background-only recordings.

One hundred songs can be a seed set, but it is too small and narrow to represent a reliable negative corpus. Prefer licensed/public-domain or self-recorded sources, record their exact provenance and license, and avoid using a source in both training and held-out evaluation. Do not split overlapping crops of the same recording across train/validation/test.

Keep validation and test material frozen. Split by TTS voice / real speaker and background recording before creating augmented clips; never let the same speaker or source recording leak across splits. Evaluate recall, false rejects, false accepts per hour, and trigger latency at several thresholds. Choose a threshold on validation data, then report the final metrics once on the untouched test set. Also test long continuous audio in streaming mode and the actual Mini 3 microphone path.

## Repository layout

```text
training/
  config.example.json       target phrase and corpus settings
  phrases/                  positive, hard-negative, and stress-only text lists
  jobs/                     generated synthesis job manifests (local output)
  corpus/                   generated / recorded WAVs and metadata (local output)
  reports/                  metrics and model candidates (local output)
  scripts/create_tts_jobs.py
  scripts/synthesize_tts_jobs.py
  scripts/make_pitch_augmented.py
  scripts/mix_backgrounds.py
  scripts/prepare_music_backgrounds.py
  scripts/synthesize_teratts_jobs.py
  scripts/preview_teratts_voices.py
  scripts/run_voice_recorder.sh
  ../web/training.html       browser recording UI
  ../src/training.rs        device capture, WAV conversion, download and metadata writer
  ../src/bin/training_recorder.rs  localhost-only browser-microphone fallback
  assets/ambience/          small CC0/public-domain seed recordings
  voices/SOURCES.md         voice provenance and licensing notes
  requirements-teratts.txt  small CPU runtime dependency list for TeraTTSv2
```

Audio, downloaded corpora, generated jobs, model weights, and reports are local training data and are ignored by Git. Commit only recipes, scripts, small text lists, and documentation. Record a source URL, license, voice/model identity, generation settings, and content hash in each sample's metadata.

## First automation step

Create a deterministic JSONL job list from the configured positive and negative phrases and a list of TTS voice identifiers:

```sh
python3 training/scripts/create_tts_jobs.py \
  --config training/config.example.json \
  --output training/jobs/tts-jobs.jsonl
```

The current recipe expands to 880 jobs: 605 positives (the wake word alone and in varied short contexts), 220 hard negatives, and 55 ambiguous stress-only samples. There are seven voice sources and five speaking-rate multipliers from 0.72 (slow) through 1.3 (fast); Piper length scale and TeraTTS duration scale are calculated inversely. The deterministic macOS voice and TeraTTS styles use one seed per rate to avoid duplicate takes. A multiplier is a requested TTS control value, not a guarantee every engine will produce exactly the same acoustic rate.

The job ID is stable for the same phrase, voice, rate, and seed. It is a synthesis manifest, not evidence that all 880 WAVs already exist. Synthesis and noisy mixing are separate pipeline stages.

### Record your own voice

On the Mini 3, open `http://<speaker-ip>/training`. The form asks for only the word `Алиса`; vary it naturally between takes (for example, whisper, call loudly, or speak from farther away). No pace or intonation labels are requested or stored. The page records directly from the speaker's four-channel microphone array with `arecord` at 16 kHz / S16_LE, downmixes each frame to mono, and stores a WAV plus JSON metadata beside the application in `training/corpus/recorded/`. Metadata keeps the fixed phrase, measured audio duration, source/microphone, speaker group, and hash. The speaker page does not need browser microphone permissions. Stop each take after speaking (the hard limit is eight seconds); use the links after the take to download the WAV and metadata. While capture is active, the wake-word detector is paused and calls are rejected so they cannot contend for ALSA.

The local Mac recorder remains available as a fallback:

Run the local recorder on the Mac from the repository root:

```sh
sh training/scripts/run_voice_recorder.sh
```

Then open `http://127.0.0.1:8170`. The separate recorder binds only to loopback, which lets the browser grant microphone access without exposing the recording page to the LAN. This fallback records from the Mac's selected microphone; it does not use the Mini 3 array.

Say only `Алиса` and vary the delivery naturally between takes. The browser fallback captures the selected Mac microphone, converts audio to a 16 kHz mono PCM WAV, and lets you preview before saving. Its JSON sidecar contains the fixed phrase, measured audio duration, random local speaker ID, source/microphone, format, and verified SHA-256; intonation and pace are not recorded. No name or account identifier is collected. Both files go into the ignored `training/corpus/recorded/` directory; set `GPT_ALICE_TRAINING_DIR` to use another local directory. Captures are not automatically added to the synthetic job manifest or mixed with noise.

### Generate the speech corpus

The configured voices include two Piper Russian adult male voices (denis and dmitri), the installed macOS Russian voice Milena, and four TeraTTSv2 Russian styles: two female and two male. The TeraTTSv2 model card recommends `ru_f1` and `ru_m5`; the other two styles broaden speaker variation. Piper and TeraTTS model files and their Python environments stay local and are gitignored; the repository stores only scripts, pinned model revision, configuration, and attribution notes. Piper's denis and dmitri model cards identify their source datasets as CC0. TeraTTSv2 has an OpenRAIL-M license with a required public attribution condition for commercial products. Review the model license before redistributing generated corpus data or using it in a product. [Piper model list](https://github.com/OHF-Voice/piper1-gpl/blob/main/docs/VOICES.md) [denis model card](https://huggingface.co/rhasspy/piper-voices/blob/main/ru/ru_RU/denis/medium/MODEL_CARD) [dmitri model card](https://huggingface.co/rhasspy/piper-voices/blob/main/ru/ru_RU/dmitri/medium/MODEL_CARD) [TeraTTSv2 model card and license](https://huggingface.co/TeraSpace/TeraTTSv2)

```sh
uv venv --python 3.12 training/.venv
uv pip install --python training/.venv/bin/python piper-tts
training/scripts/download_piper_voices.sh
python3 training/scripts/create_tts_jobs.py \
  --config training/config.example.json \
  --output training/jobs/tts-jobs.jsonl
training/.venv/bin/python training/scripts/synthesize_tts_jobs.py \
  --jobs training/jobs/tts-jobs.jsonl
uv pip install --python training/.venv/bin/python -r training/requirements-teratts.txt
training/.venv/bin/python training/scripts/synthesize_teratts_jobs.py \
  --jobs training/jobs/tts-jobs.jsonl
python3 training/scripts/mix_backgrounds.py \
  --jobs training/jobs/tts-jobs.jsonl
```

TeraTTSv2 synthesis is CPU-only ONNX Runtime, uses the distilled sampler and dictionary Russian stress mode, and downloads only the pinned model revision and required assets into ignored `training/models/huggingface/`. The first invocation downloads several hundred megabytes. Use `--only-voice teratts:ru_m5 --limit 3` for a small initial preview; omit the limit to synthesize all configured TeraTTS jobs. The model emits 44.1 kHz audio which the script converts to the corpus's standard 16 kHz mono PCM WAV. The original Milena and Piper cohorts remain in place as separate voice/model groups.

For a quick, same-text comparison without downloading the model weights, the public TeraTTSv2 demo can create four voices at three rates:

```sh
python3 training/scripts/preview_teratts_voices.py
```

This creates 12 small preview clips under ignored `training/corpus/previews/teratts/`; default text is `Алиса!`, with female styles `ru_f1`/`ru_f2`, male styles `ru_m1`/`ru_m5`, and slow/normal/fast pacing. The script uses only this fixed wake word by default and writes a provenance sidecar beside every WAV. The demo is an external service; use local synthesis for bulk generation and avoid sending private text to it.

`say:Milena` uses the macOS built-in Russian voice and requires macOS. Piper produces the two adult male voice cohorts. There is no suitable openly licensed Russian child voice configured here. Do not label pitch-shifted adult speech as child speech; if child speech is needed, add a separately consented, speaker-labeled recording cohort or a verified licensed voice source. Keep voice/model identity in every record and split evaluations by voice so synthetic voices cannot leak between train and test.

To make additional high-pitch robustness samples from the available female TTS clips, run `python3 training/scripts/make_pitch_augmented.py`. It creates +3 and +5 semitone variants and marks them `pitch_shifted_proxy_not_child`; they are stress proxies only and should not be presented as child-speaker coverage.

`mix_backgrounds.py` keeps the original clean TTS files and emits variants at −5, 0, 5, 10, and 20 dB SNR with variable leading/trailing silence. It uses the local music excerpts and committed ambient seeds, and records the selected background source ID. If `pitch-jobs.jsonl` exists, those stress proxies are mixed too. This creates useful augmentation data; it is not yet the model trainer, streaming evaluator, or device microphone capture pipeline.

## Current prepared corpus snapshot

- 100 distinct music tracks produced 300 local background excerpts.
- The repository contains three short 16 kHz mono environment seeds: city rain/traffic, rain/thunder, and wind.
- All 880 configured clean TTS WAVs have now been generated: 240 each for Piper `denis` and `dmitri`, 80 for macOS `Milena`, and 80 for each of four TeraTTSv2 adult styles (`ru_f1`, `ru_f2`, `ru_m1`, `ru_m5`). This includes positive, hard-negative, and stress-only phrases across the configured speaking rates.
- 12 TeraTTSv2 preview WAVs were generated from the public demo: the same wake word in four adult voice styles (two female, two male) at three rates. These are a voice-comparison preview, not the full phrase corpus.
- 110 high-pitch proxy clips were derived from the positive Milena clips; these are tagged as synthetic pitch stress, not child speech.
- 3,575 positive speech-plus-background WAVs were generated at five SNR levels. The full mixed set includes TTS jobs plus pitch-stress positives.
- 22 user-recorded positive WAVs are saved separately with per-take metadata. The configured speech jobs and mixed audio are local ignored data. An experimental openWakeWord classification-head pilot has been trained locally, but no detector has been deployed to the Mini 3.

## Experimental openWakeWord pilot

Run the small CPU pilot after generating the configured TTS corpus, noisy mixes, music excerpts, and personal recordings:

```sh
training/.venv/bin/python training/scripts/train_openwakeword_pilot.py --steps 600
```

To prepare a new classifier for the runtime model folder after reviewing its metrics, copy the exported head and rebuild the release package:

```sh
cp training/reports/alice-openwakeword-pilot/alice_openwakeword_head.onnx \
  assets/wakeword/alice_openwakeword_head.onnx
```

The script freezes the openWakeWord audio embedding frontend and trains a small DNN classification head for `Алиса`. It separates TTS voices and original background tracks across train/validation/test splits; the 22 personal recordings are held out from fitting and threshold selection. Results and intermediate feature arrays are written under the ignored `training/reports/alice-openwakeword-pilot/` directory. The ONNX artifact is only the classification head: the Rust runtime also loads the matching mel and embedding models to produce the same 16x96 features. The inference path is experimental and has not been deployed to Mini 3.

This is exploratory Russian-language work. The pilot achieved 0.991 validation AUC and 98.2% recall on held-out synthetic test speech at threshold 0.5, but that threshold also caused 50–160 background false activations per hour in validation; recall on the 22 held-out personal recordings was 77.3%. Tightening the threshold to the pilot's <=1 false activation/hour target reduced validation, synthetic test, and personal-recording recall to zero. The continuous test background sample was only 0.1 hours, so its zero false activations do not establish a useful operating point. Do not deploy this candidate. Collect longer representative background recordings, more real voices and Mini 3 microphone samples, and improve the training/thresholding recipe before choosing a runtime. Upstream openWakeWord docs currently describe English wake-word training; the upstream custom-model config recommends at least 20,000 positive examples, far beyond this small pilot. See [openWakeWord README](https://github.com/dscripka/openWakeWord) and [custom model configuration](https://github.com/dscripka/openWakeWord/blob/main/examples/custom_model.yml).

## Local music backgrounds

The music library is never copied into the repository. The script picks 100 distinct audio files deterministically, extracts three random 12-second excerpts from each, converts them to 16 kHz mono PCM WAV, and writes the excerpts and a provenance JSONL into the ignored `training/corpus/` directory:

```sh
python3 training/scripts/prepare_music_backgrounds.py \
  --music-root '/path/to/Music/Media.localized/Music'
```

The source track names and paths stay in the local ignored manifest. Treat excerpts from personal commercial music as local/private research data; do not publish them or include them in a public release. For shareable training/evaluation data, use recordings with a license that permits redistribution and document it. Starter CC0/public-domain environment samples and source details are in `assets/ambience/`.

## Runtime deployment validation remains

The Mini 3 audio helper now has an experimental Rust streaming path that loads the mel, embedding, and classifier ONNX files from `wakeword/` beside the executable. It owns the microphone only while idle, stops before GPT-Live opens the device, and can start a call after three consecutive scores meet the configured threshold. Automatic listening is disabled in the checked-in settings because the pilot has no usable false-accept/recall tradeoff. Measure CPU/RAM, latency, and false activations on the actual Mini 3 before enabling it. Replacing model files does not require rebuilding the binary, but restart the service after replacing them.
