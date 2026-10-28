//! The audio engine: echo cancellation, Opus encode/decode and level metering.
//!
//! Capture:  device rate -> 48 kHz -> echo/noise/gain processing (10 ms) -> Opus (20 ms) -> RTP
//! Playback: RTP -> Opus decode (48 kHz) -> device rate -> speaker ring
//! The rendered speaker signal is fed back as the echo-cancellation reference.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc as std_mpsc;
use std::thread::JoinHandle;
use std::time::Duration;
use std::time::Instant;

use ringbuf::traits::Consumer;
use ringbuf::traits::Observer;
use ringbuf::traits::Producer;
use sonora::AudioProcessing;
use sonora::StreamConfig;
use sonora::config::AdaptiveDigital;
use sonora::config::EchoCanceller;
use sonora::config::GainController2;
use sonora::config::HighPassFilter;
use sonora::config::NoiseSuppression;
use sonora::config::NoiseSuppressionLevel;
use tokio::sync::mpsc;

use super::io::AudioIo;
use super::resample::Converter;
use crate::protocol::Emitter;
use crate::transport::IncomingPacket;
use crate::transport::OutgoingFrame;

const RATE: u32 = 48_000;
const BLOCK: usize = 480; // 10 ms at 48 kHz
const FRAME: usize = 960; // 20 ms at 48 kHz
const MAX_DECODED: usize = 5760; // 120 ms at 48 kHz
const MAX_CONCEALED_PACKETS: u16 = 5;
const MAX_PENDING_PLAYBACK_MS: usize = 1_000;
const CLEAR_ACK_TIMEOUT: Duration = Duration::from_secs(1);
const LEVEL_INTERVAL: Duration = Duration::from_millis(50);
const MAX_BLOCKS_PER_SERVICE: usize = 2;
const SERVICE_INTERVAL: Duration = Duration::from_millis(5);

#[derive(Default)]
pub struct EngineControl {
    pub muted: AtomicBool,
    pub stop: AtomicBool,
    pub clear: AtomicBool,
}

pub struct Engine {
    pub control: Arc<EngineControl>,
    playback_control: Arc<super::io::PlaybackControl>,
    thread: Option<JoinHandle<()>>,
}

impl Engine {
    pub fn start(
        mut io: AudioIo,
        incoming: std_mpsc::Receiver<IncomingPacket>,
        outgoing: mpsc::Sender<OutgoingFrame>,
        emitter: Emitter,
        muted: bool,
    ) -> anyhow::Result<Self> {
        let control = Arc::new(EngineControl::default());
        control.muted.store(muted, Ordering::Release);
        let playback_control = Arc::clone(&io.control);
        let rings = io
            .rings
            .take()
            .ok_or_else(|| anyhow::anyhow!("audio already started"))?;
        let state = State::new(io.input.rate, io.output.rate)?;
        let thread_control = control.clone();
        let thread = std::thread::Builder::new()
            .name("audio-engine".into())
            .spawn(move || {
                let mut state = state;
                let mut rings = rings;
                let io = io;
                if let Err(error) = state.run(
                    &mut rings,
                    &io,
                    &incoming,
                    &outgoing,
                    &emitter,
                    &thread_control,
                ) {
                    emitter.error(format!("audio engine stopped: {error}"), true);
                }
                drop(io);
            })?;
        Ok(Self {
            control,
            playback_control,
            thread: Some(thread),
        })
    }

    pub fn queue_sfx(&self, samples: &[f32]) {
        self.playback_control.queue_sfx(samples);
    }

    pub fn set_muted(&self, muted: bool) {
        self.control.muted.store(muted, Ordering::Release);
    }

    pub fn clear_output(&self) {
        self.control.clear.store(true, Ordering::Release);
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.control.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Meter {
    sum: f64,
    count: usize,
}

impl Meter {
    fn new() -> Self {
        Self { sum: 0.0, count: 0 }
    }

    fn add(&mut self, samples: &[f32]) {
        for sample in samples {
            self.sum += f64::from(*sample) * f64::from(*sample);
        }
        self.count += samples.len();
    }

    /// Returns a perceptual 0..1 level (-60 dBFS .. 0 dBFS) and resets.
    fn take(&mut self) -> f64 {
        let level = if self.count == 0 {
            0.0
        } else {
            let rms = (self.sum / self.count as f64).sqrt();
            let db = 20.0 * rms.max(1e-9).log10();
            ((db + 60.0) / 60.0).clamp(0.0, 1.0)
        };
        self.sum = 0.0;
        self.count = 0;
        level
    }
}

struct State {
    capture: Converter,
    reference: Converter,
    playback: Converter,
    apm: AudioProcessing,
    encoder: opus::Encoder,
    decoder: opus::Decoder,
    pending: Vec<f32>,
    expected_sequence: Option<u16>,
    output_rate: u32,
    clear_started: Option<Instant>,
    mic: Meter,
    speaker: Meter,
    scratch: Vec<f32>,
}

impl State {
    fn new(input_rate: u32, output_rate: u32) -> anyhow::Result<Self> {
        let apm = AudioProcessing::builder()
            .config(sonora::Config {
                echo_canceller: Some(EchoCanceller::default()),
                high_pass_filter: Some(HighPassFilter::default()),
                // Voice calls often happen with music or a TV in the room; favor clarity.
                noise_suppression: Some(NoiseSuppression {
                    level: NoiseSuppressionLevel::High,
                    ..Default::default()
                }),
                gain_controller2: Some(GainController2 {
                    adaptive_digital: Some(AdaptiveDigital::default()),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .capture_config(StreamConfig::new(RATE, 1))
            .render_config(StreamConfig::new(RATE, 1))
            .build();
        let mut encoder = opus::Encoder::new(RATE, opus::Channels::Mono, opus::Application::Voip)?;
        encoder.set_inband_fec(true)?;
        encoder.set_packet_loss_perc(5)?;
        let decoder = opus::Decoder::new(RATE, opus::Channels::Mono)?;
        Ok(Self {
            capture: Converter::new(input_rate, RATE)?,
            reference: Converter::new(output_rate, RATE)?,
            playback: Converter::new_bounded(
                RATE,
                output_rate,
                (output_rate as usize * MAX_PENDING_PLAYBACK_MS) / 1000,
            )?,
            apm,
            encoder,
            decoder,
            pending: Vec::with_capacity(FRAME),
            expected_sequence: None,
            output_rate,
            clear_started: None,
            mic: Meter::new(),
            speaker: Meter::new(),
            scratch: vec![0.0; 8192],
        })
    }

    fn run(
        &mut self,
        rings: &mut super::io::Rings,
        io: &AudioIo,
        incoming: &std_mpsc::Receiver<IncomingPacket>,
        outgoing: &mpsc::Sender<OutgoingFrame>,
        emitter: &Emitter,
        control: &EngineControl,
    ) -> anyhow::Result<()> {
        let mut last_levels = Instant::now();
        let mut was_muted = control.muted.load(Ordering::Acquire);
        while !control.stop.load(Ordering::Acquire) {
            // Service incoming audio before DSP work: a capture backlog must not delay
            // speech until the transcript (which arrives on another thread) is complete.
            self.service_playback(&mut rings.playback, incoming, control, &io.control)?;

            // Speaker reference first so capture blocks see an up-to-date echo model.
            loop {
                let read = rings.reference.pop_slice(&mut self.scratch);
                if read == 0 {
                    break;
                }
                self.reference.push(&self.scratch[..read])?;
            }
            let mut block = [0.0f32; BLOCK];
            let mut processed = [0.0f32; BLOCK];
            for _ in 0..MAX_BLOCKS_PER_SERVICE {
                if !self.reference.take_exact(&mut block) {
                    break;
                }
                self.apm
                    .process_render_f32(&[&block], &mut [&mut processed])
                    .map_err(|_| anyhow::anyhow!("echo reference processing failed"))?;
            }

            // Capture.
            loop {
                let read = rings.capture.pop_slice(&mut self.scratch);
                if read == 0 {
                    break;
                }
                self.capture.push(&self.scratch[..read])?;
            }
            let muted = control.muted.load(Ordering::Acquire);
            if muted != was_muted {
                self.pending.clear();
                was_muted = muted;
            }
            let playback_delay_ms =
                rings.playback.occupied_len() as f64 * 1000.0 / f64::from(self.output_rate);
            let delay = (playback_delay_ms + 20.0).clamp(0.0, 500.0) as i32;
            for _ in 0..MAX_BLOCKS_PER_SERVICE {
                if !self.capture.take_exact(&mut block) {
                    break;
                }
                let _ = self.apm.set_stream_delay_ms(delay);
                self.apm
                    .process_capture_f32(&[&block], &mut [&mut processed])
                    .map_err(|_| anyhow::anyhow!("capture processing failed"))?;
                if muted || processed.iter().any(|sample| !sample.is_finite()) {
                    processed.fill(0.0);
                } else {
                    self.mic.add(&processed);
                }
                self.pending.extend_from_slice(&processed);
                if self.pending.len() >= FRAME {
                    let mut data = vec![0u8; 1275];
                    let len = self
                        .encoder
                        .encode_float(&self.pending[..FRAME], &mut data)?;
                    data.truncate(len);
                    self.pending.drain(..FRAME);
                    let _ = outgoing.try_send(OutgoingFrame { data });
                }
            }

            if last_levels.elapsed() >= LEVEL_INTERVAL {
                last_levels = Instant::now();
                emitter.emit(serde_json::json!({
                    "type": "levels",
                    "mic": (self.mic.take() * 1000.0).round() / 1000.0,
                    "speaker": (self.speaker.take() * 1000.0).round() / 1000.0,
                    "muted": muted,
                }));
            }
            let _ = io;
            std::thread::sleep(SERVICE_INTERVAL);
        }
        Ok(())
    }

    fn service_playback(
        &mut self,
        sink: &mut ringbuf::HeapProd<f32>,
        incoming: &std_mpsc::Receiver<IncomingPacket>,
        control: &EngineControl,
        playback: &super::io::PlaybackControl,
    ) -> anyhow::Result<()> {
        if control.clear.swap(false, Ordering::AcqRel) {
            self.playback.clear();
            if !playback.clear.swap(true, Ordering::AcqRel) {
                self.clear_started = Some(Instant::now());
            }
        }
        // Leave the producer idle until the renderer acknowledges clearing its ring.
        if playback.clear.load(Ordering::Acquire) {
            if self
                .clear_started
                .is_some_and(|started| started.elapsed() >= CLEAR_ACK_TIMEOUT)
                && playback.clear.load(Ordering::Acquire)
            {
                anyhow::bail!("speaker did not acknowledge playback clear within 1 second");
            }
            return Ok(());
        }
        self.clear_started = None;
        // Flush after every packet, so a large batch moves into the speaker ring as it is
        // decoded instead of overflowing the pending cap while the ring still has room.
        for _ in 0..32 {
            let Ok(packet) = incoming.try_recv() else {
                break;
            };
            self.receive(packet)?;
            self.flush_playback(sink);
        }
        self.flush_playback(sink);
        Ok(())
    }

    fn flush_playback(&mut self, sink: &mut ringbuf::HeapProd<f32>) {
        let capacity = sink.vacant_len().min(self.playback.available());
        if capacity > 0 {
            let samples: Vec<f32> = self.playback.drain(capacity).collect();
            let written = sink.push_slice(&samples);
            debug_assert_eq!(written, samples.len());
        }
    }

    fn receive(&mut self, packet: IncomingPacket) -> anyhow::Result<()> {
        let mut decoded = [0.0f32; MAX_DECODED];
        if let Some(expected) = self.expected_sequence {
            let gap = packet.sequence.wrapping_sub(expected);
            if gap >= u16::MAX / 2 {
                // Late or duplicate packet: already concealed or played.
                return Ok(());
            }
            if gap > 0 && gap <= MAX_CONCEALED_PACKETS {
                for index in 0..gap {
                    // Use in-band FEC for the packet just before this one, PLC for older gaps.
                    let (input, fec) = if index + 1 == gap {
                        (packet.payload.as_slice(), true)
                    } else {
                        (&[][..], false)
                    };
                    let frame = &mut decoded[..FRAME];
                    if let Ok(len) = self.decoder.decode_float(input, frame, fec) {
                        self.play(&decoded[..len])?;
                    }
                }
            }
        }
        self.expected_sequence = Some(packet.sequence.wrapping_add(1));
        match self
            .decoder
            .decode_float(&packet.payload, &mut decoded, false)
        {
            Ok(len) => self.play(&decoded[..len]),
            Err(_) => Ok(()),
        }
    }

    fn play(&mut self, samples: &[f32]) -> anyhow::Result<()> {
        self.speaker.add(samples);
        self.playback.push(samples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speaker_backpressure_preserves_every_sample_in_order() {
        use ringbuf::traits::Split;
        let mut state = State::new(RATE, RATE).unwrap();
        let (mut prod, mut cons) = ringbuf::HeapRb::<f32>::new(2).split();
        state.playback.push(&[1.0, 2.0, 3.0, 4.0, 5.0]).unwrap();
        state.flush_playback(&mut prod);
        assert_eq!(state.playback.available(), 3);
        // A second service while full must not consume queued samples.
        state.flush_playback(&mut prod);
        assert_eq!(state.playback.available(), 3);
        let mut actual = Vec::new();
        while state.playback.available() > 0 || !cons.is_empty() {
            while let Some(sample) = cons.try_pop() {
                actual.push(sample);
            }
            state.flush_playback(&mut prod);
        }
        assert_eq!(actual, vec![1.0, 2.0, 3.0, 4.0, 5.0]);
    }

    #[test]
    fn clear_discards_overflow_and_waits_for_the_renderer_before_refilling() {
        use ringbuf::traits::Split;
        let mut state = State::new(RATE, RATE).unwrap();
        let control = EngineControl::default();
        let playback = Arc::new(super::super::io::PlaybackControl::default());
        let (mut prod, cons) = ringbuf::HeapRb::<f32>::new(2).split();
        let (reference, _) = ringbuf::HeapRb::<f32>::new(20).split();
        let mut renderer = super::super::io::Renderer::new(cons, reference, playback.clone(), 10);
        let (_tx, incoming) = std_mpsc::channel();
        state.playback.push(&[1.0, 2.0, 3.0, 4.0, 5.0]).unwrap();
        state.flush_playback(&mut prod);
        assert_eq!(state.playback.available(), 3);
        for _ in 0..2 {
            control.clear.store(true, Ordering::Release);
            state
                .service_playback(&mut prod, &incoming, &control, &playback)
                .unwrap();
            assert_eq!(state.playback.available(), 0);
            assert!(playback.clear.load(Ordering::Acquire));
        }
        state.playback.push(&[6.0, 7.0]).unwrap();
        state
            .service_playback(&mut prod, &incoming, &control, &playback)
            .unwrap();
        assert_eq!(state.playback.available(), 2);
        let mut out = [0.0; 2];
        renderer.render(&mut out);
        assert_eq!(out, [0.0; 2]);
        assert!(!playback.clear.load(Ordering::Acquire));
        state
            .service_playback(&mut prod, &incoming, &control, &playback)
            .unwrap();
        renderer.render(&mut out);
        assert_eq!(out, [6.0, 7.0]);
        assert_eq!(state.playback.available(), 0);
    }

    #[test]
    fn a_full_batch_of_packets_reaches_the_speaker_ring_without_dropping() {
        use ringbuf::traits::Observer;
        use ringbuf::traits::Split;
        let mut state = State::new(RATE, RATE).unwrap();
        let control = EngineControl::default();
        let playback = super::super::io::PlaybackControl::default();
        let (mut sink, _cons) = ringbuf::HeapRb::<f32>::new(RATE as usize * 4).split();
        let (tx, incoming) = std_mpsc::channel();
        let mut encoder =
            opus::Encoder::new(RATE, opus::Channels::Mono, opus::Application::Voip).unwrap();
        let packet = vec![0.1f32; FRAME * 2]; // 40 ms
        for sequence in 0..32u16 {
            let payload = encoder.encode_vec_float(&packet, 4000).unwrap();
            tx.send(IncomingPacket { sequence, payload }).unwrap();
        }
        state
            .service_playback(&mut sink, &incoming, &control, &playback)
            .unwrap();
        // 32 x 40 ms is more than the 1 s pending cap, but fits the 4 s speaker ring.
        assert_eq!(
            sink.occupied_len() + state.playback.available(),
            32 * packet.len()
        );
    }

    #[test]
    fn playback_pending_audio_is_bounded_and_keeps_the_newest_samples() {
        let mut state = State::new(RATE, RATE).unwrap();
        let samples: Vec<f32> = (0..RATE as usize + 10)
            .map(|sample| sample as f32)
            .collect();
        state.playback.push(&samples).unwrap();
        assert_eq!(state.playback.available(), RATE as usize);
        assert_eq!(state.playback.drain(1).next(), Some(10.0));
    }

    #[test]
    fn clear_ack_timeout_fails_instead_of_leaving_audio_silently_stalled() {
        use ringbuf::traits::Split;
        let mut state = State::new(RATE, RATE).unwrap();
        let control = EngineControl::default();
        let playback = Arc::new(super::super::io::PlaybackControl::default());
        playback.clear.store(true, Ordering::Release);
        state.clear_started = Some(Instant::now() - CLEAR_ACK_TIMEOUT - Duration::from_millis(1));
        let (mut sink, _) = ringbuf::HeapRb::<f32>::new(2).split();
        let (_tx, incoming) = std_mpsc::channel();

        let error = state
            .service_playback(&mut sink, &incoming, &control, &playback)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("did not acknowledge playback clear")
        );
    }

    // Offline: no microphone, devices, credentials or network. Run explicitly to
    // check the DSP budget when evaluating a helper build on a target machine.
    #[test]
    #[ignore = "manual DSP timing diagnostic"]
    fn one_second_dsp_budget() {
        let mut state = State::new(RATE, RATE).unwrap();
        let input: Vec<f32> = (0..BLOCK)
            .map(|i| ((i as f32 / RATE as f32) * 440.0 * std::f32::consts::TAU).sin() * 0.1)
            .collect();
        let mut processed = [0.0; BLOCK];
        let mut encoded = [0u8; 1275];
        let started = Instant::now();
        for _ in 0..100 {
            state
                .apm
                .process_render_f32(&[&input], &mut [&mut processed])
                .unwrap();
            state
                .apm
                .process_capture_f32(&[&input], &mut [&mut processed])
                .unwrap();
            state.pending.extend_from_slice(&processed);
            if state.pending.len() >= FRAME {
                state
                    .encoder
                    .encode_float(&state.pending[..FRAME], &mut encoded)
                    .unwrap();
                state.pending.drain(..FRAME);
            }
        }
        eprintln!(
            "DSP processing for 1000 ms of audio: {} ms",
            started.elapsed().as_millis()
        );
    }
}
