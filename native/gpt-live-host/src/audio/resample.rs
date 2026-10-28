//! Streaming mono sample-rate conversion with a pass-through fast path.

use std::collections::VecDeque;

use rubato::Resampler;
use rubato::audioadapter_buffers::direct::InterleavedSlice;

pub struct Converter {
    inner: Option<Inner>,
    output: VecDeque<f32>,
    output_limit: Option<usize>,
}

struct Inner {
    resampler: rubato::Async<f32>,
    input: VecDeque<f32>,
    scratch: Vec<f32>,
}

impl Converter {
    pub fn new(from: u32, to: u32) -> anyhow::Result<Self> {
        let inner = if from == to {
            None
        } else {
            let resampler = rubato::Async::new_sinc(
                f64::from(to) / f64::from(from),
                1.0,
                &rubato::SincInterpolationParameters::default(),
                from.div_ceil(100) as usize,
                1,
                rubato::FixedAsync::Input,
            )
            .map_err(|error| anyhow::anyhow!("failed to create resampler: {error}"))?;
            let scratch = vec![0.0; resampler.output_frames_max()];
            Some(Inner {
                resampler,
                input: VecDeque::new(),
                scratch,
            })
        };
        Ok(Self {
            inner,
            output: VecDeque::new(),
            output_limit: None,
        })
    }

    pub fn new_bounded(from: u32, to: u32, max_output: usize) -> anyhow::Result<Self> {
        let mut converter = Self::new(from, to)?;
        converter.output_limit = Some(max_output);
        Ok(converter)
    }

    fn append_output(output: &mut VecDeque<f32>, limit: Option<usize>, samples: &[f32]) {
        let Some(limit) = limit else {
            output.extend(samples);
            return;
        };
        let skip = samples.len().saturating_sub(limit);
        let samples = &samples[skip..];
        let excess = output
            .len()
            .saturating_add(samples.len())
            .saturating_sub(limit);
        if excess > 0 {
            output.drain(..excess);
        }
        output.extend(samples);
    }

    pub fn push(&mut self, samples: &[f32]) -> anyhow::Result<()> {
        let Some(inner) = &mut self.inner else {
            Self::append_output(&mut self.output, self.output_limit, samples);
            return Ok(());
        };
        inner.input.extend(samples);
        while inner.input.len() >= inner.resampler.input_frames_next() {
            let input = inner.input.make_contiguous();
            let frames = input.len();
            let input = InterleavedSlice::new(input, 1, frames)
                .map_err(|_| anyhow::anyhow!("invalid resampler input"))?;
            let capacity = inner.scratch.len();
            let mut output = InterleavedSlice::new_mut(&mut inner.scratch, 1, capacity)
                .map_err(|_| anyhow::anyhow!("invalid resampler output"))?;
            let (consumed, produced) = inner
                .resampler
                .process_into_buffer(&input, &mut output, None)
                .map_err(|error| anyhow::anyhow!("resampling failed: {error}"))?;
            inner.input.drain(..consumed);
            Self::append_output(
                &mut self.output,
                self.output_limit,
                &inner.scratch[..produced],
            );
        }
        Ok(())
    }

    pub fn available(&self) -> usize {
        self.output.len()
    }

    pub fn clear(&mut self) {
        self.output.clear();
        if let Some(inner) = &mut self.inner {
            inner.input.clear();
            inner.resampler.reset();
        }
    }

    /// Takes exactly `out.len()` samples when enough are buffered.
    pub fn take_exact(&mut self, out: &mut [f32]) -> bool {
        if self.output.len() < out.len() {
            return false;
        }
        let count = out.len();
        for (slot, sample) in out.iter_mut().zip(self.output.drain(..count)) {
            *slot = sample;
        }
        true
    }

    /// Drain only samples the speaker ring can accept, preserving any overflow.
    pub fn drain(&mut self, limit: usize) -> std::collections::vec_deque::Drain<'_, f32> {
        self.output.drain(..limit.min(self.output.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::Converter;

    #[test]
    fn partial_playback_drain_preserves_order_and_overflow() {
        let mut converter = Converter::new(48_000, 48_000).unwrap();
        converter.push(&[1.0, 2.0, 3.0, 4.0]).unwrap();
        assert_eq!(converter.drain(2).collect::<Vec<_>>(), vec![1.0, 2.0]);
        assert_eq!(converter.available(), 2);
        assert_eq!(converter.drain(8).collect::<Vec<_>>(), vec![3.0, 4.0]);
    }

    #[test]
    fn full_speaker_ring_does_not_discard_pending_audio() {
        let mut converter = Converter::new(48_000, 48_000).unwrap();
        converter.push(&[1.0, 2.0]).unwrap();
        assert_eq!(converter.drain(0).count(), 0);
        assert_eq!(converter.available(), 2);
    }

    #[test]
    fn bounded_output_drops_oldest_samples() {
        let mut converter = Converter::new_bounded(48_000, 48_000, 4).unwrap();
        converter.push(&[1.0, 2.0, 3.0, 4.0]).unwrap();
        converter.push(&[5.0, 6.0]).unwrap();
        assert_eq!(converter.available(), 4);
        assert_eq!(
            converter.drain(usize::MAX).collect::<Vec<_>>(),
            vec![3.0, 4.0, 5.0, 6.0]
        );
    }

    #[test]
    fn bounded_output_stays_within_limit_after_resampling() {
        let mut converter = Converter::new_bounded(48_000, 44_100, 32).unwrap();
        converter.push(&[0.25; 4_800]).unwrap();
        assert_eq!(converter.available(), 32);
    }

    #[test]
    fn clear_discards_output_partial_input_and_resampler_history() {
        for rate in [48_000, 44_100] {
            let mut converter = Converter::new(48_000, rate).unwrap();
            converter.push(&[1.0; 4_801]).unwrap();
            assert!(converter.available() > 0);
            converter.clear();
            assert_eq!(converter.available(), 0);
            let mut fresh = Converter::new(48_000, rate).unwrap();
            let samples = [0.25; 4_800];
            converter.push(&samples).unwrap();
            fresh.push(&samples).unwrap();
            assert_eq!(
                converter.drain(usize::MAX).collect::<Vec<_>>(),
                fresh.drain(usize::MAX).collect::<Vec<_>>()
            );
        }
    }
}
