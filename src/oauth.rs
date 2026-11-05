use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::storage::Storage;
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
    storage: Storage,
    client: reqwest::Client,
    state: Mutex<State>,
    credentials: Mutex<()>,
    issuer: String,
}

#[derive(Default)]
struct State {
    pending: Option<Pending>,
    last_error: Option<String>,
}

struct Pending {
    id: uuid::Uuid,
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
    pub fn new(storage: Storage, directory: &std::path::Path) -> Result<Self, String> {
        storage.import_json::<Tokens>("oauth", &directory.join("auth.json"))?;
        Ok(Self(Arc::new(Inner {
            storage,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()
                .map_err(|e| format!("Не удалось создать HTTP-клиент: {e}"))?,
            state: Mutex::new(State::default()),
            credentials: Mutex::new(()),
            issuer: ISSUER.into(),
        })))
    }

    pub async fn refresh_loop(&self) {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            let _ = self.refresh_if_needed().await;
        }
    }

    async fn read_tokens(&self) -> Result<Option<Tokens>, String> {
        self.0.storage.get("oauth")
    }

    fn save_tokens(&self, tokens: Tokens) -> Result<(), String> {
        // Keep the write inside the credential lock, including on task cancellation.
        self.0.storage.set("oauth", &tokens)
    }

    async fn refresh_if_needed(&self) -> Result<(), String> {
        let _operation = self.0.credentials.lock().await;
        let result = self.refresh_tokens_locked().await;
        self.0.state.lock().await.last_error = result.as_ref().err().cloned();
        result
    }

    async fn refresh_tokens_locked(&self) -> Result<(), String> {
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
            .post(format!("{}/oauth/token", self.0.issuer))
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
        self.save_tokens(tokens)?;
        self.0.state.lock().await.last_error = None;
        Ok(())
    }

    /// Получить актуальный access-токен для следующих GPT-модулей приложения.
    #[allow(dead_code)]
    pub async fn access_token(&self) -> Result<String, String> {
        let _operation = self.0.credentials.lock().await;
        self.refresh_tokens_locked().await?;
        self.read_tokens()
            .await?
            .filter(|tokens| !tokens.access_token.is_empty())
            .map(|tokens| tokens.access_token)
            .ok_or_else(|| "ChatGPT ещё не авторизован".into())
    }

    /// Return the refreshed OAuth credentials needed to create a GPT-Live call.
    pub async fn live_credentials(&self) -> Result<(String, String), String> {
        let _operation = self.0.credentials.lock().await;
        self.refresh_tokens_locked().await?;
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
        let _operation = self.0.credentials.lock().await;
        let response = self
            .0
            .client
            .post(format!(
                "{}/api/accounts/deviceauth/usercode",
                self.0.issuer
            ))
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
            id: uuid::Uuid::new_v4(),
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
                id: pending.id,
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
        let _operation = self.0.credentials.lock().await;
        self.0.storage.delete("oauth")?;
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
                .is_some_and(|active| active.id == pending.id)
            {
                return;
            }
        }
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let response = inner.client.post(format!("{}/api/accounts/deviceauth/token", inner.issuer))
            .json(&serde_json::json!({"device_auth_id": device_auth_id, "user_code": pending.user_code}))
            .send().await;
        let _operation = inner.credentials.lock().await;
        if !inner
            .state
            .lock()
            .await
            .pending
            .as_ref()
            .is_some_and(|active| active.id == pending.id)
        {
            return;
        }
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                inner.state.lock().await.last_error =
                    Some(format!("Ошибка ожидания подтверждения: {error}"));
                continue;
            }
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
                .is_some_and(|active| active.id == pending.id)
            {
                return;
            }
        }
        let result = exchange_code(&inner, &grant).await;
        match result {
            Ok(tokens) => {
                if let Err(error) = inner.storage.set("oauth", &tokens) {
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
    let _operation = inner.credentials.lock().await;
    let mut state = inner.state.lock().await;
    if !state
        .pending
        .as_ref()
        .is_some_and(|active| active.id == pending.id)
    {
        return;
    }
    state.pending = None;
    if state.last_error.is_none() {
        state.last_error = Some("Время ожидания кода входа истекло. Начните вход ещё раз.".into());
    }
}

async fn exchange_code(inner: &Inner, grant: &DeviceGrant) -> Result<Tokens, String> {
    let response = inner
        .client
        .post(format!("{}/oauth/token", inner.issuer))
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, extract::State as HttpState, routing::post};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

    #[derive(Clone)]
    struct Mock {
        calls: Arc<AtomicUsize>,
        entered: Arc<Notify>,
        release: Arc<Notify>,
        block_device_poll: bool,
    }

    async fn token(HttpState(mock): HttpState<Mock>) -> Json<Value> {
        mock.calls.fetch_add(1, Ordering::SeqCst);
        if !mock.block_device_poll {
            mock.entered.notify_one();
            mock.release.notified().await;
        }
        Json(
            serde_json::json!({"access_token":"fresh-test-token","refresh_token":"rotated-test-token","expires_in":3600}),
        )
    }

    async fn device_token(HttpState(mock): HttpState<Mock>) -> Json<Value> {
        if mock.block_device_poll {
            mock.entered.notify_one();
            mock.release.notified().await;
        }
        Json(serde_json::json!({"authorization_code":"test-code","code_verifier":"test-verifier"}))
    }

    async fn setup(
        block_device_poll: bool,
    ) -> (OAuthController, Mock, tokio::task::JoinHandle<()>) {
        let mock = Mock {
            calls: Arc::new(AtomicUsize::new(0)),
            entered: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            block_device_poll,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/oauth/token", post(token))
            .route("/api/accounts/deviceauth/token", post(device_token))
            .with_state(mock.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let controller = OAuthController(Arc::new(Inner {
            storage: Storage::memory(),
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(3))
                .build()
                .unwrap(),
            state: Mutex::new(State::default()),
            credentials: Mutex::new(()),
            issuer: format!("http://{address}"),
        }));
        controller
            .save_tokens(Tokens {
                access_token: "old-test-token".into(),
                refresh_token: "old-test-refresh".into(),
                id_token: String::new(),
                account_id: "test-account".into(),
                expires_at: 0,
            })
            .unwrap();
        (controller, mock, server)
    }

    fn cleanup(_controller: OAuthController, server: tokio::task::JoinHandle<()>) {
        server.abort();
    }

    #[tokio::test]
    async fn concurrent_callers_refresh_rotating_token_only_once() {
        let (controller, mock, server) = setup(false).await;
        let mut tasks = Vec::new();
        for _ in 0..12 {
            let controller = controller.clone();
            tasks.push(tokio::spawn(
                async move { controller.live_credentials().await },
            ));
        }
        tokio::time::timeout(Duration::from_secs(3), mock.entered.notified())
            .await
            .unwrap();
        mock.release.notify_one();
        for task in tasks {
            let (token, account) = tokio::time::timeout(Duration::from_secs(4), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(token, "fresh-test-token");
            assert_eq!(account, "test-account");
        }
        assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
        cleanup(controller, server);
    }

    #[tokio::test]
    async fn logout_cannot_be_undone_by_an_in_flight_refresh() {
        let (controller, mock, server) = setup(false).await;
        let caller = controller.clone();
        let refresh = tokio::spawn(async move { caller.refresh_if_needed().await });
        tokio::time::timeout(Duration::from_secs(3), mock.entered.notified())
            .await
            .unwrap();
        let caller = controller.clone();
        let logout = tokio::spawn(async move { caller.logout().await });
        tokio::task::yield_now().await;
        mock.release.notify_one();
        refresh.await.unwrap().unwrap();
        logout.await.unwrap().unwrap();
        assert!(controller.read_tokens().await.unwrap().is_none());
        assert!(!controller.status().await.authorized);
        cleanup(controller, server);
    }

    #[tokio::test]
    async fn cancelled_device_flow_cannot_restore_tokens() {
        let (controller, mock, server) = setup(true).await;
        let id = uuid::Uuid::new_v4();
        controller.0.state.lock().await.pending = Some(Pending {
            id,
            user_code: "test-user-code".into(),
            expires_at: now() + 30,
        });
        let inner = Arc::clone(&controller.0);
        let poll = tokio::spawn(poll_device_flow(
            inner,
            "test-auth-id".into(),
            Pending {
                id,
                user_code: "test-user-code".into(),
                expires_at: now() + 30,
            },
            0,
        ));
        tokio::time::timeout(Duration::from_secs(3), mock.entered.notified())
            .await
            .unwrap();
        controller.logout().await.unwrap();
        mock.release.notify_one();
        tokio::time::timeout(Duration::from_secs(4), poll)
            .await
            .unwrap()
            .unwrap();
        assert!(controller.read_tokens().await.unwrap().is_none());
        assert_eq!(mock.calls.load(Ordering::SeqCst), 0);
        cleanup(controller, server);
    }
}
