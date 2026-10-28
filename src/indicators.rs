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
}

#[derive(Clone, Copy)]
enum DisplayState {
    Clock,
    Volume(u8),
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
        let mut frame = [0_u8; FRAMEBUFFER_BYTES];
        for pixel in frame.chunks_exact_mut(3) {
            pixel.copy_from_slice(&[
                VOICE_IDLE_COLOR[2],
                VOICE_IDLE_COLOR[1],
                VOICE_IDLE_COLOR[0],
            ]);
        }
        write_frame(&frame)
    }

    /// Draw a rotating bright pink pixel over the dim pink call-idle background.
    pub fn render_voice_active_frame(&self, bright_pixel: usize) -> Result<(), String> {
        let mut frame = [0_u8; FRAMEBUFFER_BYTES];
        for (index, pixel) in frame.chunks_exact_mut(3).enumerate() {
            let color = if index == bright_pixel % 6 {
                VOICE_ACTIVE_COLOR
            } else {
                VOICE_IDLE_COLOR
            };
            // Mini 3 framebuffer pixels are BGR.
            pixel.copy_from_slice(&[color[2], color[1], color[0]]);
        }
        write_frame(&frame)
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
        Ok(())
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
            DisplayState::Volume(volume);
        Ok(())
    }

    pub fn refresh_display(&self, utc_offset_minutes: i16) -> Result<(), String> {
        let display = *self
            .display
            .lock()
            .map_err(|_| "Не удалось получить состояние дисплея".to_owned())?;
        match display {
            DisplayState::Clock => self.render_idle_clock(utc_offset_minutes),
            DisplayState::Volume(volume) => self.render_volume(volume),
            DisplayState::Face => Ok(()),
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
