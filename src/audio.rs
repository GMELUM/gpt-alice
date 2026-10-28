use std::{
    fs,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, SyncSender, TrySendError},
    },
    thread,
    time::{Duration, Instant},
};

use crate::runtime::DeviceEvent;

const ZEN_HOVER_WAV: &[u8] = include_bytes!("../assets/audio/zen-hover.wav");
const APLAY: &str = "/usr/bin/aplay";
const WAV_PATH: &str = "/tmp/gpt-alice-zen-hover.wav";
// Keep the WAV longer than the speaker's 160 ms ALSA start buffer. The added
// tail is silence; the audible 123 ms hover cue remains unchanged.
const PLAYBACK_DURATION: Duration = Duration::from_millis(323);

#[derive(Clone)]
pub struct AudioController {
    sender: SyncSender<()>,
    generation: Arc<AtomicU64>,
}

impl AudioController {
    pub fn new() -> Result<Self, String> {
        fs::write(WAV_PATH, ZEN_HOVER_WAV)
            .map_err(|error| format!("could not write embedded button WAV: {error}"))?;
        let (sender, receiver) = mpsc::sync_channel(1);
        let generation = Arc::new(AtomicU64::new(0));
        let worker_generation = Arc::clone(&generation);

        thread::Builder::new()
            .name("button-audio".into())
            .spawn(move || {
                let mut completed_generation = 0;
                while receiver.recv().is_ok() {
                    loop {
                        let current_generation = worker_generation.load(Ordering::Acquire);
                        if current_generation <= completed_generation {
                            break;
                        }
                        eprintln!("button SFX starting generation {current_generation}");
                        match play(WAV_PATH, &worker_generation, current_generation) {
                            Ok(Some(_)) => continue,
                            Ok(None) => {
                                eprintln!("button SFX generation {current_generation} finished");
                                completed_generation = current_generation;
                                break;
                            }
                            Err(error) => {
                                eprintln!("button audio playback failed: {error}");
                                completed_generation = current_generation;
                                break;
                            }
                        }
                    }
                }
            })
            .map_err(|error| format!("could not start audio worker: {error}"))?;

        Ok(Self {
            sender,
            generation,
        })
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
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        eprintln!("button SFX request generation {generation}");
        match self.sender.try_send(()) {
            Ok(()) | Err(TrySendError::Full(())) => {}
            Err(TrySendError::Disconnected(())) => {
                eprintln!("button audio worker is unavailable");
            }
        }
    }
}

fn play(
    wav_path: &str,
    generation: &AtomicU64,
    expected_generation: u64,
) -> Result<Option<u64>, String> {
    let mut child = Command::new(APLAY)
        // The device's `default` dmix PCM stays PREPARED for short WAVs and
        // never advances its hardware pointer. Direct hardware playback was
        // verified to enter RUNNING and advance frames on Mini 3.
        .args(["-q", "-D", "hw:0,0", wav_path])
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| format!("could not start {APLAY}: {error}"))?;
    let started_at = Instant::now();
    let timeout = PLAYBACK_DURATION + Duration::from_millis(300);

    loop {
        let current_generation = generation.load(Ordering::Acquire);
        if current_generation != expected_generation {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(Some(current_generation));
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("could not poll aplay: {error}"))?
        {
            eprintln!("aplay exited with {status}");
            if !status.success() {
                return Err(format!("aplay exited with {status}"));
            }
            return Ok(None);
        }
        if started_at.elapsed() >= timeout {
            eprintln!("aplay exceeded SFX timeout; stopping process");
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(4));
    }
}
