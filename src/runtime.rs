use std::{
    collections::VecDeque,
    env,
    fs::{self, File, OpenOptions},
    io::Read,
    net::ToSocketAddrs,
    os::fd::AsRawFd,
    path::PathBuf,
    process::Command,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::indicators::{ActionResult, IndicatorController};

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
    button_callbacks: Vec<Arc<dyn Fn(DeviceEvent) + Send + Sync>>,
}

pub struct RuntimeController {
    indicators: Arc<IndicatorController>,
    state: Arc<Mutex<State>>,
    events: broadcast::Sender<DeviceEvent>,
    config_path: PathBuf,
}

impl RuntimeController {
    pub fn new(indicators: Arc<IndicatorController>) -> Self {
        let (events, _) = broadcast::channel(64);
        let config_path = env::var_os("GPT_ALICE_CONFIG_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG));
        let time_settings = load_time_settings(&config_path);
        let initial_brightness =
            ((u16::from(time_settings.clock_brightness_percent) * 255 + 50) / 100) as u8;
        let _ = indicators
            .set_clock_display_brightness(initial_brightness, time_settings.utc_offset_minutes);
        Self {
            indicators,
            state: Arc::new(Mutex::new(State {
                last_event: None,
                recent_events: VecDeque::with_capacity(16),
                event_count: 0,
                display: DisplayScene {
                    name: "clock".into(),
                    until: None,
                },
                time_settings,
                microphone_muted: None,
                volume: read_system_volume().unwrap_or(5),
                output_muted: false,
                bluetooth_enabled: read_bluetooth_power(),
                button_callbacks: Vec::new(),
            })),
            events,
            config_path,
        }
    }

    pub fn indicators(&self) -> &IndicatorController {
        &self.indicators
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
        let startup_servers = settings.ntp_servers.clone();
        thread::Builder::new()
            .name("ntp-sources".into())
            .spawn(move || {
                let _ = apply_ntp_sources(&startup_servers, &[]);
            })
            .expect("failed to start NTP source configuration");
        let _ = indicators.render_idle_clock(settings.utc_offset_minutes);
        thread::Builder::new()
            .name("display-idle".into())
            .spawn(move || {
                loop {
                    thread::sleep(Duration::from_secs(1));
                    let offset = state
                        .lock()
                        .map(|state| state.time_settings.utc_offset_minutes)
                        .unwrap_or(0);
                    let expired = state
                        .lock()
                        .ok()
                        .and_then(|mut state| {
                            state
                                .display
                                .until
                                .filter(|deadline| Instant::now() >= *deadline)
                                .map(|_| {
                                    state.display = DisplayScene {
                                        name: "clock".into(),
                                        until: None,
                                    };
                                })
                        })
                        .is_some();
                    if expired {
                        let event = DeviceEvent::SceneChanged {
                            scene: "clock".into(),
                        };
                        record_event(&state, &events, event);
                        let _ = indicators.render_idle_clock(offset);
                    } else if state
                        .lock()
                        .map(|state| state.display.until.is_none())
                        .unwrap_or(false)
                        && SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs()
                            % 60
                            == 0
                    {
                        let _ = indicators.refresh_display(offset);
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

        let (volume, output_muted, changed) = {
            let Ok(mut state) = self.state.lock() else {
                return false;
            };
            let next_volume = (i16::from(state.volume) + direction * 5).clamp(0, 100) as u8;
            let changed = next_volume != state.volume;
            if changed {
                if !state.output_muted {
                    if let Err(error) = set_system_volume(next_volume) {
                        eprintln!("system volume update failed: {error}");
                        return false;
                    }
                }
                state.volume = next_volume;
            }
            let output_muted = state.output_muted;
            state.display = DisplayScene {
                name: "volume".into(),
                until: Some(Instant::now() + Duration::from_secs(2)),
            };
            (state.volume, output_muted, changed)
        };
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
        let muted = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| "Не удалось получить состояние громкости".to_owned())?;
            if !state.output_muted {
                set_system_volume(volume)?;
            }
            state.volume = volume;
            state.display = DisplayScene {
                name: "volume".into(),
                until: Some(Instant::now() + Duration::from_secs(2)),
            };
            state.output_muted
        };
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
                let hardware_volume = if muted { 0 } else { state.volume };
                set_system_volume(hardware_volume)?;
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
        let output = Command::new("/usr/bin/bluetoothctl")
            .args(["power", if enabled { "on" } else { "off" }])
            .output()
            .map_err(|error| format!("Не удалось запустить bluetoothctl: {error}"))?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return Err(if detail.is_empty() {
                format!("bluetoothctl завершился с кодом {}", output.status)
            } else {
                detail
            });
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
                "Bluetooth включён".into()
            } else {
                "Bluetooth выключен".into()
            },
        })
    }
}

fn read_system_volume() -> Option<u8> {
    let output = Command::new("/usr/bin/amixer")
        .args(["-c", "0", "get", "Master"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines().find_map(|line| {
        let percent_start = line.find('[')? + 1;
        let percent_end = line[percent_start..].find("%]")? + percent_start;
        line[percent_start..percent_end].parse::<u8>().ok()
    })
}

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
