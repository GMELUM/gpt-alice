use std::{
    fs::{self, OpenOptions},
    io::Write,
    net::{Ipv4Addr, SocketAddrV4},
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    Json,
    extract::{Extension, rejection::JsonRejection},
    http::StatusCode,
    response::IntoResponse,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};

const SERVICE: &str = "/etc/init.d/S50awg0";
const CONFIG: &str = "/data/opt/gpt-alice/tools/awg0.conf";
const MAX_CONFIG_BYTES: usize = 16 * 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct VpnController {
    operation_lock: Arc<Mutex<()>>,
}

#[derive(Serialize)]
pub struct VpnStatus {
    pub available: bool,
    pub configured: bool,
    pub enabled: bool,
    pub connected: bool,
    pub last_handshake_seconds_ago: Option<u64>,
    pub message: String,
}

#[derive(Deserialize)]
pub struct VpnPowerRequest {
    pub enabled: bool,
}

#[derive(Deserialize)]
pub struct VpnConfigRequest {
    pub config: String,
}

#[derive(Serialize)]
pub struct VpnActionResult {
    pub ok: bool,
    pub message: String,
}

impl VpnController {
    pub fn new() -> Self {
        Self {
            operation_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn status(&self) -> VpnStatus {
        let service_exists = Path::new(SERVICE).is_file();
        let configured = Path::new(CONFIG).is_file();
        if !service_exists {
            return VpnStatus {
                available: false,
                configured,
                enabled: false,
                connected: false,
                last_handshake_seconds_ago: None,
                message: "Служба VPN не найдена на этом устройстве".to_owned(),
            };
        }

        let output = match Command::new(SERVICE).arg("status").output() {
            Ok(output) => output,
            Err(error) => {
                return VpnStatus {
                    available: false,
                    configured,
                    enabled: false,
                    connected: false,
                    last_handshake_seconds_ago: None,
                    message: format!("Не удалось проверить VPN: {error}"),
                };
            }
        };

        let text = String::from_utf8_lossy(&output.stdout);
        let enabled = text.contains("AmneziaWG awg0 is up");
        let handshake = text
            .lines()
            .filter_map(|line| line.split_whitespace().last())
            .find_map(|value| value.parse::<u64>().ok())
            .filter(|timestamp| *timestamp > 0);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let handshake_age = handshake.map(|timestamp| now.saturating_sub(timestamp));
        let connected = enabled && handshake_age.is_some_and(|age| age <= 300);

        let message = if !configured {
            "Добавьте конфигурацию AmneziaWG".to_owned()
        } else if !enabled {
            "VPN выключен".to_owned()
        } else if connected {
            "VPN подключён".to_owned()
        } else {
            "Интерфейс включён, ожидается рукопожатие с сервером".to_owned()
        };

        VpnStatus {
            available: true,
            configured,
            enabled,
            connected,
            last_handshake_seconds_ago: handshake_age,
            message,
        }
    }

    pub fn set_enabled(&self, enabled: bool) -> Result<VpnActionResult, String> {
        let _guard = self
            .operation_lock
            .lock()
            .map_err(|error| error.to_string())?;
        self.ensure_service()?;
        if enabled && !Path::new(CONFIG).is_file() {
            return Err("Конфигурация VPN ещё не установлена".to_owned());
        }
        run_service(if enabled { "start" } else { "stop" })?;

        Ok(VpnActionResult {
            ok: true,
            message: if enabled {
                "Команда включения VPN выполнена".to_owned()
            } else {
                "VPN выключен".to_owned()
            },
        })
    }

    pub fn replace_config(&self, config: String) -> Result<VpnActionResult, String> {
        let _guard = self
            .operation_lock
            .lock()
            .map_err(|error| error.to_string())?;
        self.ensure_service()?;
        validate_config(&config)?;

        let config_path = PathBuf::from(CONFIG);
        let previous = fs::read(&config_path).ok();
        let was_enabled = self.status().enabled;

        if let Some(previous_config) = &previous {
            atomic_write(&backup_path(&config_path), previous_config)?;
        }
        atomic_write(&config_path, config.as_bytes())?;

        if !was_enabled {
            return Ok(VpnActionResult {
                ok: true,
                message: "Конфигурация сохранена; VPN оставлен выключенным".to_owned(),
            });
        }

        if run_service("restart").is_ok() && self.wait_for_connection(HANDSHAKE_TIMEOUT) {
            return Ok(VpnActionResult {
                ok: true,
                message: "Конфигурация заменена, VPN переподключён".to_owned(),
            });
        }

        match previous {
            Some(previous_config) => {
                let restore_result = atomic_write(&config_path, &previous_config);
                let restart_result = run_service("restart");
                if restore_result.is_ok() && restart_result.is_ok() {
                    Err(
                        "Новая конфигурация не подключилась; предыдущая конфигурация восстановлена"
                            .to_owned(),
                    )
                } else {
                    Err("Не удалось подключить новую конфигурацию и восстановить прежнее соединение; проверьте состояние VPN".to_owned())
                }
            }
            None => {
                let _ = fs::remove_file(&config_path);
                let _ = run_service("stop");
                Err("Новая конфигурация не подключилась и была удалена".to_owned())
            }
        }
    }

    fn wait_for_connection(&self, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            let status = self.status();
            if status.connected {
                return true;
            }
            thread::sleep(Duration::from_secs(1));
        }
        false
    }

    fn ensure_service(&self) -> Result<(), String> {
        if !Path::new(SERVICE).is_file() {
            return Err("Управление VPN доступно только на колонке".to_owned());
        }
        Ok(())
    }
}

pub async fn get_status(Extension(controller): Extension<VpnController>) -> impl IntoResponse {
    match tokio::task::spawn_blocking(move || controller.status()).await {
        Ok(status) => (StatusCode::OK, Json(status)).into_response(),
        Err(error) => vpn_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

pub async fn set_power(
    Extension(controller): Extension<VpnController>,
    payload: Result<Json<VpnPowerRequest>, JsonRejection>,
) -> impl IntoResponse {
    let payload = match payload {
        Ok(Json(payload)) => payload,
        Err(error) => return vpn_error(StatusCode::BAD_REQUEST, error.body_text()),
    };
    match tokio::task::spawn_blocking(move || controller.set_enabled(payload.enabled)).await {
        Ok(Ok(result)) => (StatusCode::OK, Json(result)).into_response(),
        Ok(Err(error)) => vpn_error(StatusCode::BAD_GATEWAY, error),
        Err(error) => vpn_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

pub async fn replace_config(
    Extension(controller): Extension<VpnController>,
    payload: Result<Json<VpnConfigRequest>, JsonRejection>,
) -> impl IntoResponse {
    let payload = match payload {
        Ok(Json(payload)) => payload,
        Err(error) => return vpn_error(StatusCode::BAD_REQUEST, error.body_text()),
    };
    if let Err(error) = validate_config(&payload.config) {
        return vpn_error(StatusCode::BAD_REQUEST, error);
    }
    match tokio::task::spawn_blocking(move || controller.replace_config(payload.config)).await {
        Ok(Ok(result)) => (StatusCode::OK, Json(result)).into_response(),
        Ok(Err(error)) => vpn_error(StatusCode::BAD_GATEWAY, error),
        Err(error) => vpn_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

fn vpn_error(status: StatusCode, message: String) -> axum::response::Response {
    (status, Json(VpnActionResult { ok: false, message })).into_response()
}

fn run_service(action: &str) -> Result<(), String> {
    let output = Command::new(SERVICE)
        .arg(action)
        .output()
        .map_err(|_| "Не удалось запустить службу VPN".to_owned())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "Служба VPN завершилась с кодом {:?}",
            output.status.code()
        ))
    }
}

fn backup_path(config_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.previous", config_path.display()))
}

fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), String> {
    let temporary_path = PathBuf::from(format!("{}.{}.tmp", path.display(), std::process::id()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary_path)
            .map_err(|_| "Не удалось создать защищённый файл конфигурации".to_owned())?;
        file.write_all(contents)
            .and_then(|()| file.sync_all())
            .map_err(|_| "Не удалось записать конфигурацию VPN".to_owned())?;
        fs::rename(&temporary_path, path)
            .map_err(|_| "Не удалось заменить конфигурацию VPN".to_owned())?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}

fn validate_config(config: &str) -> Result<(), String> {
    if config.len() > MAX_CONFIG_BYTES {
        return Err("Конфигурация слишком большая (максимум 16 КБ)".to_owned());
    }
    if config.trim().is_empty() || config.as_bytes().contains(&0) {
        return Err("Вставьте полную конфигурацию AmneziaWG".to_owned());
    }

    let mut section = "";
    let mut interface = std::collections::HashMap::<String, String>::new();
    let mut peer = std::collections::HashMap::<String, String>::new();
    let mut interface_count = 0;
    let mut peer_count = 0;

    for line in config.lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        if line.is_empty() || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = &line[1..line.len() - 1];
            match section {
                "Interface" => interface_count += 1,
                "Peer" => peer_count += 1,
                _ => return Err("Поддерживаются только секции [Interface] и [Peer]".to_owned()),
            }
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err("В конфигурации есть строка без знака '='".to_owned());
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        if value.is_empty() {
            return Err("В конфигурации есть пустое значение".to_owned());
        }
        let fields = match section {
            "Interface" => &mut interface,
            "Peer" => &mut peer,
            _ => return Err("Параметр расположен вне секции".to_owned()),
        };
        if fields.insert(key, value.to_owned()).is_some() {
            return Err("В конфигурации повторяется параметр".to_owned());
        }
    }

    if interface_count != 1 || peer_count != 1 {
        return Err("Нужны ровно одна секция [Interface] и одна [Peer]".to_owned());
    }
    for key in ["privatekey", "address", "dns", "headerprotectionkey"] {
        if !interface.contains_key(key) {
            return Err(format!("В секции [Interface] отсутствует {key}"));
        }
    }
    for key in ["publickey", "endpoint", "allowedips"] {
        if !peer.contains_key(key) {
            return Err(format!("В секции [Peer] отсутствует {key}"));
        }
    }

    for (section_name, fields, key) in [
        ("[Interface]", &interface, "privatekey"),
        ("[Interface]", &interface, "headerprotectionkey"),
        ("[Peer]", &peer, "publickey"),
    ] {
        let decoded = STANDARD
            .decode(fields.get(key).expect("required key checked"))
            .map_err(|_| format!("Некорректный Base64 в {section_name} {key}"))?;
        if decoded.len() != 32 {
            return Err(format!("{section_name} {key} должен содержать 32 байта"));
        }
    }

    if let Some(value) = peer.get("presharedkey") {
        let decoded = STANDARD
            .decode(value)
            .map_err(|_| "Некорректный Base64 в [Peer] presharedkey".to_owned())?;
        if decoded.len() != 32 {
            return Err("[Peer] presharedkey должен содержать 32 байта".to_owned());
        }
    }

    let address = interface.get("address").expect("required key checked");
    let (address, prefix) = address
        .split_once('/')
        .ok_or_else(|| "Address должен быть указан как IPv4/префикс".to_owned())?;
    address
        .parse::<Ipv4Addr>()
        .map_err(|_| "Address должен содержать IPv4-адрес".to_owned())?;
    if prefix.parse::<u8>().ok().is_none_or(|prefix| prefix > 32) {
        return Err("Префикс Address должен быть от 0 до 32".to_owned());
    }

    let endpoint = peer.get("endpoint").expect("required key checked");
    endpoint
        .parse::<SocketAddrV4>()
        .map_err(|_| "Endpoint должен быть числовым IPv4-адресом с портом".to_owned())?;

    if interface
        .get("dns")
        .expect("required key checked")
        .split(',')
        .any(|server| server.trim().parse::<Ipv4Addr>().is_err())
    {
        return Err("DNS должен содержать IPv4-адреса через запятую".to_owned());
    }

    Ok(())
}
