# Wake-word model files

The runtime loads these files from `wakeword/` beside `gpt-live-host`:

- `melspectrogram.onnx` and `embedding_model.onnx` are the shared openWakeWord 0.6.0 feature models.
- `alice_openwakeword_head.onnx` is the experimental Russian `Алиса` classifier from the local pilot.
- `settings.json` controls automatic listening and the classifier threshold. The checked-in candidate is disabled by default.

The openWakeWord feature models are distributed under the upstream Apache-2.0 license. See the [openWakeWord repository](https://github.com/dscripka/openWakeWord) for source and license. The classifier head was trained for this project and remains experimental: the pilot did not find a threshold with both useful recall and an acceptable false-activation rate. Keep `enabled` false until a better candidate is trained and validated on long background recordings and the Mini 3 microphone.

To update a model without rebuilding the service, replace its ONNX file in this directory and restart `gpt-alice`. Keep the filenames and input/output shapes compatible with the Rust runtime. Preserve the two openWakeWord feature models when replacing only the classifier.
