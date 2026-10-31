use std::{env, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    extract::{State, rejection::JsonRejection},
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncBufReadExt;
use tokio::sync::watch;

mod audio;
mod indicators;
mod mixer;
mod oauth;
mod runtime;
mod scheduler;
mod training;
mod user_context;
mod voice;
mod vpn;
mod web_search;

use audio::AudioController;
use indicators::{ActionResult, IndicatorController, IndicatorStatus};
use runtime::{RuntimeController, RuntimeStatus, TimeSettings, TimeStatus};

const DEFAULT_ADDRESS: &str = "0.0.0.0:80";

async fn index() -> Html<&'static str> {
    Html(include_str!("../web/index.html"))
}

async fn training_page() -> Html<&'static str> {
    Html(include_str!("../web/training.html"))
}

#[derive(Deserialize)]
struct RingColorRequest {
    color: String,
}

#[derive(Deserialize)]
struct ClockBrightnessRequest {
    segment: String,
    brightness: u8,
}

#[derive(Deserialize)]
struct ClockDisplayBrightnessRequest {
    percent: u8,
}

#[derive(Deserialize)]
struct WakeWordUpdate {
    enabled: bool,
}

#[derive(Deserialize)]
struct BluetoothEnabledRequest {
    enabled: bool,
}

#[derive(Deserialize)]
struct BluetoothVolumeRequest {
    volume: u8,
}

#[derive(Deserialize)]
struct BluetoothPlaybackRequest {
    action: String,
}

async fn set_bluetooth_enabled(
    State(controller): State<Arc<RuntimeController>>,
    payload: Result<Json<BluetoothEnabledRequest>, JsonRejection>,
) -> impl IntoResponse {
    let payload = match payload {
        Ok(Json(payload)) => payload,
        Err(error) => return api_error(StatusCode::BAD_REQUEST, error.body_text()),
    };
    let controller = Arc::clone(&controller);
    match tokio::task::spawn_blocking(move || controller.set_bluetooth_enabled(payload.enabled))
        .await
    {
        Ok(Ok(result)) => (StatusCode::OK, Json(result)).into_response(),
        Ok(Err(error)) => api_error(StatusCode::BAD_GATEWAY, error),
        Err(error) => api_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

async fn set_bluetooth_volume(
    State(controller): State<Arc<RuntimeController>>,
    payload: Result<Json<BluetoothVolumeRequest>, JsonRejection>,
) -> impl IntoResponse {
    let payload = match payload {
        Ok(Json(payload)) => payload,
        Err(error) => return api_error(StatusCode::BAD_REQUEST, error.body_text()),
    };
    match controller.set_bluetooth_volume(payload.volume) {
        Ok(result) => (StatusCode::OK, Json(result)).into_response(),
        Err(error) => api_error(StatusCode::BAD_REQUEST, error),
    }
}

async fn bluetooth_playback(
    State(controller): State<Arc<RuntimeController>>,
    payload: Result<Json<BluetoothPlaybackRequest>, JsonRejection>,
) -> impl IntoResponse {
    let payload = match payload {
        Ok(Json(payload)) => payload,
        Err(error) => return api_error(StatusCode::BAD_REQUEST, error.body_text()),
    };
    let controller = Arc::clone(&controller);
    match tokio::task::spawn_blocking(move || controller.bluetooth_playback(&payload.action)).await
    {
        Ok(Ok(result)) => (StatusCode::OK, Json(result)).into_response(),
        Ok(Err(error)) => api_error(StatusCode::BAD_GATEWAY, error),
        Err(error) => api_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

async fn indicator_status(
    State(controller): State<Arc<RuntimeController>>,
) -> Json<IndicatorStatus> {
    Json(controller.indicators().status())
}

async fn set_ring_color(
    State(controller): State<Arc<RuntimeController>>,
    payload: Result<Json<RingColorRequest>, JsonRejection>,
) -> impl IntoResponse {
    let payload = match payload {
        Ok(Json(payload)) => payload,
        Err(error) => return api_error(StatusCode::BAD_REQUEST, error.body_text()),
    };
    match controller.indicators().set_ring_color(&payload.color) {
        Ok(result) => (StatusCode::OK, Json(result)).into_response(),
        Err(error) => api_error(StatusCode::BAD_REQUEST, error),
    }
}

async fn restore_ring(State(controller): State<Arc<RuntimeController>>) -> impl IntoResponse {
    match controller.indicators().restore_ring() {
        Ok(result) => (StatusCode::OK, Json(result)).into_response(),
        Err(error) => api_error(StatusCode::SERVICE_UNAVAILABLE, error),
    }
}

async fn set_clock_brightness(
    State(controller): State<Arc<RuntimeController>>,
    payload: Result<Json<ClockBrightnessRequest>, JsonRejection>,
) -> impl IntoResponse {
    let payload = match payload {
        Ok(Json(payload)) => payload,
        Err(error) => return api_error(StatusCode::BAD_REQUEST, error.body_text()),
    };
    match controller
        .indicators()
        .set_clock_brightness(&payload.segment, payload.brightness)
    {
        Ok(result) => (StatusCode::OK, Json(result)).into_response(),
        Err(error) => api_error(StatusCode::BAD_REQUEST, error),
    }
}

async fn set_clock_display_brightness(
    State(controller): State<Arc<RuntimeController>>,
    payload: Result<Json<ClockDisplayBrightnessRequest>, JsonRejection>,
) -> impl IntoResponse {
    let payload = match payload {
        Ok(Json(payload)) => payload,
        Err(error) => return api_error(StatusCode::BAD_REQUEST, error.body_text()),
    };
    match controller.set_clock_brightness_percent(payload.percent) {
        Ok(result) => (StatusCode::OK, Json(result)).into_response(),
        Err(error) => api_error(StatusCode::BAD_REQUEST, error),
    }
}

async fn runtime_status(State(controller): State<Arc<RuntimeController>>) -> Json<RuntimeStatus> {
    Json(controller.status())
}

async fn audio_test(State(controller): State<Arc<RuntimeController>>) -> impl IntoResponse {
    match controller.queue_audio_test_tone() {
        Ok(result) => (StatusCode::OK, Json(result)).into_response(),
        Err(error) => api_error(StatusCode::SERVICE_UNAVAILABLE, error),
    }
}

async fn time_status(State(controller): State<Arc<RuntimeController>>) -> Json<TimeStatus> {
    Json(controller.time_status())
}

async fn update_time_settings(
    State(controller): State<Arc<RuntimeController>>,
    payload: Result<Json<TimeSettings>, JsonRejection>,
) -> impl IntoResponse {
    let payload = match payload {
        Ok(Json(payload)) => payload,
        Err(error) => return api_error(StatusCode::BAD_REQUEST, error.body_text()),
    };
    match controller.update_time_settings(payload) {
        Ok(result) => (StatusCode::OK, Json(result)).into_response(),
        Err(error) => api_error(StatusCode::BAD_REQUEST, error),
    }
}

#[derive(Deserialize)]
struct SceneRequest {
    face: String,
    duration_seconds: Option<u64>,
}

async fn show_face(
    State(controller): State<Arc<RuntimeController>>,
    payload: Result<Json<SceneRequest>, JsonRejection>,
) -> impl IntoResponse {
    let payload = match payload {
        Ok(Json(payload)) => payload,
        Err(error) => return api_error(StatusCode::BAD_REQUEST, error.body_text()),
    };
    match controller.show_face(&payload.face, payload.duration_seconds.unwrap_or(10)) {
        Ok(result) => (StatusCode::OK, Json(result)).into_response(),
        Err(error) => api_error(StatusCode::BAD_REQUEST, error),
    }
}

fn api_error(status: StatusCode, message: String) -> axum::response::Response {
    (status, Json(ActionResult { ok: false, message })).into_response()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let address = env::var("GPT_ALICE_HTTP_ADDR").unwrap_or_else(|_| DEFAULT_ADDRESS.to_owned());
    let address: SocketAddr = address.parse()?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    let oauth = oauth::OAuthController::new_near_executable().map_err(std::io::Error::other)?;
    let oauth_refresh = oauth.clone();
    tokio::spawn(async move {
        oauth_refresh.refresh_loop().await;
    });
    let indicators = Arc::new(IndicatorController::new());
    let mixer = Arc::new(mixer::AudioMixer::start().map_err(std::io::Error::other)?);
    let user_context = user_context::UserContextController::new_near_executable();
    let runtime = Arc::new(RuntimeController::new(
        Arc::clone(&indicators),
        Arc::clone(&mixer),
    ));
    let scheduler = scheduler::Scheduler::new();
    let audio = AudioController::new(Arc::clone(&mixer));
    let voice = voice::VoiceController::new(
        indicators,
        Arc::clone(&runtime),
        scheduler.clone(),
        user_context.clone(),
    )
    .map_err(std::io::Error::other)?;
    scheduler.start(Arc::clone(&runtime), voice.clone());
    let wakeword = WakeWordController::new();
    let vpn = vpn::VpnController::new();
    let training_recorder = training::DeviceRecorder::default();
    let voice_button = voice.clone();
    let oauth_button = oauth.clone();
    let button_training_recorder = training_recorder.clone();
    let button_runtime = tokio::runtime::Handle::current();
    let volume_runtime = Arc::clone(&runtime);
    runtime.on_button(move |event| {
        let volume_changed = volume_runtime.handle_volume_button(&event);
        let center_pressed = matches!(
            &event,
            runtime::DeviceEvent::Touch {
                button,
                action,
                ..
            } if button == "center" && action == "pressed"
        );
        if center_pressed {
            let voice = voice_button.clone();
            let oauth = oauth_button.clone();
            let training_recorder = button_training_recorder.clone();
            button_runtime.spawn(async move {
                voice.log("физическая центральная кнопка нажата").await;
                if training_recorder.is_active().await {
                    voice
                        .log("звонок не запущен: микрофон занят записью обучающего примера")
                        .await;
                    return;
                }
                if let Err(error) = voice.toggle(&oauth).await {
                    eprintln!("voice call button failed: {error}");
                }
            });
        } else {
            let should_play_sfx = AudioController::should_play_button(&event, volume_changed);
            if should_play_sfx {
                let voice = voice_button.clone();
                let audio = audio.clone();
                button_runtime.spawn(async move {
                    if voice.audio_is_owned().await {
                        if let Err(error) = voice.play_sfx().await {
                            eprintln!("shared call SFX failed: {error}");
                        }
                    } else {
                        audio.handle_button(&event, volume_changed);
                    }
                });
            }
        }
    });
    runtime.start();

    let trigger_voice = voice.clone();
    let trigger_oauth = oauth.clone();
    let trigger_wakeword = wakeword.clone();
    let trigger_training_recorder = training_recorder.clone();
    tokio::spawn(async move {
        trigger_loop(
            trigger_voice,
            trigger_oauth,
            trigger_wakeword,
            trigger_training_recorder,
        )
        .await;
    });

    println!("GPT Alice web interface: http://{address}");
    if address.ip().is_unspecified() {
        println!("Open this address from a device on the same network using the speaker's LAN IP.");
    }

    let app = Router::new()
        .route("/", get(index))
        .route("/training", get(training_page))
        .route("/api/indicators", get(indicator_status))
        .route("/api/indicators/ring", post(set_ring_color))
        .route("/api/indicators/ring/restore", post(restore_ring))
        .route("/api/indicators/clock", post(set_clock_brightness))
        .route(
            "/api/indicators/clock/brightness",
            post(set_clock_display_brightness),
        )
        .route("/api/runtime", get(runtime_status))
        .route("/api/audio/test", post(audio_test))
        .route("/api/bluetooth/power", post(set_bluetooth_enabled))
        .route("/api/bluetooth/volume", post(set_bluetooth_volume))
        .route("/api/bluetooth/playback", post(bluetooth_playback))
        .route("/api/vpn", get(vpn::get_status))
        .route("/api/vpn/power", post(vpn::set_power))
        .route("/api/vpn/config", post(vpn::replace_config))
        .route("/api/time", get(time_status).post(update_time_settings))
        .route("/api/display/face", post(show_face))
        .route("/api/auth/status", get(oauth::get_status))
        .route("/api/auth/start", post(oauth::begin))
        .route("/api/auth/logout", post(oauth::logout))
        .route("/api/voice", get(voice::get_status))
        .route("/api/voice/silence", get(voice::get_silence_status))
        .route("/api/voice/logs", get(voice::get_logs))
        .route("/api/voice/toggle", post(toggle_voice))
        .route(
            "/api/context",
            get(user_context::get_profile).post(user_context::update_profile),
        )
        .route("/api/wakeword", get(wakeword_status).post(update_wakeword))
        .route("/api/training/capture/start", post(start_training_capture))
        .route(
            "/api/training/capture/stop",
            post(training::stop_device_capture),
        )
        .route(
            "/api/training/recordings/{filename}",
            get(training::download_recording),
        )
        .layer(axum::Extension(training_recorder))
        .layer(axum::Extension(voice))
        .layer(axum::Extension(user_context))
        .layer(axum::Extension(wakeword))
        .layer(axum::Extension(vpn))
        .layer(axum::Extension(oauth))
        .with_state(runtime);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn start_training_capture(
    axum::Extension(recorder): axum::Extension<training::DeviceRecorder>,
    axum::Extension(voice): axum::Extension<voice::VoiceController>,
    payload: Result<Json<training::CaptureMetadata>, JsonRejection>,
) -> impl IntoResponse {
    if voice.audio_is_owned().await {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"ok": false, "message": "Сначала заверши звонок: микрофон колонки сейчас занят"})),
        )
            .into_response();
    }
    let payload = match payload {
        Ok(payload) => payload,
        Err(error) => return api_error(StatusCode::BAD_REQUEST, error.body_text()),
    };
    training::start_device_capture(axum::Extension(recorder), payload).await
}

async fn toggle_voice(
    axum::Extension(controller): axum::Extension<voice::VoiceController>,
    axum::Extension(oauth): axum::Extension<oauth::OAuthController>,
    axum::Extension(recorder): axum::Extension<training::DeviceRecorder>,
) -> impl IntoResponse {
    if recorder.is_active().await {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"ok": false, "message": "Сначала останови запись примера: микрофон колонки сейчас занят"})),
        )
            .into_response();
    }
    controller
        .log("веб-интерфейс запросил переключение звонка")
        .await;
    match controller.toggle(&oauth).await {
        Ok(result) => (StatusCode::OK, Json(result)).into_response(),
        Err(error) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"ok":false,"message":error})),
        )
            .into_response(),
    }
}

#[derive(Clone)]
struct WakeWordController {
    directory: PathBuf,
    settings_path: PathBuf,
    changes: watch::Sender<WakeWordSettings>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(default)]
struct WakeWordSettings {
    enabled: bool,
    threshold: f32,
}

impl Default for WakeWordSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            threshold: 0.80,
        }
    }
}

impl WakeWordController {
    fn new() -> Self {
        let helper = env::var_os("GPT_ALICE_LIVE_HOST")
            .map(PathBuf::from)
            .or_else(|| {
                env::current_exe()
                    .ok()
                    .and_then(|path| path.parent().map(|dir| dir.join("gpt-live-host")))
            });
        let directory = helper
            .and_then(|path| path.parent().map(|dir| dir.join("wakeword")))
            .unwrap_or_else(|| PathBuf::from("wakeword"));
        let settings_path = directory.join("settings.json");
        let settings = std::fs::read(&settings_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<WakeWordSettings>(&bytes).ok())
            .unwrap_or_default();
        let (changes, _) = watch::channel(settings);
        Self {
            directory,
            settings_path,
            changes,
        }
    }

    fn subscribe(&self) -> watch::Receiver<WakeWordSettings> {
        self.changes.subscribe()
    }

    fn models_available(&self) -> bool {
        [
            "melspectrogram.onnx",
            "embedding_model.onnx",
            "alice_openwakeword_head.onnx",
        ]
        .iter()
        .all(|name| self.directory.join(name).is_file())
    }

    fn status(&self) -> serde_json::Value {
        let settings = *self.changes.borrow();
        serde_json::json!({
            "enabled": settings.enabled,
            "threshold": settings.threshold,
            "available": self.models_available(),
            "message": if self.models_available() { "Модель триггера найдена" } else { "Файлы модели триггера не найдены рядом с аудиодвижком" }
        })
    }

    fn update(&self, enabled: bool) -> Result<serde_json::Value, String> {
        if enabled && !self.models_available() {
            return Err("Не найдены файлы wake-word модели рядом с аудиодвижком".into());
        }
        let mut settings = *self.changes.borrow();
        settings.enabled = enabled;
        let bytes = serde_json::to_vec_pretty(&settings)
            .map_err(|error| format!("Не удалось сериализовать настройки триггера: {error}"))?;
        let temporary = self.settings_path.with_extension("json.tmp");
        std::fs::write(&temporary, bytes)
            .map_err(|error| format!("Не удалось записать настройки триггера: {error}"))?;
        std::fs::rename(&temporary, &self.settings_path)
            .map_err(|error| format!("Не удалось сохранить настройки триггера: {error}"))?;
        self.changes.send_replace(settings);
        Ok(self.status())
    }
}

async fn wakeword_status(
    axum::Extension(controller): axum::Extension<WakeWordController>,
) -> axum::Json<serde_json::Value> {
    axum::Json(controller.status())
}

async fn update_wakeword(
    axum::Extension(controller): axum::Extension<WakeWordController>,
    payload: Result<Json<WakeWordUpdate>, JsonRejection>,
) -> impl IntoResponse {
    let payload = match payload {
        Ok(Json(payload)) => payload,
        Err(error) => return api_error(StatusCode::BAD_REQUEST, error.body_text()),
    };
    match controller.update(payload.enabled) {
        Ok(status) => (StatusCode::OK, Json(status)).into_response(),
        Err(error) => api_error(StatusCode::BAD_REQUEST, error),
    }
}

async fn trigger_loop(
    voice: voice::VoiceController,
    oauth: oauth::OAuthController,
    wakeword: WakeWordController,
    training_recorder: training::DeviceRecorder,
) {
    let mut settings_rx = wakeword.subscribe();
    let executable = match env::var_os("GPT_ALICE_LIVE_HOST") {
        Some(path) => PathBuf::from(path),
        None => match env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(|dir| dir.join("gpt-live-host")))
        {
            Some(path) => path,
            None => {
                eprintln!("wake-word detector disabled: cannot locate gpt-live-host");
                return;
            }
        },
    };
    loop {
        if training_recorder.is_active().await {
            tokio::time::sleep(Duration::from_millis(250)).await;
            continue;
        }
        let settings = *settings_rx.borrow();
        if !settings.enabled || !wakeword.models_available() {
            tokio::select! {
                _ = settings_rx.changed() => {},
                _ = tokio::time::sleep(Duration::from_secs(1)) => {},
            }
            continue;
        }
        if voice.audio_is_owned().await {
            tokio::select! {
                _ = settings_rx.changed() => {},
                _ = tokio::time::sleep(Duration::from_millis(250)) => {},
            }
            continue;
        }
        let directory = wakeword.directory.clone();
        let mut child = match tokio::process::Command::new(&executable)
            .arg("--wakeword")
            .arg(&directory)
            .arg(settings.threshold.to_string())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                eprintln!("could not start wake-word detector: {error}");
                tokio::select! {
                    _ = settings_rx.changed() => {},
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {},
                }
                continue;
            }
        };
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill().await;
            tokio::select! {
                _ = settings_rx.changed() => {},
                _ = tokio::time::sleep(Duration::from_secs(5)) => {},
            }
            continue;
        };
        let mut lines = tokio::io::BufReader::new(stdout).lines();
        let mut should_restart = false;
        let mut trigger_detected = false;
        loop {
            tokio::select! {
                _ = settings_rx.changed() => {
                    should_restart = true;
                    break;
                }
                line = lines.next_line() => match line {
                    Ok(Some(line)) => {
                        let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
                            eprintln!("wake-word detector emitted invalid JSON: {line}");
                            continue;
                        };
                        match event.get("type").and_then(serde_json::Value::as_str) {
                            Some("wakeword_ready") => eprintln!("wake-word microphone stream ready"),
                            Some("wakeword_detected") => {
                                let score = event.get("score").and_then(serde_json::Value::as_f64).unwrap_or_default();
                                eprintln!("wake-word detected (score={score:.3}); preparing GPT-Live call");
                                trigger_detected = true;
                                should_restart = true;
                                break;
                            }
                            Some("error") => eprintln!("wake-word detector error: {}", event.get("message").and_then(serde_json::Value::as_str).unwrap_or("unknown error")),
                            _ => {}
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        eprintln!("wake-word detector event stream failed: {error}");
                        break;
                    }
                },
                _ = tokio::time::sleep(Duration::from_millis(250)) => {
                    if voice.audio_is_owned().await || training_recorder.is_active().await {
                        should_restart = true;
                        break;
                    }
                }
            }
        }
        let _ = child.kill().await;
        let _ = child.wait().await;
        if trigger_detected && !training_recorder.is_active().await {
            // Release the capture device before GPT-Live tries to open it.
            if let Err(error) = voice.start_from_wakeword(&oauth).await {
                eprintln!("wake-word call start failed: {error}");
            }
        }
        // A failed call must not be retried in a tight loop after a trigger.
        if should_restart {
            tokio::select! {
                _ = settings_rx.changed() => {},
                _ = tokio::time::sleep(Duration::from_secs(5)) => {},
            }
        }
    }
}
