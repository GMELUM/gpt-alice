use crate::{indicators::ActionResult, storage::Storage};
use axum::{
    Json,
    extract::{Extension, rejection::JsonRejection},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct TelegramController(Arc<Inner>);
struct Inner {
    storage: Storage,
    client: reqwest::Client,
    operation: Mutex<()>,
    api: String,
}

#[derive(Default, Serialize, Deserialize)]
struct Config {
    enabled: bool,
    token: String,
    chat_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigUpdate {
    enabled: bool,
    token: Option<String>,
    chat_id: String,
}

#[derive(Serialize)]
pub struct Status {
    enabled: bool,
    configured: bool,
    token_set: bool,
    chat_id: String,
}

impl TelegramController {
    pub fn new(storage: Storage) -> Result<Self, String> {
        let _: Option<Config> = storage.get("telegram")?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(25))
            .connect_timeout(Duration::from_secs(8))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| "Не удалось создать клиент Telegram")?;
        Ok(Self(Arc::new(Inner {
            storage,
            client,
            operation: Mutex::new(()),
            api: "https://api.telegram.org".into(),
        })))
    }

    fn config(&self) -> Result<Config, String> {
        Ok(self.0.storage.get("telegram")?.unwrap_or_default())
    }

    fn status(&self) -> Result<Status, String> {
        let config = self.config()?;
        Ok(Status {
            enabled: config.enabled,
            configured: !config.token.is_empty() && !config.chat_id.is_empty(),
            token_set: !config.token.is_empty(),
            chat_id: config.chat_id,
        })
    }

    async fn update(&self, update: ConfigUpdate) -> Result<(), String> {
        let _operation = self.0.operation.lock().await;
        let old = self.config()?;
        let token = update
            .token
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
            .unwrap_or(old.token);
        let chat_id = update.chat_id.trim().to_owned();
        if !token.is_empty() {
            validate_token(&token)?;
        }
        if !chat_id.is_empty() {
            validate_chat(&chat_id)?;
        }
        if update.enabled && (token.is_empty() || chat_id.is_empty()) {
            return Err("Для включения Telegram укажи токен бота и ID чата".into());
        }
        self.0.storage.set(
            "telegram",
            &Config {
                enabled: update.enabled,
                token,
                chat_id,
            },
        )
    }

    pub async fn send_text(&self, text: &str) -> Result<String, String> {
        validate_text(text, 4096, false)?;
        self.send("sendMessage", json!({"text":text})).await
    }

    pub async fn send_link(&self, url: &str, text: &str) -> Result<String, String> {
        validate_url(url)?;
        let message = if text.trim().is_empty() {
            url.to_owned()
        } else {
            format!("{}\n{url}", text.trim())
        };
        self.send_text(&message).await
    }

    pub async fn send_photo(&self, url: &str, caption: &str) -> Result<String, String> {
        validate_url(url)?;
        validate_text(caption, 1024, true)?;
        self.send("sendPhoto", json!({"photo":url,"caption":caption}))
            .await
    }

    async fn send(&self, method: &str, mut body: Value) -> Result<String, String> {
        let _operation = self.0.operation.lock().await;
        let config = self.config()?;
        if !config.enabled {
            return Err("Telegram выключен. Включи его в веб-панели колонки".into());
        }
        validate_token(&config.token)?;
        validate_chat(&config.chat_id)?;
        body["chat_id"] = json!(config.chat_id);
        // Never format reqwest errors: their URL includes the bot token.
        // No automatic retries: a lost response may mean delivery already happened.
        let mut response = self
            .0
            .client
            .post(format!("{}/bot{}/{method}", self.0.api, config.token))
            .json(&body)
            .send()
            .await
            .map_err(|_| uncertain())?;
        let status = response.status();
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| uncertain())? {
            if bytes.len() + chunk.len() > 128 * 1024 {
                return Err(uncertain());
            }
            bytes.extend_from_slice(&chunk);
        }
        let result: Value = serde_json::from_slice(&bytes).map_err(|_| uncertain())?;
        if !status.is_success() || result.get("ok").and_then(Value::as_bool) != Some(true) {
            let code = result
                .get("error_code")
                .and_then(Value::as_u64)
                .unwrap_or(u64::from(status.as_u16()));
            return Err(match code {
                401 => "Telegram отклонил токен бота. Проверь настройки".into(),
                403 => "Боту запрещена отправка. Открой чат с ботом и нажми Start или выдай права в группе/канале".into(),
                400 => "Telegram отклонил сообщение. Проверь ID чата; для картинки нужна прямая общедоступная ссылка на изображение".into(),
                429 => "Telegram временно ограничил отправку. Повтори просьбу позже".into(),
                _ => uncertain(),
            });
        }
        if result
            .pointer("/result/message_id")
            .and_then(Value::as_i64)
            .is_none()
        {
            return Err(uncertain());
        }
        Ok(if method == "sendPhoto" {
            "Картинка отправлена в настроенный Telegram-чат"
        } else {
            "Сообщение отправлено в настроенный Telegram-чат"
        }
        .into())
    }
}

fn uncertain() -> String {
    "Не удалось подтвердить доставку в Telegram. Сообщение могло уйти; проверь чат перед повторной отправкой".into()
}
fn validate_token(token: &str) -> Result<(), String> {
    let valid = token.split_once(':').is_some_and(|(id, secret)| {
        !id.is_empty()
            && id.bytes().all(|b| b.is_ascii_digit())
            && secret.len() >= 20
            && secret
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    });
    if !valid || token.len() > 256 {
        return Err("Некорректный токен Telegram-бота".into());
    }
    Ok(())
}
fn validate_chat(chat: &str) -> Result<(), String> {
    let valid = chat.parse::<i64>().is_ok_and(|id| id != 0)
        || chat.strip_prefix('@').is_some_and(|v| {
            (5..=32).contains(&v.len()) && v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        });
    if !valid {
        return Err("Укажи числовой ID чата или @username группы/канала".into());
    }
    Ok(())
}
fn validate_url(value: &str) -> Result<(), String> {
    let url = reqwest::Url::parse(value).map_err(|_| "Некорректная ссылка")?;
    if value.len() > 4096
        || !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("Нужна HTTP(S)-ссылка без логина и пароля".into());
    }
    Ok(())
}
fn validate_text(text: &str, limit: usize, empty: bool) -> Result<(), String> {
    if (!empty && text.trim().is_empty()) || text.chars().count() > limit {
        return Err(format!(
            "Текст должен содержать {}–{limit} символов",
            if empty { 0 } else { 1 }
        ));
    }
    Ok(())
}

pub async fn get_status(Extension(controller): Extension<TelegramController>) -> Response {
    match controller.status() {
        Ok(status) => ([(header::CACHE_CONTROL, "no-store")], Json(status)).into_response(),
        Err(message) => failure(StatusCode::INTERNAL_SERVER_ERROR, message),
    }
}
pub async fn update_config(
    Extension(controller): Extension<TelegramController>,
    payload: Result<Json<ConfigUpdate>, JsonRejection>,
) -> Response {
    let Ok(Json(update)) = payload else {
        return failure(
            StatusCode::BAD_REQUEST,
            "Некорректные поля настроек Telegram".into(),
        );
    };
    match controller.update(update).await {
        Ok(()) => Json(ActionResult {
            ok: true,
            message: "Настройки Telegram сохранены".into(),
        })
        .into_response(),
        Err(message) => failure(StatusCode::BAD_REQUEST, message),
    }
}
pub async fn clear_config(Extension(controller): Extension<TelegramController>) -> Response {
    let _operation = controller.0.operation.lock().await;
    match controller.0.storage.delete("telegram") {
        Ok(()) => Json(ActionResult {
            ok: true,
            message: "Токен и чат Telegram удалены; отправка выключена".into(),
        })
        .into_response(),
        Err(message) => failure(StatusCode::INTERNAL_SERVER_ERROR, message),
    }
}
pub async fn send_test(Extension(controller): Extension<TelegramController>) -> Response {
    match controller
        .send_text("GPT Alice: проверка отправки с колонки.")
        .await
    {
        Ok(message) => Json(ActionResult { ok: true, message }).into_response(),
        Err(message) => failure(StatusCode::BAD_GATEWAY, message),
    }
}
fn failure(status: StatusCode, message: String) -> Response {
    (status, Json(ActionResult { ok: false, message })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        extract::{Path, State},
        routing::post,
    };
    const TOKEN: &str = "12345:abcdefghijklmnopqrstuvwxyz";

    #[tokio::test]
    async fn config_preserves_token_and_status_never_exposes_it() {
        let controller = TelegramController::new(Storage::memory()).unwrap();
        controller
            .update(ConfigUpdate {
                enabled: true,
                token: Some(TOKEN.into()),
                chat_id: "42".into(),
            })
            .await
            .unwrap();
        controller
            .update(ConfigUpdate {
                enabled: true,
                token: None,
                chat_id: "-100123".into(),
            })
            .await
            .unwrap();
        assert_eq!(controller.config().unwrap().token, TOKEN);
        let status = serde_json::to_string(&controller.status().unwrap()).unwrap();
        assert!(!status.contains(TOKEN));
        assert_eq!(controller.status().unwrap().chat_id, "-100123");
        assert!(
            controller
                .update(ConfigUpdate {
                    enabled: true,
                    token: Some("bad/secret".into()),
                    chat_id: "42".into()
                })
                .await
                .is_err()
        );
        assert_eq!(controller.config().unwrap().token, TOKEN);
    }

    #[tokio::test]
    async fn payloads_use_saved_recipient_and_delivery_requires_telegram_confirmation() {
        let requests = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
        async fn receive(
            State(requests): State<Arc<Mutex<Vec<(String, Value)>>>>,
            Path(path): Path<String>,
            Json(body): Json<Value>,
        ) -> Json<Value> {
            let fail = body.get("text").and_then(Value::as_str) == Some("fail");
            requests.lock().await.push((path, body));
            if fail {
                Json(json!({"ok":false,"error_code":403,"description":TOKEN}))
            } else {
                Json(json!({"ok":true,"result":{"message_id":12}}))
            }
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/{*path}", post(receive))
            .with_state(requests.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let controller = TelegramController(Arc::new(Inner {
            storage: Storage::memory(),
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap(),
            operation: Mutex::new(()),
            api: format!("http://{address}"),
        }));
        controller
            .update(ConfigUpdate {
                enabled: true,
                token: Some(TOKEN.into()),
                chat_id: "42".into(),
            })
            .await
            .unwrap();
        controller.send_text("Привет <без HTML>").await.unwrap();
        controller
            .send_link("https://example.com/page", "Ссылка")
            .await
            .unwrap();
        controller
            .send_photo("https://example.com/image.jpg", "Картинка")
            .await
            .unwrap();
        let error = controller.send_text("fail").await.unwrap_err();
        assert!(!error.contains(TOKEN));
        assert!(controller.send_text(&"я".repeat(4097)).await.is_err());
        assert!(
            controller
                .send_photo("file:///etc/passwd", "")
                .await
                .is_err()
        );
        let calls = requests.lock().await;
        assert_eq!(calls.len(), 4); // No retries, no requests for invalid input.
        assert!(
            calls
                .iter()
                .all(|(_, body)| body["chat_id"] == "42" && body.get("parse_mode").is_none())
        );
        assert!(calls[0].0.ends_with("sendMessage"));
        assert_eq!(calls[1].1["text"], "Ссылка\nhttps://example.com/page");
        assert!(calls[2].0.ends_with("sendPhoto"));
        assert_eq!(calls[2].1["photo"], "https://example.com/image.jpg");
        drop(calls);
        controller
            .update(ConfigUpdate {
                enabled: false,
                token: None,
                chat_id: "42".into(),
            })
            .await
            .unwrap();
        assert!(controller.send_text("disabled").await.is_err());
        assert_eq!(requests.lock().await.len(), 4);
        server.abort();
    }
}
