# TTS voice sources

Voice model weights are downloaded only into the ignored `training/models/` directory. Do not commit the ONNX weights or generated voice audio.

| Voice ID | Intended cohort | Model source | Dataset/model terms |
|---|---|---|---|
| `piper:ru_RU-denis-medium` | Synthetic adult male | [Piper voice model card](https://huggingface.co/rhasspy/piper-voices/blob/main/ru/ru_RU/denis/medium/MODEL_CARD) | Model card identifies the source dataset as CC0. |
| `piper:ru_RU-dmitri-medium` | Synthetic adult male | [Piper voice model card](https://huggingface.co/rhasspy/piper-voices/blob/main/ru/ru_RU/dmitri/medium/MODEL_CARD) | Model card identifies the source dataset as CC0. |
| `say:Milena` | Synthetic adult female | Installed Russian macOS voice | Synthesized on this Mac only; voice weights are not included in the repository. |
| `teratts:ru_f1` | Synthetic adult female, neural TTS | [TeraTTSv2 model](https://huggingface.co/TeraSpace/TeraTTSv2) | OpenRAIL-M with a commercial attribution condition; model revision is pinned in the synthesis script. |
| `teratts:ru_f2` | Synthetic adult female, neural TTS | [TeraTTSv2 model](https://huggingface.co/TeraSpace/TeraTTSv2) | OpenRAIL-M with a commercial attribution condition; model revision is pinned in the synthesis script. |
| `teratts:ru_m1` | Synthetic adult male, neural TTS | [TeraTTSv2 model](https://huggingface.co/TeraSpace/TeraTTSv2) | OpenRAIL-M with a commercial attribution condition; model revision is pinned in the synthesis script. |
| `teratts:ru_m5` | Synthetic adult male, neural TTS | [TeraTTSv2 model](https://huggingface.co/TeraSpace/TeraTTSv2) | OpenRAIL-M with a commercial attribution condition; model revision is pinned in the synthesis script. |

This setup does not contain a natural Russian child voice. Do not describe pitch-shifted adult speech as a child speaker. Add a child cohort only from a separately licensed voice source or properly consented recordings, with speaker IDs separated in evaluation.

TeraTTSv2 includes distinct Russian female (`ru_f1`, `ru_f2`) and male (`ru_m1`, `ru_m5`) voice styles. Its model card recommends `ru_f1` and `ru_m5`. The local generated clips and downloaded weights remain in gitignored paths. If a product uses this model, keep the required public attribution and link to TeraTTSv2; see the upstream model card and included license files for full restrictions.
