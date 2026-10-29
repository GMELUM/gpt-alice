//! gpt-live-host: owns the WebRTC peer, microphone, speaker and echo cancellation for
//! GPT-Live voice sessions. Controlled by the opencode-gpt-live plugin over stdio using
//! newline-delimited JSON (see `protocol.rs`). Authentication never reaches this process:
//! the plugin exchanges the SDP offer for an answer and passes the answer back.

mod audio;
mod duck;
mod protocol;
mod transport;
mod wakeword;

use std::io::{BufRead, Cursor};
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use audio::engine::Engine;
use audio::io::AudioIo;
use audio::io::InputKind;
use audio::io::OutputKind;
use protocol::Command;
use protocol::Emitter;
use protocol::InputSpec;
use protocol::OutputSpec;
use transport::Transport;

/// Release builds get the plugin version injected (GPT_LIVE_VERSION); local builds use Cargo's.
const VERSION: &str = match option_env!("GPT_LIVE_VERSION") {
    Some(version) if !version.is_empty() => version,
    _ => env!("CARGO_PKG_VERSION"),
};
const BUTTON_SFX_WAV: &[u8] = include_bytes!("../../../assets/audio/zen-hover.wav");

fn button_sfx_samples() -> anyhow::Result<Vec<f32>> {
    let mut reader = hound::WavReader::new(Cursor::new(BUTTON_SFX_WAV))?;
    let spec = reader.spec();
    anyhow::ensure!(
        spec.sample_format == hound::SampleFormat::Int && spec.bits_per_sample == 16,
        "embedded button SFX must be 16-bit PCM"
    );
    let channels = usize::from(spec.channels.max(1));
    let samples: Vec<i16> = reader.samples::<i16>().collect::<Result<_, _>>()?;
    anyhow::ensure!(
        samples.len() % channels == 0,
        "embedded button SFX has an incomplete PCM frame"
    );
    Ok(samples
        .chunks_exact(channels)
        .map(|frame| {
            frame
                .iter()
                .map(|sample| f32::from(*sample) / 32768.0)
                .sum::<f32>()
                / channels as f32
        })
        .collect())
}

enum Control {
    Command(Command),
    Invalid(String),
    Eof,
}

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--version") => {
            println!("{VERSION}");
            return;
        }
        Some("--devices") => {
            println!("{}", audio::io::describe_defaults());
            return;
        }
        Some("--wakeword") => {
            let Some(directory) = args.next() else {
                eprintln!("usage: gpt-live-host --wakeword <model-directory> [threshold]");
                std::process::exit(2);
            };
            let threshold = args
                .next()
                .and_then(|value| value.parse::<f32>().ok())
                .unwrap_or(0.80);
            if !(0.0..=1.0).contains(&threshold) {
                eprintln!("wake-word threshold must be between 0 and 1");
                std::process::exit(2);
            }
            if let Err(error) = run_wakeword(std::path::Path::new(&directory), threshold) {
                Emitter::new().error(format!("wake-word detector stopped: {error:#}"), true);
                std::process::exit(1);
            }
            return;
        }
        Some(other) => {
            eprintln!("unknown argument: {other}");
            std::process::exit(2);
        }
        None => {}
    }

    let emitter = Emitter::new();
    let (control_tx, control_rx) = std_mpsc::channel::<Control>();
    std::thread::Builder::new()
        .name("control".into())
        .spawn(move || {
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                let message = match serde_json::from_str::<Command>(&line) {
                    Ok(command) => Control::Command(command),
                    Err(error) => Control::Invalid(error.to_string()),
                };
                if control_tx.send(message).is_err() {
                    return;
                }
            }
            let _ = control_tx.send(Control::Eof);
            // Exit even if the main thread is stuck, e.g. after the parent died.
            std::thread::sleep(Duration::from_secs(3));
            std::process::exit(0);
        })
        .expect("failed to spawn control thread");

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            emitter.error(format!("failed to start runtime: {error}"), true);
            std::process::exit(1);
        }
    };

    emitter.emit(serde_json::json!({
        "type": "ready",
        "version": VERSION,
        "protocol": protocol::PROTOCOL_VERSION,
    }));

    let mut session = Session::default();
    while let Ok(message) = control_rx.recv() {
        match message {
            Control::Command(Command::Close {}) | Control::Eof => break,
            Control::Command(command) => {
                if let Err(error) = session.handle(command, &runtime, &emitter) {
                    emitter.error(format!("{error:#}"), false);
                }
            }
            Control::Invalid(error) => emitter.error(format!("invalid command: {error}"), false),
        }
    }
    session.shutdown(&runtime);
    emitter.emit(serde_json::json!({ "type": "closed" }));
}

fn run_wakeword(directory: &std::path::Path, threshold: f32) -> anyhow::Result<()> {
    use ringbuf::traits::Consumer;
    use std::time::{Duration, Instant};

    let emitter = Emitter::new();
    let mut detector = wakeword::WakeWordModel::load(directory)?;
    let mut io = AudioIo::open(InputKind::Device, OutputKind::None, emitter.clone())?;
    let rings = io
        .rings
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("microphone ring is missing"))?;
    let mut resampler = audio::resample::Converter::new_bounded(io.input.rate, 16_000, 32_000)?;
    let mut input = vec![0.0f32; 8192];
    let mut block = [0.0f32; 1280];
    let mut above_threshold = 0usize;
    let mut last_trigger = Instant::now() - Duration::from_secs(10);
    emitter.emit(serde_json::json!({
        "type": "wakeword_ready",
        "input": {"name": io.input.name, "rate": io.input.rate, "channels": io.input.channels},
        "threshold": threshold,
        "model": directory.display().to_string()
    }));

    loop {
        let read = rings.capture.pop_slice(&mut input);
        if read == 0 {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
        resampler.push(&input[..read])?;
        while resampler.take_exact(&mut block) {
            for score in detector.push_audio(&block)? {
                if score >= threshold {
                    above_threshold = above_threshold.saturating_add(1);
                } else {
                    above_threshold = 0;
                }
                if above_threshold >= 3 && last_trigger.elapsed() >= Duration::from_secs(5) {
                    last_trigger = Instant::now();
                    above_threshold = 0;
                    emitter.emit(serde_json::json!({"type": "wakeword_detected", "phrase": "Алиса", "score": score}));
                }
            }
        }
    }
}

#[derive(Default)]
struct Session {
    transport: Option<Transport>,
    engine: Option<Engine>,
    muted: bool,
    /// Other apps' audio lowered for the call, restored when dropped.
    ducking: Option<duck::Ducking>,
}

impl Session {
    fn handle(
        &mut self,
        command: Command,
        runtime: &tokio::runtime::Runtime,
        emitter: &Emitter,
    ) -> anyhow::Result<()> {
        match command {
            Command::Start {
                input,
                output,
                duck_others,
            } => {
                if self.transport.is_some() {
                    anyhow::bail!("session already started");
                }
                if duck_others && self.ducking.is_none() {
                    self.ducking = duck::Ducking::start();
                }
                let input = match input {
                    InputSpec::File { file } => InputKind::File(file),
                    InputSpec::Device => InputKind::Device,
                };
                let output = match output {
                    OutputSpec::File { file } => OutputKind::File(file),
                    OutputSpec::Named(name) if name == "none" => OutputKind::None,
                    OutputSpec::Named(_) => OutputKind::Broker,
                };
                // Open devices first so permission prompts and missing hardware fail fast.
                let io = AudioIo::open(input, output, emitter.clone())?;
                emitter.emit(serde_json::json!({
                    "type": "audio",
                    "input": { "name": io.input.name, "rate": io.input.rate, "channels": io.input.channels },
                    "output": { "name": io.output.name, "rate": io.output.rate, "channels": io.output.channels },
                }));
                let (incoming_tx, incoming_rx) = std_mpsc::sync_channel(256);
                let (outgoing_tx, outgoing_rx) = tokio::sync::mpsc::channel(3000);
                let transport =
                    runtime.block_on(Transport::new(emitter.clone(), incoming_tx, outgoing_rx))?;
                self.transport = Some(transport);
                // Capture and encode while ICE gathering and HTTP/WebRTC negotiation
                // are in progress. The transport retains frames until GPT-Live connects.
                self.engine = Some(Engine::start(
                    io,
                    incoming_rx,
                    outgoing_tx,
                    emitter.clone(),
                    self.muted,
                )?);
                let transport = self
                    .transport
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("transport was not initialized"))?;
                let sdp = runtime.block_on(transport.offer())?;
                emitter.emit(serde_json::json!({ "type": "offer", "sdp": sdp }));
            }
            Command::Answer { sdp } => {
                let transport = self
                    .transport
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("session not started"))?;
                runtime.block_on(transport.apply_answer(sdp))?;
                emitter.emit(serde_json::json!({ "type": "connected" }));
            }
            Command::Mute { muted } => {
                self.muted = muted;
                if let Some(engine) = &self.engine {
                    engine.set_muted(muted);
                }
                emitter.emit(serde_json::json!({ "type": "muted", "muted": muted }));
            }
            Command::Clear {} => {
                // The engine clears its pending audio before asking the endpoint to clear.
                if let Some(engine) = &self.engine {
                    engine.clear_output();
                }
            }
            Command::PlaySfx {} => {
                let samples = button_sfx_samples()?;
                if let Some(engine) = &self.engine {
                    engine.queue_sfx(&samples);
                } else {
                    anyhow::bail!("audio output is not ready for SFX");
                }
                emitter.emit(serde_json::json!({ "type": "sfx_queued" }));
            }
            Command::SendEvent { event } => {
                let transport = self
                    .transport
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("session not started"))?;
                let event_type = event
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned();
                runtime.block_on(transport.send_event(event))?;
                emitter.emit(serde_json::json!({
                    "type": "event_queued",
                    "event": event_type,
                }));
            }
            Command::Devices {} => emitter.emit(audio::io::describe_defaults()),
            Command::Close {} => {}
        }
        Ok(())
    }

    fn shutdown(&mut self, runtime: &tokio::runtime::Runtime) {
        self.engine.take();
        if let Some(transport) = self.transport.take() {
            runtime.block_on(transport.close());
        }
        self.ducking.take();
    }
}
