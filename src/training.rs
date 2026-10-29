use std::{
    env,
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use axum::{
    Extension, Json,
    body::Bytes,
    extract::{Path, Query},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
    sync::Mutex,
    task::JoinHandle,
};
use uuid::Uuid;

#[derive(Deserialize)]
pub struct RecordingMetadata {
    duration_ms: u32,
    speaker_id: String,
    sha256: String,
}

#[derive(Clone, Deserialize)]
pub struct CaptureMetadata {
    speaker_id: String,
}

struct ActiveCapture {
    child: Child,
    audio: JoinHandle<std::io::Result<Vec<u8>>>,
}

#[derive(Clone, Default)]
pub struct DeviceRecorder {
    active: Arc<AtomicBool>,
    capture: Arc<Mutex<Option<ActiveCapture>>>,
}

impl DeviceRecorder {
    pub async fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
}

#[derive(Serialize)]
pub struct RecordingSaved {
    ok: bool,
    message: String,
    filename: String,
    sha256: String,
}

#[derive(Serialize)]
struct StoredMetadata<'a> {
    schema_version: u8,
    label: &'static str,
    text: &'a str,
    language: &'static str,
    speaker_id: &'a str,
    speaker_group: &'static str,
    source_recording_id: &'a str,
    source: &'static str,
    microphone: &'static str,
    duration_ms: u32,
    sample_rate_hz: u32,
    channels: u8,
    sample_format: &'static str,
    sha256: &'a str,
}

pub async fn save_recording(
    Query(metadata): Query<RecordingMetadata>,
    wav: Bytes,
) -> Result<Json<RecordingSaved>, (StatusCode, String)> {
    validate(&metadata, &wav)?;

    let hash = format!("{:x}", Sha256::digest(&wav));
    if hash != metadata.sha256.to_ascii_lowercase() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Контрольная сумма записи не совпала".into(),
        ));
    }

    persist_recording(
        metadata,
        wav.to_vec(),
        hash,
        "browser_microphone_recording",
        "selected_by_browser_user",
    )
    .await
    .map(Json)
}

async fn persist_recording(
    metadata: RecordingMetadata,
    audio_bytes: Vec<u8>,
    hash: String,
    source: &'static str,
    microphone: &'static str,
) -> Result<RecordingSaved, (StatusCode, String)> {
    let id = Uuid::new_v4().to_string();
    let filename = format!("{id}.wav");
    let metadata_json = serde_json::to_vec_pretty(&StoredMetadata {
        schema_version: 2,
        label: "positive",
        text: "Алиса",
        language: "ru-RU",
        speaker_id: &metadata.speaker_id,
        speaker_group: "adult_real_self_recorded",
        source_recording_id: &id,
        source,
        microphone,
        duration_ms: metadata.duration_ms,
        sample_rate_hz: 16_000,
        channels: 1,
        sample_format: "pcm_s16le",
        sha256: &hash,
    })
    .map_err(internal_error)?;

    let directory = recordings_directory().map_err(internal_error)?;
    let wav_path = directory.join(&filename);
    let metadata_path = directory.join(format!("{id}.json"));
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        fs::create_dir_all(&directory)?;
        let mut audio_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&wav_path)?;
        audio_file.write_all(&audio_bytes)?;
        audio_file.sync_all()?;
        let mut sidecar = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&metadata_path)?;
        sidecar.write_all(&metadata_json)?;
        sidecar.write_all(b"\n")?;
        sidecar.sync_all()?;
        Ok(())
    })
    .await
    .map_err(internal_error)?
    .map_err(internal_error)?;

    Ok(RecordingSaved {
        ok: true,
        message: "Запись сохранена локально".into(),
        filename,
        sha256: hash,
    })
}

pub async fn start_device_capture(
    Extension(recorder): Extension<DeviceRecorder>,
    Json(metadata): Json<CaptureMetadata>,
) -> Response {
    if let Err((status, message)) = validate_capture_metadata(&metadata) {
        return (
            status,
            Json(serde_json::json!({"ok": false, "message": message})),
        )
            .into_response();
    }
    if recorder
        .active
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"ok": false, "message": "Запись уже идёт"})),
        )
            .into_response();
    }

    // Give the wake-word supervisor time to stop its existing microphone process.
    tokio::time::sleep(std::time::Duration::from_millis(350)).await;

    let mut command = Command::new("arecord");
    command
        .args([
            "-D", "hw:0,1", "-f", "S16_LE", "-r", "16000", "-c", "4", "-t", "raw", "-d", "8",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            recorder.active.store(false, Ordering::Release);
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"ok": false, "message": format!("Не удалось открыть микрофон колонки: {error}")})),
            )
                .into_response();
        }
    };
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill().await;
        recorder.active.store(false, Ordering::Release);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"ok": false, "message": "Не удалось открыть поток записи"})),
        )
            .into_response();
    };
    let audio = tokio::spawn(async move {
        let mut bytes = Vec::with_capacity(16000 * 4 * 2 * 4);
        stdout.read_to_end(&mut bytes).await?;
        Ok(bytes)
    });
    *recorder.capture.lock().await = Some(ActiveCapture { child, audio });
    let expiration = recorder.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(9)).await;
        if let Some(mut capture) = expiration.capture.lock().await.take() {
            let _ = capture.child.start_kill();
            let _ = capture.child.wait().await;
            let _ = capture.audio.await;
            expiration.active.store(false, Ordering::Release);
        }
    });

    // Metadata stays in the UI until stop; this endpoint only reserves the microphone.
    (
        StatusCode::OK,
        Json(serde_json::json!({"ok": true, "message": "Микрофон колонки готов к записи", "max_duration_ms": 8000})),
    )
        .into_response()
}

pub async fn stop_device_capture(
    Extension(recorder): Extension<DeviceRecorder>,
    Json(metadata): Json<CaptureMetadata>,
) -> Response {
    if let Err((status, message)) = validate_capture_metadata(&metadata) {
        return (
            status,
            Json(serde_json::json!({"ok": false, "message": message})),
        )
            .into_response();
    }
    let capture = recorder.capture.lock().await.take();
    let Some(mut capture) = capture else {
        recorder.active.store(false, Ordering::Release);
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"ok": false, "message": "Сейчас нет активной записи"})),
        )
            .into_response();
    };
    let _ = capture.child.start_kill();
    let _ = capture.child.wait().await;
    let raw = match capture.audio.await {
        Ok(Ok(raw)) => raw,
        Ok(Err(error)) => {
            recorder.active.store(false, Ordering::Release);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"ok": false, "message": format!("Ошибка чтения микрофона: {error}")})),
            )
                .into_response();
        }
        Err(error) => {
            recorder.active.store(false, Ordering::Release);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"ok": false, "message": format!("Ошибка потока записи: {error}")})),
            )
                .into_response();
        }
    };
    let duration_ms = (raw.len() / (4 * 2) * 1000 / 16_000).min(8_000) as u32;
    recorder.active.store(false, Ordering::Release);
    let wav = match quad_channel_pcm_to_mono_wav(&raw) {
        Ok(wav) => wav,
        Err(message) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"ok": false, "message": message})),
            )
                .into_response();
        }
    };
    let sha256 = format!("{:x}", Sha256::digest(&wav));
    let recording = RecordingMetadata {
        duration_ms,
        speaker_id: metadata.speaker_id,
        sha256: sha256.clone(),
    };
    if let Err((status, message)) = validate(&recording, &wav) {
        return (
            status,
            Json(serde_json::json!({"ok": false, "message": message})),
        )
            .into_response();
    }
    match persist_recording(
        recording,
        wav,
        sha256,
        "mini3_internal_microphone_recording",
        "mini3_internal_array_hw_0_1",
    )
    .await
    {
        Ok(saved) => (StatusCode::OK, Json(saved)).into_response(),
        Err((status, message)) => (
            status,
            Json(serde_json::json!({"ok": false, "message": message})),
        )
            .into_response(),
    }
}

pub async fn download_recording(Path(filename): Path<String>) -> Response {
    let Some((id, extension)) = filename.rsplit_once('.') else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if Uuid::parse_str(id).is_err() || !matches!(extension, "wav" | "json") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let path = match recordings_directory() {
        Ok(directory) => directory.join(&filename),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    match fs::read(path) {
        Ok(bytes) => (
            [(
                header::CONTENT_TYPE,
                if extension == "wav" {
                    "audio/wav".to_owned()
                } else {
                    "application/json; charset=utf-8".to_owned()
                },
            )],
            bytes,
        )
            .into_response(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            StatusCode::NOT_FOUND.into_response()
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

fn validate_capture_metadata(metadata: &CaptureMetadata) -> Result<(), (StatusCode, String)> {
    if !(8..=64).contains(&metadata.speaker_id.len())
        || !metadata
            .speaker_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "Некорректный локальный идентификатор диктора".into(),
        ));
    }
    Ok(())
}

fn quad_channel_pcm_to_mono_wav(raw: &[u8]) -> Result<Vec<u8>, String> {
    const CHANNELS: usize = 4;
    if raw.len() % (CHANNELS * 2) != 0 {
        return Err("Микрофон вернул неполный PCM-кадр; повтори запись".into());
    }
    let frames = raw.len() / (CHANNELS * 2);
    let duration_ms = frames * 1000 / 16_000;
    if !(150..=8_000).contains(&duration_ms) {
        return Err("Запись слишком короткая или длиннее 8 секунд".into());
    }
    let data_bytes = frames * 2;
    let mut wav = Vec::with_capacity(44 + data_bytes);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&((36 + data_bytes) as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&16_000u32.to_le_bytes());
    wav.extend_from_slice(&32_000u32.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data_bytes as u32).to_le_bytes());
    for frame in raw.chunks_exact(CHANNELS * 2) {
        let sum: i32 = frame
            .chunks_exact(2)
            .map(|sample| i16::from_le_bytes([sample[0], sample[1]]) as i32)
            .sum();
        let mono = (sum / CHANNELS as i32) as i16;
        wav.extend_from_slice(&mono.to_le_bytes());
    }
    Ok(wav)
}

fn validate(metadata: &RecordingMetadata, wav: &[u8]) -> Result<(), (StatusCode, String)> {
    if !(150..=12_000).contains(&metadata.duration_ms) {
        return Err((
            StatusCode::BAD_REQUEST,
            "Длительность записи вне допустимого диапазона".into(),
        ));
    }
    if !(8..=64).contains(&metadata.speaker_id.len())
        || !metadata
            .speaker_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "Некорректный локальный идентификатор диктора".into(),
        ));
    }
    if metadata.sha256.len() != 64 || !metadata.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "Некорректная контрольная сумма".into(),
        ));
    }
    if wav.len() < 44
        || wav.len() > 2 * 1024 * 1024
        || &wav[0..4] != b"RIFF"
        || &wav[8..12] != b"WAVE"
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "Ожидался WAV-файл размером до 2 МБ".into(),
        ));
    }
    if &wav[12..16] != b"fmt "
        || u16::from_le_bytes([wav[20], wav[21]]) != 1
        || u16::from_le_bytes([wav[22], wav[23]]) != 1
        || u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]) != 16_000
        || u16::from_le_bytes([wav[34], wav[35]]) != 16
        || &wav[36..40] != b"data"
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "WAV должен быть моно PCM 16 кГц / 16 бит".into(),
        ));
    }
    Ok(())
}

fn recordings_directory() -> std::io::Result<PathBuf> {
    if let Some(directory) = env::var_os("GPT_ALICE_TRAINING_DIR") {
        return Ok(PathBuf::from(directory));
    }
    let directory = match env::current_exe()?.parent() {
        Some(directory) => PathBuf::from(directory),
        None => env::current_dir()?,
    };
    Ok(directory.join("training/corpus/recorded"))
}

fn internal_error(error: impl std::fmt::Display) -> (StatusCode, String) {
    eprintln!("training recording save failed: {error}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "Не удалось сохранить запись на диск".into(),
    )
}
