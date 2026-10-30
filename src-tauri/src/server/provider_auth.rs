use super::error::{ApiError, ApiResult};
use super::{AppState, ScopedState};
use axum::extract::{Extension, Json};
use axum::routing::{get, post};
use axum::Router;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;
use url::Url;

const CODEX_ISSUER: &str = "https://auth.openai.com";
const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_SCOPE: &str =
    "openid profile email offline_access api.connectors.read api.connectors.invoke";
const CODEX_ORIGINATOR: &str = "codex_cli_rs";
const CODEX_CALLBACK_PORTS: [u16; 2] = [1455, 1457];
const CODEX_CALLBACK_PATH: &str = "/auth/callback";
const CODEX_LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
const CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CODEX_REVOKE_URL: &str = "https://auth.openai.com/oauth/revoke";
const CODEX_MODELS_URL: &str = "https://chatgpt.com/backend-api/codex/models";
const CODEX_CLIENT_VERSION: &str = "0.153.4";
const CODEX_PROFILE_ID: &str = "llm-profile-openai-codex-managed";
const CODEX_MANAGED_BY: &str = "openai_codex_oauth";
const MAX_CATALOG_BYTES: usize = 8 * 1024 * 1024;
const MAX_MODELS: usize = 512;

#[derive(Clone)]
pub(super) struct ProviderAuthManager {
    inner: Arc<Mutex<AuthInner>>,
    store_lock: Arc<Mutex<()>>,
}

struct AuthInner {
    codex: CodexRuntime,
    codebuddy: CodeBuddyRuntime,
}

impl ProviderAuthManager {
    pub(super) fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(AuthInner {
                codex: CodexRuntime::default(),
                codebuddy: CodeBuddyRuntime::default(),
            })),
            store_lock: Arc::new(Mutex::new(())),
        }
    }

    pub(super) async fn codex_status(&self, state: &AppState) -> Result<Value, String> {
        let inner = self.inner.lock().await;
        codex_status_value(state, &inner.codex)
    }

    pub(super) async fn start_codex_login(&self, state: AppState) -> ApiResult<Value> {
        let mut inner = self.inner.lock().await;
        if codex_operation_active(&inner.codex) {
            return Ok(codex_start_payload(
                inner
                    .codex
                    .operation
                    .as_ref()
                    .map(|runtime| &runtime.operation)
                    .ok_or_else(|| ApiError::conflict("Codex 登录状态损坏。"))?,
            ));
        }

        let listener = bind_codex_callback().await.map_err(|message| {
            ApiError::new(
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "callback_unavailable",
                message,
                true,
            )
        })?;
        let port = listener
            .local_addr()
            .map_err(|error| ApiError::internal(format!("无法读取 Codex 回调端口：{error}")))?
            .port();
        let state_secret = random_urlsafe(32);
        let verifier = random_urlsafe(64);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let redirect_uri = format!("http://localhost:{port}{CODEX_CALLBACK_PATH}");
        let authorize_url = build_codex_authorize_url(&redirect_uri, &state_secret, &challenge)
            .map_err(ApiError::internal)?;
        let operation = CodexOperation {
            operation_id: random_urlsafe(24),
            state_secret,
            redirect_uri,
            authorize_url,
            callback_port: port,
            callback_forward_port: 3782,
            deadline: Instant::now() + CODEX_LOGIN_TIMEOUT,
            operation_state: "waiting".into(),
            error_code: None,
            activated: false,
            cancel: CancellationToken::new(),
        };
        let payload = codex_start_payload(&operation);
        let expected_generation = codex_store(&state).generation().unwrap_or_default();
        let (callback_tx, callback_rx) = mpsc::channel(1);
        inner.codex.operation = Some(CodexOperationRuntime {
            operation: operation.clone(),
            callback_tx,
        });
        drop(inner);

        let manager = self.clone();
        let task_state = state.clone();
        let task_operation = operation.clone();
        let code_verifier = verifier;
        tokio::spawn(async move {
            manager
                .run_codex_login(
                    task_state,
                    task_operation,
                    listener,
                    callback_rx,
                    code_verifier,
                    expected_generation,
                )
                .await;
        });
        Ok(payload)
    }

    pub(super) async fn complete_codex_callback(
        &self,
        state: &AppState,
        callback_url: &str,
    ) -> ApiResult<Value> {
        let inner = self.inner.lock().await;
        let runtime = inner
            .codex
            .operation
            .as_ref()
            .filter(|runtime| codex_operation_active(&inner.codex))
            .ok_or_else(|| {
                ApiError::new(
                    axum::http::StatusCode::CONFLICT,
                    "login_not_active",
                    "Codex 登录当前不在等待回调。",
                    false,
                )
            })?;
        let callback = parse_codex_callback_url(callback_url, &runtime.operation.redirect_uri)
            .map_err(|message| ApiError::bad_request_with_code("callback_url_invalid", message))?;
        runtime
            .callback_tx
            .try_send(callback)
            .map_err(|_| ApiError::conflict("Codex 登录回调已经提交。"))?;
        drop(inner);
        self.codex_status(state)
            .await
            .map_err(|error| ApiError::internal(format!("无法读取 Codex 登录状态：{error}")))
    }

    pub(super) async fn cancel_codex_login(&self, state: &AppState) -> Result<Value, String> {
        let mut inner = self.inner.lock().await;
        if let Some(runtime) = inner.codex.operation.as_mut() {
            runtime.operation.cancel.cancel();
            runtime.operation.operation_state = "cancelled".into();
            runtime.operation.error_code = Some("login_cancelled".into());
            runtime.operation.authorize_url.clear();
        }
        codex_status_value(state, &inner.codex)
    }

    pub(super) async fn logout_codex(&self, state: &AppState) -> ApiResult<Value> {
        self.cancel_codex_login(state)
            .await
            .map_err(ApiError::internal)?;
        let store = codex_store(state);
        let _store_guard = self.store_lock.lock().await;
        let credentials = store.load_credentials().map_err(ApiError::internal)?;
        if let Some(credentials) = credentials.as_ref() {
            let token = if credentials.refresh_token.is_empty() {
                credentials.access_token.as_str()
            } else {
                credentials.refresh_token.as_str()
            };
            let hint = if credentials.refresh_token.is_empty() {
                "access_token"
            } else {
                "refresh_token"
            };
            let _ = state
                .client
                .post(CODEX_REVOKE_URL)
                .form(&[
                    ("client_id", CODEX_CLIENT_ID),
                    ("token", token),
                    ("token_type_hint", hint),
                ])
                .timeout(Duration::from_secs(15))
                .send()
                .await;
        }
        store.clear().map_err(ApiError::internal)?;
        remove_codex_catalog(state).map_err(ApiError::internal)?;
        let mut inner = self.inner.lock().await;
        inner.codex.operation = None;
        codex_status_value(state, &inner.codex).map_err(ApiError::internal)
    }

    pub(super) async fn refresh_codex_models(&self, state: &AppState) -> ApiResult<Value> {
        let _store_guard = self.store_lock.lock().await;
        let store = codex_store(state);
        let mut credentials = store
            .load_credentials()
            .map_err(ApiError::internal)?
            .ok_or_else(|| {
                ApiError::new(
                    axum::http::StatusCode::UNAUTHORIZED,
                    "authentication_required",
                    "请先登录 OpenAI Codex。",
                    false,
                )
            })?;
        if credentials.expires_at <= unix_now() + 300 {
            credentials = refresh_codex_credentials(&state.client, &store, credentials)
                .await
                .map_err(codex_failure_to_api)?;
        }
        let (models, etag) = match fetch_codex_models(&state.client, &credentials).await {
            Ok(result) => result,
            Err(CodexFailure::Unauthorized) => {
                credentials = refresh_codex_credentials(&state.client, &store, credentials)
                    .await
                    .map_err(codex_failure_to_api)?;
                fetch_codex_models(&state.client, &credentials)
                    .await
                    .map_err(codex_failure_to_api)?
            }
            Err(error) => return Err(codex_failure_to_api(error)),
        };
        let mut cache = codex_cache(&credentials, &models);
        cache.etag = etag;
        store.save_cache(&cache).map_err(ApiError::internal)?;
        sync_codex_catalog(state, &credentials, &models).map_err(ApiError::internal)?;
        drop(_store_guard);
        let inner = self.inner.lock().await;
        let mut status = codex_status_value(state, &inner.codex).map_err(ApiError::internal)?;
        status["activated"] = Value::Bool(false);
        Ok(status)
    }

    pub(super) async fn set_codex_reasoning_effort(
        &self,
        state: &AppState,
        model: &str,
        effort: Option<&str>,
    ) -> ApiResult<Value> {
        let model = model.trim();
        if model.is_empty() {
            return Err(ApiError::bad_request("缺少 Codex 模型标识。"));
        }
        let mut catalog = state
            .database
            .get_setting("catalog")
            .map_err(ApiError::internal)?
            .unwrap_or_else(|| json!({}));
        let profile = codex_profile_mut(&mut catalog).ok_or_else(|| {
            ApiError::new(
                axum::http::StatusCode::CONFLICT,
                "codex_catalog_unavailable",
                "请先登录并刷新 Codex 模型目录。",
                false,
            )
        })?;
        let Some(models) = profile.get_mut("models").and_then(Value::as_array_mut) else {
            return Err(ApiError::conflict("Codex 模型目录损坏。"));
        };
        for item in models {
            if item.get("model").and_then(Value::as_str) != Some(model) {
                continue;
            }
            let supported = item
                .get("codex_supported_reasoning_levels")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if effort
                .is_some_and(|effort| !supported.iter().any(|value| value.as_str() == Some(effort)))
            {
                return Err(ApiError::new(
                    axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                    "reasoning_effort_unsupported",
                    "所选 Codex 模型不支持这个推理强度。",
                    false,
                ));
            }
            if let Some(effort) = effort {
                item["reasoning_effort"] = Value::String(effort.to_string());
            } else if let Some(object) = item.as_object_mut() {
                object.remove("reasoning_effort");
            }
            state
                .database
                .set_setting("catalog", &catalog)
                .map_err(ApiError::internal)?;
            let inner = self.inner.lock().await;
            return codex_status_value(state, &inner.codex).map_err(ApiError::internal);
        }
        Err(ApiError::new(
            axum::http::StatusCode::NOT_FOUND,
            "codex_model_not_found",
            "这个模型不属于当前 Codex 账户。",
            false,
        ))
    }

    pub(super) async fn ensure_codex_access_token(
        &self,
        state: &AppState,
    ) -> Result<CodexAccess, String> {
        let _store_guard = self.store_lock.lock().await;
        let store = codex_store(state);
        let mut credentials = store
            .load_credentials()?
            .ok_or_else(|| "OpenAI Codex 尚未登录。请在设置 → 服务商中登录。".to_string())?;
        if store.authentication_rejected(credentials.generation)? {
            return Err("OpenAI Codex 登录已失效，请重新登录。".to_string());
        }
        if credentials.expires_at <= unix_now() + 300 {
            credentials = refresh_codex_credentials(&state.client, &store, credentials)
                .await
                .map_err(|failure| failure.public_message().to_string())?;
        }
        Ok(CodexAccess {
            access_token: credentials.access_token,
            account_id: credentials.account_id,
            generation: credentials.generation,
        })
    }

    pub(super) async fn recover_codex_unauthorized(
        &self,
        state: &AppState,
        generation: u64,
    ) -> Result<CodexAccess, String> {
        let _store_guard = self.store_lock.lock().await;
        let store = codex_store(state);
        let credentials = store
            .load_credentials()?
            .ok_or_else(|| "OpenAI Codex 尚未登录。请在设置 → 服务商中登录。".to_string())?;
        if credentials.generation != generation {
            return self.ensure_codex_access_token(state).await;
        }
        if store.authentication_rejected(generation)? {
            return Err("OpenAI Codex 登录已失效，请重新登录。".to_string());
        }
        let refreshed = refresh_codex_credentials(&state.client, &store, credentials)
            .await
            .map_err(|failure| failure.public_message().to_string())?;
        Ok(CodexAccess {
            access_token: refreshed.access_token,
            account_id: refreshed.account_id,
            generation: refreshed.generation,
        })
    }

    async fn run_codex_login(
        &self,
        state: AppState,
        operation: CodexOperation,
        listener: TcpListener,
        mut callback_rx: mpsc::Receiver<CodexCallback>,
        verifier: String,
        expected_generation: u64,
    ) {
        let deadline = tokio::time::Instant::from_std(operation.deadline);
        let callback = tokio::select! {
            _ = operation.cancel.cancelled() => {
                self.set_codex_state(
                    &operation.operation_id,
                    "cancelled",
                    Some("login_cancelled"),
                    false,
                ).await;
                return;
            }
            _ = tokio::time::sleep_until(deadline) => {
                self.set_codex_state(
                    &operation.operation_id,
                    "expired",
                    Some("login_timeout"),
                    false,
                ).await;
                return;
            }
            value = wait_for_codex_callback(&listener, &operation.state_secret) => value,
            value = callback_rx.recv() => match value {
                Some(value) => Ok(value),
                None => Err(CodexFailure::CallbackChanged),
            },
        };
        let callback = match callback {
            Ok(callback) => callback,
            Err(error) => {
                self.set_codex_state(
                    &operation.operation_id,
                    if matches!(error, CodexFailure::Cancelled) {
                        "cancelled"
                    } else if matches!(error, CodexFailure::Timeout) {
                        "expired"
                    } else {
                        "failed"
                    },
                    Some(error.code()),
                    false,
                )
                .await;
                return;
            }
        };
        if callback.error.is_some() {
            self.set_codex_state(
                &operation.operation_id,
                "failed",
                Some("authorization_denied"),
                false,
            )
            .await;
            return;
        }
        if !constant_time_eq(callback.state.as_deref(), Some(&operation.state_secret)) {
            self.set_codex_state(
                &operation.operation_id,
                "failed",
                Some("state_mismatch"),
                false,
            )
            .await;
            return;
        }
        let Some(code) = callback.code.filter(|value| !value.is_empty()) else {
            self.set_codex_state(
                &operation.operation_id,
                "failed",
                Some("authorization_code_missing"),
                false,
            )
            .await;
            return;
        };

        self.set_codex_state(&operation.operation_id, "exchanging", None, false)
            .await;
        let store = codex_store(&state);
        let credentials = {
            let _store_guard = self.store_lock.lock().await;
            let result = async {
                let response = state
                    .client
                    .post(CODEX_TOKEN_URL)
                    .form(&[
                        ("client_id", CODEX_CLIENT_ID),
                        ("grant_type", "authorization_code"),
                        ("code", code.as_str()),
                        ("redirect_uri", operation.redirect_uri.as_str()),
                        ("code_verifier", verifier.as_str()),
                    ])
                    .timeout(Duration::from_secs(30))
                    .send()
                    .await
                    .map_err(|_| CodexFailure::TokenExchange)?;
                let response = checked_json_response(response, CodexFailure::TokenExchange).await?;
                let credentials =
                    credentials_from_token_payload(&response, expected_generation, None)?;
                store
                    .commit_credentials(&credentials, expected_generation)
                    .map_err(CodexFailure::Storage)
            }
            .await;
            result
        };
        let mut credentials = match credentials {
            Ok(credentials) => credentials,
            Err(error) => {
                self.set_codex_state(&operation.operation_id, "failed", Some(error.code()), false)
                    .await;
                return;
            }
        };
        self.set_codex_state(&operation.operation_id, "fetching_models", None, false)
            .await;
        let models = match fetch_codex_models(&state.client, &credentials).await {
            Ok((models, etag)) => {
                let mut cache = codex_cache(&credentials, &models);
                cache.etag = etag;
                if let Err(error) = store.save_cache(&cache) {
                    self.set_codex_state(
                        &operation.operation_id,
                        "failed",
                        Some("catalog_storage_failed"),
                        false,
                    )
                    .await;
                    tracing::warn!(%error, "Codex 模型缓存写入失败");
                    return;
                }
                models
            }
            Err(CodexFailure::Unauthorized) => {
                let refresh_result = {
                    let _store_guard = self.store_lock.lock().await;
                    refresh_codex_credentials(&state.client, &store, credentials).await
                };
                match refresh_result {
                    Ok(refreshed) => {
                        credentials = refreshed;
                        match fetch_codex_models(&state.client, &credentials).await {
                            Ok((models, etag)) => {
                                let mut cache = codex_cache(&credentials, &models);
                                cache.etag = etag;
                                if let Err(error) = store.save_cache(&cache) {
                                    tracing::warn!(%error, "Codex 模型缓存写入失败");
                                    self.set_codex_state(
                                        &operation.operation_id,
                                        "failed",
                                        Some("catalog_storage_failed"),
                                        false,
                                    )
                                    .await;
                                    return;
                                }
                                models
                            }
                            Err(error) => {
                                self.set_codex_state(
                                    &operation.operation_id,
                                    "failed",
                                    Some(error.code()),
                                    false,
                                )
                                .await;
                                return;
                            }
                        }
                    }
                    Err(error) => {
                        self.set_codex_state(
                            &operation.operation_id,
                            "failed",
                            Some(error.code()),
                            false,
                        )
                        .await;
                        return;
                    }
                }
            }
            Err(error) => {
                self.set_codex_state(&operation.operation_id, "failed", Some(error.code()), false)
                    .await;
                return;
            }
        };
        let activated = match sync_codex_catalog(&state, &credentials, &models) {
            Ok(activated) => activated,
            Err(error) => {
                tracing::warn!(%error, "Codex 模型目录同步失败");
                self.set_codex_state(
                    &operation.operation_id,
                    "failed",
                    Some("catalog_storage_failed"),
                    false,
                )
                .await;
                return;
            }
        };
        self.set_codex_state(&operation.operation_id, "completed", None, activated)
            .await;
    }

    async fn set_codex_state(
        &self,
        operation_id: &str,
        state: &str,
        error_code: Option<&str>,
        activated: bool,
    ) {
        let mut inner = self.inner.lock().await;
        if let Some(runtime) = inner.codex.operation.as_mut() {
            if runtime.operation.operation_id != operation_id {
                return;
            }
            runtime.operation.operation_state = state.to_string();
            runtime.operation.error_code = error_code.map(str::to_string);
            runtime.operation.activated = activated;
            if state != "waiting" && state != "exchanging" && state != "fetching_models" {
                runtime.operation.authorize_url.clear();
            }
        }
    }

    pub(super) async fn codebuddy_status(&self, state: &AppState) -> Value {
        let credentials = load_codebuddy_credentials(&state.paths);
        let Some(credentials) = credentials else {
            let mut inner = self.inner.lock().await;
            if inner.codebuddy.operation_state.as_deref() == Some("waiting") {
                return codebuddy_status_value(&inner.codebuddy);
            }
            inner.codebuddy = CodeBuddyRuntime::default();
            return codebuddy_status_value(&inner.codebuddy);
        };
        let label = probe_codebuddy_account(&state.client, &credentials).await;
        let mut inner = self.inner.lock().await;
        match label {
            Some(label) => {
                inner.codebuddy.connection = "connected".into();
                inner.codebuddy.operation_state = Some("completed".into());
                inner.codebuddy.user_label = Some(label);
                inner.codebuddy.error_code = None;
                inner.codebuddy.authorize_url = None;
            }
            None => {
                inner.codebuddy.connection = "error".into();
                inner.codebuddy.operation_state = Some("failed".into());
                inner.codebuddy.user_label = None;
                inner.codebuddy.error_code = Some("auth_failed".into());
            }
        }
        codebuddy_status_value(&inner.codebuddy)
    }

    pub(super) async fn start_codebuddy_login(&self, state: &AppState) -> Value {
        let current = self.codebuddy_status(state).await;
        if current.get("connection").and_then(Value::as_str) == Some("connected") {
            return current;
        }
        let mut inner = self.inner.lock().await;
        inner.codebuddy.connection = "error".into();
        inner.codebuddy.operation_state = Some("failed".into());
        inner.codebuddy.authorize_url = None;
        inner.codebuddy.user_label = None;
        inner.codebuddy.error_code = Some("sdk_missing".into());
        codebuddy_status_value(&inner.codebuddy)
    }

    pub(super) async fn cancel_codebuddy_login(&self) -> Value {
        let mut inner = self.inner.lock().await;
        inner.codebuddy.connection = "disconnected".into();
        inner.codebuddy.operation_state = Some("cancelled".into());
        inner.codebuddy.authorize_url = None;
        inner.codebuddy.error_code = None;
        codebuddy_status_value(&inner.codebuddy)
    }

    pub(super) async fn logout_codebuddy(&self, state: &AppState) -> Value {
        let mut inner = self.inner.lock().await;
        if load_codebuddy_credentials(&state.paths).is_some() {
            inner.codebuddy.connection = "connected".into();
            inner.codebuddy.operation_state = Some("failed".into());
            inner.codebuddy.authorize_url = None;
            inner.codebuddy.error_code = Some("logout_external".into());
        } else {
            inner.codebuddy = CodeBuddyRuntime::default();
        }
        codebuddy_status_value(&inner.codebuddy)
    }
}

#[derive(Clone, Debug)]
pub(super) struct CodexAccess {
    pub access_token: String,
    pub account_id: String,
    pub generation: u64,
}

#[derive(Default)]
struct CodexRuntime {
    operation: Option<CodexOperationRuntime>,
}

struct CodexOperationRuntime {
    operation: CodexOperation,
    callback_tx: mpsc::Sender<CodexCallback>,
}

#[derive(Clone)]
struct CodexOperation {
    operation_id: String,
    state_secret: String,
    redirect_uri: String,
    authorize_url: String,
    callback_port: u16,
    callback_forward_port: u16,
    deadline: Instant,
    operation_state: String,
    error_code: Option<String>,
    activated: bool,
    cancel: CancellationToken,
}

fn codex_operation_active(runtime: &CodexRuntime) -> bool {
    runtime.operation.as_ref().is_some_and(|runtime| {
        matches!(
            runtime.operation.operation_state.as_str(),
            "waiting" | "exchanging" | "fetching_models"
        ) && runtime.operation.deadline > Instant::now()
            && !runtime.operation.cancel.is_cancelled()
    })
}

fn codex_start_payload(operation: &CodexOperation) -> Value {
    let expires_in = operation
        .deadline
        .saturating_duration_since(Instant::now())
        .as_secs();
    json!({
        "operation_id": operation.operation_id,
        "authorize_url": operation.authorize_url,
        "expires_in": expires_in,
        "callback_port": operation.callback_port,
        "callback_forward_port": operation.callback_forward_port,
        "redirect_uri": operation.redirect_uri,
        "ssh_forward_command": format!(
            "ssh -N -L {}:127.0.0.1:{} <ssh-user>@<server-host>",
            operation.callback_port, operation.callback_forward_port
        )
    })
}

fn codex_status_value(state: &AppState, runtime: &CodexRuntime) -> Result<Value, String> {
    let store = codex_store(state);
    let credentials_result = store.load_credentials();
    let (credentials, storage_error) = match credentials_result {
        Ok(credentials) => {
            let rejected = credentials
                .as_ref()
                .map(|credentials| store.authentication_rejected(credentials.generation))
                .transpose()?
                .unwrap_or(false);
            (
                credentials,
                rejected.then_some("authentication_required".to_string()),
            )
        }
        Err(error) => (None, Some(error)),
    };
    let active = codex_operation_active(runtime);
    let operation = runtime.operation.as_ref().map(|runtime| &runtime.operation);
    let connection = if active {
        "authorizing"
    } else if storage_error.is_some() {
        "error"
    } else if credentials.is_some() {
        "connected"
    } else if operation.is_some_and(|operation| operation.operation_state == "failed") {
        "error"
    } else {
        "disconnected"
    };
    let cache = credentials
        .as_ref()
        .and_then(|credentials| matching_codex_cache(&store, credentials).ok().flatten());
    let catalog = state
        .database
        .get_setting("catalog")?
        .unwrap_or_else(|| json!({}));
    let profile = codex_profile(&catalog);
    let models = profile.map(codex_reasoning_models).unwrap_or_default();
    let active_model = active_codex_model(&catalog);
    Ok(json!({
        "connection": connection,
        "operation_id": operation.map(|operation| operation.operation_id.clone()),
        "operation_state": operation.map(|operation| operation.operation_state.clone()),
        "authorize_url": operation
            .filter(|_| active)
            .map(|operation| operation.authorize_url.clone())
            .filter(|value| !value.is_empty()),
        "expires_in": operation.filter(|_| active).map(|operation| {
            operation.deadline.saturating_duration_since(Instant::now()).as_secs()
        }),
        "callback_port": operation.map(|operation| operation.callback_port),
        "callback_forward_port": operation.map(|operation| operation.callback_forward_port),
        "redirect_uri": operation.map(|operation| operation.redirect_uri.clone()),
        "model_count": models.len(),
        "catalog_source": cache.as_ref().map(|cache| cache.source.clone()),
        "catalog_fetched_at": cache.as_ref().map(|cache| cache.fetched_at),
        "active_model": active_model,
        "models": models,
        "activated": operation.is_some_and(|operation| operation.activated),
        "error_code": storage_error.or_else(|| {
            operation.and_then(|operation| operation.error_code.clone())
        })
    }))
}

fn active_codex_model(catalog: &Value) -> Option<String> {
    let service = catalog.pointer("/services/llm")?;
    if service.get("active_profile_id").and_then(Value::as_str) != Some(CODEX_PROFILE_ID) {
        return None;
    }
    let active_model_id = service.get("active_model_id").and_then(Value::as_str)?;
    codex_profile(catalog)?
        .get("models")?
        .as_array()?
        .iter()
        .find(|model| model.get("id").and_then(Value::as_str) == Some(active_model_id))
        .and_then(|model| model.get("model"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn codex_reasoning_models(profile: &Value) -> Vec<Value> {
    profile
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|model| {
            let slug = model.get("model").and_then(Value::as_str)?;
            Some(json!({
                "model": slug,
                "name": model.get("name").and_then(Value::as_str).unwrap_or(slug),
                "supported_reasoning_levels": model
                    .get("codex_supported_reasoning_levels")
                    .cloned()
                    .unwrap_or_else(|| json!([])),
                "reasoning_effort": model.get("reasoning_effort")
            }))
        })
        .collect()
}

fn codex_profile(catalog: &Value) -> Option<&Value> {
    catalog
        .pointer("/services/llm/profiles")?
        .as_array()?
        .iter()
        .find(|profile| profile.get("managed_by").and_then(Value::as_str) == Some(CODEX_MANAGED_BY))
}

fn codex_profile_mut(catalog: &mut Value) -> Option<&mut Value> {
    catalog
        .pointer_mut("/services/llm/profiles")?
        .as_array_mut()?
        .iter_mut()
        .find(|profile| profile.get("managed_by").and_then(Value::as_str) == Some(CODEX_MANAGED_BY))
}

fn remove_codex_catalog(state: &AppState) -> Result<(), String> {
    let mut catalog = state
        .database
        .get_setting("catalog")?
        .unwrap_or_else(|| json!({}));
    let Some(llm) = catalog
        .pointer_mut("/services/llm")
        .and_then(Value::as_object_mut)
    else {
        return Ok(());
    };
    if llm.get("active_profile_id").and_then(Value::as_str) == Some(CODEX_PROFILE_ID) {
        llm.insert("active_profile_id".into(), Value::Null);
        llm.insert("active_model_id".into(), Value::Null);
    }
    if let Some(profiles) = llm.get_mut("profiles").and_then(Value::as_array_mut) {
        profiles.retain(|profile| {
            profile.get("managed_by").and_then(Value::as_str) != Some(CODEX_MANAGED_BY)
        });
    }
    state.database.set_setting("catalog", &catalog)
}

fn sync_codex_catalog(
    state: &AppState,
    credentials: &CodexCredentials,
    models: &[CodexModel],
) -> Result<bool, String> {
    if models.is_empty() {
        return Err("Codex 模型目录为空。".into());
    }
    let mut catalog = state
        .database
        .get_setting("catalog")?
        .unwrap_or_else(|| json!({}));
    if !catalog.get("services").is_some_and(Value::is_object) {
        catalog["services"] = json!({});
    }
    if !catalog
        .pointer("/services/llm")
        .is_some_and(Value::is_object)
    {
        catalog["services"]["llm"] =
            json!({"profiles": [], "active_profile_id": null, "active_model_id": null});
    }
    let account_binding = sha256_hex(&credentials.account_id);
    let existing = codex_profile(&catalog).cloned();
    let same_account = existing
        .as_ref()
        .and_then(|profile| profile.get("codex_account_binding"))
        .and_then(Value::as_str)
        == Some(account_binding.as_str());
    let efforts = if same_account {
        existing
            .as_ref()
            .and_then(|profile| profile.get("models"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|model| {
                Some((
                    model.get("model")?.as_str()?.to_string(),
                    model.get("reasoning_effort")?.as_str()?.to_string(),
                ))
            })
            .collect::<BTreeMap<_, _>>()
    } else {
        BTreeMap::new()
    };
    let managed_models = models
        .iter()
        .map(|model| {
            let mut item = json!({
                "id": codex_model_id(&model.slug),
                "name": model.display_name,
                "model": model.slug,
                "managed_by": CODEX_MANAGED_BY,
                "codex_priority": model.priority,
                "codex_default_reasoning_level": model.default_reasoning_level,
                "codex_supported_reasoning_levels": model.supported_reasoning_levels,
                "codex_supports_reasoning_summary": model.supports_reasoning_summary,
                "codex_supports_parallel_tool_calls": model.supports_parallel_tool_calls,
                "codex_use_responses_lite": model.use_responses_lite
            });
            if let Some(effort) = efforts.get(&model.slug) {
                item["reasoning_effort"] = Value::String(effort.clone());
            }
            if let Some(context) = model.context_window.or(model.max_context_window) {
                item["context_window"] = Value::String(context.to_string());
                item["context_window_source"] = Value::String("metadata".into());
            }
            item
        })
        .collect::<Vec<_>>();
    let profile = json!({
        "id": CODEX_PROFILE_ID,
        "name": "OpenAI Codex",
        "binding": "openai_codex",
        "base_url": "https://chatgpt.com/backend-api",
        "api_key": "",
        "api_version": "",
        "api_format": "openai_codex",
        "wire_api": "responses",
        "extra_headers": {},
        "managed_by": CODEX_MANAGED_BY,
        "read_only": true,
        "owner_bound": true,
        "codex_account_binding": account_binding,
        "models": managed_models
    });
    let first_model_id = profile
        .get("models")
        .and_then(Value::as_array)
        .and_then(|models| models.first())
        .and_then(|model| model.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let active_profile = catalog
        .pointer("/services/llm/active_profile_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let active_model = catalog
        .pointer("/services/llm/active_model_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let active_model_resolvable = active_model.as_deref().is_some_and(|id| {
        profile
            .get("models")
            .and_then(Value::as_array)
            .is_some_and(|models| {
                models
                    .iter()
                    .any(|model| model.get("id").and_then(Value::as_str) == Some(id))
            })
    });
    let llm = catalog
        .pointer_mut("/services/llm")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| "语言模型目录结构损坏。".to_string())?;
    let profiles = llm
        .entry("profiles")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(|| "语言模型配置列表损坏。".to_string())?;
    let insert_at = profiles
        .iter()
        .position(|item| item.get("managed_by").and_then(Value::as_str) == Some(CODEX_MANAGED_BY))
        .unwrap_or(profiles.len());
    profiles
        .retain(|item| item.get("managed_by").and_then(Value::as_str) != Some(CODEX_MANAGED_BY));
    profiles.insert(insert_at.min(profiles.len()), profile);
    let mut activated = false;
    if active_profile.is_none() {
        llm.insert(
            "active_profile_id".into(),
            Value::String(CODEX_PROFILE_ID.into()),
        );
        llm.insert(
            "active_model_id".into(),
            first_model_id.map(Value::String).unwrap_or(Value::Null),
        );
        activated = true;
    } else if active_profile.as_deref() == Some(CODEX_PROFILE_ID) {
        if !active_model_resolvable {
            llm.insert(
                "active_model_id".into(),
                first_model_id.map(Value::String).unwrap_or(Value::Null),
            );
        }
    }
    state.database.set_setting("catalog", &catalog)?;
    Ok(activated)
}

fn codex_model_id(slug: &str) -> String {
    format!("llm-model-openai-codex-{}", &sha256_hex(slug)[..16])
}

async fn bind_codex_callback() -> Result<TcpListener, String> {
    for port in CODEX_CALLBACK_PORTS {
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", port)).await {
            return Ok(listener);
        }
    }
    Err("无法监听 localhost:1455 或 localhost:1457，请关闭占用端口的程序后重试。".into())
}

fn build_codex_authorize_url(
    redirect_uri: &str,
    state: &str,
    challenge: &str,
) -> Result<String, String> {
    let mut url = Url::parse(&format!("{CODEX_ISSUER}/oauth/authorize"))
        .map_err(|error| format!("无法生成 Codex 授权地址：{error}"))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CODEX_CLIENT_ID)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", CODEX_SCOPE)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("originator", CODEX_ORIGINATOR)
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true");
    Ok(url.to_string())
}

async fn wait_for_codex_callback(
    listener: &TcpListener,
    expected_state: &str,
) -> Result<CodexCallback, CodexFailure> {
    loop {
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|_| CodexFailure::Callback)?;
        let mut buffer = vec![0u8; 16 * 1024];
        let mut read = 0usize;
        let request_end = loop {
            if read >= buffer.len() {
                break None;
            }
            let count =
                tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buffer[read..]))
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .unwrap_or_default();
            if count == 0 {
                break None;
            }
            read += count;
            if let Some(position) = buffer[..read]
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
            {
                break Some(position + 4);
            }
        };
        let Some(request_end) = request_end else {
            let _ = write_callback_response(&mut stream, "400 Bad Request", false).await;
            continue;
        };
        let request = String::from_utf8_lossy(&buffer[..request_end]);
        let target = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("/");
        let Ok(url) = Url::parse(&format!("http://localhost{target}")) else {
            let _ = write_callback_response(&mut stream, "400 Bad Request", false).await;
            continue;
        };
        if url.path() != CODEX_CALLBACK_PATH {
            let _ = write_callback_response(&mut stream, "404 Not Found", false).await;
            continue;
        }
        let query = url
            .query_pairs()
            .into_owned()
            .collect::<HashMap<String, String>>();
        if !constant_time_eq(query.get("state").map(String::as_str), Some(expected_state)) {
            let _ = write_callback_response(&mut stream, "400 Bad Request", false).await;
            return Err(CodexFailure::StateMismatch);
        }
        let callback = CodexCallback {
            code: query.get("code").cloned(),
            state: query.get("state").cloned(),
            error: query.get("error").cloned(),
        };
        if callback.code.is_none() && callback.error.is_none() {
            let _ = write_callback_response(&mut stream, "400 Bad Request", false).await;
            return Err(CodexFailure::Callback);
        }
        let _ = write_callback_response(&mut stream, "200 OK", true).await;
        return Ok(callback);
    }
}

async fn write_callback_response(
    stream: &mut tokio::net::TcpStream,
    status: &str,
    success: bool,
) -> Result<(), std::io::Error> {
    let message = if success {
        "认证回调已收到，请返回 DeepTutor。"
    } else {
        "认证回调无效，请返回 DeepTutor 后重试。"
    };
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>DeepTutor Codex</title><p>{message}</p>"
    );
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

fn parse_codex_callback_url(raw: &str, expected_redirect: &str) -> Result<CodexCallback, String> {
    let pasted = Url::parse(raw.trim()).map_err(|_| "请粘贴完整的本地回调地址。".to_string())?;
    let expected = Url::parse(expected_redirect)
        .map_err(|error| format!("Codex 回调地址配置损坏：{error}"))?;
    if pasted.scheme() != expected.scheme()
        || pasted.host_str() != expected.host_str()
        || pasted.port() != expected.port()
        || pasted.path().trim_end_matches('/') != expected.path().trim_end_matches('/')
        || pasted.fragment().is_some()
    {
        return Err("这个地址不是当前 Codex 登录等待的回调地址。".into());
    }
    let mut values: HashMap<String, Vec<String>> = HashMap::new();
    for (key, value) in url::form_urlencoded::parse(pasted.query().unwrap_or("").as_bytes()) {
        values
            .entry(key.into_owned())
            .or_default()
            .push(value.into_owned());
    }
    if values.values().any(|items| items.len() > 1) {
        return Err("回调地址包含重复的 OAuth 参数。".into());
    }
    let first = |key: &str| values.get(key).and_then(|items| items.first()).cloned();
    let callback = CodexCallback {
        code: first("code"),
        state: first("state"),
        error: first("error"),
    };
    if callback.code.is_none() && callback.error.is_none() {
        return Err("回调地址没有携带授权结果。".into());
    }
    Ok(callback)
}

#[derive(Debug)]
struct CodexCallback {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

#[derive(Debug)]
enum CodexFailure {
    Unauthorized,
    TokenExchange,
    TokenRefresh,
    Catalog,
    CatalogTooLarge,
    Callback,
    CallbackChanged,
    Cancelled,
    Timeout,
    StateMismatch,
    Storage(String),
}

impl CodexFailure {
    fn code(&self) -> &'static str {
        match self {
            Self::Unauthorized => "authentication_required",
            Self::TokenExchange => "token_exchange_failed",
            Self::TokenRefresh => "token_refresh_failed",
            Self::Catalog => "catalog_unavailable",
            Self::CatalogTooLarge => "catalog_too_large",
            Self::Callback => "callback_unavailable",
            Self::CallbackChanged => "callback_unavailable",
            Self::Cancelled => "login_cancelled",
            Self::Timeout => "login_timeout",
            Self::StateMismatch => "state_mismatch",
            Self::Storage(_) => "storage_error",
        }
    }

    fn public_message(&self) -> &'static str {
        match self {
            Self::Unauthorized => "Codex 登录已失效，请重新登录。",
            Self::TokenExchange => "Codex 登录授权无法完成。",
            Self::TokenRefresh => "Codex 登录无法刷新，请重新登录。",
            Self::Catalog => "Codex 模型目录暂时不可用。",
            Self::CatalogTooLarge => "Codex 模型目录超过安全大小限制。",
            Self::Callback => "无法接收 Codex 本地登录回调。",
            Self::CallbackChanged => "Codex 登录回调已经失效。",
            Self::Cancelled => "Codex 登录已取消。",
            Self::Timeout => "Codex 登录超时，没有收到浏览器回调。",
            Self::StateMismatch => "Codex 登录返回了无效的 state。",
            Self::Storage(_) => "Codex 凭据无法写入便携数据目录。",
        }
    }
}

fn codex_failure_to_api(error: CodexFailure) -> ApiError {
    let status = match error {
        CodexFailure::Unauthorized => axum::http::StatusCode::UNAUTHORIZED,
        CodexFailure::TokenRefresh | CodexFailure::TokenExchange => {
            axum::http::StatusCode::BAD_GATEWAY
        }
        CodexFailure::Catalog => axum::http::StatusCode::SERVICE_UNAVAILABLE,
        _ => axum::http::StatusCode::INTERNAL_SERVER_ERROR,
    };
    ApiError::new(status, error.code(), error.public_message(), true)
}

async fn checked_json_response(
    response: reqwest::Response,
    failure: CodexFailure,
) -> Result<Value, CodexFailure> {
    if !response.status().is_success() {
        return Err(failure);
    }
    response.json::<Value>().await.map_err(|_| failure)
}

async fn refresh_codex_credentials(
    client: &reqwest::Client,
    store: &CodexStore,
    current: CodexCredentials,
) -> Result<CodexCredentials, CodexFailure> {
    let response = client
        .post(CODEX_TOKEN_URL)
        .json(&json!({
            "client_id": CODEX_CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": current.refresh_token
        }))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|_| CodexFailure::TokenRefresh)?;
    if matches!(
        response.status(),
        reqwest::StatusCode::BAD_REQUEST | reqwest::StatusCode::UNAUTHORIZED
    ) {
        let rejection = response.json::<Value>().await.ok();
        if rejection
            .as_ref()
            .and_then(|value| value.get("error"))
            .and_then(Value::as_str)
            .is_some_and(|value| matches!(value, "invalid_grant" | "invalid_token"))
        {
            let _ = store.mark_rejected(current.generation);
            return Err(CodexFailure::Unauthorized);
        }
        return Err(CodexFailure::TokenRefresh);
    }
    let payload = checked_json_response(response, CodexFailure::TokenRefresh).await?;
    let refreshed = credentials_from_token_payload(&payload, current.generation, Some(&current))?;
    if refreshed.account_id != current.account_id {
        return Err(CodexFailure::TokenRefresh);
    }
    store
        .commit_credentials(&refreshed, current.generation)
        .map_err(CodexFailure::Storage)
}

async fn fetch_codex_models(
    client: &reqwest::Client,
    credentials: &CodexCredentials,
) -> Result<(Vec<CodexModel>, Option<String>), CodexFailure> {
    let mut url = Url::parse(CODEX_MODELS_URL).map_err(|_| CodexFailure::Catalog)?;
    url.query_pairs_mut()
        .append_pair("client_version", CODEX_CLIENT_VERSION);
    let response = client
        .get(url)
        .bearer_auth(&credentials.access_token)
        .header("chatgpt-account-id", &credentials.account_id)
        .header("Accept", "application/json")
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|_| CodexFailure::Catalog)?;
    match response.status() {
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => {
            return Err(CodexFailure::Unauthorized)
        }
        status if !status.is_success() => return Err(CodexFailure::Catalog),
        _ => {}
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_CATALOG_BYTES as u64)
    {
        return Err(CodexFailure::CatalogTooLarge);
    }
    let etag = response
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let bytes = response.bytes().await.map_err(|_| CodexFailure::Catalog)?;
    if bytes.len() > MAX_CATALOG_BYTES {
        return Err(CodexFailure::CatalogTooLarge);
    }
    let payload = serde_json::from_slice::<Value>(&bytes).map_err(|_| CodexFailure::Catalog)?;
    Ok((parse_codex_models(&payload)?, etag))
}

fn parse_codex_models(payload: &Value) -> Result<Vec<CodexModel>, CodexFailure> {
    let rows = payload
        .get("models")
        .and_then(Value::as_array)
        .ok_or(CodexFailure::Catalog)?;
    if rows.len() > MAX_MODELS {
        return Err(CodexFailure::CatalogTooLarge);
    }
    let mut models = rows
        .iter()
        .filter_map(|row| {
            if row.get("visibility").and_then(Value::as_str) != Some("list") {
                return None;
            }
            let slug = row.get("slug").and_then(Value::as_str)?.to_string();
            let display_name = row
                .get("display_name")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .unwrap_or(&slug)
                .to_string();
            let levels = row
                .get("supported_reasoning_levels")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|level| {
                    level
                        .as_str()
                        .or_else(|| level.get("effort").and_then(Value::as_str))
                })
                .filter(|value| !value.is_empty())
                .fold(Vec::<String>::new(), |mut items, value| {
                    if !items.iter().any(|item| item == value) {
                        items.push(value.to_string());
                    }
                    items
                });
            Some(CodexModel {
                slug: slug.clone(),
                display_name,
                priority: row
                    .get("priority")
                    .and_then(Value::as_i64)
                    .unwrap_or_default(),
                default_reasoning_level: row
                    .get("default_reasoning_level")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                supported_reasoning_levels: normalize_reasoning_levels(&slug, levels),
                supports_reasoning_summary: row
                    .get("supports_reasoning_summary_parameter")
                    .and_then(Value::as_bool)
                    .or_else(|| {
                        row.get("supports_reasoning_summaries")
                            .and_then(Value::as_bool)
                    })
                    .unwrap_or(false),
                supports_parallel_tool_calls: row
                    .get("supports_parallel_tool_calls")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                use_responses_lite: row
                    .get("use_responses_lite")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                context_window: positive_i64(row.get("context_window")),
                max_context_window: positive_i64(row.get("max_context_window")),
            })
        })
        .collect::<Vec<_>>();
    models.sort_by(|left, right| {
        left.priority
            .cmp(&right.priority)
            .then_with(|| left.slug.cmp(&right.slug))
    });
    Ok(models)
}

fn normalize_reasoning_levels(slug: &str, levels: Vec<String>) -> Vec<String> {
    if slug == "gpt-5.6-luna" && !levels.iter().any(|level| level == "none") {
        std::iter::once("none".to_string()).chain(levels).collect()
    } else {
        levels
    }
}

fn positive_i64(value: Option<&Value>) -> Option<i64> {
    value.and_then(Value::as_i64).filter(|value| *value > 0)
}

fn credentials_from_token_payload(
    payload: &Value,
    generation: u64,
    fallback: Option<&CodexCredentials>,
) -> Result<CodexCredentials, CodexFailure> {
    let token = |key: &str, fallback: Option<&str>| {
        payload
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .or_else(|| fallback.map(str::to_string))
            .ok_or(CodexFailure::TokenExchange)
    };
    let access_token = token(
        "access_token",
        fallback.map(|value| value.access_token.as_str()),
    )?;
    let refresh_token = token(
        "refresh_token",
        fallback.map(|value| value.refresh_token.as_str()),
    )?;
    let id_token = token("id_token", fallback.map(|value| value.id_token.as_str()))?;
    let mut account_id = payload
        .get("account_id")
        .or_else(|| payload.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let mut expires_at = payload.get("expires_at").and_then(Value::as_i64);
    for token in [&id_token, &access_token] {
        let claims = decode_jwt_claims(token);
        if account_id.is_none() {
            account_id = claims.as_ref().and_then(|claims| claims.account_id.clone());
        }
        if expires_at.is_none() {
            expires_at = claims.and_then(|claims| claims.expires_at);
        }
    }
    if account_id.is_none() {
        account_id = fallback.map(|value| value.account_id.clone());
    }
    if expires_at.is_none() {
        expires_at = payload
            .get("expires_in")
            .and_then(Value::as_i64)
            .map(|seconds| unix_now() + seconds.max(1));
    }
    if expires_at.is_none() {
        expires_at = fallback.map(|value| value.expires_at);
    }
    Ok(CodexCredentials {
        schema_version: 1,
        access_token,
        refresh_token,
        id_token,
        account_id: account_id.ok_or(CodexFailure::TokenExchange)?,
        expires_at: expires_at.ok_or(CodexFailure::TokenExchange)?,
        generation,
    })
}

#[derive(Debug)]
struct JwtClaims {
    expires_at: Option<i64>,
    account_id: Option<String>,
}

fn decode_jwt_claims(token: &str) -> Option<JwtClaims> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let value = serde_json::from_slice::<Value>(&bytes).ok()?;
    Some(JwtClaims {
        expires_at: value.get("exp").and_then(Value::as_i64),
        account_id: value
            .pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

#[derive(Serialize, Deserialize, Clone)]
struct CodexCredentials {
    schema_version: u64,
    access_token: String,
    refresh_token: String,
    id_token: String,
    account_id: String,
    expires_at: i64,
    generation: u64,
}

#[derive(Serialize, Deserialize, Clone)]
struct CodexModel {
    slug: String,
    display_name: String,
    priority: i64,
    default_reasoning_level: Option<String>,
    supported_reasoning_levels: Vec<String>,
    supports_reasoning_summary: bool,
    supports_parallel_tool_calls: bool,
    use_responses_lite: bool,
    context_window: Option<i64>,
    max_context_window: Option<i64>,
}

#[derive(Serialize, Deserialize)]
struct CodexCache {
    models: Vec<CodexModel>,
    source: String,
    fetched_at: i64,
    etag: Option<String>,
    generation: u64,
    account_hash: String,
    client_version: String,
    models_valid: bool,
}

fn codex_cache(credentials: &CodexCredentials, models: &[CodexModel]) -> CodexCache {
    CodexCache {
        models: models.to_vec(),
        source: "live".into(),
        fetched_at: unix_now(),
        etag: None,
        generation: credentials.generation,
        account_hash: sha256_hex(&credentials.account_id),
        client_version: CODEX_CLIENT_VERSION.into(),
        models_valid: true,
    }
}

fn matching_codex_cache(
    store: &CodexStore,
    credentials: &CodexCredentials,
) -> Result<Option<CodexCache>, String> {
    let Some(cache) = store.load_cache()? else {
        return Ok(None);
    };
    if !cache.models_valid
        || cache.generation != credentials.generation
        || cache.account_hash != sha256_hex(&credentials.account_id)
    {
        return Ok(None);
    }
    Ok(Some(cache))
}

#[derive(Serialize, Deserialize, Default)]
struct CodexStateFile {
    schema_version: u64,
    generation: u64,
    rejected_generation: Option<u64>,
}

struct CodexStore {
    root: PathBuf,
}

impl CodexStore {
    fn new(paths: &crate::portable::PortablePaths) -> Self {
        Self {
            root: paths.data.join("private").join("openai-codex"),
        }
    }

    fn credentials_path(&self) -> PathBuf {
        self.root.join("credentials.v1.json")
    }

    fn state_path(&self) -> PathBuf {
        self.root.join("state.v1.json")
    }

    fn cache_path(&self) -> PathBuf {
        self.root.join("models-cache.v1.json")
    }

    fn load_state(&self) -> Result<CodexStateFile, String> {
        if !self.state_path().is_file() {
            return Ok(CodexStateFile {
                schema_version: 1,
                generation: 0,
                rejected_generation: None,
            });
        }
        read_json_file(&self.state_path())
            .map_err(|error| format!("无法读取 Codex 登录状态：{error}"))
    }

    fn generation(&self) -> Result<u64, String> {
        Ok(self.load_state()?.generation)
    }

    fn authentication_rejected(&self, generation: u64) -> Result<bool, String> {
        let state = self.load_state()?;
        Ok(state.generation == generation && state.rejected_generation == Some(generation))
    }

    fn mark_rejected(&self, generation: u64) -> Result<(), String> {
        let mut state = self.load_state()?;
        if state.generation == generation {
            state.rejected_generation = Some(generation);
            write_json_file(&self.state_path(), &state)?;
        }
        Ok(())
    }

    fn load_credentials(&self) -> Result<Option<CodexCredentials>, String> {
        let state = self.load_state()?;
        let path = self.credentials_path();
        if !path.is_file() {
            return Ok(None);
        }
        let credentials: CodexCredentials =
            read_json_file(&path).map_err(|error| format!("无法读取 Codex 凭据：{error}"))?;
        if credentials.schema_version != 1 || credentials.generation != state.generation {
            return Ok(None);
        }
        Ok(Some(credentials))
    }

    fn commit_credentials(
        &self,
        credentials: &CodexCredentials,
        expected_generation: u64,
    ) -> Result<CodexCredentials, String> {
        let mut state = self.load_state()?;
        if state.generation != expected_generation {
            return Err("Codex 登录在授权期间发生变化，请重试。".into());
        }
        state.schema_version = 1;
        state.generation = state.generation.saturating_add(1);
        state.rejected_generation = None;
        let mut committed = credentials.clone();
        committed.generation = state.generation;
        write_json_file(&self.state_path(), &state)?;
        write_json_file(&self.credentials_path(), &committed)?;
        Ok(committed)
    }

    fn clear(&self) -> Result<(), String> {
        let mut state = self.load_state()?;
        state.schema_version = 1;
        state.generation = state.generation.saturating_add(1);
        state.rejected_generation = None;
        write_json_file(&self.state_path(), &state)?;
        for path in [self.credentials_path(), self.cache_path()] {
            if path.exists() {
                fs::remove_file(&path)
                    .map_err(|error| format!("无法删除 {}：{error}", path.display()))?;
            }
        }
        Ok(())
    }

    fn load_cache(&self) -> Result<Option<CodexCache>, String> {
        if !self.cache_path().is_file() {
            return Ok(None);
        }
        read_json_file(&self.cache_path()).map(Some)
    }

    fn save_cache(&self, cache: &CodexCache) -> Result<(), String> {
        write_json_file(&self.cache_path(), cache)
    }
}

fn codex_store(state: &AppState) -> CodexStore {
    CodexStore::new(&state.paths)
}

fn read_json_file<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, String> {
    let bytes = fs::read(path).map_err(|error| format!("无法读取 {}：{error}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|error| format!("无法解析 {}：{error}", path.display()))
}

fn write_json_file<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "凭据文件缺少父目录。".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("无法创建凭据目录 {}：{error}", parent.display()))?;
    let temp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("credentials"),
        uuid::Uuid::new_v4()
    ));
    let bytes =
        serde_json::to_vec_pretty(value).map_err(|error| format!("无法编码认证数据：{error}"))?;
    fs::write(&temp, bytes).map_err(|error| format!("无法写入 {}：{error}", temp.display()))?;
    if path.exists() {
        fs::remove_file(path).map_err(|error| format!("无法替换 {}：{error}", path.display()))?;
    }
    fs::rename(&temp, path).map_err(|error| {
        let _ = fs::remove_file(&temp);
        format!("无法保存 {}：{error}", path.display())
    })
}

#[derive(Default, Clone)]
struct CodeBuddyRuntime {
    connection: String,
    operation_state: Option<String>,
    authorize_url: Option<String>,
    user_label: Option<String>,
    error_code: Option<String>,
}

fn codebuddy_status_value(runtime: &CodeBuddyRuntime) -> Value {
    json!({
        "connection": if runtime.connection.is_empty() { "disconnected" } else { &runtime.connection },
        "operation_state": runtime.operation_state,
        "authorize_url": runtime.authorize_url,
        "user_label": runtime.user_label,
        "error_code": runtime.error_code
    })
}

#[derive(Clone)]
struct CodeBuddyCredentials {
    access_token: String,
    refresh_token: String,
    user_id: String,
    user_label: String,
    domain: String,
    source: PathBuf,
}

fn load_codebuddy_credentials(
    paths: &crate::portable::PortablePaths,
) -> Option<CodeBuddyCredentials> {
    let mut candidates = Vec::new();
    if let Some(explicit) = std::env::var_os("DEEPTUTOR_CODEBUDDY_AUTH_FILE") {
        candidates.push(PathBuf::from(explicit));
    }
    candidates.extend([
        paths
            .cache
            .join("appdata/local")
            .join("CodeBuddyExtension/Data/Public/auth/Tencent-Cloud.coding-copilot.info"),
        paths
            .home
            .join("CodeBuddyExtension/Data/Public/auth/Tencent-Cloud.coding-copilot.info"),
        paths
            .home
            .join(".codebuddy/local_storage/Tencent-Cloud.coding-copilot.info"),
    ]);
    let source = candidates.into_iter().find(|path| path.is_file())?;
    let payload = read_json_file::<Value>(&source).ok()?;
    let auth = payload.get("auth")?;
    let access_token = auth
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())?
        .to_string();
    let account = payload.get("account").and_then(Value::as_object);
    let user_label = account
        .and_then(|account| {
            ["nickname", "uin", "uid"]
                .iter()
                .find_map(|key| account.get(*key).filter(|value| !value.is_null()))
        })
        .map(|value| match value {
            Value::String(value) => value.clone(),
            value => value.to_string(),
        })
        .unwrap_or_default();
    Some(CodeBuddyCredentials {
        access_token,
        refresh_token: auth
            .get("refreshToken")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        user_id: account
            .and_then(|account| account.get("uid"))
            .map(|value| match value {
                Value::String(value) => value.clone(),
                value => value.to_string(),
            })
            .unwrap_or_default(),
        user_label,
        domain: auth
            .get("domain")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        source,
    })
}

fn codebuddy_api_base(credentials: &CodeBuddyCredentials) -> String {
    if let Ok(value) = std::env::var("CODEBUDDY_BASE_URL") {
        let value = value.trim().trim_end_matches('/');
        if !value.is_empty() {
            return if value.ends_with("/v2") {
                value.to_string()
            } else {
                format!("{value}/v2")
            };
        }
    }
    let internal = std::env::var("CODEBUDDY_INTERNET_ENVIRONMENT")
        .ok()
        .is_some_and(|value| matches!(value.to_ascii_lowercase().as_str(), "internal" | "ioa"));
    if internal
        || ["codebuddy.cn", "workbuddy.cn", "copilot.tencent.com"]
            .iter()
            .any(|marker| credentials.domain.to_ascii_lowercase().contains(marker))
    {
        "https://copilot.tencent.com/v2".into()
    } else {
        "https://www.codebuddy.ai/v2".into()
    }
}

async fn probe_codebuddy_account(
    client: &reqwest::Client,
    credentials: &CodeBuddyCredentials,
) -> Option<String> {
    let base = codebuddy_api_base(credentials)
        .trim_end_matches("/v2")
        .to_string();
    let mut request = client
        .get(format!("{base}/v2/accounts"))
        .bearer_auth(&credentials.access_token)
        .timeout(Duration::from_secs(20));
    if !credentials.user_id.is_empty() {
        request = request.header("X-User-Id", &credentials.user_id);
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(_) => {
            return (!credentials.user_label.is_empty()).then(|| credentials.user_label.clone())
        }
    };
    if !response.status().is_success() {
        return None;
    }
    let payload = response.json::<Value>().await.ok()?;
    let accounts = payload
        .pointer("/data/accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for account in accounts {
        if !credentials.user_id.is_empty()
            && account.get("uid").map(|value| match value {
                Value::String(value) => value.as_str(),
                _ => "",
            }) != Some(credentials.user_id.as_str())
        {
            continue;
        }
        for key in ["nickname", "uin", "uid"] {
            if let Some(value) = account.get(key).filter(|value| !value.is_null()) {
                return Some(match value {
                    Value::String(value) => value.clone(),
                    value => value.to_string(),
                });
            }
        }
    }
    (!credentials.user_label.is_empty()).then(|| credentials.user_label.clone())
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/settings/providers/openai-codex/oauth/status",
            get(codex_status),
        )
        .route(
            "/settings/providers/openai-codex/oauth/start",
            post(codex_start),
        )
        .route(
            "/settings/providers/openai-codex/oauth/complete",
            post(codex_complete),
        )
        .route(
            "/settings/providers/openai-codex/oauth/cancel",
            post(codex_cancel),
        )
        .route(
            "/settings/providers/openai-codex/oauth/logout",
            post(codex_logout),
        )
        .route(
            "/settings/providers/openai-codex/models/refresh",
            post(codex_refresh_models),
        )
        .route(
            "/settings/providers/openai-codex/models/reasoning-effort",
            post(codex_reasoning_effort),
        )
        .route(
            "/settings/providers/codebuddy/auth/status",
            get(codebuddy_status),
        )
        .route(
            "/settings/providers/codebuddy/auth/start",
            post(codebuddy_start),
        )
        .route(
            "/settings/providers/codebuddy/auth/cancel",
            post(codebuddy_cancel),
        )
        .route(
            "/settings/providers/codebuddy/auth/logout",
            post(codebuddy_logout),
        )
}

async fn codex_status(
    ScopedState(state): ScopedState,
    Extension(principal): Extension<super::auth::Principal>,
) -> ApiResult<Json<Value>> {
    require_provider_admin(&principal)?;
    state
        .provider_auth
        .codex_status(&state)
        .await
        .map(Json)
        .map_err(ApiError::internal)
}

async fn codex_start(
    ScopedState(state): ScopedState,
    Extension(principal): Extension<super::auth::Principal>,
) -> ApiResult<Json<Value>> {
    require_provider_admin(&principal)?;
    let provider_auth = state.provider_auth.clone();
    provider_auth.start_codex_login(state).await.map(Json)
}

async fn codex_complete(
    ScopedState(state): ScopedState,
    Extension(principal): Extension<super::auth::Principal>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    require_provider_admin(&principal)?;
    let callback_url = body
        .get("callback_url")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad_request("缺少 callback_url。"))?;
    state
        .provider_auth
        .complete_codex_callback(&state, callback_url)
        .await
        .map(Json)
}

async fn codex_cancel(
    ScopedState(state): ScopedState,
    Extension(principal): Extension<super::auth::Principal>,
) -> ApiResult<Json<Value>> {
    require_provider_admin(&principal)?;
    state
        .provider_auth
        .cancel_codex_login(&state)
        .await
        .map(Json)
        .map_err(ApiError::internal)
}

async fn codex_logout(
    ScopedState(state): ScopedState,
    Extension(principal): Extension<super::auth::Principal>,
) -> ApiResult<Json<Value>> {
    require_provider_admin(&principal)?;
    state.provider_auth.logout_codex(&state).await.map(Json)
}

async fn codex_refresh_models(
    ScopedState(state): ScopedState,
    Extension(principal): Extension<super::auth::Principal>,
) -> ApiResult<Json<Value>> {
    require_provider_admin(&principal)?;
    state
        .provider_auth
        .refresh_codex_models(&state)
        .await
        .map(Json)
}

async fn codex_reasoning_effort(
    ScopedState(state): ScopedState,
    Extension(principal): Extension<super::auth::Principal>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    require_provider_admin(&principal)?;
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad_request("缺少 Codex 模型标识。"))?;
    let effort = body.get("reasoning_effort").and_then(Value::as_str);
    state
        .provider_auth
        .set_codex_reasoning_effort(&state, model, effort)
        .await
        .map(Json)
}

async fn codebuddy_status(
    ScopedState(state): ScopedState,
    Extension(principal): Extension<super::auth::Principal>,
) -> ApiResult<Json<Value>> {
    require_provider_admin(&principal)?;
    Ok(Json(state.provider_auth.codebuddy_status(&state).await))
}

async fn codebuddy_start(
    ScopedState(state): ScopedState,
    Extension(principal): Extension<super::auth::Principal>,
) -> ApiResult<Json<Value>> {
    require_provider_admin(&principal)?;
    Ok(Json(
        state.provider_auth.start_codebuddy_login(&state).await,
    ))
}

async fn codebuddy_cancel(
    ScopedState(state): ScopedState,
    Extension(principal): Extension<super::auth::Principal>,
) -> ApiResult<Json<Value>> {
    require_provider_admin(&principal)?;
    Ok(Json(state.provider_auth.cancel_codebuddy_login().await))
}

async fn codebuddy_logout(
    ScopedState(state): ScopedState,
    Extension(principal): Extension<super::auth::Principal>,
) -> ApiResult<Json<Value>> {
    require_provider_admin(&principal)?;
    Ok(Json(state.provider_auth.logout_codebuddy(&state).await))
}

fn require_provider_admin(principal: &super::auth::Principal) -> ApiResult<()> {
    super::auth::require_admin(principal)
}

pub(super) async fn handle_codex_oauth_callback(
    state: &AppState,
    code: Option<String>,
    callback_state: Option<String>,
    error: Option<String>,
) -> Result<(), String> {
    let callback = CodexCallback {
        code,
        state: callback_state.clone(),
        error,
    };
    if let Some(runtime) = state
        .provider_auth
        .inner
        .lock()
        .await
        .codex
        .operation
        .as_ref()
    {
        if constant_time_eq(
            callback.state.as_deref(),
            Some(&runtime.operation.state_secret),
        ) {
            runtime
                .callback_tx
                .try_send(callback)
                .map_err(|_| "Codex 登录回调已经提交。".to_string())?;
            return Ok(());
        }
    }
    Err("Codex 登录不在等待这个回调，或 state 无效。".into())
}

fn random_urlsafe(byte_length: usize) -> String {
    let mut bytes = vec![0u8; byte_length];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn constant_time_eq(actual: Option<&str>, expected: Option<&str>) -> bool {
    let (Some(actual), Some(expected)) = (actual, expected) else {
        return false;
    };
    if actual.len() != expected.len() {
        return false;
    }
    actual
        .as_bytes()
        .iter()
        .zip(expected.as_bytes())
        .fold(0u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

fn sha256_hex(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_callback_requires_the_registered_loopback_target() {
        let expected = "http://localhost:1455/auth/callback";
        let valid = parse_codex_callback_url(
            "http://localhost:1455/auth/callback?code=a&state=b",
            expected,
        )
        .expect("回调应有效");
        assert_eq!(valid.code.as_deref(), Some("a"));
        assert_eq!(valid.state.as_deref(), Some("b"));
        assert!(parse_codex_callback_url(
            "http://127.0.0.1:1455/auth/callback?code=a&state=b",
            expected
        )
        .is_err());
        assert!(parse_codex_callback_url(
            "http://localhost:1457/auth/callback?code=a&state=b",
            expected
        )
        .is_err());
    }

    #[test]
    fn model_ids_are_stable_and_bounded() {
        let first = codex_model_id("gpt-5.6-sol");
        assert_eq!(first, codex_model_id("gpt-5.6-sol"));
        assert!(first.starts_with("llm-model-openai-codex-"));
        assert_eq!(first.len(), "llm-model-openai-codex-".len() + 16);
    }
}
