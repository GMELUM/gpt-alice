use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    process::Command,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::Serialize;

const FRAMEBUFFER: &str = "/dev/fb0";
const FRAMEBUFFER_BYTES: usize = 18;
const VOICE_IDLE_COLOR: [u8; 3] = [0x0f, 0x00, 0x0f];
const VOICE_ACTIVE_COLOR: [u8; 3] = [0xff, 0x00, 0xff];
const SEARCH_COLOR: [u8; 3] = [0xff, 0xc0, 0x00];
const BLUETOOTH_COLOR: [u8; 3] = [0x00, 0x40, 0xff];
const LED_ROOT: &str = "/sys/class/leds/bergamot:white:clock";
const CLOCK_SEGMENTS: [&str; 32] = [
    "A0", "A1", "A2", "A3", "B0", "B1", "B2", "B3", "C0", "C1", "C2", "C3", "D0", "D1", "D2", "D3",
    "E0", "E1", "E2", "E3", "F0", "F1", "F2", "F3", "G0", "G1", "G2", "G3", "DEGREE", "DH", "DL",
    "DM",
];

#[derive(Clone)]
pub struct IndicatorController {
    initial_frame: Arc<Mutex<Option<[u8; FRAMEBUFFER_BYTES]>>>,
    display: Arc<Mutex<DisplayState>>,
    clock_brightness: Arc<Mutex<u8>>,
    ring_state: Arc<Mutex<RingState>>,
    web_search_count: Arc<std::sync::atomic::AtomicUsize>,
    colon_lit: Arc<Mutex<Option<bool>>>,
    colon_should_lit: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Default)]
struct RingState {
    call_active: bool,
    bluetooth_last_audio: Option<std::time::Instant>,
    muted: bool,
    alarm_active: bool,
}

#[derive(Clone, Copy)]
enum DisplayState {
    Clock,
    Volume,
    Timer,
    Face,
}

#[derive(Serialize)]
pub struct IndicatorStatus {
    ring_available: bool,
    ring_resolution: &'static str,
    ring_frame_bytes: usize,
    pub clock_display_brightness: u8,
    clock_segments: Vec<ClockSegmentStatus>,
    platform_service_note: &'static str,
}

#[derive(Serialize)]
pub struct ClockSegmentStatus {
    id: &'static str,
    brightness: Option<u8>,
}

#[derive(Serialize)]
pub struct ActionResult {
    pub ok: bool,
    pub message: String,
}

impl IndicatorController {
    pub fn new() -> Self {
        let initial_frame = read_frame().ok();
        Self {
            initial_frame: Arc::new(Mutex::new(initial_frame)),
            display: Arc::new(Mutex::new(DisplayState::Clock)),
            clock_brightness: Arc::new(Mutex::new(150)),
            ring_state: Arc::new(Mutex::new(RingState::default())),
            web_search_count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            colon_lit: Arc::new(Mutex::new(None)),
            colon_should_lit: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }

    pub fn status(&self) -> IndicatorStatus {
        let clock_segments = CLOCK_SEGMENTS
            .iter()
            .map(|id| ClockSegmentStatus {
                id,
                brightness: read_clock_brightness(id),
            })
            .collect();

        IndicatorStatus {
            ring_available: fs::metadata(FRAMEBUFFER).is_ok(),
            ring_resolution: "6×1",
            ring_frame_bytes: FRAMEBUFFER_BYTES,
            clock_display_brightness: self
                .clock_brightness
                .lock()
                .map(|value| *value)
                .unwrap_or(150),
            clock_segments,
            platform_service_note: "maind --platformd отключён: штатный DSP микрофона и автоматическое обновление часов остановлены.",
        }
    }

    pub fn set_ring_color(&self, hex: &str) -> Result<ActionResult, String> {
        let color = parse_hex_color(hex)?;
        let mut frame = [0_u8; FRAMEBUFFER_BYTES];
        for pixel in frame.chunks_exact_mut(3) {
            // Mini 3 framebuffer pixels are BGR, as documented for this model.
            pixel.copy_from_slice(&[color[2], color[1], color[0]]);
        }
        write_frame(&frame)?;
        let display_color = hex.strip_prefix('#').unwrap_or(hex).to_uppercase();
        Ok(ActionResult {
            ok: true,
            message: format!("Цвет кольца установлен: #{display_color}"),
        })
    }

    pub fn restore_ring(&self) -> Result<ActionResult, String> {
        let frame = self
            .initial_frame
            .lock()
            .map_err(|_| "Не удалось получить сохранённый кадр кольца".to_owned())?
            .ok_or_else(|| {
                "Начальный кадр кольца не был доступен при запуске сервера".to_owned()
            })?;
        write_frame(&frame)?;
        Ok(ActionResult {
            ok: true,
            message: "Начальный кадр кольца восстановлен".to_owned(),
        })
    }

    pub fn render_voice_idle(&self) -> Result<(), String> {
        if let Ok(mut state) = self.ring_state.lock() {
            state.call_active = false;
        }
        self.render_ring_frame(0)
    }

    pub fn set_voice_call_active(&self, active: bool) {
        if let Ok(mut state) = self.ring_state.lock() {
            state.call_active = active;
        }
    }

    /// Mark a client web search as active. A counter allows overlapping calls to
    /// finish independently without restoring the call color too early.
    pub fn set_web_search_active(&self, active: bool) {
        use std::sync::atomic::Ordering;
        if active {
            self.web_search_count.fetch_add(1, Ordering::AcqRel);
        } else {
            let mut current = self.web_search_count.load(Ordering::Acquire);
            while current > 0 {
                match self.web_search_count.compare_exchange_weak(
                    current,
                    current - 1,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => break,
                    Err(actual) => current = actual,
                }
            }
        }
    }

    pub fn mark_bluetooth_audio(&self) {
        if let Ok(mut state) = self.ring_state.lock() {
            state.bluetooth_last_audio = Some(std::time::Instant::now());
        }
    }

    pub fn set_microphone_muted(&self, muted: bool) {
        if let Ok(mut state) = self.ring_state.lock() {
            state.muted = muted;
        }
    }

    pub fn set_alarm_active(&self, active: bool) {
        if let Ok(mut state) = self.ring_state.lock() {
            state.alarm_active = active;
        }
    }

    /// Render the current ring state. Priority: alarm, microphone mute, search,
    /// live conversation, active Bluetooth stream, then the dim idle color.
    pub fn render_ring_frame(&self, phase: usize) -> Result<(), String> {
        let state = self
            .ring_state
            .lock()
            .map_err(|_| "Не удалось получить состояние кольца".to_owned())?;
        let searching = self
            .web_search_count
            .load(std::sync::atomic::Ordering::Acquire)
            > 0;
        let bluetooth_active = state
            .bluetooth_last_audio
            .is_some_and(|at| at.elapsed() < Duration::from_millis(700));
        let alarm = state.alarm_active;
        let color = if alarm {
            [0xff, 0x00, 0x00]
        } else if state.muted {
            [0xff, 0x50, 0x00]
        } else if searching {
            SEARCH_COLOR
        } else if state.call_active {
            VOICE_ACTIVE_COLOR
        } else if bluetooth_active {
            BLUETOOTH_COLOR
        } else {
            VOICE_IDLE_COLOR
        };
        let animated = state.alarm_active || searching || state.call_active || bluetooth_active;
        let dim = if alarm && phase % 2 == 1 {
            [0x20, 0x00, 0x00]
        } else if searching {
            [0x20, 0x18, 0x00]
        } else if state.call_active && !searching {
            VOICE_IDLE_COLOR
        } else if bluetooth_active && !searching {
            [0x00, 0x08, 0x20]
        } else {
            color
        };
        drop(state);
        let mut frame = [0_u8; FRAMEBUFFER_BYTES];
        for (index, pixel) in frame.chunks_exact_mut(3).enumerate() {
            let pixel_color = if animated && index == phase % 6 {
                color
            } else {
                dim
            };
            // Mini 3 framebuffer pixels are BGR.
            pixel.copy_from_slice(&[pixel_color[2], pixel_color[1], pixel_color[0]]);
        }
        write_frame(&frame)
    }

    /// Update only the two clock separator LEDs at 2 Hz; avoid repainting the
    /// full 32-segment display on every blink tick.
    pub fn refresh_clock_separator(&self) -> Result<(), String> {
        let display = *self
            .display
            .lock()
            .map_err(|_| "Не удалось получить состояние дисплея".to_owned())?;
        if !matches!(display, DisplayState::Clock | DisplayState::Timer) {
            return Ok(());
        }
        let lit = self
            .colon_should_lit
            .load(std::sync::atomic::Ordering::Acquire);
        let mut previous = self
            .colon_lit
            .lock()
            .map_err(|_| "Не удалось получить состояние двоеточия".to_owned())?;
        if *previous == Some(lit) {
            return Ok(());
        }
        let brightness = if lit {
            self.clock_brightness
                .lock()
                .map(|value| *value)
                .unwrap_or(150)
        } else {
            0
        };
        self.write_clock_brightness("DH", brightness)?;
        self.write_clock_brightness("DL", brightness)?;
        *previous = Some(lit);
        Ok(())
    }

    pub fn advance_clock_separator(&self) -> Result<(), String> {
        self.colon_should_lit
            .fetch_xor(true, std::sync::atomic::Ordering::AcqRel);
        self.refresh_clock_separator()
    }

    pub fn set_clock_brightness(
        &self,
        segment: &str,
        brightness: u8,
    ) -> Result<ActionResult, String> {
        if !CLOCK_SEGMENTS.contains(&segment) {
            return Err("Неизвестный сегмент часов".to_owned());
        }
        let path = format!("{LED_ROOT}{segment}/brightness");
        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg("printf '%s\\n' \"$1\" > \"$2\"")
            .arg("gpt-alice-clock")
            .arg(brightness.to_string())
            .arg(&path)
            .output()
            .map_err(|error| format!("Не удалось запустить запись сегмента {segment}: {error}"))?;
        if !output.status.success() {
            let mut actual = read_clock_brightness(segment);
            for _ in 0..20 {
                if actual == Some(brightness) {
                    break;
                }
                thread::sleep(Duration::from_millis(50));
                actual = read_clock_brightness(segment);
            }
            if actual != Some(brightness) {
                return Err(format!(
                    "Не удалось изменить яркость сегмента {segment}: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
        }
        Ok(ActionResult {
            ok: true,
            message: format!("Яркость сегмента {segment}: {brightness}"),
        })
    }

    pub fn set_clock_display_brightness(
        &self,
        brightness: u8,
        utc_offset_minutes: i16,
    ) -> Result<ActionResult, String> {
        *self
            .clock_brightness
            .lock()
            .map_err(|_| "Не удалось получить яркость экрана часов".to_owned())? = brightness;
        // A clock brightness command must affect the visible display now, even
        // when a volume overlay or face left the indicator controller in a
        // different scene. Runtime and indicator scene state can be updated by
        // separate input paths, so redraw the clock explicitly.
        self.render_idle_clock(utc_offset_minutes)?;
        Ok(ActionResult {
            ok: true,
            message: format!(
                "Яркость экрана часов установлена: {}%",
                (u16::from(brightness) * 100 / 255)
            ),
        })
    }

    pub fn render_face(&self, lit: Vec<(&'static str, u8)>) -> Result<(), String> {
        for segment in &CLOCK_SEGMENTS {
            let brightness = lit
                .iter()
                .find(|(id, _)| id == segment)
                .map(|(_, value)| *value)
                .unwrap_or(0);
            if read_clock_brightness(segment) != Some(brightness) {
                self.write_clock_brightness(segment, brightness)?;
            }
        }
        *self
            .display
            .lock()
            .map_err(|_| "Не удалось получить состояние дисплея".to_owned())? = DisplayState::Face;
        *self
            .colon_lit
            .lock()
            .map_err(|_| "Не удалось получить состояние двоеточия".to_owned())? = None;
        Ok(())
    }

    pub fn render_idle_clock(&self, utc_offset_minutes: i16) -> Result<(), String> {
        let brightness = self
            .clock_brightness
            .lock()
            .map(|value| *value)
            .unwrap_or(150);
        let utc_seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| format!("Системное время раньше Unix epoch: {error}"))?
            .as_secs();
        let local_seconds =
            (i128::from(utc_seconds) + i128::from(utc_offset_minutes) * 60).rem_euclid(86_400);
        let digits = format!(
            "{:02}{:02}",
            local_seconds / 3_600,
            (local_seconds % 3_600) / 60
        );
        let digits: Vec<u8> = digits.bytes().map(|b| b - b'0').collect();
        let patterns: [[&str; 7]; 10] = [
            ["A", "B", "C", "D", "E", "F", ""],
            ["B", "C", "", "", "", "", ""],
            ["A", "B", "G", "E", "D", "", ""],
            ["A", "B", "G", "C", "D", "", ""],
            ["F", "G", "B", "C", "", "", ""],
            ["A", "F", "G", "C", "D", "", ""],
            ["A", "F", "G", "E", "C", "D", ""],
            ["A", "B", "C", "", "", "", ""],
            ["A", "B", "C", "D", "E", "F", "G"],
            ["A", "B", "C", "D", "F", "G", ""],
        ];
        let mut lit = Vec::new();
        for (digit_index, digit) in digits.into_iter().enumerate() {
            for segment in patterns[digit as usize] {
                if !segment.is_empty() {
                    lit.push((format!("{segment}{digit_index}"), brightness));
                }
            }
        }
        let lit: Vec<(&'static str, u8)> = lit
            .into_iter()
            .filter_map(|(id, b)| {
                CLOCK_SEGMENTS
                    .iter()
                    .find(|known| **known == id)
                    .map(|id| (*id, b))
            })
            .collect();
        self.render_face(lit)?;
        *self
            .display
            .lock()
            .map_err(|_| "Не удалось получить состояние дисплея".to_owned())? = DisplayState::Clock;
        self.refresh_clock_separator()
    }

    pub fn render_volume(&self, volume: u8) -> Result<(), String> {
        let volume = volume.min(100);
        let value = volume.to_string();
        let mut lit = Vec::new();
        let first_position = 4 - value.len();
        for (index, digit) in value.bytes().enumerate() {
            let digit = usize::from(digit - b'0');
            let position = first_position + index;
            for segment in digit_segments(digit) {
                let id = format!("{segment}{position}");
                if let Some(id) = CLOCK_SEGMENTS.iter().find(|known| **known == id) {
                    lit.push((*id, 200));
                }
            }
        }
        self.render_face(lit)?;
        *self
            .display
            .lock()
            .map_err(|_| "Не удалось получить состояние дисплея".to_owned())? =
            DisplayState::Volume;
        Ok(())
    }

    pub fn render_timer_countdown(&self, seconds: u64) -> Result<(), String> {
        let brightness = self
            .clock_brightness
            .lock()
            .map(|value| *value)
            .unwrap_or(150);
        let seconds = seconds.min(99 * 60 + 59);
        let digits = [
            seconds / 60 / 10,
            seconds / 60 % 10,
            seconds % 60 / 10,
            seconds % 10,
        ];
        let mut lit = Vec::new();
        for (position, digit) in digits.into_iter().enumerate() {
            for segment in digit_segments(digit as usize) {
                let id = format!("{segment}{position}");
                if let Some(id) = CLOCK_SEGMENTS.iter().find(|known| **known == id) {
                    lit.push((*id, brightness));
                }
            }
        }
        self.render_face(lit)?;
        *self
            .display
            .lock()
            .map_err(|_| "Не удалось получить состояние дисплея".to_owned())? = DisplayState::Timer;
        self.refresh_clock_separator()
    }

    pub fn refresh_display(&self, utc_offset_minutes: i16) -> Result<(), String> {
        let display = *self
            .display
            .lock()
            .map_err(|_| "Не удалось получить состояние дисплея".to_owned())?;
        match display {
            DisplayState::Clock => self.render_idle_clock(utc_offset_minutes),
            DisplayState::Volume | DisplayState::Timer | DisplayState::Face => Ok(()),
        }
    }

    fn write_clock_brightness(&self, segment: &str, brightness: u8) -> Result<(), String> {
        let path = format!("{LED_ROOT}{segment}/brightness");
        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg("printf '%s\\n' \"$1\" > \"$2\"")
            .arg("gpt-alice-clock")
            .arg(brightness.to_string())
            .arg(&path)
            .output()
            .map_err(|error| format!("Не удалось запустить запись сегмента {segment}: {error}"))?;
        if !output.status.success() {
            let mut actual = read_clock_brightness(segment);
            for _ in 0..20 {
                if actual == Some(brightness) {
                    break;
                }
                thread::sleep(Duration::from_millis(50));
                actual = read_clock_brightness(segment);
            }
            if actual != Some(brightness) {
                return Err(format!(
                    "Не удалось изменить яркость сегмента {segment}: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
        }
        Ok(())
    }
}

fn digit_segments(digit: usize) -> &'static [&'static str] {
    const DIGITS: [&[&str]; 10] = [
        &["A", "B", "C", "D", "E", "F"],
        &["B", "C"],
        &["A", "B", "G", "E", "D"],
        &["A", "B", "G", "C", "D"],
        &["F", "G", "B", "C"],
        &["A", "F", "G", "C", "D"],
        &["A", "F", "G", "E", "C", "D"],
        &["A", "B", "C"],
        &["A", "B", "C", "D", "E", "F", "G"],
        &["A", "B", "C", "D", "F", "G"],
    ];
    DIGITS[digit.min(9)]
}

fn parse_hex_color(value: &str) -> Result<[u8; 3], String> {
    let value = value.strip_prefix('#').unwrap_or(value);
    if value.len() != 6 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("Укажите цвет в формате #RRGGBB".to_owned());
    }
    let red = u8::from_str_radix(&value[0..2], 16).map_err(|_| "Некорректный цвет".to_owned())?;
    let green = u8::from_str_radix(&value[2..4], 16).map_err(|_| "Некорректный цвет".to_owned())?;
    let blue = u8::from_str_radix(&value[4..6], 16).map_err(|_| "Некорректный цвет".to_owned())?;
    Ok([red, green, blue])
}

fn read_frame() -> Result<[u8; FRAMEBUFFER_BYTES], String> {
    let mut file = OpenOptions::new()
        .read(true)
        .open(FRAMEBUFFER)
        .map_err(|error| format!("Не удалось открыть {FRAMEBUFFER}: {error}"))?;
    let mut frame = [0_u8; FRAMEBUFFER_BYTES];
    file.read_exact(&mut frame)
        .map_err(|error| format!("Не удалось прочитать кадр кольца: {error}"))?;
    Ok(frame)
}

fn write_frame(frame: &[u8; FRAMEBUFFER_BYTES]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .open(FRAMEBUFFER)
        .map_err(|error| format!("Не удалось открыть {FRAMEBUFFER}: {error}"))?;
    file.write_all(frame)
        .map_err(|error| format!("Не удалось записать кадр кольца: {error}"))
}

fn read_clock_brightness(segment: &str) -> Option<u8> {
    let path = format!("{LED_ROOT}{segment}/brightness");
    fs::read_to_string(path).ok()?.trim().parse().ok()
}
