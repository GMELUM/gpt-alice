use std::{
    collections::{HashSet, VecDeque},
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use axum::{Json, extract::Extension, http::StatusCode, response::IntoResponse};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::{Mutex, mpsc, watch},
    task::JoinHandle,
    time::timeout,
};

use crate::{
    indicators::{ActionResult, IndicatorController},
    oauth::OAuthController,
    runtime::RuntimeController,
};

const CALL_URL: &str =
    "https://chatgpt.com/backend-api/codex/realtime/calls?intent=quicksilver&architecture=avas";
const MODEL: &str = "gpt-live-1-codex";
const HELPER_NAME: &str = "gpt-live-host";
const LIVE_INSTRUCTIONS: &str = r#"You are GPT Alice, the voice assistant running on this physical Yandex Station Mini 3. The user is speaking to you through this speaker. GPT Alice is the device-control application installed on this same speaker; its local controller can actually change the speaker's volume, mute the speaker output, adjust the front clock display brightness, and enable or disable Bluetooth. It can read the current values of those settings and microphone mute state. You are connected to that controller through client delegations and application-provided speakable context.

When the user asks to control this speaker, do not say that you cannot control it. Do not give instructions for changing settings on an iPhone, Android phone, computer, or in another app. Instead, send the exact device command as a client delegation and wait for the controller's result. Only say that a change succeeded after the controller confirms it. When the application provides a verified command result in speakable context, tell the user that result briefly without asking them to repeat the request. If a requested function is not in the supported action list, say that this speaker application does not support it yet; do not invent another device or offer unrelated setup advice.

Respond naturally and concisely in the user's language. For ordinary conversation, answer directly. When the user asks to search the web, look up a website, or verify current information, create a client delegation for web search and wait for the application to return search findings. Do not claim to have searched until those findings arrive. If the user clearly says to finish or end this voice conversation (for example, “заверши диалог”, “заверши разговор”, or “давай закончим”), invoke the end_call function. Do not merely agree to end while leaving the call active.

For EVERY request to change or query a device setting, create a client delegation. The complete delegated input MUST be exactly one JSON object using one of these actions: {"action":"set_volume","value":0..100}, {"action":"adjust_volume","delta":signed integer}, {"action":"set_mute","muted":true|false}, {"action":"set_clock_brightness","percent":0..100}, {"action":"adjust_clock_brightness","delta":signed integer}, {"action":"set_bluetooth","enabled":true|false}, {"action":"get_volume"}, {"action":"get_mute"}, {"action":"get_clock_brightness"}, {"action":"get_bluetooth"}, {"action":"get_microphone_mute"}, {"action":"end_call"}. Never web-search a device command or state question. Never answer a state question from memory; call the matching get action. Examples: “поставь громкость на 30” -> {"action":"set_volume","value":30}; “сделай громче на 10” -> {"action":"adjust_volume","delta":10}; “уменьши яркость до 20” -> {"action":"set_clock_brightness","percent":20}; “сделай потемнее” -> {"action":"adjust_clock_brightness","delta":-10}; “ярче на 5” -> {"action":"adjust_clock_brightness","delta":5}; “сколько громкости” -> {"action":"get_volume"}; “какая яркость” -> {"action":"get_clock_brightness"}; “Bluetooth включён?” -> {"action":"get_bluetooth"}; “выключи Bluetooth” -> {"action":"set_bluetooth","enabled":false}. Use a signed delta for “на N” changes; use an absolute value for “до N” or “установи N”. Do not request or invent shell commands. Do not claim a setting changed until the app confirms it. If the user asks to end after a device action, include "end_call_after":true in that action. The app will say goodbye and close the call after the final speech finishes."#;

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum DeviceCommand {
    SetVolume { value: u8 },
    AdjustVolume { delta: i16 },
    SetMute { muted: bool },
    SetClockBrightness { percent: u8 },
    AdjustClockBrightness { delta: i16 },
    SetBluetooth { enabled: bool },
    GetVolume,
    GetMute,
    GetClockBrightness,
    GetBluetooth,
    GetMicrophoneMute,
    EndCall,
}

impl DeviceCommand {
    fn label(&self) -> &'static str {
        match self {
            Self::SetVolume { .. } => "громкость",
            Self::AdjustVolume { .. } => "изменение громкости",
            Self::SetMute { .. } => "mute звука",
            Self::SetClockBrightness { .. } => "яркость часов",
            Self::AdjustClockBrightness { .. } => "изменение яркости часов",
            Self::SetBluetooth { .. } => "Bluetooth",
            Self::GetVolume => "запрос громкости",
            Self::GetMute => "запрос состояния звука",
            Self::GetClockBrightness => "запрос яркости часов",
            Self::GetBluetooth => "запрос состояния Bluetooth",
            Self::GetMicrophoneMute => "запрос состояния микрофона",
            Self::EndCall => "завершение звонка",
        }
    }
}

async fn classify_device_command(
    client: &reqwest::Client,
    oauth: &OAuthController,
    history: &[(String, String)],
) -> Result<Option<(DeviceCommand, bool)>, String> {
    let (token, account_id) = oauth.live_credentials().await?;
    let Some((_, latest)) = history.iter().rev().find(|(role, text)| role == "user" && !text.trim().is_empty()) else {
        return Ok(None);
    };
    let context = history
        .iter()
        .rev()
        .take(8)
        .rev()
        .map(|(role, text)| format!("{}: {}", if role == "assistant" { "Ассистент" } else { "Пользователь" }, text.chars().take(600).collect::<String>()))
        .collect::<Vec<_>>()
        .join("\n");
    let tools = [
        device_function_tool("set_volume", "Установить точную громкость колонки", Some(("value", json!({"type":"integer","minimum":0,"maximum":100})))),
        device_function_tool("adjust_volume", "Изменить громкость на указанное число пунктов", Some(("delta", json!({"type":"integer","minimum":-100,"maximum":100})))),
        device_function_tool("set_mute", "Включить или выключить mute звука колонки", Some(("muted", json!({"type":"boolean"})))),
        device_function_tool("set_clock_brightness", "Установить яркость часов на точный процент", Some(("percent", json!({"type":"integer","minimum":0,"maximum":100})))),
        device_function_tool("adjust_clock_brightness", "Изменить яркость часов на указанное число пунктов", Some(("delta", json!({"type":"integer","minimum":-100,"maximum":100})))),
        device_function_tool("set_bluetooth", "Включить или выключить Bluetooth колонки", Some(("enabled", json!({"type":"boolean"})))),
        device_function_tool("get_volume", "Узнать текущую громкость", None),
        device_function_tool("get_mute", "Узнать, включён ли mute звука", None),
        device_function_tool("get_clock_brightness", "Узнать текущую яркость часов", None),
        device_function_tool("get_bluetooth", "Узнать, включён ли Bluetooth", None),
        device_function_tool("get_microphone_mute", "Узнать состояние mute микрофонов", None),
        device_function_tool("end_call", "Завершить текущий голосовой звонок", None),
    ];
    let response = client
        .post("https://chatgpt.com/backend-api/codex/responses")
        .timeout(Duration::from_secs(20))
        .bearer_auth(&token)
        .header("ChatGPT-Account-ID", &account_id)
        .header("originator", "codex_cli_rs")
        .header("User-Agent", concat!("gpt-alice/", env!("CARGO_PKG_VERSION")))
        .json(&json!({
            "model": "gpt-6-luna",
            "store": false,
            "stream": true,
            "instructions": "Ты — классификатор команд физической колонки GPT Alice (Yandex Station Mini 3). Определи только явную просьбу пользователя изменить или узнать состояние самой этой колонки. При обычном разговоре, вопросах о других устройствах или неоднозначности не вызывай инструмент. Для команды колонки вызови именно соответствующую функцию: каждая функция уже задаёт обязательный параметр, никогда не пропускай его и не подставляй null. Для точных значений используй set_*; для относительных изменений adjust_*; для вопросов о текущем состоянии get_*. Если пользователь просит завершить этот звонок или диалог, включая фразы «заверши диалог», «заверши разговор», «закончим» или «пока», обязательно вызови end_call. Не ограничивайся подтверждением словами. Не объясняй пользователю, как делать это в телефоне.",
            "tools": tools,
            "tool_choice": "auto",
            "parallel_tool_calls": false,
            "input": [{"role":"user","content":[{"type":"input_text","text":format!("Распознанная последняя реплика: {latest}\n\nКонтекст диалога:\n{context}")}]}]
        }))
        .send()
        .await
        .map_err(|error| format!("Не удалось распознать команду колонки через Codex: {error}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        let safe: String = body.replace(&token, "[скрыто]").replace(&account_id, "[скрыто]").chars().take(240).collect();
        return Err(format!("Codex command classification HTTP {status}: {safe}"));
    }
    let body = response
        .text()
        .await
        .map_err(|error| format!("Не удалось прочитать поток распознавания команды: {error}"))?;
    let normalized_body = body.replace("\r\n", "\n");
    for block in normalized_body.split("\n\n") {
        let data = block
            .lines()
            .filter_map(|line| line.trim_end_matches('\r').strip_prefix("data:"))
            .map(str::trim_start)
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(&data) else {
            continue;
        };
        let event_type = event.get("type").and_then(Value::as_str).unwrap_or_default();
        // `response.output_item.added` carries an initial function_call item whose
        // arguments are commonly empty. Only consume the completed item/output.
        if event_type != "response.output_item.done" && event_type != "response.completed" {
            continue;
        }
        let item = if event_type == "response.output_item.done" {
            event.get("item")
        } else {
            event.pointer("/response/output").and_then(Value::as_array).and_then(|output| output.iter().find(|item| item.get("type").and_then(Value::as_str) == Some("function_call")))
        };
        let Some(item) = item else { continue; };
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            continue;
        }
        let Some(action) = item.get("name").and_then(Value::as_str) else { continue; };
        if !matches!(action, "set_volume" | "adjust_volume" | "set_mute" | "set_clock_brightness" | "adjust_clock_brightness" | "set_bluetooth" | "get_volume" | "get_mute" | "get_clock_brightness" | "get_bluetooth" | "get_microphone_mute" | "end_call") {
            continue;
        }
        let args = item.get("arguments").and_then(Value::as_str).ok_or_else(|| "В вызове команды отсутствуют аргументы".to_owned())?;
        let mut arguments: Value = serde_json::from_str(args).map_err(|error| format!("Некорректные аргументы функции {action}: {error}"))?;
        if let Some(object) = arguments.as_object_mut() {
            object.insert("action".into(), Value::String(action.to_owned()));
        }
        let (command, end_after) = decode_device_command(&arguments.to_string())?.ok_or_else(|| {
            format!(
                "GPT вернул function call без action (arguments: {})",
                args.chars().take(160).collect::<String>()
            )
        })?;
        return Ok(Some((command, end_after)));
    }
    Ok(None)
}

fn device_function_tool(
    name: &str,
    description: &str,
    argument: Option<(&str, Value)>,
) -> Value {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    if let Some((key, schema)) = argument {
        properties.insert(key.to_owned(), schema);
        required.push(Value::String(key.to_owned()));
    }
    properties.insert("end_call_after".into(), json!({"type":"boolean"}));
    required.push(Value::String("end_call_after".into()));
    json!({
        "type":"function",
        "name":name,
        "description":description,
        "parameters":{
            "type":"object",
            "properties":properties,
            "required":required,
            "additionalProperties":false
        },
        "strict":true
    })
}

#[derive(Clone, Serialize)]
pub struct VoiceStatus {
    pub state: String,
    pub message: String,
    pub call_id: Option<String>,
}

struct VoiceInner {
    status: VoiceStatus,
    session: Option<LiveSession>,
    logs: VecDeque<VoiceLog>,
}

#[derive(Clone, Serialize)]
pub struct VoiceLog {
    at: String,
    message: String,
}

struct LiveSession {
    call_id: String,
    child: Child,
    stdin: ChildStdin,
    indicator_stop: Arc<AtomicBool>,
    indicator_task: JoinHandle<()>,
    indicators: Arc<IndicatorController>,
    session_closed: watch::Receiver<bool>,
}

#[derive(Clone)]
pub struct VoiceController {
    inner: Arc<Mutex<VoiceInner>>,
    busy: Arc<AtomicBool>,
    client: reqwest::Client,
    indicators: Arc<IndicatorController>,
    runtime: Arc<RuntimeController>,
}

struct BusyGuard(Arc<AtomicBool>);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl VoiceController {
    pub fn new(
        indicators: Arc<IndicatorController>,
        runtime: Arc<RuntimeController>,
    ) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(35))
            .build()
            .map_err(|error| format!("Не удалось создать HTTP-клиент звонка: {error}"))?;
        if let Err(error) = indicators.render_voice_idle() {
            eprintln!("voice idle indicator could not be drawn: {error}");
        }
        Ok(Self {
            inner: Arc::new(Mutex::new(VoiceInner {
                status: VoiceStatus {
                    state: "idle".into(),
                    message: "Звонок не запущен".into(),
                    call_id: None,
                },
                session: None,
                logs: VecDeque::with_capacity(80),
            })),
            busy: Arc::new(AtomicBool::new(false)),
            client,
            indicators,
            runtime,
        })
    }

    pub async fn status(&self) -> VoiceStatus {
        self.inner.lock().await.status.clone()
    }

    pub async fn audio_is_owned(&self) -> bool {
        let inner = self.inner.lock().await;
        inner.session.is_some() || matches!(inner.status.state.as_str(), "connecting" | "stopping")
    }

    pub async fn play_sfx(&self) -> Result<(), String> {
        let mut inner = self.inner.lock().await;
        let session = inner
            .session
            .as_mut()
            .ok_or_else(|| "Аудиосессия не активна".to_owned())?;
        send_command(&mut session.stdin, json!({"type":"playSfx"})).await?;
        drop(inner);
        self.log("SFX поставлен в общий аудиопоток звонка").await;
        Ok(())
    }

    async fn send_live_event(&self, event: Value) -> Result<(), String> {
        let mut inner = self.inner.lock().await;
        let session = inner
            .session
            .as_mut()
            .ok_or_else(|| "Аудиосессия уже завершена".to_owned())?;
        send_command(
            &mut session.stdin,
            json!({"type":"sendEvent", "event":event}),
        )
        .await
    }

    async fn send_delegation_text(
        &self,
        delegation_id: &str,
        content: &str,
    ) -> Result<usize, String> {
        let chunks = split_utf8_bytes(content, 500);
        for chunk in &chunks {
            self.send_live_event(json!({
                "type":"delegation.context.append",
                "delegation_item_id":delegation_id,
                "channel":"speakable",
                "content":[{"type":"input_text", "text":chunk}]
            }))
            .await?;
        }
        Ok(chunks.len())
    }

    async fn execute_device_command(&self, command: &DeviceCommand) -> Result<String, String> {
        let result = match command {
            DeviceCommand::SetVolume { value } => self.runtime.set_output_volume(*value),
            DeviceCommand::AdjustVolume { delta } => self.runtime.adjust_output_volume(*delta),
            DeviceCommand::SetMute { muted } => self.runtime.set_output_muted(*muted),
            DeviceCommand::SetClockBrightness { percent } => {
                self.runtime.set_clock_brightness_percent(*percent)
            }
            DeviceCommand::AdjustClockBrightness { delta } => {
                self.runtime.adjust_clock_brightness_percent(*delta)
            }
            DeviceCommand::SetBluetooth { enabled } => {
                let runtime = Arc::clone(&self.runtime);
                let enabled = *enabled;
                tokio::task::spawn_blocking(move || runtime.set_bluetooth_enabled(enabled))
                    .await
                    .map_err(|error| format!("Задача Bluetooth завершилась с ошибкой: {error}"))?
            }
            DeviceCommand::GetVolume => {
                let status = self.runtime.status();
                Ok(ActionResult {
                    ok: true,
                    message: format!("Сейчас громкость {}%", status.volume),
                })
            }
            DeviceCommand::GetMute => {
                let status = self.runtime.status();
                Ok(ActionResult {
                    ok: true,
                    message: if status.output_muted {
                        "Звук сейчас выключен".into()
                    } else {
                        format!("Звук сейчас включён, громкость {}%", status.volume)
                    },
                })
            }
            DeviceCommand::GetClockBrightness => {
                let raw = self.runtime.indicators().status().clock_display_brightness;
                Ok(ActionResult {
                    ok: true,
                    message: format!(
                        "Яркость экрана часов сейчас {}%",
                        u16::from(raw) * 100 / 255
                    ),
                })
            }
            DeviceCommand::GetBluetooth => {
                let enabled = self.runtime.status().bluetooth.enabled;
                Ok(ActionResult {
                    ok: true,
                    message: match enabled {
                        Some(true) => "Bluetooth сейчас включён".into(),
                        Some(false) => "Bluetooth сейчас выключен".into(),
                        None => "Состояние Bluetooth сейчас недоступно".into(),
                    },
                })
            }
            DeviceCommand::GetMicrophoneMute => {
                let muted = self.runtime.status().microphone.muted;
                Ok(ActionResult {
                    ok: true,
                    message: match muted {
                        Some(true) => "Микрофон сейчас выключен".into(),
                        Some(false) => "Микрофон сейчас включён".into(),
                        None => "Состояние микрофона сейчас недоступно".into(),
                    },
                })
            }
            DeviceCommand::EndCall => {
                return Ok("Завершаю звонок".into());
            }
        };
        result.map(|result| result.message)
    }

    async fn finish_from_agent(&self, expected_call_id: &str) {
        if self
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            self.log("автозавершение звонка пропущено: выполняется ручное переключение")
                .await;
            return;
        }
        let _busy = BusyGuard(Arc::clone(&self.busy));
        let session = {
            let mut inner = self.inner.lock().await;
            if inner.status.call_id.as_deref() != Some(expected_call_id) {
                return;
            }
            let session = inner.session.take();
            inner.status = VoiceStatus {
                state: "stopping".into(),
                message: "Завершаю звонок по запросу агента…".into(),
                call_id: Some(expected_call_id.to_owned()),
            };
            session
        };
        if let Some(session) = session {
            let mut session = session;
            self.log("отправляю GPT-Live session.close и жду session.closed")
                .await;
            let confirmed = request_live_close(&mut session, Duration::from_secs(3)).await;
            if confirmed {
                self.log("GPT-Live подтвердил закрытие сессии событием session.closed")
                    .await;
            } else {
                self.log("GPT-Live не подтвердил session.closed за 3 секунды; закрываю локальный транспорт")
                    .await;
            }
            stop_session(session).await;
        }
        let mut inner = self.inner.lock().await;
        if inner.status.call_id.as_deref() == Some(expected_call_id) {
            inner.status = VoiceStatus {
                state: "idle".into(),
                message: "Звонок завершён агентом".into(),
                call_id: None,
            };
        }
    }

    pub async fn logs(&self) -> Vec<VoiceLog> {
        self.inner.lock().await.logs.iter().cloned().collect()
    }

    pub async fn log(&self, message: impl Into<String>) {
        let message = message.into();
        eprintln!("voice: {message}");
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let day_seconds = (seconds + 3 * 60 * 60) % 86_400;
        let at = format!(
            "{:02}:{:02}:{:02}",
            day_seconds / 3600,
            day_seconds / 60 % 60,
            day_seconds % 60
        );
        let mut inner = self.inner.lock().await;
        if inner.logs.len() == 80 {
            inner.logs.pop_front();
        }
        inner.logs.push_back(VoiceLog { at, message });
    }

    pub async fn toggle(&self, oauth: &OAuthController) -> Result<ActionResult, String> {
        self.log("получен запрос на переключение звонка").await;
        if self
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            self.log("переключение отклонено: операция уже выполняется")
                .await;
            return Err("Управление звонком уже выполняется".into());
        }
        let _busy = BusyGuard(Arc::clone(&self.busy));

        let active = {
            let mut inner = self.inner.lock().await;
            if let Some(session) = inner.session.take() {
                inner.status = VoiceStatus {
                    state: "stopping".into(),
                    message: "Завершаю звонок…".into(),
                    call_id: Some(session.call_id.clone()),
                };
                Some(session)
            } else {
                inner.status = VoiceStatus {
                    state: "connecting".into(),
                    message: "Подключаюсь к GPT-Live…".into(),
                    call_id: None,
                };
                None
            }
        };

        if let Some(session) = active {
            let call_id = session.call_id.clone();
            self.log(format!("завершаю звонок {call_id}")).await;
            let mut session = session;
            if let Err(error) = send_command(&mut session.stdin, json!({"type":"playSfx"})).await {
                self.log(format!(
                    "не удалось поставить звук завершения в общий поток: {error}"
                ))
                .await;
            } else {
                tokio::time::sleep(Duration::from_millis(350)).await;
            }
            let confirmed = request_live_close(&mut session, Duration::from_secs(3)).await;
            self.log(if confirmed {
                "GPT-Live подтвердил закрытие по центральной кнопке"
            } else {
                "GPT-Live не подтвердил закрытие; завершаю локальный транспорт"
            })
            .await;
            stop_session(session).await;
            self.inner.lock().await.status = VoiceStatus {
                state: "idle".into(),
                message: "Звонок завершён".into(),
                call_id: None,
            };
            return Ok(ActionResult {
                ok: true,
                message: format!("Звонок {call_id} завершён"),
            });
        }

        self.log("запуск нового звонка").await;
        match self.start_session(oauth).await {
            Ok(session) => {
                let call_id = session.call_id.clone();
                let mut inner = self.inner.lock().await;
                inner.status = VoiceStatus {
                    state: "active".into(),
                    message: "Звонок подключён · центральная кнопка завершит его".into(),
                    call_id: Some(call_id.clone()),
                };
                inner.session = Some(session);
                drop(inner);
                self.log(format!("звонок подключён, id={call_id}")).await;
                Ok(ActionResult {
                    ok: true,
                    message: "Звонок с GPT-Live подключён".into(),
                })
            }
            Err(error) => {
                self.log(format!("ошибка запуска звонка: {error}")).await;
                self.inner.lock().await.status = VoiceStatus {
                    state: "error".into(),
                    message: error.clone(),
                    call_id: None,
                };
                Err(error)
            }
        }
    }

    async fn start_session(&self, oauth: &OAuthController) -> Result<LiveSession, String> {
        self.log("проверяю OAuth и нахожу аудиопомощник").await;
        let (token, account_id) = oauth.live_credentials().await?;
        let helper = helper_path()?;
        self.log(format!("запускаю аудиопомощник: {}", helper.display()))
            .await;
        let mut child = Command::new(&helper)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| {
                format!(
                    "Не удалось запустить аудиодвижок {}: {error}",
                    helper.display()
                )
            })?;
        let Some(mut stdin) = child.stdin.take() else {
            let _ = child.kill().await;
            return Err("Аудиодвижок не открыл управляющий канал".into());
        };
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill().await;
            return Err("Аудиодвижок не открыл канал событий".into());
        };
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    eprintln!("gpt-live-host: {line}");
                }
            });
        }
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<Value>();
        let (session_closed_tx, session_closed_rx) = watch::channel(false);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                match serde_json::from_str::<Value>(&line) {
                    Ok(event) => {
                        let summary = helper_event_summary(&event);
                        if !summary.is_empty() {
                            eprintln!("gpt-live-host: {summary}");
                        }
                        if event_tx.send(event).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        eprintln!("gpt-live-host returned invalid JSON ({error}): {line}")
                    }
                }
            }
        });

        let startup = async {
            wait_for_type(&mut event_rx, "ready", Duration::from_secs(10)).await?;
            send_command(&mut stdin, json!({"type":"start"})).await?;
            let offer = wait_for_type(&mut event_rx, "offer", Duration::from_secs(30)).await?;
            let sdp = offer
                .get("sdp")
                .and_then(Value::as_str)
                .ok_or_else(|| "Аудиодвижок вернул пустой WebRTC offer".to_owned())?;
            send_command(&mut stdin, json!({"type":"playSfx"})).await?;

            let ids = [
                uuid::Uuid::new_v4(),
                uuid::Uuid::new_v4(),
                uuid::Uuid::new_v4(),
            ];
            let response = self
                .client
                .post(CALL_URL)
                .bearer_auth(&token)
                .header("ChatGPT-Account-ID", &account_id)
                .header("OpenAI-Alpha", "quicksilver=v2")
                .header("session-id", ids[1].to_string())
                .header("thread-id", ids[2].to_string())
                .header("x-session-id", ids[0].to_string())
                .header("originator", "opencode")
                .header(
                    "User-Agent",
                    concat!("gpt-alice/", env!("CARGO_PKG_VERSION")),
                )
                .json(&json!({
                    "sdp": sdp,
                    "session": {
                        "model": MODEL,
                        "instructions": LIVE_INSTRUCTIONS,
                        "audio": { "output": { "voice": "maple" } },
                        "delegation": { "type": "client" }
                    }
                }))
                .send()
                .await
                .map_err(|error| format!("Не удалось создать GPT-Live звонок: {error}"))?;
            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                let detail: String = body
                    .replace(&token, "[скрыто]")
                    .replace(&account_id, "[скрыто]")
                    .chars()
                    .take(260)
                    .collect();
                return Err(if detail.trim().is_empty() {
                    format!("GPT-Live отклонил звонок (HTTP {status})")
                } else {
                    format!("GPT-Live отклонил звонок (HTTP {status}): {detail}")
                });
            }
            let call_id = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|location| {
                    location.split('/').find(|part| {
                        part.starts_with("rtc_")
                            || (part.len() == 36 && part.as_bytes().get(8) == Some(&b'-'))
                    })
                })
                .or_else(|| {
                    response
                        .headers()
                        .get("openai-session-id")
                        .and_then(|v| v.to_str().ok())
                })
                .unwrap_or("unknown")
                .to_owned();
            let answer = response
                .text()
                .await
                .map_err(|error| format!("Не удалось прочитать WebRTC answer: {error}"))?;
            if answer.trim().is_empty() {
                return Err("GPT-Live вернул пустой WebRTC answer".into());
            }
            send_command(&mut stdin, json!({"type":"answer", "sdp":answer})).await?;
            wait_for_type(&mut event_rx, "connected", Duration::from_secs(35)).await?;
            Ok::<_, String>(call_id)
        }
        .await;

        match startup {
            Ok(call_id) => {
                let monitor = Arc::clone(&self.inner);
                let monitored_call_id = call_id.clone();
                let indicator_stop = Arc::new(AtomicBool::new(false));
                let animation_stop = Arc::clone(&indicator_stop);
                let indicators = Arc::clone(&self.indicators);
                let animation_indicators = Arc::clone(&indicators);
                let indicator_task = tokio::spawn(async move {
                    let mut frame = 0;
                    let mut interval = tokio::time::interval(Duration::from_millis(150));
                    while !animation_stop.load(Ordering::Acquire) {
                        let indicators = Arc::clone(&animation_indicators);
                        let _ = tokio::task::spawn_blocking(move || {
                            indicators.render_voice_active_frame(frame)
                        })
                        .await;
                        frame = (frame + 1) % 6;
                        interval.tick().await;
                    }
                });
                let failed_indicator_stop = Arc::clone(&indicator_stop);
                let failed_indicators = Arc::clone(&indicators);
                let voice_monitor = self.clone();
                let search_oauth = oauth.clone();
                let search_client = self.client.clone();
                let intent_oauth = oauth.clone();
                let intent_client = self.client.clone();
                let end_call_pending = Arc::new(AtomicBool::new(false));
                let monitor_end_call_pending = Arc::clone(&end_call_pending);
                let monitor_session_closed = session_closed_tx.clone();
                tokio::spawn(async move {
                    let mut history: VecDeque<(String, String)> = VecDeque::with_capacity(24);
                    let mut current_user = String::new();
                    let mut current_assistant = String::new();
                    let mut handled_delegations = HashSet::new();
                    let mut active_delegation_id = String::new();
                    while let Some(event) = event_rx.recv().await {
                        let kind = event
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if kind == "event"
                            && let Some(data) = event.get("data").and_then(Value::as_str)
                            && let Ok(live_event) = serde_json::from_str::<Value>(data)
                        {
                            let live_type = live_event
                                .get("type")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            if live_type == "session.closed" {
                                monitor_session_closed.send_replace(true);
                                voice_monitor
                                    .log("получено событие GPT-Live session.closed")
                                    .await;
                                let closer = voice_monitor.clone();
                                let call_id = monitored_call_id.clone();
                                tokio::spawn(async move {
                                    closer.finish_from_agent(&call_id).await;
                                });
                                return;
                            }
                            match live_type {
                                "input_transcript.added" => {
                                    if let Some(fragment) = transcript_fragment(&live_event) {
                                        current_user.push_str(fragment);
                                    }
                                }
                                "turn.delta" => {
                                    let role = live_event
                                        .pointer("/turn/role")
                                        .or_else(|| live_event.get("role"))
                                        .and_then(Value::as_str)
                                        .unwrap_or_default();
                                    if role == "user"
                                        && let Some(fragment) = transcript_fragment(&live_event)
                                    {
                                        current_user.push_str(fragment);
                                    } else if role == "assistant"
                                        && let Some(fragment) = transcript_fragment(&live_event)
                                    {
                                        current_assistant.push_str(fragment);
                                    }
                                }
                                "session.input_transcript.delta" => {
                                    if let Some(delta) =
                                        live_event.get("delta").and_then(Value::as_str)
                                    {
                                        current_user.push_str(delta);
                                    }
                                }
                                "session.output_transcript.delta" => {
                                    if let Some(delta) =
                                        live_event.get("delta").and_then(Value::as_str)
                                    {
                                        current_assistant.push_str(delta);
                                    }
                                }
                                "session.input_transcript.done" => {
                                    let transcript = live_event
                                        .get("transcript")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default();
                                    if !transcript.trim().is_empty() {
                                        current_user = transcript.to_owned();
                                    }
                                }
                                "session.output_transcript.done" => {
                                    let transcript = live_event
                                        .get("transcript")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default();
                                    if !transcript.trim().is_empty() {
                                        current_assistant = transcript.to_owned();
                                    }
                                    push_turn(&mut history, "assistant", &mut current_assistant);
                                }
                                // Codex GPT-Live emits complete user/assistant turns here.
                                "turn.done" => {
                                    let role = live_event
                                        .pointer("/turn/role")
                                        .or_else(|| live_event.get("role"))
                                        .and_then(Value::as_str)
                                        .unwrap_or_default();
                                    let transcript = live_event
                                        .pointer("/turn/transcript")
                                        .or_else(|| live_event.get("transcript"))
                                        .and_then(Value::as_str)
                                        .or_else(|| {
                                            (role == "user" && !current_user.trim().is_empty())
                                                .then_some(current_user.as_str())
                                        });
                                    if let Some(transcript) = transcript {
                                        match role {
                                            "user" => {
                                                current_user = transcript.to_owned();
                                                voice_monitor
                                                    .log(format!(
                                                        "получена распознанная реплика пользователя ({} символов)",
                                                        current_user.chars().count()
                                                    ))
                                                    .await;
                                                push_turn(&mut history, "user", &mut current_user);
                                                let user_text = history
                                                    .iter()
                                                    .rev()
                                                    .find(|(turn_role, _)| turn_role == "user")
                                                    .map(|(_, text)| text.clone())
                                                    .unwrap_or_default();
                                                let delegation_id = std::mem::take(&mut active_delegation_id);
                                                if !user_text.trim().is_empty() && delegation_id.is_empty() {
                                                    let context: Vec<(String, String)> = history.iter().cloned().collect();
                                                    let voice = voice_monitor.clone();
                                                    let client = intent_client.clone();
                                                    let oauth = intent_oauth.clone();
                                                    let call_id = monitored_call_id.clone();
                                                    let pending = Arc::clone(&end_call_pending);
                                                    tokio::spawn(async move {
                                                        match classify_device_command(&client, &oauth, &context).await {
                                                            Ok(Some((command, end_after))) => {
                                                                voice.log(format!("GPT-классификатор распознал команду устройства: {}", command.label())).await;
                                                                let result = voice.execute_device_command(&command).await;
                                                                let succeeded = result.is_ok();
                                                                let end_after = end_after || matches!(command, DeviceCommand::EndCall);
                                                                let spoken = match result {
                                                                    Ok(message) if end_after => format!("{message}. Скажи пользователю короткое прощание и заверши звонок."),
                                                                    Ok(message) => message,
                                                                    Err(error) => format!("Не удалось выполнить команду колонки: {error}"),
                                                                };
                                                                voice.log(format!("команда устройства выполнена: {spoken}")).await;
                                                                let context_event = json!({
                                                                    "type":"session.context.append",
                                                                    "channel":"speakable",
                                                                    "content":[{"type":"input_text","text":format!("Подтверждённый результат команды на колонке: {spoken}. Если отвечаешь пользователю, коротко сообщи именно этот результат. Приложение уже выполнило действие.")}]
                                                                });
                                                                if let Err(error) = voice.send_live_event(context_event).await {
                                                                    voice.log(format!("не удалось передать результат команды в GPT-Live: {error}")).await;
                                                                } else {
                                                                    voice.log("результат команды добавлен в общий контекст GPT-Live").await;
                                                                }
                                                                if end_after && succeeded {
                                                                    pending.store(true, Ordering::Release);
                                                                    let timeout_voice = voice.clone();
                                                                    tokio::spawn(async move {
                                                                        tokio::time::sleep(Duration::from_secs(8)).await;
                                                                        if pending.swap(false, Ordering::AcqRel) {
                                                                            timeout_voice.log("агент не завершил прощальный ход за 8 секунд; закрываю звонок").await;
                                                                            timeout_voice.finish_from_agent(&call_id).await;
                                                                        }
                                                                    });
                                                                }
                                                            }
                                                            Ok(None) => {}
                                                            Err(error) => voice.log(format!("ошибка распознавания команды колонки: {error}")).await,
                                                        }
                                                    });
                                                }
                                            }
                                            "assistant" => {
                                                current_assistant = transcript.to_owned();
                                                push_turn(
                                                    &mut history,
                                                    "assistant",
                                                    &mut current_assistant,
                                                );
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                                _ => {}
                            }
                            if live_type == "turn.done"
                                && live_event.pointer("/turn/role").and_then(Value::as_str)
                                    == Some("assistant")
                                && monitor_end_call_pending.swap(false, Ordering::AcqRel)
                            {
                                voice_monitor
                                    .log("агент закончил прощальный ответ; дожидаюсь окончания аудио")
                                    .await;
                                tokio::time::sleep(Duration::from_secs(2)).await;
                                let closer = voice_monitor.clone();
                                let call_id = monitored_call_id.clone();
                                tokio::spawn(async move {
                                    closer.finish_from_agent(&call_id).await;
                                });
                                continue;
                            }
                            if live_type == "error" {
                                let detail = live_event
                                    .pointer("/error/message")
                                    .or_else(|| live_event.pointer("/message"))
                                    .and_then(Value::as_str)
                                    .unwrap_or("Ошибка события GPT-Live");
                                let code = live_event
                                    .pointer("/error/code")
                                    .or_else(|| live_event.pointer("/code"))
                                    .and_then(Value::as_str)
                                    .unwrap_or("unknown");
                                voice_monitor
                                    .log(format!("событие GPT-Live error ({code}): {detail}"))
                                    .await;
                                if code == "responses_validation_failed" {
                                    let mut inner = monitor.lock().await;
                                    if inner.status.call_id.as_deref()
                                        == Some(monitored_call_id.as_str())
                                    {
                                        inner.status.state = "error".into();
                                        inner.status.message = format!(
                                            "GPT-Live не принял настройки поиска: {detail}"
                                        );
                                    }
                                }
                            } else if live_type == "delegation.context.appended" {
                                voice_monitor
                                    .log("GPT-Live подтвердил получение результата делегации")
                                    .await;
                            } else if live_type == "session.commentary.appended" {
                                voice_monitor
                                    .log("GPT-Live принял speakable commentary (передача в контекст подтверждена)")
                                    .await;
                            } else if matches!(
                                live_type,
                                "session.delegation.created" | "delegation.created"
                            ) && live_event
                                .pointer("/delegation/target")
                                .or_else(|| live_event.pointer("/item/target"))
                                .and_then(Value::as_str)
                                == Some("client")
                            {
                                let delegation_id = live_event
                                    .pointer("/delegation/id")
                                    .or_else(|| live_event.pointer("/item/id"))
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_owned();
                                // Attach a client delegation only to the user turn
                                // currently being transcribed. A late delegation must
                                // not suppress command handling for the next utterance.
                                if !current_user.trim().is_empty() {
                                    active_delegation_id = delegation_id.clone();
                                }
                                if !delegation_id.is_empty()
                                    && handled_delegations.insert(delegation_id.clone())
                                {
                                    let delegated_text = live_event
                                        .pointer("/item/content")
                                        .and_then(Value::as_array)
                                        .map(|parts| {
                                            parts
                                                .iter()
                                                .filter(|part| {
                                                    part.get("type").and_then(Value::as_str)
                                                        == Some("input_text")
                                                })
                                                .filter_map(|part| {
                                                    part.get("text").and_then(Value::as_str)
                                                })
                                                .collect::<String>()
                                        })
                                        .unwrap_or_default();
                                    let command = decode_device_command(&delegated_text);
                                    match command {
                                        Ok(Some((command, end_after))) => {
                                            voice_monitor
                                                .log(format!(
                                                    "распознал голосовую команду устройства: {}",
                                                    command.label()
                                                ))
                                                .await;
                                            let voice = voice_monitor.clone();
                                            let pending = Arc::clone(&end_call_pending);
                                            let monitored_call_id = monitored_call_id.clone();
                                            let delegation_id = delegation_id.clone();
                                            tokio::spawn(async move {
                                                let result =
                                                    voice.execute_device_command(&command).await;
                                                let succeeded = result.is_ok();
                                                let end_after = end_after
                                                    || matches!(command, DeviceCommand::EndCall);
                                                let content = match result {
                                                    Ok(message) if end_after => format!(
                                                        "{message}. Скажи короткое прощание пользователю."
                                                    ),
                                                    Ok(message) => message,
                                                    Err(error) => format!(
                                                        "Команду выполнить не удалось: {error}"
                                                    ),
                                                };
                                                match voice.send_delegation_text(&delegation_id, &content).await {
                                                    Ok(chunks) => {
                                                        voice.log(format!("результат команды передан GPT-Live (фрагментов: {chunks})")).await;
                                                        if end_after && succeeded {
                                                            pending.store(true, Ordering::Release);
                                                            let timeout_voice = voice.clone();
                                                            tokio::spawn(async move {
                                                                tokio::time::sleep(Duration::from_secs(8)).await;
                                                                if pending.swap(false, Ordering::AcqRel) {
                                                                    timeout_voice.log("агент не завершил прощальный ход за 8 секунд; закрываю звонок").await;
                                                                    timeout_voice.finish_from_agent(&monitored_call_id).await;
                                                                }
                                                            });
                                                        }
                                                    }
                                                    Err(error) => voice.log(format!("не удалось передать результат команды в GPT-Live: {error}")).await,
                                                }
                                            });
                                            continue;
                                        }
                                        Ok(None) => {}
                                        Err(error) => {
                                            voice_monitor.log("GPT-Live прислал некорректную команду устройства").await;
                                            let voice = voice_monitor.clone();
                                            let delegation_id = delegation_id.clone();
                                            tokio::spawn(async move {
                                                let response =
                                                    format!("Формат команды не распознан: {error}");
                                                if let Err(error) = voice
                                                    .send_delegation_text(&delegation_id, &response)
                                                    .await
                                                {
                                                    voice.log(format!("не удалось сообщить об ошибке команды: {error}")).await;
                                                }
                                            });
                                            continue;
                                        }
                                    }
                                    voice_monitor
                                        .log("GPT-Live передал запрос на клиентский web search")
                                        .await;
                                    let mut context: Vec<(String, String)> =
                                        history.iter().cloned().collect();
                                    if !delegated_text.trim().is_empty()
                                        && !context
                                            .iter()
                                            .rev()
                                            .find(|(role, _)| role == "user")
                                            .is_some_and(|(_, text)| {
                                                text.trim() == delegated_text.trim()
                                            })
                                    {
                                        context.push(("user".into(), delegated_text));
                                    }
                                    if !current_user.trim().is_empty() {
                                        context
                                            .push(("user".into(), current_user.trim().to_owned()));
                                    }
                                    let voice = voice_monitor.clone();
                                    let oauth = search_oauth.clone();
                                    let client = search_client.clone();
                                    let intent_oauth = intent_oauth.clone();
                                    let intent_client = intent_client.clone();
                                    let command_pending = Arc::clone(&end_call_pending);
                                    let command_call_id = monitored_call_id.clone();
                                    tokio::spawn(async move {
                                        match classify_device_command(&intent_client, &intent_oauth, &context).await {
                                            Ok(Some((command, end_after))) => {
                                                voice.log(format!("команда устройства распознана до web search: {}", command.label())).await;
                                                let result = voice.execute_device_command(&command).await;
                                                let succeeded = result.is_ok();
                                                let end_after = end_after || matches!(command, DeviceCommand::EndCall);
                                                let content = match result {
                                                    Ok(message) if end_after => format!("{message}. Попрощайся с пользователем коротко."),
                                                    Ok(message) => message,
                                                    Err(error) => format!("Команду колонки выполнить не удалось: {error}"),
                                                };
                                                match voice.send_delegation_text(&delegation_id, &content).await {
                                                    Ok(chunks) => voice.log(format!("результат команды отправлен по исходной клиентской делегации (фрагментов: {chunks}); web search не запускался")).await,
                                                    Err(error) => voice.log(format!("не удалось передать результат команды по клиентской делегации: {error}")).await,
                                                }
                                                if end_after && succeeded {
                                                    command_pending.store(true, Ordering::Release);
                                                    let timeout_voice = voice.clone();
                                                    tokio::spawn(async move {
                                                        tokio::time::sleep(Duration::from_secs(8)).await;
                                                        if command_pending.swap(false, Ordering::AcqRel) {
                                                            timeout_voice.log("агент не завершил прощальный ход за 8 секунд; закрываю звонок").await;
                                                            timeout_voice.finish_from_agent(&command_call_id).await;
                                                        }
                                                    });
                                                }
                                                return;
                                            }
                                            Ok(None) => {}
                                            Err(error) => voice.log(format!("классификатор не смог проверить делегацию перед поиском: {error}; продолжаю обычный поиск")).await,
                                        }
                                        voice.log("выполняю web search через Codex OAuth").await;
                                        let result =
                                            crate::web_search::search(&client, &oauth, &context)
                                                .await;
                                        let (content, log_message) = match result {
                                            Ok(result) => (
                                                result,
                                                "web search завершён, результат передан в GPT-Live"
                                                    .to_owned(),
                                            ),
                                            Err(error) => (
                                                format!("Не удалось выполнить поиск: {error}"),
                                                format!("ошибка web search: {error}"),
                                            ),
                                        };
                                        match voice.send_delegation_text(&delegation_id, &content).await {
                                            Ok(chunks) => voice.log(format!("{log_message} (фрагментов: {chunks})")).await,
                                            Err(error) => voice.log(format!("не удалось вернуть результат в GPT-Live: {error}")).await,
                                        }
                                    });
                                }
                            } else if live_type.starts_with("response.web_search_call.")
                                || (live_type == "response.output_item.added"
                                    && live_event.pointer("/item/type").and_then(Value::as_str)
                                        == Some("web_search_call"))
                            {
                                voice_monitor
                                    .log(format!("GPT-Live search event: {live_type}"))
                                    .await;
                            }
                        }
                        if kind == "error" {
                            let detail = event
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("Ошибка аудиосессии");
                            let mut inner = monitor.lock().await;
                            if inner.status.call_id.as_deref() == Some(monitored_call_id.as_str()) {
                                inner.status.state = "error".into();
                                inner.status.message = detail.to_owned();
                            }
                            failed_indicator_stop.store(true, Ordering::Release);
                            let _ = failed_indicators.render_voice_idle();
                            return;
                        }
                        if kind == "peer" {
                            let state = event.get("state").and_then(Value::as_str).unwrap_or("unknown");
                            voice_monitor.log(format!("состояние WebRTC peer: {state}")).await;
                            match state {
                                // ICE may briefly disconnect while GPT-Live audio and
                                // the data channel are still usable. Keep the event
                                // monitor alive so subsequent commands are processed.
                                "disconnected" | "failed" => {
                                    let mut inner = monitor.lock().await;
                                    if inner.status.call_id.as_deref() == Some(monitored_call_id.as_str()) {
                                        inner.status.state = "active".into();
                                        inner.status.message = "Звонок активен · восстанавливается WebRTC".into();
                                    }
                                    continue;
                                }
                                "connected" => {
                                    let mut inner = monitor.lock().await;
                                    if inner.status.call_id.as_deref() == Some(monitored_call_id.as_str()) {
                                        inner.status.state = "active".into();
                                        inner.status.message = "Звонок подключён · центральная кнопка завершит его".into();
                                    }
                                    drop(inner);
                                    voice_monitor.log("WebRTC peer восстановил соединение").await;
                                    continue;
                                }
                                "closed" => {
                                    let mut inner = monitor.lock().await;
                                    if inner.status.call_id.as_deref() == Some(monitored_call_id.as_str()) {
                                        inner.status.state = "error".into();
                                        inner.status.message = "WebRTC peer закрыт".into();
                                    }
                                    failed_indicator_stop.store(true, Ordering::Release);
                                    let _ = failed_indicators.render_voice_idle();
                                    return;
                                }
                                _ => continue,
                            }
                        }
                    }
                    let mut inner = monitor.lock().await;
                    if inner.status.call_id.as_deref() == Some(monitored_call_id.as_str()) {
                        inner.status.state = "error".into();
                        inner.status.message = "Аудиосоединение завершилось".into();
                    }
                    failed_indicator_stop.store(true, Ordering::Release);
                    let _ = failed_indicators.render_voice_idle();
                });
                Ok(LiveSession {
                    call_id,
                    child,
                    stdin,
                    indicator_stop,
                    indicator_task,
                    indicators,
                    session_closed: session_closed_rx,
                })
            }
            Err(error) => {
                self.log(format!("этап запуска завершился ошибкой: {error}"))
                    .await;
                stop_child(child, stdin).await;
                Err(error)
            }
        }
    }
}

fn helper_event_summary(event: &Value) -> String {
    let kind = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    match kind {
        // Realtime event payloads may include raw user and assistant transcripts. Keep
        // diagnostics to event names only; the event itself still reaches the handler.
        "event" => event
            .get("data")
            .and_then(Value::as_str)
            .and_then(|data| serde_json::from_str::<Value>(data).ok())
            .and_then(|data| data.get("type").and_then(Value::as_str).map(str::to_owned))
            .map(|name| format!("realtime event: {name}"))
            .unwrap_or_else(|| "realtime event".into()),
        "peer" => format!(
            "peer state: {}",
            event
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        ),
        "error" => format!(
            "helper error: {}",
            event
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
        ),
        "levels" => String::new(),
        "offer" => "SDP offer ready".into(),
        _ => format!("event: {kind}"),
    }
}

fn push_turn(history: &mut VecDeque<(String, String)>, role: &str, text: &mut String) {
    let text = std::mem::take(text);
    if text.trim().is_empty() {
        return;
    }
    history.push_back((role.to_owned(), text));
    while history.len() > 24 {
        history.pop_front();
    }
}

fn transcript_fragment(event: &Value) -> Option<&str> {
    event
        .get("delta")
        .or_else(|| event.get("text"))
        .or_else(|| event.get("transcript"))
        .and_then(Value::as_str)
        .or_else(|| event.pointer("/item/transcript").and_then(Value::as_str))
        .or_else(|| event.pointer("/item/text").and_then(Value::as_str))
        .or_else(|| {
            event
                .pointer("/item/content/0/text")
                .and_then(Value::as_str)
        })
}

fn decode_device_command(text: &str) -> Result<Option<(DeviceCommand, bool)>, String> {
    let Ok(mut value) = serde_json::from_str::<Value>(text.trim()) else {
        return Ok(None);
    };
    let Some(action) = value.get("action").and_then(Value::as_str) else {
        return Ok(None);
    };
    let end_after = action == "end_call"
        || value
            .get("end_call_after")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    // Strict tool schemas require every property. Models fill irrelevant
    // nullable fields with null; remove those before deserializing the typed
    // command so a valid action is not rejected as `null` for u8/bool.
    if let Some(object) = value.as_object_mut() {
        object.retain(|_, field| !field.is_null());
    }
    let command = serde_json::from_value(value)
        .map_err(|error| format!("Некорректные параметры команды: {error}"))?;
    Ok(Some((command, end_after)))
}

fn split_utf8_bytes(text: &str, max_bytes: usize) -> Vec<String> {
    if text.is_empty() || max_bytes == 0 {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut end = 0;
    for (index, character) in text.char_indices() {
        let next = index + character.len_utf8();
        if next - start > max_bytes && end > start {
            chunks.push(text[start..end].to_owned());
            start = index;
        }
        end = next;
    }
    if end > start {
        chunks.push(text[start..end].to_owned());
    }
    chunks
}

fn helper_path() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("GPT_ALICE_LIVE_HOST") {
        return Ok(PathBuf::from(path));
    }
    let executable = std::env::current_exe()
        .map_err(|error| format!("Не удалось определить каталог приложения: {error}"))?;
    let parent = executable
        .parent()
        .ok_or_else(|| "У приложения не найден каталог".to_owned())?;
    Ok(parent.join(HELPER_NAME))
}

async fn send_command(stdin: &mut ChildStdin, value: Value) -> Result<(), String> {
    let mut line = serde_json::to_vec(&value).map_err(|error| error.to_string())?;
    line.push(b'\n');
    stdin
        .write_all(&line)
        .await
        .map_err(|error| format!("Не удалось передать команду аудиодвижку: {error}"))?;
    stdin
        .flush()
        .await
        .map_err(|error| format!("Не удалось отправить команду аудиодвижку: {error}"))
}

async fn wait_for_type(
    events: &mut mpsc::UnboundedReceiver<Value>,
    expected: &str,
    wait: Duration,
) -> Result<Value, String> {
    timeout(wait, async {
        loop {
            let event = events
                .recv()
                .await
                .ok_or_else(|| "Аудиодвижок закрыл канал событий".to_owned())?;
            match event
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
            {
                "error" => {
                    let detail = event
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("Неизвестная ошибка аудиодвижка");
                    return Err(format!("Аудиодвижок: {detail}"));
                }
                kind if kind == expected => return Ok(event),
                _ => {}
            }
        }
    })
    .await
    .map_err(|_| format!("Истекло время ожидания события {expected}"))?
}

async fn request_live_close(session: &mut LiveSession, wait_for: Duration) -> bool {
    let mut closed = session.session_closed.clone();
    if *closed.borrow() {
        return true;
    }
    let event = json!({
        "type":"session.close",
        "event_id":uuid::Uuid::new_v4().to_string()
    });
    if let Err(error) = send_command(
        &mut session.stdin,
        json!({"type":"sendEvent","event":event}),
    )
    .await
    {
        eprintln!("could not send GPT-Live session.close: {error}");
        return false;
    }
    timeout(wait_for, async {
        loop {
            if *closed.borrow() {
                return true;
            }
            if closed.changed().await.is_err() {
                return false;
            }
        }
    })
    .await
    .unwrap_or(false)
}

async fn stop_session(session: LiveSession) {
    session.indicator_stop.store(true, Ordering::Release);
    let _ = session.indicator_task.await;
    if let Err(error) = session.indicators.render_voice_idle() {
        eprintln!("voice idle indicator could not be drawn: {error}");
    }
    stop_child(session.child, session.stdin).await;
}

async fn stop_child(mut child: Child, mut stdin: ChildStdin) {
    let _ = send_command(&mut stdin, json!({"type":"close"})).await;
    if timeout(Duration::from_secs(4), child.wait()).await.is_err() {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
}

pub async fn get_status(Extension(controller): Extension<VoiceController>) -> Json<VoiceStatus> {
    Json(controller.status().await)
}

pub async fn get_logs(Extension(controller): Extension<VoiceController>) -> Json<Vec<VoiceLog>> {
    Json(controller.logs().await)
}

pub async fn toggle(
    Extension(controller): Extension<VoiceController>,
    Extension(oauth): Extension<OAuthController>,
) -> impl IntoResponse {
    controller
        .log("веб-интерфейс запросил переключение звонка")
        .await;
    match controller.toggle(&oauth).await {
        Ok(result) => (StatusCode::OK, Json(result)).into_response(),
        Err(error) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"ok":false,"message":error})),
        )
            .into_response(),
    }
}
