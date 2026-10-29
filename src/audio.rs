use std::sync::Arc;

use crate::{mixer::AudioMixer, runtime::DeviceEvent};

const ZEN_HOVER_WAV: &[u8] = include_bytes!("../assets/audio/zen-hover.wav");
const SFX_GAIN: f32 = 2.0;
const SFX_STRETCH: f32 = 2.5;
const SFX_MIN_DURATION: std::time::Duration = std::time::Duration::from_millis(600);
// Keep the WAV longer than the speaker's 160 ms ALSA start buffer. The added
// tail is silence; the short hover transient is stretched to remain audible.

#[derive(Clone)]
pub struct AudioController {
    mixer: Arc<AudioMixer>,
}

impl AudioController {
    pub fn new(mixer: Arc<AudioMixer>) -> Self {
        Self { mixer }
    }

    pub fn handle_button(&self, event: &DeviceEvent, volume_changed: bool) {
        if Self::should_play_button(event, volume_changed) {
            self.request_sfx();
        }
    }

    pub fn should_play_button(event: &DeviceEvent, volume_changed: bool) -> bool {
        let (button, action) = match event {
            DeviceEvent::Touch { button, action, .. }
            | DeviceEvent::Button { button, action, .. } => (button, action),
            _ => return false,
        };
        if action != "pressed" {
            return false;
        }
        if button == "volume_up" || button == "volume_down" {
            volume_changed
        } else {
            true
        }
    }

    fn request_sfx(&self) {
        match prepare_button_wav(ZEN_HOVER_WAV, SFX_GAIN, SFX_STRETCH)
            .and_then(|wav| self.mixer.queue_sfx(&wav))
        {
            Ok(()) => eprintln!("button SFX queued in shared audio mixer"),
            Err(error) => eprintln!("button audio playback failed: {error}"),
        }
    }
}

fn prepare_button_wav(input: &[u8], gain: f32, stretch: f32) -> Result<Vec<u8>, String> {
    if input.get(0..4) != Some(b"RIFF") || input.get(8..12) != Some(b"WAVE") {
        return Err("embedded button audio is not a RIFF/WAVE file".into());
    }
    let output = input.to_vec();
    let mut offset = 12;
    let mut pcm16 = false;
    let mut channels = 0_usize;
    let mut sample_rate = 0_usize;
    let mut block_align = 0_usize;
    let mut data_range = None;
    while offset + 8 <= output.len() {
        let chunk_id = &output[offset..offset + 4];
        let chunk_size = u32::from_le_bytes(
            output[offset + 4..offset + 8]
                .try_into()
                .expect("fixed-size WAV chunk header"),
        ) as usize;
        let start = offset + 8;
        let end = start.saturating_add(chunk_size);
        if end > output.len() {
            return Err("embedded button audio has an invalid WAV chunk".into());
        }
        match chunk_id {
            b"fmt " if chunk_size >= 16 => {
                let audio_format = u16::from_le_bytes([output[start], output[start + 1]]);
                channels = u16::from_le_bytes([output[start + 2], output[start + 3]]) as usize;
                sample_rate =
                    u32::from_le_bytes(output[start + 4..start + 8].try_into().unwrap()) as usize;
                block_align = u16::from_le_bytes([output[start + 12], output[start + 13]]) as usize;
                let bits_per_sample = u16::from_le_bytes([output[start + 14], output[start + 15]]);
                pcm16 = audio_format == 1 && bits_per_sample == 16;
            }
            b"data" => data_range = Some(start..end),
            _ => {}
        }
        offset = end + (chunk_size & 1);
    }
    if !pcm16 {
        return Err("embedded button audio must be 16-bit PCM".into());
    }
    if channels == 0 || block_align != channels * 2 || sample_rate == 0 {
        return Err("embedded button audio has invalid PCM parameters".into());
    }
    let range = data_range.ok_or_else(|| "embedded button audio has no data chunk".to_owned())?;
    let source = &output[range.clone()];
    let frames = source.len() / block_align;
    let active_end = (0..frames)
        .rev()
        .find(|frame| {
            let start = frame * block_align;
            source[start..start + block_align]
                .chunks_exact(2)
                .any(|sample| i16::from_le_bytes([sample[0], sample[1]]).unsigned_abs() > 128)
        })
        .map_or(0, |frame| frame + 1);
    let stretched_frames = (active_end as f32 * stretch).round() as usize;
    let mut audio = Vec::with_capacity(source.len().max(sample_rate * block_align));
    for frame in 0..stretched_frames {
        let position = frame as f32 / stretch;
        let left = (position.floor() as usize).min(active_end.saturating_sub(1));
        let right = (left + 1).min(active_end.saturating_sub(1));
        let fraction = position - left as f32;
        for channel in 0..channels {
            let left_offset = left * block_align + channel * 2;
            let right_offset = right * block_align + channel * 2;
            let left_value = i16::from_le_bytes([source[left_offset], source[left_offset + 1]]);
            let right_value = i16::from_le_bytes([source[right_offset], source[right_offset + 1]]);
            let value = (f32::from(left_value) * (1.0 - fraction)
                + f32::from(right_value) * fraction)
                * gain;
            let sample = value
                .round()
                .clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16;
            audio.extend_from_slice(&sample.to_le_bytes());
        }
    }
    audio.extend_from_slice(&source[active_end * block_align..]);
    let minimum_bytes = sample_rate * block_align * SFX_MIN_DURATION.as_millis() as usize / 1000;
    if audio.len() < minimum_bytes {
        audio.resize(minimum_bytes, 0);
    }

    let mut prepared = Vec::with_capacity(output.len() + audio.len() - source.len());
    prepared.extend_from_slice(&output[..range.start]);
    prepared.extend_from_slice(&audio);
    prepared.extend_from_slice(&output[range.end..]);
    let data_size_offset = range.start - 4;
    prepared[data_size_offset..data_size_offset + 4]
        .copy_from_slice(&(audio.len() as u32).to_le_bytes());
    let riff_size = (prepared.len() - 8) as u32;
    prepared[4..8].copy_from_slice(&riff_size.to_le_bytes());
    Ok(prepared)
}
