use std::{env, net::SocketAddr, sync::Arc};

use axum::{
    Json, Router,
    extract::{State, rejection::JsonRejection},
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::{get, post},
};
use serde::Deserialize;

mod audio;
mod indicators;
mod oauth;
mod runtime;
mod voice;
mod web_search;

use audio::AudioController;
use indicators::{ActionResult, IndicatorController, IndicatorStatus};
use runtime::{RuntimeController, RuntimeStatus, TimeSettings, TimeStatus};

const DEFAULT_ADDRESS: &str = "0.0.0.0:80";

async fn index() -> Html<&'static str> {
    Html(include_str!("../web/index.html"))
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
    let runtime = Arc::new(RuntimeController::new(Arc::clone(&indicators)));
    let audio = AudioController::new().map_err(std::io::Error::other)?;
    let voice = voice::VoiceController::new(indicators, Arc::clone(&runtime))
        .map_err(std::io::Error::other)?;
    let voice_button = voice.clone();
    let oauth_button = oauth.clone();
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
            button_runtime.spawn(async move {
                voice.log("физическая центральная кнопка нажата").await;
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

    println!("GPT Alice web interface: http://{address}");
    if address.ip().is_unspecified() {
        println!("Open this address from a device on the same network using the speaker's LAN IP.");
    }

    let app = Router::new()
        .route("/", get(index))
        .route("/api/indicators", get(indicator_status))
        .route("/api/indicators/ring", post(set_ring_color))
        .route("/api/indicators/ring/restore", post(restore_ring))
        .route("/api/indicators/clock", post(set_clock_brightness))
        .route(
            "/api/indicators/clock/brightness",
            post(set_clock_display_brightness),
        )
        .route("/api/runtime", get(runtime_status))
        .route("/api/time", get(time_status).post(update_time_settings))
        .route("/api/display/face", post(show_face))
        .route("/api/auth/status", get(oauth::get_status))
        .route("/api/auth/start", post(oauth::begin))
        .route("/api/auth/logout", post(oauth::logout))
        .route("/api/voice", get(voice::get_status))
        .route("/api/voice/logs", get(voice::get_logs))
        .route("/api/voice/toggle", post(voice::toggle))
        .layer(axum::Extension(voice))
        .layer(axum::Extension(oauth))
        .with_state(runtime);
    axum::serve(listener, app).await?;
    Ok(())
}
