//! Streaming openWakeWord-compatible feature extraction and small classifier.
//!
//! Model files are loaded from a directory beside the application, so replacing
//! a wake-word model does not require rebuilding the Rust service.

use std::{collections::VecDeque, path::Path, sync::Arc};

use tract_onnx::prelude::*;

const MEL_INPUT_SAMPLES: usize = 1_760;
const AUDIO_CHUNK_SAMPLES: usize = 1_280;
const MEL_BINS: usize = 32;
const MEL_HISTORY: usize = 76;
const EMBEDDING_SIZE: usize = 96;
const EMBEDDING_HISTORY: usize = 16;

type Runnable = Arc<TypedRunnableModel>;

pub struct WakeWordModel {
    melspectrogram: Runnable,
    embedding: Runnable,
    classifier: Runnable,
    recent_audio: VecDeque<f32>,
    mel_history: VecDeque<[f32; MEL_BINS]>,
    embeddings: VecDeque<[f32; EMBEDDING_SIZE]>,
    chunk: Vec<f32>,
}

impl WakeWordModel {
    pub fn load(directory: &Path) -> anyhow::Result<Self> {
        let melspectrogram = load_model(
            &directory.join("melspectrogram.onnx"),
            f32::fact([1, MEL_INPUT_SAMPLES]).into(),
        )?;
        let embedding = load_model(
            &directory.join("embedding_model.onnx"),
            f32::fact([1, MEL_HISTORY, MEL_BINS, 1]).into(),
        )?;
        let classifier = load_model(
            &directory.join("alice_openwakeword_head.onnx"),
            f32::fact([1, EMBEDDING_HISTORY, EMBEDDING_SIZE]).into(),
        )?;

        let mut model = Self {
            melspectrogram,
            embedding,
            classifier,
            recent_audio: VecDeque::with_capacity(MEL_INPUT_SAMPLES),
            mel_history: VecDeque::with_capacity(MEL_HISTORY),
            embeddings: VecDeque::with_capacity(EMBEDDING_HISTORY),
            chunk: Vec::with_capacity(AUDIO_CHUNK_SAMPLES),
        };
        model.reset();
        Ok(model)
    }

    pub fn reset(&mut self) {
        self.recent_audio.clear();
        self.recent_audio.extend(std::iter::repeat_n(
            0.0,
            MEL_INPUT_SAMPLES - AUDIO_CHUNK_SAMPLES,
        ));
        self.mel_history.clear();
        self.mel_history
            .extend(std::iter::repeat_n([1.0; MEL_BINS], MEL_HISTORY));
        self.embeddings.clear();
        self.chunk.clear();
    }

    /// Accepts normalized mono audio. Returns one classifier score every 80 ms.
    pub fn push_audio(&mut self, samples: &[f32]) -> anyhow::Result<Vec<f32>> {
        let mut scores = Vec::new();
        for sample in samples {
            self.recent_audio
                .push_back((sample * 32768.0).clamp(-32768.0, 32767.0));
            if self.recent_audio.len() > MEL_INPUT_SAMPLES {
                self.recent_audio.pop_front();
            }
            self.chunk.push(*sample);
            if self.chunk.len() == AUDIO_CHUNK_SAMPLES {
                self.update_features()?;
                self.chunk.clear();
                if self.embeddings.len() == EMBEDDING_HISTORY {
                    scores.push(self.score()?);
                }
            }
        }
        Ok(scores)
    }

    fn update_features(&mut self) -> anyhow::Result<()> {
        let input =
            Tensor::from_shape(&[1, MEL_INPUT_SAMPLES], self.recent_audio.make_contiguous())?;
        let mel = self.melspectrogram.run(tvec!(input.into()))?;
        let mel = mel[0].to_plain_array_view::<f32>()?;
        let frames = mel.len() / MEL_BINS;
        anyhow::ensure!(frames > 0, "melspectrogram returned no frames");
        for frame in mel
            .iter()
            .copied()
            .collect::<Vec<_>>()
            .chunks_exact(MEL_BINS)
        {
            let mut bins = [0.0; MEL_BINS];
            for (out, value) in bins.iter_mut().zip(frame) {
                *out = *value / 10.0 + 2.0;
            }
            self.mel_history.push_back(bins);
            if self.mel_history.len() > MEL_HISTORY {
                self.mel_history.pop_front();
            }
        }

        // openWakeWord advances one embedding window every 8 mel frames (80 ms).
        let mel_data: Vec<f32> = self
            .mel_history
            .iter()
            .flat_map(|frame| frame.iter().copied())
            .collect();
        let input = Tensor::from_shape(&[1, MEL_HISTORY, MEL_BINS, 1], &mel_data)?;
        let embedding = self.embedding.run(tvec!(input.into()))?;
        let embedding = embedding[0].to_plain_array_view::<f32>()?;
        anyhow::ensure!(
            embedding.len() == EMBEDDING_SIZE,
            "unexpected embedding size {}",
            embedding.len()
        );
        let mut values = [0.0; EMBEDDING_SIZE];
        values.copy_from_slice(
            embedding
                .as_slice()
                .ok_or_else(|| anyhow::anyhow!("embedding is not contiguous"))?,
        );
        self.embeddings.push_back(values);
        if self.embeddings.len() > EMBEDDING_HISTORY {
            self.embeddings.pop_front();
        }
        Ok(())
    }

    fn score(&self) -> anyhow::Result<f32> {
        let features: Vec<f32> = self
            .embeddings
            .iter()
            .flat_map(|frame| frame.iter().copied())
            .collect();
        let input = Tensor::from_shape(&[1, EMBEDDING_HISTORY, EMBEDDING_SIZE], &features)?;
        let output = self.classifier.run(tvec!(input.into()))?;
        let score = output[0].to_plain_array_view::<f32>()?;
        score
            .iter()
            .next()
            .copied()
            .ok_or_else(|| anyhow::anyhow!("classifier returned no score"))
    }
}

fn load_model(path: &Path, fact: InferenceFact) -> anyhow::Result<Runnable> {
    tract_onnx::onnx()
        .model_for_path(path)
        .map_err(|error| anyhow::anyhow!("could not load {}: {error}", path.display()))?
        .with_input_fact(0, fact)?
        .into_optimized()?
        .into_runnable()
        .map_err(Into::into)
}
