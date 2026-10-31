//! Single owner for speaker playback. All application sources are mixed here and
//! written through one long-lived `audio-sink` process owning ALSA hw:0,0.

use std::{
    collections::VecDeque,
    io::Write,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, Ordering},
        mpsc::{self, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

const SOCKET: &str = "127.0.0.1:19874";
const AUDIO_SINK: &str = "/data/opt/gpt-alice/tools/audio-sink";
const RATE: usize = 48_000;
const BLOCK: usize = 480; // 10 ms
const MAX_SOURCE_SAMPLES: usize = RATE / 4;

#[derive(Clone, Copy)]
struct Frame(f32, f32);

enum Source {
    Sfx(Vec<Frame>),
    Bluetooth(Vec<Frame>),
}

#[derive(Clone)]
pub struct AudioMixer {
    sender: SyncSender<Source>,
    bluetooth_gain: Arc<Mutex<f32>>,
    output_volume: Arc<AtomicU8>,
    clear_agent_voice: Arc<std::sync::atomic::AtomicBool>,
}

impl AudioMixer {
    pub fn start() -> Result<Self, String> {
        let socket = std::net::UdpSocket::bind(SOCKET)
            .map_err(|error| format!("не удалось открыть аудиовход {SOCKET}: {error}"))?;
        socket
            .set_nonblocking(true)
            .map_err(|error| format!("не удалось настроить аудиовход: {error}"))?;
        let (sender, receiver) = mpsc::sync_channel(32);
        let bluetooth_gain = Arc::new(Mutex::new(1.0));
        let worker_gain = Arc::clone(&bluetooth_gain);
        let output_volume = Arc::new(AtomicU8::new(100));
        let worker_output_volume = Arc::clone(&output_volume);
        let clear_agent_voice = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_clear_agent_voice = Arc::clone(&clear_agent_voice);
        thread::Builder::new()
            .name("shared-audio-output".into())
            .spawn(move || {
                run_mixer(
                    socket,
                    receiver,
                    worker_gain,
                    worker_output_volume,
                    worker_clear_agent_voice,
                )
            })
            .map_err(|error| format!("не удалось запустить общий аудиовыход: {error}"))?;
        Ok(Self {
            sender,
            bluetooth_gain,
            output_volume,
            clear_agent_voice,
        })
    }

    /// Sets the software master gain applied equally to voice, Bluetooth, and SFX.
    pub fn set_output_volume(&self, volume: u8) {
        self.output_volume.store(volume.min(100), Ordering::Release);
    }

    pub fn queue_test_tone(&self) -> Result<(), String> {
        let frames = (0..RATE)
            .map(|index| {
                let t = index as f32 / RATE as f32;
                let fade = (t / 0.03).min(1.0).min(((1.0 - t) / 0.03).max(0.0));
                let sample = (std::f32::consts::TAU * 440.0 * t).sin() * 0.35 * fade;
                Frame(sample, sample)
            })
            .collect();
        self.sender
            .try_send(Source::Sfx(frames))
            .map_err(|error| format!("не удалось поставить тестовый тон в аудиомикшер: {error}"))
    }

    pub fn queue_sfx(&self, wav: &[u8]) -> Result<(), String> {
        let samples = wav_to_stereo_48k(wav)?;
        eprintln!(
            "shared audio SFX queued: frames={}, peak={:.3}",
            samples.len(),
            samples.iter().fold(0.0f32, |peak, frame| peak
                .max(frame.0.abs())
                .max(frame.1.abs()))
        );
        self.sender
            .try_send(Source::Sfx(samples))
            .map_err(|error| format!("не удалось поставить SFX в аудиомикшер: {error}"))
    }

    /// Immediately discard queued GPT-Live output while keeping Bluetooth audio and SFX.
    pub fn clear_agent_voice(&self) {
        self.clear_agent_voice.store(true, Ordering::Release);
    }

    /// Accepts interleaved stereo S16_LE frames from BlueALSA at 44.1 kHz.
    pub fn push_bluetooth(&self, pcm: &[u8], gain: f32) {
        if let Ok(mut current) = self.bluetooth_gain.lock() {
            *current = gain.clamp(0.0, 1.0);
        }
        let input: Vec<Frame> = pcm
            .chunks_exact(4)
            .map(|frame| {
                Frame(
                    i16::from_le_bytes([frame[0], frame[1]]) as f32 / 32768.0,
                    i16::from_le_bytes([frame[2], frame[3]]) as f32 / 32768.0,
                )
            })
            .collect();
        if input.is_empty() {
            return;
        }
        let frames = resample_stereo(&input, 44_100, RATE as u32);
        let _ = self.sender.try_send(Source::Bluetooth(frames));
    }
}

fn run_mixer(
    socket: std::net::UdpSocket,
    receiver: mpsc::Receiver<Source>,
    gain: Arc<Mutex<f32>>,
    output_volume: Arc<AtomicU8>,
    clear_agent_voice: Arc<std::sync::atomic::AtomicBool>,
) {
    let mut output = match Output::start() {
        Ok(output) => Some(output),
        Err(error) => {
            eprintln!("shared audio output failed to start: {error}");
            None
        }
    };
    let mut voice = VecDeque::<Frame>::new();
    let mut bluetooth = VecDeque::<Frame>::new();
    let mut sfx = VecDeque::<Frame>::new();
    let mut datagram = [0u8; 4096];
    let mut next = Instant::now();
    let mut retry_output_at = Instant::now();
    let mut voice_packets = 0u64;
    let mut output_blocks = 0u64;
    let mut last_voice_peak = 0.0f32;
    loop {
        while let Ok(source) = receiver.try_recv() {
            match source {
                Source::Sfx(frames) => {
                    sfx.clear();
                    sfx.extend(frames);
                }
                Source::Bluetooth(frames) => append_bounded(&mut bluetooth, frames),
            }
        }
        loop {
            match socket.recv(&mut datagram) {
                Ok(length) if length > 1 && datagram[0] == 1 => {
                    voice_packets += 1;
                    let values = datagram[1..length].chunks_exact(4).map(|bytes| {
                        let sample = f32::from_le_bytes(bytes.try_into().expect("4 bytes"));
                        last_voice_peak = last_voice_peak.max(sample.abs());
                        Frame(sample, sample)
                    });
                    append_bounded(&mut voice, values.collect());
                    if voice_packets.is_multiple_of(100) {
                        eprintln!(
                            "shared audio input: {voice_packets} voice packets received, peak={last_voice_peak:.4}, queued_frames={}",
                            voice.len()
                        );
                        last_voice_peak = 0.0;
                    }
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    eprintln!("shared audio input failed: {error}");
                    break;
                }
            }
        }
        if clear_agent_voice.swap(false, Ordering::AcqRel) {
            let discarded_frames = voice.len();
            voice.clear();
            eprintln!(
                "shared audio interruption: discarded {discarded_frames} queued voice frames"
            );
        }
        let now = Instant::now();
        if now < next {
            thread::sleep(next - now);
        } else if now.duration_since(next) > Duration::from_millis(20) {
            // Do not burst stale audio after a scheduler stall. The source queues
            // already retain only their newest 250 ms, so the stream catches up.
            next = now;
        }
        next += Duration::from_millis(10);
        let bt_gain = gain.lock().map(|value| *value).unwrap_or(1.0);
        let master_gain = f32::from(output_volume.load(Ordering::Acquire)) / 100.0;
        let mut pcm = Vec::with_capacity(BLOCK * 4);
        for _ in 0..BLOCK {
            let (mut left, mut right) = voice
                .pop_front()
                .map_or((0.0, 0.0), |frame| (frame.0, frame.1));
            if let Some(frame) = bluetooth.pop_front() {
                left += frame.0 * bt_gain;
                right += frame.1 * bt_gain;
            }
            if let Some(frame) = sfx.pop_front() {
                left += frame.0;
                right += frame.1;
            }
            for value in [left, right] {
                let sample = (value * master_gain).clamp(-1.0, 1.0);
                let sample = (sample * i16::MAX as f32).round() as i16;
                pcm.extend_from_slice(&sample.to_le_bytes());
            }
        }
        if output.is_none() && Instant::now() >= retry_output_at {
            match Output::start() {
                Ok(started) => output = Some(started),
                Err(error) => {
                    eprintln!("shared audio output unavailable: {error}");
                    retry_output_at = Instant::now() + Duration::from_secs(2);
                }
            }
        }
        if let Some(mut active) = output.take() {
            if let Err(error) = active.write(&pcm) {
                eprintln!("shared audio output failed; reopening ALSA: {error}");
                active.stop();
                retry_output_at = Instant::now() + Duration::from_secs(1);
            } else {
                output_blocks += 1;
                if output_blocks.is_multiple_of(500) {
                    let peak = pcm
                        .chunks_exact(2)
                        .map(|bytes| i16::from_le_bytes([bytes[0], bytes[1]]).unsigned_abs())
                        .max()
                        .unwrap_or(0);
                    eprintln!(
                        "shared audio output: {output_blocks} blocks written, pcm_peak={peak}, volume={}",
                        output_volume.load(Ordering::Acquire)
                    );
                }
                output = Some(active);
            }
        }
    }
}

fn append_bounded(queue: &mut VecDeque<Frame>, frames: Vec<Frame>) {
    queue.extend(frames);
    while queue.len() > MAX_SOURCE_SAMPLES {
        queue.pop_front();
    }
}

struct Output {
    child: Child,
    stdin: ChildStdin,
}

impl Output {
    fn start() -> Result<Self, String> {
        let mut command = Command::new(AUDIO_SINK);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        #[cfg(target_os = "linux")]
        unsafe {
            use std::os::unix::process::CommandExt;
            let parent_pid = libc::getpid();
            command.pre_exec(move || {
                if libc::prctl(1, libc::SIGTERM) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != parent_pid {
                    return Err(std::io::Error::other(
                        "audio service exited before audio-sink startup",
                    ));
                }
                Ok(())
            });
        }
        let mut child = command
            .spawn()
            .map_err(|error| format!("не удалось запустить audio-sink: {error}"))?;
        if let Some(stderr) = child.stderr.take() {
            thread::Builder::new()
                .name("shared-audio-sink-log".into())
                .spawn(move || {
                    use std::io::{BufRead, BufReader};
                    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                        eprintln!("shared audio ALSA: {line}");
                    }
                })
                .map_err(|error| format!("не удалось включить журнал audio-sink: {error}"))?;
        }
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "audio-sink не открыл PCM-вход".to_owned())?;
        eprintln!("shared audio ALSA output started: {AUDIO_SINK}");
        Ok(Self { child, stdin })
    }
    fn write(&mut self, pcm: &[u8]) -> Result<(), String> {
        self.stdin.write_all(pcm).map_err(|error| error.to_string())
    }
    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn wav_to_stereo_48k(wav: &[u8]) -> Result<Vec<Frame>, String> {
    if wav.get(0..4) != Some(b"RIFF") || wav.get(8..12) != Some(b"WAVE") {
        return Err("SFX должен быть WAV".into());
    }
    let mut offset = 12usize;
    let (mut channels, mut rate, mut bits, mut format) = (0usize, 0usize, 0u16, 0u16);
    let mut data = None;
    while offset + 8 <= wav.len() {
        let id = &wav[offset..offset + 4];
        let size = u32::from_le_bytes(wav[offset + 4..offset + 8].try_into().unwrap()) as usize;
        let start = offset + 8;
        let end = start.saturating_add(size);
        if end > wav.len() {
            return Err("повреждён WAV SFX".into());
        }
        match id {
            b"fmt " if size >= 16 => {
                format = u16::from_le_bytes([wav[start], wav[start + 1]]);
                channels = u16::from_le_bytes([wav[start + 2], wav[start + 3]]) as usize;
                rate = u32::from_le_bytes(wav[start + 4..start + 8].try_into().unwrap()) as usize;
                bits = u16::from_le_bytes([wav[start + 14], wav[start + 15]]);
            }
            b"data" => data = Some(&wav[start..end]),
            _ => {}
        }
        offset = end + (size & 1);
    }
    if format != 1 || bits != 16 || channels == 0 || rate == 0 {
        return Err("неподдерживаемый формат WAV SFX".into());
    }
    let data = data.ok_or_else(|| "в WAV SFX нет PCM данных".to_owned())?;
    let frames: Vec<Frame> = data
        .chunks_exact(channels * 2)
        .map(|frame| {
            let sample = |channel: usize| {
                i16::from_le_bytes([frame[channel * 2], frame[channel * 2 + 1]]) as f32 / 32768.0
            };
            if channels == 1 {
                let value = sample(0);
                Frame(value, value)
            } else {
                Frame(sample(0), sample(1))
            }
        })
        .collect();
    Ok(resample_stereo(&frames, rate as u32, RATE as u32))
}

fn resample_stereo(input: &[Frame], from: u32, to: u32) -> Vec<Frame> {
    if input.is_empty() || from == to {
        return input.to_vec();
    }
    let count = (input.len() as u64 * u64::from(to) / u64::from(from)) as usize;
    (0..count)
        .map(|index| {
            let position = index as f64 * f64::from(from) / f64::from(to);
            let left = (position.floor() as usize).min(input.len() - 1);
            let right = (left + 1).min(input.len() - 1);
            let fraction = (position - left as f64) as f32;
            Frame(
                input[left].0 * (1.0 - fraction) + input[right].0 * fraction,
                input[left].1 * (1.0 - fraction) + input[right].1 * fraction,
            )
        })
        .collect()
}
