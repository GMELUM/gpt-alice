use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{Json, extract::Extension, http::StatusCode, response::IntoResponse};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

const ISSUER: &str = "https://auth.openai.com";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const VERIFY_URL: &str = "https://auth.openai.com/codex/device";
const REFRESH_SKEW: Duration = Duration::from_secs(300);

#[derive(Clone)]
pub struct OAuthController(Arc<Inner>);

struct Inner {
    path: PathBuf,
    client: reqwest::Client,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    pending: Option<Pending>,
    last_error: Option<String>,
}

struct Pending {
    user_code: String,
    expires_at: u64,
}

#[derive(Clone, Serialize, Deserialize)]
struct Tokens {
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    id_token: String,
    #[serde(default)]
    account_id: String,
    expires_at: u64,
}

#[derive(Serialize)]
pub struct PublicStatus {
    authorized: bool,
    expires_at: Option<u64>,
    pending: bool,
    user_code: Option<String>,
    verification_url: Option<&'static str>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct DeviceStart {
    device_auth_id: String,
    user_code: String,
    #[serde(default = "default_interval")]
    interval: Value,
}

fn default_interval() -> Value {
    Value::from(5)
}

#[derive(Deserialize)]
struct DeviceGrant {
    authorization_code: String,
    code_verifier: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    id_token: String,
    #[serde(default)]
    expires_in: u64,
}

#[derive(Serialize)]
struct TokenRequest<'a> {
    grant_type: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    redirect_uri: Option<&'a str>,
    client_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    code_verifier: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    refresh_token: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<&'a str>,
}

impl OAuthController {
    pub fn new_near_executable() -> Result<Self, String> {
        let executable = std::env::current_exe()
            .map_err(|e| format!("Не удалось определить путь к программе: {e}"))?;
        let parent = executable.parent().ok_or("У программы нет каталога")?;
        Ok(Self(Arc::new(Inner {
            path: parent.join("auth.json"),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()
                .map_err(|e| format!("Не удалось создать HTTP-клиент: {e}"))?,
            state: Mutex::new(State::default()),
        })))
    }

    pub async fn refresh_loop(&self) {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            if let Err(error) = self.refresh_if_needed().await {
                self.0.state.lock().await.last_error = Some(error);
            }
        }
    }

    async fn read_tokens(&self) -> Result<Option<Tokens>, String> {
        let path = self.0.path.clone();
        tokio::task::spawn_blocking(move || match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| format!("Не удалось прочитать auth.json: {e}")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("Не удалось прочитать auth.json: {error}")),
        })
        .await
        .map_err(|e| format!("Ошибка файловой задачи: {e}"))?
    }

    async fn save_tokens(&self, tokens: Tokens) -> Result<(), String> {
        let path = self.0.path.clone();
        tokio::task::spawn_blocking(move || save_private_json(&path, &tokens))
            .await
            .map_err(|e| format!("Ошибка файловой задачи: {e}"))?
    }

    async fn refresh_if_needed(&self) -> Result<(), String> {
        let Some(mut tokens) = self.read_tokens().await? else {
            return Ok(());
        };
        if tokens.expires_at > now() + REFRESH_SKEW.as_secs() {
            return Ok(());
        }
        if tokens.refresh_token.is_empty() {
            return Err("Нет refresh-токена для обновления авторизации".into());
        }

        let response = self
            .0
            .client
            .post(format!("{ISSUER}/oauth/token"))
            .form(&TokenRequest {
                grant_type: "refresh_token",
                code: None,
                redirect_uri: None,
                client_id: CLIENT_ID,
                code_verifier: None,
                refresh_token: Some(&tokens.refresh_token),
                scope: Some("openid profile email offline_access"),
            })
            .send()
            .await
            .map_err(|e| format!("Не удалось обновить авторизацию: {e}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "Не удалось обновить авторизацию (HTTP {})",
                response.status()
            ));
        }
        let fresh: TokenResponse = response
            .json()
            .await
            .map_err(|e| format!("Некорректный ответ обновления авторизации: {e}"))?;
        if fresh.access_token.is_empty() {
            return Err("Сервер не вернул access-токен".into());
        }
        tokens.access_token = fresh.access_token;
        if !fresh.refresh_token.is_empty() {
            tokens.refresh_token = fresh.refresh_token;
        }
        if !fresh.id_token.is_empty() {
            tokens.id_token = fresh.id_token;
        }
        tokens.account_id = account_id(&tokens.id_token)
            .or_else(|| account_id(&tokens.access_token))
            .unwrap_or(tokens.account_id);
        tokens.expires_at =
            token_expiry(&tokens.access_token).unwrap_or_else(|| now() + fresh.expires_in);
        self.save_tokens(tokens).await?;
        self.0.state.lock().await.last_error = None;
        Ok(())
    }

    /// Получить актуальный access-токен для следующих GPT-модулей приложения.
    #[allow(dead_code)]
    pub async fn access_token(&self) -> Result<String, String> {
        self.refresh_if_needed().await?;
        self.read_tokens()
            .await?
            .filter(|tokens| !tokens.access_token.is_empty())
            .map(|tokens| tokens.access_token)
            .ok_or_else(|| "ChatGPT ещё не авторизован".into())
    }

    /// Return the refreshed OAuth credentials needed to create a GPT-Live call.
    pub async fn live_credentials(&self) -> Result<(String, String), String> {
        self.refresh_if_needed().await?;
        let tokens = self
            .read_tokens()
            .await?
            .filter(|tokens| !tokens.access_token.is_empty())
            .ok_or_else(|| "ChatGPT ещё не авторизован".to_owned())?;
        if tokens.account_id.is_empty() {
            return Err("В OAuth-токене не найден ChatGPT account ID".into());
        }
        Ok((tokens.access_token, tokens.account_id))
    }

    async fn status(&self) -> PublicStatus {
        let tokens = self.read_tokens().await.ok().flatten();
        let state = self.0.state.lock().await;
        let pending = state.pending.as_ref().filter(|p| p.expires_at > now());
        PublicStatus {
            authorized: tokens.as_ref().is_some_and(|t| !t.access_token.is_empty()),
            expires_at: tokens.map(|t| t.expires_at),
            pending: pending.is_some(),
            user_code: pending.map(|p| p.user_code.clone()),
            verification_url: pending.map(|_| VERIFY_URL),
            error: state.last_error.clone(),
        }
    }

    async fn start(&self) -> Result<(), String> {
        let response = self
            .0
            .client
            .post(format!("{ISSUER}/api/accounts/deviceauth/usercode"))
            .json(&serde_json::json!({"client_id": CLIENT_ID}))
            .send()
            .await
            .map_err(|e| format!("Не удалось начать вход: {e}"))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            let detail: String = body.chars().take(240).collect();
            return Err(if detail.trim().is_empty() {
                format!("Не удалось начать вход (HTTP {status})")
            } else {
                format!("Не удалось начать вход (HTTP {status}): {detail}")
            });
        }
        let started: DeviceStart = response
            .json()
            .await
            .map_err(|e| format!("Некорректный ответ входа: {e}"))?;
        if started.device_auth_id.is_empty() || started.user_code.is_empty() {
            return Err("Сервер вернул неполный код входа".into());
        }
        let state = Arc::clone(&self.0);
        let interval = interval_seconds(&started.interval).saturating_add(3).max(3);
        let pending = Pending {
            user_code: started.user_code,
            expires_at: now() + 900,
        };
        {
            let mut locked = state.state.lock().await;
            if locked
                .pending
                .as_ref()
                .is_some_and(|active| active.expires_at > now())
            {
                return Err(
                    "Вход уже ожидает подтверждения. Завершите его или дождитесь истечения кода."
                        .into(),
                );
            }
            locked.pending = Some(Pending {
                user_code: pending.user_code.clone(),
                expires_at: pending.expires_at,
            });
            locked.last_error = None;
        }
        tokio::spawn(async move {
            poll_device_flow(state, started.device_auth_id, pending, interval).await;
        });
        Ok(())
    }

    async fn logout(&self) -> Result<(), String> {
        let path = self.0.path.clone();
        tokio::task::spawn_blocking(move || match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("Не удалось удалить auth.json: {error}")),
        })
        .await
        .map_err(|e| format!("Ошибка файловой задачи: {e}"))??;
        let mut state = self.0.state.lock().await;
        state.pending = None;
        state.last_error = None;
        Ok(())
    }
}

async fn poll_device_flow(
    inner: Arc<Inner>,
    device_auth_id: String,
    pending: Pending,
    interval: u64,
) {
    while now() < pending.expires_at {
        {
            let state = inner.state.lock().await;
            if !state
                .pending
                .as_ref()
                .is_some_and(|active| active.user_code == pending.user_code)
            {
                return;
            }
        }
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let response = match inner.client.post(format!("{ISSUER}/api/accounts/deviceauth/token"))
            .json(&serde_json::json!({"device_auth_id": device_auth_id, "user_code": pending.user_code}))
            .send().await {
                Ok(response) => response,
                Err(error) => { inner.state.lock().await.last_error = Some(format!("Ошибка ожидания подтверждения: {error}")); continue; }
            };
        if response.status() == StatusCode::FORBIDDEN || response.status() == StatusCode::NOT_FOUND
        {
            continue;
        }
        if !response.status().is_success() {
            inner.state.lock().await.last_error = Some(format!(
                "Ошибка подтверждения входа (HTTP {})",
                response.status()
            ));
            break;
        }
        let grant: DeviceGrant = match response.json().await {
            Ok(grant) => grant,
            Err(error) => {
                inner.state.lock().await.last_error =
                    Some(format!("Некорректный ответ подтверждения: {error}"));
                break;
            }
        };
        {
            let state = inner.state.lock().await;
            if !state
                .pending
                .as_ref()
                .is_some_and(|active| active.user_code == pending.user_code)
            {
                return;
            }
        }
        let result = exchange_code(&inner, &grant).await;
        match result {
            Ok(tokens) => {
                if let Err(error) = save_private_json(&inner.path, &tokens) {
                    inner.state.lock().await.last_error = Some(error);
                } else {
                    inner.state.lock().await.last_error = None;
                }
            }
            Err(error) => inner.state.lock().await.last_error = Some(error),
        }
        inner.state.lock().await.pending = None;
        return;
    }
    let mut state = inner.state.lock().await;
    state.pending = None;
    if state.last_error.is_none() {
        state.last_error = Some("Время ожидания кода входа истекло. Начните вход ещё раз.".into());
    }
}

async fn exchange_code(inner: &Inner, grant: &DeviceGrant) -> Result<Tokens, String> {
    let response = inner
        .client
        .post(format!("{ISSUER}/oauth/token"))
        .form(&TokenRequest {
            grant_type: "authorization_code",
            code: Some(&grant.authorization_code),
            redirect_uri: Some("https://auth.openai.com/deviceauth/callback"),
            client_id: CLIENT_ID,
            code_verifier: Some(&grant.code_verifier),
            refresh_token: None,
            scope: None,
        })
        .send()
        .await
        .map_err(|e| format!("Не удалось завершить вход: {e}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "Обмен кода входа не выполнен (HTTP {})",
            response.status()
        ));
    }
    let result: TokenResponse = response
        .json()
        .await
        .map_err(|e| format!("Некорректный ответ токенов: {e}"))?;
    if result.access_token.is_empty() {
        return Err("Сервер не вернул access-токен".into());
    }
    let expires_at =
        token_expiry(&result.access_token).unwrap_or_else(|| now() + result.expires_in);
    Ok(Tokens {
        account_id: account_id(&result.id_token)
            .or_else(|| account_id(&result.access_token))
            .unwrap_or_default(),
        access_token: result.access_token,
        refresh_token: result.refresh_token,
        id_token: result.id_token,
        expires_at,
    })
}

fn save_private_json(path: &Path, tokens: &Tokens) -> Result<(), String> {
    let data = serde_json::to_vec_pretty(tokens)
        .map_err(|e| format!("Не удалось сериализовать токены: {e}"))?;
    let mut temporary = path.to_path_buf();
    temporary.set_extension(format!("json.tmp-{}", std::process::id()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|e| format!("Не удалось создать закрытый файл токенов: {e}"))?;
        file.write_all(&data)
            .map_err(|e| format!("Не удалось записать токены: {e}"))?;
        file.sync_all()
            .map_err(|e| format!("Не удалось сохранить токены на диск: {e}"))?;
        fs::rename(&temporary, path).map_err(|e| format!("Не удалось заменить auth.json: {e}"))?;
        #[cfg(unix)]
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("Не удалось закрыть права auth.json: {e}"))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

fn interval_seconds(value: &Value) -> u64 {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(5)
        .clamp(1, 60)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()
}

fn token_expiry(token: &str) -> Option<u64> {
    jwt_claims(token)?.get("exp")?.as_u64()
}

fn account_id(token: &str) -> Option<String> {
    jwt_claims(token)?
        .get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()
        .map(str::to_owned)
}

pub async fn get_status(Extension(controller): Extension<OAuthController>) -> Json<PublicStatus> {
    Json(controller.status().await)
}

pub async fn begin(Extension(controller): Extension<OAuthController>) -> impl IntoResponse {
    match controller.start().await {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({"ok": true, "message": "Код входа готов"})),
        )
            .into_response(),
        Err(error) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"ok": false, "message": error})),
        )
            .into_response(),
    }
}

pub async fn logout(Extension(controller): Extension<OAuthController>) -> impl IntoResponse {
    match controller.logout().await {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({"ok": true, "message": "Авторизация удалена"})),
        )
            .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"ok": false, "message": error})),
        )
            .into_response(),
    }
}
