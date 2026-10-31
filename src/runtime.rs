use std::{
    collections::VecDeque,
    env,
    fs::{self, File, OpenOptions},
    io::Read,
    net::ToSocketAddrs,
    os::fd::AsRawFd,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::indicators::{ActionResult, IndicatorController};
use crate::mixer::AudioMixer;

const GPIO_KEYS: &str = "/dev/input/event0";
const TOUCH_KEYS: &str = "/dev/input/event1";
#[cfg(target_os = "linux")]
const EVIOCGSW_16: libc::Ioctl = 0x8002_451b_u32 as libc::Ioctl;
#[cfg(not(target_os = "linux"))]
const EVIOCGSW_16: libc::c_ulong = 0x8002_451b_u32 as libc::c_ulong;
const EV_SW: u16 = 5;
const SW_MUTE_DEVICE: u16 = 14;
const DEFAULT_CONFIG: &str = "/data/opt/gpt-alice/config.json";
const DEFAULT_NTP_SERVERS: [&str; 2] = ["time.cloudflare.com", "time.apple.com"];

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DeviceEvent {
    Button {
        button: String,
        action: String,
        key_code: u16,
        key_name: String,
        at_ms: u128,
    },
    Touch {
        button: String,
        action: String,
        key_code: u16,
        key_name: String,
        at_ms: u128,
    },
    InputUnavailable {
        device: String,
        error: String,
    },
    SceneChanged {
        scene: String,
    },
    MicrophoneMuteChanged {
        muted: bool,
        source: String,
        at_ms: u128,
    },
}

#[derive(Serialize)]
pub struct RuntimeStatus {
    pub input_devices: Vec<InputDeviceStatus>,
    pub last_event: Option<DeviceEvent>,
    pub recent_events: Vec<DeviceEvent>,
    pub event_count: u64,
    pub display_scene: String,
    pub display_until_ms: Option<u128>,
    pub microphone: MicrophoneStatus,
    pub volume: u8,
    pub output_muted: bool,
    pub bluetooth: BluetoothStatus,
}

#[derive(Clone, Serialize)]
pub struct BluetoothStatus {
    pub available: bool,
    pub enabled: Option<bool>,
    pub connected: bool,
    pub device_name: Option<String>,
    pub volume: u8,
    pub ducked: bool,
}

#[derive(Serialize)]
pub struct InputDeviceStatus {
    path: &'static str,
    name: &'static str,
    available: bool,
}

#[derive(Serialize)]
pub struct MicrophoneStatus {
    pub available: bool,
    pub muted: Option<bool>,
    pub pcm: &'static str,
    pub channels: u8,
    pub sample_rate: u32,
    pub note: &'static str,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TimeSettings {
    pub utc_offset_minutes: i16,
    pub ntp_servers: Vec<String>,
    pub clock_brightness_percent: u8,
    pub output_volume_percent: u8,
}

impl Default for TimeSettings {
    fn default() -> Self {
        Self {
            utc_offset_minutes: 180,
            ntp_servers: DEFAULT_NTP_SERVERS
                .iter()
                .map(|server| (*server).to_owned())
                .collect(),
            clock_brightness_percent: 59,
            output_volume_percent: 50,
        }
    }
}

#[derive(Serialize)]
pub struct TimeStatus {
    pub utc_offset_minutes: i16,
    pub timezone_label: String,
    pub local_time: String,
    pub ntp_servers: Vec<String>,
    pub ntp_synchronized: bool,
    pub ntp_tracking: String,
    pub ntp_sources: String,
}

#[derive(Clone)]
struct DisplayScene {
    name: String,
    until: Option<Instant>,
}

struct State {
    last_event: Option<DeviceEvent>,
    recent_events: VecDeque<DeviceEvent>,
    event_count: u64,
    display: DisplayScene,
    time_settings: TimeSettings,
    microphone_muted: Option<bool>,
    volume: u8,
    output_muted: bool,
    bluetooth_enabled: Option<bool>,
    bluetooth_volume: u8,
    bluetooth_ducked: bool,
    bluetooth_duck_gain: f32,
    timer_remaining_seconds: Option<u64>,
    button_callbacks: Vec<Arc<dyn Fn(DeviceEvent) + Send + Sync>>,
}

pub struct RuntimeController {
    indicators: Arc<IndicatorController>,
    audio_mixer: Arc<AudioMixer>,
    state: Arc<Mutex<State>>,
    events: broadcast::Sender<DeviceEvent>,
    config_path: PathBuf,
}

impl RuntimeController {
    pub fn new(indicators: Arc<IndicatorController>, audio_mixer: Arc<AudioMixer>) -> Self {
        let (events, _) = broadcast::channel(64);
        let config_path = env::var_os("GPT_ALICE_CONFIG_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG));
        let time_settings = load_time_settings(&config_path);
        let initial_volume = time_settings.output_volume_percent;
        audio_mixer.set_output_volume(initial_volume);
        // The shared mixer owns software volume. Keep the hardware mixer at unity
        // so it cannot mute or scale only part of the mixed signal.
        if let Err(error) = set_system_volume(100) {
            eprintln!("could not normalize ALSA hardware volume: {error}");
        }
        let initial_brightness =
            ((u16::from(time_settings.clock_brightness_percent) * 255 + 50) / 100) as u8;
        let _ = indicators
            .set_clock_display_brightness(initial_brightness, time_settings.utc_offset_minutes);
        let state = Arc::new(Mutex::new(State {
            last_event: None,
            recent_events: VecDeque::with_capacity(16),
            event_count: 0,
            display: DisplayScene {
                name: "clock".into(),
                until: None,
            },
            time_settings,
            microphone_muted: None,
            volume: initial_volume,
            output_muted: false,
            bluetooth_enabled: read_bluetooth_power(),
            bluetooth_volume: read_bluetooth_volume().unwrap_or(88),
            bluetooth_ducked: false,
            bluetooth_duck_gain: 1.0,
            timer_remaining_seconds: None,
            button_callbacks: Vec::new(),
        }));
        Self::bluetooth_duck_loop(Arc::clone(&state));
        Self {
            indicators,
            audio_mixer,
            state,
            events,
            config_path,
        }
    }

    pub fn indicators(&self) -> &IndicatorController {
        &self.indicators
    }

    pub fn queue_audio_test_tone(&self) -> Result<ActionResult, String> {
        self.audio_mixer.queue_test_tone()?;
        Ok(ActionResult {
            ok: true,
            message: "Тестовый тон поставлен в общий аудиомикшер".into(),
        })
    }

    pub fn set_timer_display(&self, remaining_seconds: Option<u64>) {
        if let Ok(mut state) = self.state.lock() {
            state.timer_remaining_seconds = remaining_seconds;
            if state.display.until.is_none() {
                if let Some(seconds) = remaining_seconds {
                    let _ = self.indicators.render_timer_countdown(seconds);
                } else {
                    let offset = state.time_settings.utc_offset_minutes;
                    let _ = self.indicators.render_idle_clock(offset);
                }
            }
        }
    }

    pub fn start(&self) {
        for (path, is_touch) in [(GPIO_KEYS, false), (TOUCH_KEYS, true)] {
            let state = Arc::clone(&self.state);
            let events = self.events.clone();
            thread::Builder::new()
                .name(format!(
                    "input-{}",
                    path.rsplit('/').next().unwrap_or("event")
                ))
                .spawn(move || input_loop(path, is_touch, state, events))
                .expect("failed to start input reader");
        }
        let state = Arc::clone(&self.state);
        let events = self.events.clone();
        let indicators = Arc::clone(&self.indicators);
        let config_path = self.config_path.clone();
        let settings = self
            .state
            .lock()
            .expect("runtime state lock poisoned")
            .time_settings
            .clone();
        let _ = save_time_settings(&config_path, &settings);
        Self::bluetooth_audio_bridge_loop(
            Arc::clone(&self.state),
            Arc::clone(&self.audio_mixer),
            Arc::clone(&self.indicators),
        );
        let startup_servers = settings.ntp_servers.clone();
        thread::Builder::new()
            .name("ntp-sources".into())
            .spawn(move || {
                let _ = apply_ntp_sources(&startup_servers, &[]);
            })
            .expect("failed to start NTP source configuration");
        let _ = indicators.render_idle_clock(settings.utc_offset_minutes);
        let ring_state = Arc::clone(&state);
        let ring_indicators = Arc::clone(&indicators);
        thread::Builder::new()
            .name("ring-status".into())
            .spawn(move || {
                let mut phase = 0;
                loop {
                    let muted = ring_state
                        .lock()
                        .ok()
                        .and_then(|state| state.microphone_muted)
                        .unwrap_or(false);
                    ring_indicators.set_microphone_muted(muted);
                    let _ = ring_indicators.render_ring_frame(phase);
                    phase = (phase + 1) % 6;
                    thread::sleep(Duration::from_millis(150));
                }
            })
            .expect("failed to start ring status renderer");
        let separator_indicators = Arc::clone(&indicators);
        thread::Builder::new()
            .name("clock-separator".into())
            .spawn(move || {
                let _ = separator_indicators.refresh_clock_separator();
                loop {
                    thread::sleep(Duration::from_millis(500));
                    let _ = separator_indicators.advance_clock_separator();
                }
            })
            .expect("failed to start clock separator renderer");
        thread::Builder::new()
            .name("display-idle".into())
            .spawn(move || {
                loop {
                    thread::sleep(Duration::from_secs(1));
                    let offset = state
                        .lock()
                        .map(|state| state.time_settings.utc_offset_minutes)
                        .unwrap_or(0);
                    let (expired, timer_remaining, idle) = state
                        .lock()
                        .map(|mut state| {
                            let expired = state
                                .display
                                .until
                                .is_some_and(|deadline| Instant::now() >= deadline);
                            if expired {
                                state.display = DisplayScene {
                                    name: "clock".into(),
                                    until: None,
                                };
                            }
                            (
                                expired,
                                state.timer_remaining_seconds,
                                state.display.until.is_none(),
                            )
                        })
                        .unwrap_or((false, None, false));
                    if expired {
                        let event = DeviceEvent::SceneChanged {
                            scene: "clock".into(),
                        };
                        record_event(&state, &events, event);
                        if let Some(seconds) = timer_remaining {
                            let _ = indicators.render_timer_countdown(seconds);
                        } else {
                            let _ = indicators.render_idle_clock(offset);
                        }
                    } else if idle {
                        if let Some(seconds) = timer_remaining {
                            let _ = indicators.render_timer_countdown(seconds);
                        }
                    }
                }
            })
            .expect("failed to start display idle manager");
    }

    pub fn status(&self) -> RuntimeStatus {
        let state = self.state.lock().expect("runtime state lock poisoned");
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let display_until_ms = state.display.until.map(|deadline| {
            now_ms
                + deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis()
        });
        let connected_device = read_bluetooth_connected();
        RuntimeStatus {
            input_devices: vec![
                InputDeviceStatus {
                    path: GPIO_KEYS,
                    name: "gpio-keys (KEY_MUTE + SW_MUTE_DEVICE)",
                    available: OpenOptions::new().read(true).open(GPIO_KEYS).is_ok(),
                },
                InputDeviceStatus {
                    path: TOUCH_KEYS,
                    name: "aw933xx touch sensor",
                    available: OpenOptions::new().read(true).open(TOUCH_KEYS).is_ok(),
                },
            ],
            last_event: state.last_event.clone(),
            recent_events: state.recent_events.iter().cloned().collect(),
            event_count: state.event_count,
            display_scene: state.display.name.clone(),
            display_until_ms,
            microphone: MicrophoneStatus {
                available: std::path::Path::new("/dev/snd/pcmC0D1c").exists(),
                muted: state.microphone_muted,
                pcm: "hw:0,1",
                channels: 4,
                sample_rate: 16_000,
                note: "Устройство захвата найдено; прямой arecord -D hw:0,1 -f S16_LE -r 16000 -c 4 -d 1 /dev/null прошёл. VQE/DSP Яндекса не применяется.",
            },
            volume: state.volume,
            output_muted: state.output_muted,
            bluetooth: BluetoothStatus {
                available: std::path::Path::new("/sys/class/bluetooth/hci0").exists(),
                enabled: state.bluetooth_enabled,
                connected: connected_device.is_some(),
                device_name: connected_device,
                volume: state.bluetooth_volume,
                ducked: state.bluetooth_ducked,
            },
        }
    }

    pub fn time_status(&self) -> TimeStatus {
        let settings = self
            .state
            .lock()
            .expect("runtime state lock poisoned")
            .time_settings
            .clone();
        let utc_seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let (hours, minutes) = local_clock(utc_seconds, settings.utc_offset_minutes);
        let tracking = chronyc_output(&["tracking"]).unwrap_or_else(|error| error);
        let sources = chronyc_output(&["-n", "sources"]).unwrap_or_else(|error| error);
        TimeStatus {
            utc_offset_minutes: settings.utc_offset_minutes,
            timezone_label: format_utc_offset(settings.utc_offset_minutes),
            local_time: format!("{hours:02}:{minutes:02}"),
            ntp_servers: settings.ntp_servers,
            ntp_synchronized: sources.lines().any(|line| {
                line.split_whitespace()
                    .next()
                    .is_some_and(|marker| marker == "^*" || marker == "=*")
            }),
            ntp_tracking: tracking,
            ntp_sources: sources,
        }
    }

    pub fn update_time_settings(&self, settings: TimeSettings) -> Result<ActionResult, String> {
        validate_time_settings(&settings)?;
        let old_servers = self
            .state
            .lock()
            .map_err(|_| "Не удалось прочитать текущие настройки времени".to_owned())?
            .time_settings
            .ntp_servers
            .clone();
        apply_ntp_sources(&settings.ntp_servers, &old_servers)?;
        save_time_settings(&self.config_path, &settings)?;
        let offset = settings.utc_offset_minutes;
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| "Не удалось сохранить настройки времени".to_owned())?;
            state.time_settings = settings;
            if state.display.until.is_none() {
                self.indicators.render_idle_clock(offset)?;
            }
        }
        Ok(ActionResult {
            ok: true,
            message: format!(
                "Часовой пояс {} сохранён; серверы синхронизации добавлены в chronyd",
                format_utc_offset(offset)
            ),
        })
    }

    pub fn show_face(&self, face: &str, seconds: u64) -> Result<ActionResult, String> {
        if seconds == 0 || seconds > 3600 {
            return Err("Длительность должна быть от 1 до 3600 секунд".into());
        }
        let brightness = face_brightness(face)?;
        self.indicators.render_face(brightness)?;
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| "Не удалось получить состояние дисплея".to_owned())?;
            state.display = DisplayScene {
                name: face.to_owned(),
                until: Some(Instant::now() + Duration::from_secs(seconds)),
            };
        }
        let event = DeviceEvent::SceneChanged {
            scene: face.to_owned(),
        };
        self.record(event);
        Ok(ActionResult {
            ok: true,
            message: format!("Сцена {face} показана на {seconds} с; затем вернутся часы"),
        })
    }

    fn record(&self, event: DeviceEvent) {
        record_event(&self.state, &self.events, event);
    }

    pub fn on_button(&self, callback: impl Fn(DeviceEvent) + Send + Sync + 'static) {
        self.state
            .lock()
            .expect("runtime state lock poisoned")
            .button_callbacks
            .push(Arc::new(callback));
    }

    pub fn handle_volume_button(&self, event: &DeviceEvent) -> bool {
        let direction = match event {
            DeviceEvent::Touch { button, .. } | DeviceEvent::Button { button, .. }
                if button == "volume_up" =>
            {
                1_i16
            }
            DeviceEvent::Touch { button, .. } | DeviceEvent::Button { button, .. }
                if button == "volume_down" =>
            {
                -1_i16
            }
            _ => return false,
        };

        let (volume, output_muted, changed, settings) = {
            let Ok(mut state) = self.state.lock() else {
                return false;
            };
            let next_volume = (i16::from(state.volume) + direction * 5).clamp(0, 100) as u8;
            let changed = next_volume != state.volume;
            if changed {
                if !state.output_muted {
                    self.audio_mixer.set_output_volume(next_volume);
                }
                state.volume = next_volume;
                state.time_settings.output_volume_percent = next_volume;
            }
            let output_muted = state.output_muted;
            state.display = DisplayScene {
                name: "volume".into(),
                until: Some(Instant::now() + Duration::from_secs(2)),
            };
            (
                state.volume,
                output_muted,
                changed,
                state.time_settings.clone(),
            )
        };
        if changed {
            if let Err(error) = save_time_settings(&self.config_path, &settings) {
                eprintln!("could not save output volume: {error}");
            }
        }
        if let Err(error) = self
            .indicators
            .render_volume(if output_muted { 0 } else { volume })
        {
            eprintln!("volume display update failed: {error}");
        }
        changed
    }

    pub fn set_output_volume(&self, volume: u8) -> Result<ActionResult, String> {
        if volume > 100 {
            return Err("Громкость должна быть от 0 до 100".into());
        }
        let (muted, settings) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| "Не удалось получить состояние громкости".to_owned())?;
            if !state.output_muted {
                self.audio_mixer.set_output_volume(volume);
            }
            state.volume = volume;
            state.time_settings.output_volume_percent = volume;
            state.display = DisplayScene {
                name: "volume".into(),
                until: Some(Instant::now() + Duration::from_secs(2)),
            };
            (state.output_muted, state.time_settings.clone())
        };
        save_time_settings(&self.config_path, &settings)?;
        self.indicators
            .render_volume(if muted { 0 } else { volume })?;
        Ok(ActionResult {
            ok: true,
            message: if muted {
                format!("Уровень громкости сохранён: {volume}%, звук пока выключен")
            } else {
                format!("Громкость установлена: {volume}%")
            },
        })
    }

    pub fn adjust_output_volume(&self, delta: i16) -> Result<ActionResult, String> {
        let current = self
            .state
            .lock()
            .map_err(|_| "Не удалось получить состояние громкости".to_owned())?
            .volume;
        let next = (i16::from(current) + delta).clamp(0, 100) as u8;
        self.set_output_volume(next)
    }

    pub fn set_output_muted(&self, muted: bool) -> Result<ActionResult, String> {
        let volume = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| "Не удалось получить состояние громкости".to_owned())?;
            if state.output_muted == muted {
                state.volume
            } else {
                self.audio_mixer
                    .set_output_volume(if muted { 0 } else { state.volume });
                state.output_muted = muted;
                state.volume
            }
        };
        if let Ok(mut state) = self.state.lock() {
            state.display = DisplayScene {
                name: "volume".into(),
                until: Some(Instant::now() + Duration::from_secs(2)),
            };
        }
        self.indicators
            .render_volume(if muted { 0 } else { volume })?;
        Ok(ActionResult {
            ok: true,
            message: if muted {
                "Звук выключен".into()
            } else {
                format!("Звук включён, громкость {volume}%")
            },
        })
    }

    pub fn set_clock_brightness_percent(&self, percent: u8) -> Result<ActionResult, String> {
        if percent > 100 {
            return Err("Яркость часов должна быть от 0 до 100 процентов".into());
        }
        let offset = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| "Не удалось получить настройки экрана".to_owned())?;
            let mut settings = state.time_settings.clone();
            settings.clock_brightness_percent = percent;
            save_time_settings(&self.config_path, &settings)?;
            state.time_settings = settings.clone();
            state.display = DisplayScene {
                name: "clock".into(),
                until: None,
            };
            settings.utc_offset_minutes
        };
        let brightness = ((u16::from(percent) * 255 + 50) / 100) as u8;
        self.indicators
            .set_clock_display_brightness(brightness, offset)
    }

    pub fn adjust_clock_brightness_percent(&self, delta: i16) -> Result<ActionResult, String> {
        let current = self
            .state
            .lock()
            .map_err(|_| "Не удалось получить текущую яркость часов".to_owned())?
            .time_settings
            .clock_brightness_percent;
        let next = (i16::from(current) + delta).clamp(0, 100) as u8;
        self.set_clock_brightness_percent(next)
    }

    pub fn set_bluetooth_enabled(&self, enabled: bool) -> Result<ActionResult, String> {
        if enabled {
            run_bluetoothctl(&["power", "on"])?;
            run_bluetoothctl(&["discoverable-timeout", "0"])?;
            run_bluetoothctl(&["pairable", "on"])?;
            run_bluetoothctl(&["discoverable", "on"])?;
        } else {
            run_bluetoothctl(&["discoverable", "off"])?;
            run_bluetoothctl(&["power", "off"])?;
        }
        let mut actual = read_bluetooth_power();
        for _ in 0..20 {
            if actual == Some(enabled) {
                break;
            }
            thread::sleep(Duration::from_millis(100));
            actual = read_bluetooth_power();
        }
        if actual != Some(enabled) {
            return Err("Bluetooth не подтвердил изменение состояния".into());
        }
        self.state
            .lock()
            .map_err(|_| "Не удалось сохранить состояние Bluetooth".to_owned())?
            .bluetooth_enabled = actual;
        Ok(ActionResult {
            ok: true,
            message: if enabled {
                "Bluetooth включён, колонка доступна для сопряжения".into()
            } else {
                "Bluetooth выключен".into()
            },
        })
    }

    pub fn set_bluetooth_volume(&self, volume: u8) -> Result<ActionResult, String> {
        if volume > 100 {
            return Err("Громкость Bluetooth должна быть от 0 до 100".into());
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| "Не удалось получить состояние Bluetooth".to_owned())?;
        state.bluetooth_volume = volume;
        Ok(ActionResult {
            ok: true,
            message: format!("Громкость Bluetooth установлена: {volume}%"),
        })
    }

    pub fn adjust_bluetooth_volume(&self, delta: i16) -> Result<ActionResult, String> {
        let current = self
            .state
            .lock()
            .map_err(|_| "Не удалось получить состояние Bluetooth".to_owned())?
            .bluetooth_volume;
        self.set_bluetooth_volume((i16::from(current) + delta).clamp(0, 100) as u8)
    }

    pub fn bluetooth_playback(&self, action: &str) -> Result<ActionResult, String> {
        let action = match action {
            "play" | "pause" | "next" | "previous" => action,
            _ => return Err("Неизвестная команда воспроизведения Bluetooth".into()),
        };
        let output = Command::new("/usr/bin/bluetoothctl")
            .args(["--timeout", "3", "player", action])
            .output()
            .map_err(|error| format!("Не удалось запустить bluetoothctl: {error}"))?;
        let text = format!(
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if !output.status.success()
            || text.to_ascii_lowercase().contains("not available")
            || text.to_ascii_lowercase().contains("not connected")
            || text.to_ascii_lowercase().contains("no player")
            || text.to_ascii_lowercase().contains("failed")
        {
            return Err(if text.trim().is_empty() {
                "Управление недоступно: подключи телефон как Bluetooth-источник".into()
            } else {
                text.trim().to_owned()
            });
        }
        let message = match action {
            "play" => "Воспроизведение продолжено",
            "pause" => "Воспроизведение приостановлено",
            "next" => "Переключил на следующий трек",
            _ => "Переключил на предыдущий трек",
        };
        Ok(ActionResult {
            ok: true,
            message: message.into(),
        })
    }

    pub fn set_agent_speaking(&self, speaking: bool) {
        if let Ok(mut state) = self.state.lock() {
            state.bluetooth_ducked = speaking;
        }
    }

    pub fn clear_agent_audio(&self) {
        self.audio_mixer.clear_agent_voice();
    }

    fn bluetooth_duck_loop(state: Arc<Mutex<State>>) {
        thread::Builder::new()
            .name("bluetooth-duck".into())
            .spawn(move || {
                let mut gain = 1.0_f32;
                loop {
                    let target = {
                        let Ok(state) = state.lock() else { break };
                        if state.bluetooth_ducked { 0.2 } else { 1.0 }
                    };
                    let step = 0.04_f32;
                    gain = if gain < target {
                        (gain + step).min(target)
                    } else {
                        (gain - step).max(target)
                    };
                    if let Ok(mut state) = state.lock() {
                        state.bluetooth_duck_gain = gain;
                    }
                    thread::sleep(Duration::from_millis(40));
                }
            })
            .ok();
    }

    fn bluetooth_audio_bridge_loop(
        state: Arc<Mutex<State>>,
        mixer: Arc<AudioMixer>,
        indicators: Arc<crate::indicators::IndicatorController>,
    ) {
        thread::Builder::new()
            .name("bluetooth-audio-bridge".into())
            .spawn(move || {
                // Bluetooth is captured from BlueALSA and handed to the shared mixer.
                let _ = Command::new("/etc/init.d/S53bluealsa-aplay")
                    .arg("stop")
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();

                let mut bridge: Option<BluetoothAudioBridge> = None;
                loop {
                    let enabled = state
                        .lock()
                        .map(|state| state.bluetooth_enabled == Some(true))
                        .unwrap_or(false);
                    let source = enabled.then(read_bluetooth_capture_source).flatten();

                    if bridge
                        .as_ref()
                        .is_some_and(|current| source.as_deref() != Some(&current.address))
                    {
                        if let Some(mut current) = bridge.take() {
                            current.stop();
                        }
                    }

                    if let Some(current) = bridge.as_mut() {
                        let capture_exited = current.capture.try_wait().ok().flatten().is_some();
                        if capture_exited {
                            eprintln!(
                                "Bluetooth audio bridge stopped (capture_exited={capture_exited})"
                            );
                            if let Some(mut current) = bridge.take() {
                                current.stop();
                            }
                        }
                    }

                    if bridge.is_none()
                        && let Some(address) = source
                    {
                        match BluetoothAudioBridge::start(
                            address,
                            Arc::clone(&state),
                            Arc::clone(&mixer),
                            Arc::clone(&indicators),
                        ) {
                            Ok(started) => {
                                eprintln!("Bluetooth A2DP audio bridge started");
                                bridge = Some(started);
                            }
                            Err(error) => {
                                eprintln!("Bluetooth A2DP audio bridge failed to start: {error}");
                            }
                        }
                    }

                    thread::sleep(Duration::from_secs(2));
                }
            })
            .ok();
    }
}

struct BluetoothAudioBridge {
    address: String,
    capture: Child,
    worker: Option<thread::JoinHandle<()>>,
}

impl BluetoothAudioBridge {
    fn start(
        address: String,
        state: Arc<Mutex<State>>,
        mixer: Arc<AudioMixer>,
        indicators: Arc<crate::indicators::IndicatorController>,
    ) -> Result<Self, String> {
        let mut capture_command = Command::new("/usr/bin/arecord");
        capture_command
            .args([
                "-q",
                "-D",
                &format!("bluealsa:DEV={address},PROFILE=a2dp"),
                "-t",
                "raw",
                "-f",
                "S16_LE",
                "-r",
                "44100",
                "-c",
                "2",
                "-",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        set_parent_death_signal(&mut capture_command);
        let mut capture = capture_command
            .spawn()
            .map_err(|error| format!("не удалось запустить arecord: {error}"))?;
        let mut capture_output = capture
            .stdout
            .take()
            .ok_or_else(|| "arecord не открыл аудиопоток".to_owned())?;
        let worker = thread::Builder::new()
            .name("bluetooth-audio-mixer".into())
            .spawn(move || {
                use std::io::Read;
                let mut samples = [0_u8; 4096];
                loop {
                    if capture_output.read_exact(&mut samples).is_err() {
                        break;
                    }
                    let (volume, duck_gain) = match state.lock() {
                        Ok(state) => (state.bluetooth_volume, state.bluetooth_duck_gain),
                        Err(_) => break,
                    };
                    let gain = f32::from(volume) * duck_gain / 100.0;
                    mixer.push_bluetooth(&samples, gain);
                    indicators.mark_bluetooth_audio();
                }
            })
            .map_err(|error| format!("не удалось запустить аудиомикшер Bluetooth: {error}"))?;
        Ok(Self {
            address,
            capture,
            worker: Some(worker),
        })
    }

    fn stop(&mut self) {
        let _ = self.capture.kill();
        let _ = self.capture.wait();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for BluetoothAudioBridge {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(target_os = "linux")]
fn set_parent_death_signal(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // The bridge children must not outlive gpt-alice on a service restart.
    let parent_pid = unsafe { libc::getpid() };
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == 0 {
                if libc::getppid() == parent_pid {
                    Ok(())
                } else {
                    Err(std::io::Error::other(
                        "gpt-alice exited during bridge startup",
                    ))
                }
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn set_parent_death_signal(_command: &mut Command) {}

fn set_system_volume(volume: u8) -> Result<(), String> {
    let output = Command::new("/usr/bin/amixer")
        .args(["-c", "0", "set", "Master", &format!("{volume}%")])
        .output()
        .map_err(|error| format!("не удалось запустить amixer: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    Ok(())
}

fn read_bluetooth_power() -> Option<bool> {
    let output = Command::new("/usr/bin/bluetoothctl")
        .arg("show")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.trim().strip_prefix("Powered:"))
        .map(|value| value.trim() == "yes")
}

fn run_bluetoothctl(args: &[&str]) -> Result<(), String> {
    let output = Command::new("/usr/bin/bluetoothctl")
        .args(args)
        .output()
        .map_err(|error| format!("Не удалось запустить bluetoothctl: {error}"))?;
    let detail = format!(
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let lower = detail.to_ascii_lowercase();
    if output.status.success()
        && ![
            "failed",
            "not available",
            "not supported",
            "not connected",
            "invalid command",
            "unknown command",
        ]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        return Ok(());
    }
    let detail = detail.trim().to_owned();
    Err(if detail.is_empty() {
        format!("bluetoothctl завершился с кодом {}", output.status)
    } else {
        detail
    })
}

fn read_bluetooth_connected() -> Option<String> {
    let output = Command::new("/usr/bin/bluealsa-aplay")
        .arg("--list-devices")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut device_list = false;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let line = line.trim();
        if line.contains("List of PLAYBACK Bluetooth Devices")
            || line.contains("List of CAPTURE Bluetooth Devices")
        {
            device_list = true;
            continue;
        }
        if device_list && !line.is_empty() && !line.starts_with('*') {
            return Some("Bluetooth-устройство".into());
        }
    }
    None
}

fn read_bluetooth_capture_source() -> Option<String> {
    let output = Command::new("/usr/bin/bluealsa-aplay")
        .arg("--list-devices")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut capture_devices = false;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let line = line.trim();
        if line.contains("List of PLAYBACK Bluetooth Devices") {
            capture_devices = false;
            continue;
        }
        if line.contains("List of CAPTURE Bluetooth Devices") {
            capture_devices = true;
            continue;
        }
        if capture_devices && line.starts_with("hci") {
            return line
                .split_once(':')
                .map(|(_, remainder)| remainder.trim())
                .and_then(|remainder| remainder.split_whitespace().next())
                .filter(|address| address.matches(':').count() == 5)
                .map(str::to_owned);
        }
    }
    None
}

fn read_bluetooth_volume() -> Option<u8> {
    let output = Command::new("/usr/bin/amixer")
        .args(["-c", "0", "get", "Bluetooth Master"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| {
            let start = line.find('[')? + 1;
            let end = line[start..].find("%]")? + start;
            line[start..end].parse::<u8>().ok()
        })
}

fn face_brightness(face: &str) -> Result<Vec<(&'static str, u8)>, String> {
    let segments: Vec<(&'static str, u8)> = match face {
        "smile" => vec![
            ("A1", 220),
            ("A2", 220),
            ("B0", 180),
            ("B3", 180),
            ("C0", 140),
            ("C3", 140),
            ("D1", 220),
            ("D2", 220),
        ],
        "neutral" => vec![
            ("A1", 220),
            ("A2", 220),
            ("C0", 180),
            ("C1", 180),
            ("C2", 180),
            ("C3", 180),
        ],
        "sleep" => vec![("A0", 160), ("A1", 160), ("A2", 160), ("A3", 160)],
        _ => return Err("Неизвестная сцена: используйте smile, neutral или sleep".into()),
    };
    Ok(segments)
}

fn load_time_settings(path: &PathBuf) -> TimeSettings {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<TimeSettings>(&bytes).ok())
        .filter(|settings| validate_time_settings(settings).is_ok())
        .unwrap_or_default()
}

fn save_time_settings(path: &PathBuf, settings: &TimeSettings) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "У файла настроек времени нет родительского каталога".to_owned())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Не удалось создать каталог настроек времени: {error}"))?;
    let temporary = path.with_extension("json.tmp");
    let contents = serde_json::to_vec_pretty(settings)
        .map_err(|error| format!("Не удалось сериализовать настройки времени: {error}"))?;
    fs::write(&temporary, contents)
        .map_err(|error| format!("Не удалось записать настройки времени: {error}"))?;
    fs::rename(&temporary, path)
        .map_err(|error| format!("Не удалось заменить файл настроек времени: {error}"))
}

fn validate_time_settings(settings: &TimeSettings) -> Result<(), String> {
    if settings.clock_brightness_percent > 100 {
        return Err("Яркость экрана часов должна быть от 0 до 100 процентов".into());
    }
    if settings.output_volume_percent > 100 {
        return Err("Громкость должна быть от 0 до 100 процентов".into());
    }
    if !(-720..=840).contains(&settings.utc_offset_minutes) || settings.utc_offset_minutes % 15 != 0
    {
        return Err(
            "Смещение часового пояса должно быть от UTC−12:00 до UTC+14:00 с шагом 15 минут".into(),
        );
    }
    if settings.ntp_servers.is_empty() || settings.ntp_servers.len() > 8 {
        return Err("Укажите от 1 до 8 NTP-серверов".into());
    }
    for server in &settings.ntp_servers {
        if server.len() > 253
            || server.is_empty()
            || !server.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && label
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                    && label.as_bytes()[0] != b'-'
                    && label.as_bytes()[label.len() - 1] != b'-'
            })
        {
            return Err(format!("Некорректное имя NTP-сервера: {server}"));
        }
    }
    Ok(())
}

fn local_clock(utc_seconds: u64, offset_minutes: i16) -> (u8, u8) {
    let local_seconds = i128::from(utc_seconds) + i128::from(offset_minutes) * 60;
    let seconds_in_day = local_seconds.rem_euclid(86_400);
    (
        (seconds_in_day / 3_600) as u8,
        ((seconds_in_day % 3_600) / 60) as u8,
    )
}

fn format_utc_offset(offset_minutes: i16) -> String {
    let sign = if offset_minutes < 0 { '−' } else { '+' };
    let absolute = offset_minutes.unsigned_abs();
    format!("UTC{sign}{:02}:{:02}", absolute / 60, absolute % 60)
}

fn chronyc_output(arguments: &[&str]) -> Result<String, String> {
    let output = Command::new("/usr/bin/chronyc")
        .args(arguments)
        .output()
        .map_err(|error| format!("Не удалось запустить chronyc: {error}"))?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if output.status.success() {
        Ok(text)
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

fn apply_ntp_sources(new_servers: &[String], old_servers: &[String]) -> Result<(), String> {
    let mut new_addresses = Vec::new();
    for server in new_servers {
        let addresses = (server.as_str(), 123)
            .to_socket_addrs()
            .map_err(|error| format!("Не удалось разрешить NTP-сервер {server}: {error}"))?;
        for address in addresses {
            let address = address.ip().to_string();
            if !new_addresses.contains(&address) {
                new_addresses.push(address);
            }
        }
    }
    if new_addresses.is_empty() {
        return Err("NTP-серверы не разрешились ни в один IP-адрес".into());
    }

    let mut added_any = false;
    let mut add_errors = Vec::new();
    for address in &new_addresses {
        let arguments = ["add", "server", address.as_str(), "iburst"];
        match chronyc_output(&arguments) {
            Ok(_) => added_any = true,
            Err(error) if error.to_ascii_lowercase().contains("exist") => added_any = true,
            Err(error) => add_errors.push(format!("{address}: {error}")),
        }
    }
    if !added_any {
        return Err(format!(
            "chronyd не принял NTP-источники: {}",
            add_errors.join("; ")
        ));
    }

    if let Ok(current_sources) = chronyc_output(&["-n", "sources"]) {
        for line in current_sources.lines() {
            let mut fields = line.split_whitespace();
            let Some(marker) = fields.next() else {
                continue;
            };
            let Some(address) = fields.next() else {
                continue;
            };
            if !(marker.starts_with('^') || marker.starts_with('='))
                || new_addresses.contains(&address.to_owned())
                || address.parse::<std::net::IpAddr>().is_err()
            {
                continue;
            }
            let _ = chronyc_output(&["delete", address]);
        }
    }

    let desired: Vec<String> = new_servers
        .iter()
        .map(|server| server.to_ascii_lowercase())
        .collect();
    let mut obsolete = old_servers
        .iter()
        .filter(|server| !desired.contains(&server.to_ascii_lowercase()))
        .cloned()
        .collect::<Vec<_>>();
    if let Ok(contents) = fs::read_to_string("/etc/chrony.conf") {
        for line in contents.lines() {
            let mut fields = line.split_whitespace();
            let Some(kind) = fields.next() else { continue };
            if !matches!(kind, "pool" | "server" | "peer") {
                continue;
            }
            if let Some(server) = fields.next()
                && !desired.contains(&server.to_ascii_lowercase())
                && !obsolete.contains(&server.to_owned())
            {
                obsolete.push(server.to_owned());
            }
        }
    }
    for server in obsolete {
        if let Ok(addresses) = (server.as_str(), 123).to_socket_addrs() {
            for address in addresses {
                let address = address.ip().to_string();
                if !new_addresses.contains(&address) {
                    let _ = chronyc_output(&["delete", address.as_str()]);
                }
            }
        }
    }
    let _ = chronyc_output(&["burst", "4/4"]);
    Ok(())
}

fn input_loop(
    path: &'static str,
    touch: bool,
    state: Arc<Mutex<State>>,
    events: broadcast::Sender<DeviceEvent>,
) {
    loop {
        match OpenOptions::new().read(true).open(path) {
            Ok(mut file) => read_input(&mut file, touch, &state, &events),
            Err(error) => record_event(
                &state,
                &events,
                DeviceEvent::InputUnavailable {
                    device: path.into(),
                    error: error.to_string(),
                },
            ),
        }
        thread::sleep(Duration::from_secs(2));
    }
}

fn read_input(
    file: &mut File,
    touch: bool,
    state: &Arc<Mutex<State>>,
    events: &broadcast::Sender<DeviceEvent>,
) {
    if !touch {
        if let Some(muted) = read_mute_switch_state(file) {
            set_microphone_mute(state, events, muted, "startup_switch");
        }
    }
    let mut bytes = [0_u8; 24]; // Native 64-bit Linux input_event: timeval + type/code/value.
    loop {
        if file.read_exact(&mut bytes).is_err() {
            break;
        }
        let event_type = u16::from_ne_bytes([bytes[16], bytes[17]]);
        let code = u16::from_ne_bytes([bytes[18], bytes[19]]);
        let value = i32::from_ne_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
        if event_type == EV_SW && code == SW_MUTE_DEVICE {
            set_microphone_mute(state, events, value != 0, "hardware_switch");
            continue;
        }
        if event_type != 1 || value == 2 {
            continue;
        } // EV_KEY in Linux input-event protocol.
        let action = if value == 0 {
            "released"
        } else if value == 1 {
            "pressed"
        } else {
            "unknown"
        };
        let at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let event = if touch {
            DeviceEvent::Touch {
                button: button_name(code).into(),
                action: action.into(),
                key_code: code,
                key_name: key_name(code).into(),
                at_ms,
            }
        } else {
            DeviceEvent::Button {
                button: button_name(code).into(),
                action: action.into(),
                key_code: code,
                key_name: key_name(code).into(),
                at_ms,
            }
        };
        record_event(state, events, event.clone());
        if value == 1 {
            let callbacks = state
                .lock()
                .map(|state| state.button_callbacks.clone())
                .unwrap_or_default();
            for callback in callbacks {
                callback(event.clone());
            }
        }
    }
}

fn read_mute_switch_state(file: &File) -> Option<bool> {
    let mut switches = [0_u8; 2];
    // EVIOCGSW(2) reads switch codes 0..15, including SW_MUTE_DEVICE (14).
    let result = unsafe {
        libc::ioctl(
            file.as_raw_fd(),
            EVIOCGSW_16,
            switches.as_mut_ptr().cast::<libc::c_void>(),
        )
    };
    if result < 0 {
        return None;
    }
    Some(switches[SW_MUTE_DEVICE as usize / 8] & (1 << (SW_MUTE_DEVICE % 8)) != 0)
}

fn set_microphone_mute(
    state: &Arc<Mutex<State>>,
    events: &broadcast::Sender<DeviceEvent>,
    muted: bool,
    source: &str,
) {
    let changed = state
        .lock()
        .map(|mut state| {
            if state.microphone_muted == Some(muted) {
                false
            } else {
                state.microphone_muted = Some(muted);
                true
            }
        })
        .unwrap_or(false);
    if changed {
        record_event(
            state,
            events,
            DeviceEvent::MicrophoneMuteChanged {
                muted,
                source: source.to_owned(),
                at_ms: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis(),
            },
        );
    }
}

fn button_name(code: u16) -> &'static str {
    match code {
        115 => "volume_up",
        114 => "volume_down",
        113 => "mute",
        582 => "center",
        116 => "power",
        28 => "center",
        352 => "center",
        _ => "unknown",
    }
}

fn key_name(code: u16) -> &'static str {
    match code {
        113 => "KEY_MUTE",
        114 => "KEY_VOLUMEDOWN",
        115 => "KEY_VOLUMEUP",
        116 => "KEY_POWER",
        164 => "KEY_PLAYPAUSE",
        166 => "KEY_CENTER",
        28 => "KEY_ENTER",
        158 => "KEY_BACK",
        352 => "KEY_OK",
        582 => "KEY_VOICECOMMAND",
        _ => "KEY_UNKNOWN",
    }
}

fn record_event(
    state: &Arc<Mutex<State>>,
    events: &broadcast::Sender<DeviceEvent>,
    event: DeviceEvent,
) {
    if let Ok(mut state) = state.lock() {
        state.event_count += 1;
        state.last_event = Some(event.clone());
        if state.recent_events.len() == 16 {
            state.recent_events.pop_front();
        }
        state.recent_events.push_back(event.clone());
    }
    let _ = events.send(event);
}
