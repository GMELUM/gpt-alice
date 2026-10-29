use std::{
    io::{self, Read},
    time::{Duration, Instant},
};

use alsa::pcm::{Access, Format, HwParams, PCM, State};
use alsa::{Direction, ValueOr};

const RATE: u32 = 48_000;
const CHANNELS: usize = 2;
const FRAMES_PER_BLOCK: usize = 480;
const BYTES_PER_FRAME: usize = CHANNELS * 2;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    while stream_output()? {}
    Ok(())
}

fn capture_owner() -> Option<String> {
    let status = std::fs::read_to_string("/proc/asound/card0/pcm1c/sub0/status").ok()?;
    if !status.lines().any(|line| line == "state: RUNNING") {
        return None;
    }
    status
        .lines()
        .find(|line| line.starts_with("owner_pid"))
        .map(str::to_owned)
}

fn stream_output() -> Result<bool, Box<dyn std::error::Error>> {
    // The Rust mixer already combines sources. Open the hardware directly
    // instead of passing through the firmware's additional dmix layer.
    let pcm = PCM::new("hw:0,0", Direction::Playback, false)?;
    let hardware = HwParams::any(&pcm)?;
    hardware.set_channels(CHANNELS as u32)?;
    hardware.set_rate(RATE, ValueOr::Nearest)?;
    hardware.set_format(Format::s16())?;
    hardware.set_access(Access::MMapInterleaved)?;
    hardware.set_period_size_near(960, ValueOr::Nearest)?;
    hardware.set_buffer_size_near(7_680)?;
    pcm.hw_params(&hardware)?;

    let hardware = pcm.hw_params_current()?;
    let period_frames = hardware.get_period_size()?;
    let software = pcm.sw_params_current()?;
    software.set_start_threshold(period_frames)?;
    software.set_avail_min(period_frames)?;
    pcm.sw_params(&software)?;

    eprintln!("audio-sink: ALSA hw:0,0 ready, mmap, {RATE} Hz stereo");
    let initial_capture = capture_owner();
    let mut capture_change: Option<(Option<String>, Instant)> = None;
    let mut check_capture_at = Instant::now();
    let io = pcm.io_i16()?;
    let mut bytes = [0u8; FRAMES_PER_BLOCK * BYTES_PER_FRAME];
    let mut samples = [0i16; FRAMES_PER_BLOCK * CHANNELS];

    let mut primed_frames = 0usize;
    loop {
        if Instant::now() >= check_capture_at {
            check_capture_at = Instant::now() + Duration::from_millis(100);
            let current = capture_owner();
            if current == initial_capture {
                capture_change = None;
            } else if let Some((candidate, since)) = &capture_change {
                if *candidate == current && since.elapsed() >= Duration::from_millis(200) {
                    // Mini 3 capture setup can invalidate the active playback
                    // DMA configuration without reporting an ALSA xrun.
                    eprintln!("audio-sink: capture changed; reopening hardware playback");
                    return Ok(true);
                } else if *candidate != current {
                    capture_change = Some((current, Instant::now()));
                }
            } else {
                capture_change = Some((current, Instant::now()));
            }
        }
        let mut filled = 0;
        while filled < bytes.len() {
            match io::stdin().read(&mut bytes[filled..]) {
                Ok(0) if filled == 0 => {
                    pcm.drain()?;
                    return Ok(false);
                }
                Ok(0) => break,
                Ok(count) => filled += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            }
        }
        let aligned_bytes = filled - (filled % BYTES_PER_FRAME);
        if aligned_bytes == 0 {
            pcm.drain()?;
            return Ok(false);
        }
        let sample_count = aligned_bytes / 2;
        for (sample, encoded) in samples[..sample_count]
            .iter_mut()
            .zip(bytes[..aligned_bytes].chunks_exact(2))
        {
            *sample = i16::from_le_bytes([encoded[0], encoded[1]]);
        }

        let mut written_samples = 0;
        while written_samples < sample_count {
            let result = pcm.avail_update().and_then(|_| {
                io.mmap((sample_count - written_samples) / CHANNELS, |target| {
                    let count = target.len().min(sample_count - written_samples);
                    target[..count]
                        .copy_from_slice(&samples[written_samples..written_samples + count]);
                    count / CHANNELS
                })
            });
            match result {
                Ok(0) => {
                    pcm.wait(None)?;
                }
                Ok(written_frames) => {
                    written_samples += written_frames * CHANNELS;
                    if pcm.state() == State::Prepared {
                        primed_frames += written_frames;
                        if primed_frames >= period_frames as usize {
                            pcm.start()?;
                            primed_frames = 0;
                        }
                    }
                }
                Err(error) => {
                    eprintln!("audio-sink: recovering ALSA write error: {error}");
                    pcm.recover(-error.errno().abs(), true)?;
                    primed_frames = 0;
                }
            }
        }
    }
}
