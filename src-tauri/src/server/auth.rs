use super::error::{ApiError, ApiResult};
use super::AppState;
use argon2::password_hash::{
    rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString,
};
use argon2::Argon2;
use axum::extract::{DefaultBodyLimit, Extension, Multipart, Path, Query, Request, State};
use axum::http::header::{self, HeaderMap, HeaderValue};
use axum::http::{StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::{Rng, RngCore};
use rusqlite::{params, OptionalExtension, Row};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use uuid::Uuid;

const COOKIE_NAME: &str = "dt_token";
const DEFAULT_TOKEN_HOURS: i64 = 7 * 24;
const MAX_AVATAR_BYTES: usize = 1024 * 1024;
const MAX_IMPORT_BYTES: usize = 2 * 1024 * 1024;
const MAX_IMPORT_ROWS: usize = 500;
const MAX_DEVICE_DAYS: i64 = 365;
const DEVICE_HEARTBEAT_GRACE_SECONDS: i64 = 5 * 60;

#[derive(Clone, Debug)]
pub struct Principal {
    pub user_id: String,
    pub username: String,
    pub role: String,
    pub disabled: bool,
    pub avatar: String,
    pub preset: String,
    pub device_credential_id: String,
    pub device_session_nonce_hash: String,
}

impl Principal {
    pub fn local_admin() -> Self {
        Self {
            user_id: "local-admin".into(),
            username: "local".into(),
            role: "admin".into(),
            disabled: false,
            avatar: String::new(),
            preset: "standard".into(),
            device_credential_id: String::new(),
            device_session_nonce_hash: String::new(),
        }
    }

    pub fn is_admin(&self) -> bool {
        self.role == "admin"
    }
}

#[derive(Clone, Debug)]
pub(super) struct Account {
    pub(super) id: String,
    pub(super) username: String,
    pub(super) password_hash: String,
    pub(super) role: String,
    pub(super) disabled: bool,
    pub(super) avatar: String,
    pub(super) preset: String,
    pub(super) learner_profile: Value,
    pub(super) book_permission: Value,
    pub(super) created_at: String,
}

impl Account {
    fn principal(&self) -> Principal {
        Principal {
            user_id: self.id.clone(),
            username: self.username.clone(),
            role: self.role.clone(),
            disabled: self.disabled,
            avatar: self.avatar.clone(),
            preset: self.preset.clone(),
            device_credential_id: String::new(),
            device_session_nonce_hash: String::new(),
        }
    }

    pub(super) fn public(&self) -> Value {
        json!({
            "id": self.id,
            "username": self.username,
            "role": self.role,
            "created_at": self.created_at,
            "disabled": self.disabled,
            "avatar": self.avatar,
            "preset": self.preset,
            "learner_profile": self.learner_profile,
            "book_permission": self.book_permission,
        })
    }
}

#[derive(Debug, Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Debug, Deserialize)]
struct RegisterRequest {
    username: String,
    password: String,
}

#[derive(Debug, Deserialize)]
struct AdminCreateUserRequest {
    username: String,
    password: String,
    #[serde(default = "default_preset")]
    preset: String,
}

#[derive(Debug, Deserialize)]
struct DeviceLoginRequest {
    pairing_code: String,
    pin: String,
}

#[derive(Debug, Deserialize)]
struct DeviceCreateRequest {
    user_id: String,
    device_name: String,
    expires_in_days: i64,
    daily_limit_minutes: i64,
}

#[derive(Debug, Deserialize)]
struct DeviceListQuery {
    user_id: Option<String>,
    #[serde(default)]
    include_revoked: bool,
}

#[derive(Debug, Deserialize)]
struct SetRoleRequest {
    role: String,
}

#[derive(Debug, Deserialize)]
struct UpdateProfileRequest {
    avatar: String,
}

#[derive(Debug, Deserialize)]
struct BatchDeleteRequest {
    usernames: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ResetLearnerPasswordRequest {
    new_password: String,
}

#[derive(Debug, Deserialize)]
struct SessionHandoffCreateRequest {
    public_origin: String,
}

#[derive(Debug, Deserialize)]
struct SessionHandoffExchangeRequest {
    code: String,
}

#[derive(Debug, Deserialize)]
struct SessionHandoffCompleteRequest {
    ticket: String,
}

#[derive(Debug, Deserialize)]
struct CodexCallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

fn default_preset() -> String {
    "standard".into()
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/auth/status", get(auth_status))
        .route("/auth/login", post(auth_login))
        .route("/auth/device-login", post(device_login))
        .route("/auth/logout", post(auth_logout))
        .route("/auth/register", post(auth_register))
        .route("/auth/is_first_user", get(auth_is_first_user))
        .route("/auth/profile", get(auth_profile).put(update_auth_profile))
        .route(
            "/auth/profile/avatar",
            put(upload_avatar).delete(remove_avatar),
        )
        .route(
            "/auth/profile/learner-profile",
            get(get_own_learner_profile).put(put_own_learner_profile),
        )
        .route("/auth/avatar/{user_id}", get(get_avatar))
        .route("/auth/devices", get(list_devices).post(create_device))
        .route("/auth/devices/{device_id}", delete(revoke_device))
        .route("/auth/device/heartbeat", post(device_heartbeat))
        .route("/auth/users", get(list_users).post(admin_create_user))
        .route("/auth/users/import", post(import_users))
        .route("/auth/users/batch-delete", post(batch_delete_users))
        .route("/auth/users/{username}", delete(delete_user))
        .route("/auth/users/{username}/role", put(update_user_role))
        .route(
            "/auth/users/{username}/learner-profile",
            get(get_learner_profile).put(put_learner_profile),
        )
        .route("/auth/session-handoff", post(create_session_handoff))
        .route(
            "/auth/session-handoff/exchange",
            post(exchange_session_handoff),
        )
        .route(
            "/auth/session-handoff/complete",
            post(complete_session_handoff),
        )
        .route("/auth/openai-codex/callback", get(codex_oauth_callback))
        .layer(DefaultBodyLimit::max(MAX_IMPORT_BYTES + 64 * 1024))
}

pub async fn middleware(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let enabled = match auth_enabled(&state) {
        Ok(enabled) => enabled,
        Err(error) => return error.into_response(),
    };
    if !enabled {
        request.extensions_mut().insert(Principal::local_admin());
        return next.run(request).await;
    }
    if is_public_path(request.uri()) {
        if let Some(token) = token_from_request(&request) {
            match principal_for_token(&state, &token) {
                Ok(Some(principal)) => {
                    request.extensions_mut().insert(principal);
                }
                Ok(None) => {}
                Err(error) => return error.into_response(),
            }
        }
        return next.run(request).await;
    }
    let token = token_from_request(&request);
    let Some(token) = token else {
        return ApiError::unauthorized("未登录或登录已失效").into_response();
    };
    let principal = match principal_for_token(&state, &token) {
        Ok(Some(principal)) => principal,
        Ok(None) => return ApiError::unauthorized("登录已失效，请重新登录").into_response(),
        Err(error) => return error.into_response(),
    };
    request.extensions_mut().insert(principal);
    next.run(request).await
}

fn is_public_path(uri: &Uri) -> bool {
    let path = uri.path().trim_end_matches('/');
    matches!(
        path,
        "/api/auth/status"
            | "/api/auth/login"
            | "/api/auth/device-login"
            | "/api/auth/logout"
            | "/api/auth/register"
            | "/api/auth/is_first_user"
            | "/api/auth/session-handoff/exchange"
            | "/api/auth/session-handoff/complete"
            | "/api/auth/openai-codex/callback"
            | "/api/settings/ui"
            | "/api/marginnote4/sync"
            | "/api/marginnote4/heartbeat"
            | "/auth/status"
            | "/auth/login"
            | "/auth/device-login"
            | "/auth/logout"
            | "/auth/register"
            | "/auth/is_first_user"
            | "/auth/session-handoff/exchange"
            | "/auth/session-handoff/complete"
            | "/auth/openai-codex/callback"
            | "/settings/ui"
            | "/marginnote4/sync"
            | "/marginnote4/heartbeat"
    )
}

fn token_from_request(request: &Request) -> Option<String> {
    bearer_token(request.headers())
        .or_else(|| cookie_token(request.headers()))
        .or_else(|| query_token(request.uri()))
}

fn token_from_headers(headers: &HeaderMap) -> Option<String> {
    bearer_token(headers).or_else(|| cookie_token(headers))
}

fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::AUTHORIZATION)?.to_str().ok()?.trim();
    let (scheme, token) = raw.split_once(' ')?;
    (scheme.eq_ignore_ascii_case("bearer") && !token.trim().is_empty())
        .then(|| token.trim().to_string())
}

fn cookie_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|part| {
            let (name, value) = part.trim().split_once('=')?;
            (name == COOKIE_NAME && !value.is_empty()).then(|| value.to_string())
        })
}

fn query_token(uri: &Uri) -> Option<String> {
    uri.query()?.split('&').find_map(|part| {
        let (key, value) = part.split_once('=')?;
        (key == "token" && !value.is_empty()).then(|| percent_decode(value))
    })
}

fn percent_decode(value: &str) -> String {
    let mut out = Vec::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = &value[index + 1..index + 3];
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(if bytes[index] == b'+' {
            b' '
        } else {
            bytes[index]
        });
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn auth_enabled(state: &AppState) -> Result<bool, String> {
    Ok(state
        .database
        .get_setting("auth")?
        .and_then(|value| value.get("enabled").and_then(Value::as_bool))
        .unwrap_or(false))
}

fn token_hours(state: &AppState) -> i64 {
    state
        .database
        .get_setting("auth")
        .ok()
        .flatten()
        .and_then(|value| value.get("token_expire_hours").and_then(Value::as_i64))
        .filter(|value| (1..=24 * 365).contains(value))
        .unwrap_or(DEFAULT_TOKEN_HOURS)
}

fn cookie_secure(state: &AppState) -> bool {
    state
        .database
        .get_setting("auth")
        .ok()
        .flatten()
        .and_then(|value| value.get("cookie_secure").and_then(Value::as_bool))
        .unwrap_or(false)
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn unix_now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4().simple())
}

fn random_secret(bytes: usize) -> String {
    let mut value = vec![0_u8; bytes];
    rand::rngs::OsRng.fill_bytes(&mut value);
    URL_SAFE_NO_PAD.encode(value)
}

pub(super) fn hash_secret(value: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(value.as_bytes());
    hex::encode(digest.finalize())
}

pub(super) async fn hash_password(password: &str) -> Result<String, ApiError> {
    let password = password.to_string();
    tokio::task::spawn_blocking(move || {
        let salt = SaltString::generate(&mut OsRng);
        Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map(|value| value.to_string())
            .map_err(|error| format!("无法安全保存密码：{error}"))
    })
    .await
    .map_err(|error| ApiError::internal(format!("密码哈希任务失败：{error}")))?
    .map_err(ApiError::internal)
}

async fn verify_password(password: &str, encoded: &str) -> Result<bool, ApiError> {
    let password = password.to_string();
    let encoded = encoded.to_string();
    tokio::task::spawn_blocking(move || {
        let parsed =
            PasswordHash::new(&encoded).map_err(|error| format!("密码哈希格式损坏：{error}"))?;
        Ok::<bool, String>(
            Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok(),
        )
    })
    .await
    .map_err(|error| ApiError::internal(format!("密码校验任务失败：{error}")))?
    .map_err(ApiError::internal)
}

fn validate_username(username: &str) -> Result<String, ApiError> {
    let value = username.trim();
    if value.is_empty() {
        return Err(ApiError::unprocessable("用户名不能为空"));
    }
    let email_ok = {
        let mut parts = value.split('@');
        let local = parts.next().unwrap_or_default();
        let domain = parts.next().unwrap_or_default();
        !local.is_empty()
            && !domain.is_empty()
            && domain.contains('.')
            && parts.next().is_none()
            && !value.chars().any(char::is_whitespace)
    };
    let plain_ok = (3..=64).contains(&value.len())
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "_-.".contains(character));
    if !email_ok && !plain_ok {
        return Err(ApiError::unprocessable(
            "用户名应为 3-64 位字母、数字、下划线、连字符或点，或有效邮箱地址",
        ));
    }
    if value.len() > 64 {
        return Err(ApiError::unprocessable("用户名不能超过 64 个字符"));
    }
    Ok(value.to_string())
}

pub(super) fn validate_password(password: &str) -> Result<(), ApiError> {
    if password.chars().count() < 8 {
        return Err(ApiError::unprocessable("密码至少需要 8 个字符"));
    }
    if password.chars().count() > 256 {
        return Err(ApiError::unprocessable("密码不能超过 256 个字符"));
    }
    Ok(())
}

fn validate_preset(value: &str) -> Result<String, ApiError> {
    match value {
        "standard" | "learner" | "custom" => Ok(value.to_string()),
        _ => Err(ApiError::unprocessable(
            "账户预设必须是 standard、learner 或 custom",
        )),
    }
}

pub(super) fn normalize_book_permission(value: &Value) -> Value {
    let create = value.get("create").and_then(Value::as_bool).unwrap_or(true);
    let default = match value.get("default").and_then(Value::as_str) {
        Some("read") => "read",
        _ => "none",
    };
    let mut books = Map::new();
    if let Some(items) = value.get("books").and_then(Value::as_object) {
        for (book_id, level) in items {
            let level = match level.as_str() {
                Some("read") => Some("read"),
                Some("edit") | Some("delete") => Some("edit"),
                Some("none") => Some("none"),
                _ => None,
            };
            if let Some(level) = level {
                books.insert(book_id.clone(), Value::String(level.into()));
            }
        }
    }
    json!({"create": create, "default": default, "books": books})
}

fn account_from_row(row: &Row<'_>) -> rusqlite::Result<Account> {
    let learner_raw: String = row.get(7)?;
    let permission_raw: String = row.get(8)?;
    Ok(Account {
        id: row.get(0)?,
        username: row.get(1)?,
        password_hash: row.get(2)?,
        role: row.get(3)?,
        disabled: row.get::<_, i64>(4)? != 0,
        avatar: row.get(5)?,
        preset: row.get(6)?,
        learner_profile: serde_json::from_str(&learner_raw).unwrap_or_else(|_| json!({})),
        book_permission: serde_json::from_str(&permission_raw)
            .map(|value: Value| normalize_book_permission(&value))
            .unwrap_or_else(|_| normalize_book_permission(&json!({}))),
        created_at: row.get(9)?,
    })
}

const ACCOUNT_SELECT: &str = r#"
    SELECT id, username, password_hash, role, disabled, avatar, preset,
           learner_profile_json, book_permission_json, created_at
      FROM auth_accounts
"#;

pub(super) fn account_by_username(
    state: &AppState,
    username: &str,
) -> Result<Option<Account>, String> {
    state.database.with_connection(|connection| {
        connection
            .query_row(
                &format!("{ACCOUNT_SELECT} WHERE username = ?1 COLLATE NOCASE"),
                params![username],
                account_from_row,
            )
            .optional()
            .map_err(|error| format!("无法读取账户：{error}"))
    })
}

pub(super) fn account_by_id(state: &AppState, user_id: &str) -> Result<Option<Account>, String> {
    state.database.with_connection(|connection| {
        connection
            .query_row(
                &format!("{ACCOUNT_SELECT} WHERE id = ?1"),
                params![user_id],
                account_from_row,
            )
            .optional()
            .map_err(|error| format!("无法读取账户：{error}"))
    })
}

pub(super) fn list_accounts(state: &AppState) -> Result<Vec<Account>, String> {
    state.database.with_connection(|connection| {
        let mut statement = connection
            .prepare(&format!("{ACCOUNT_SELECT} ORDER BY created_at, username"))
            .map_err(|error| format!("无法查询账户：{error}"))?;
        let rows = statement
            .query_map([], account_from_row)
            .map_err(|error| format!("无法查询账户：{error}"))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| format!("无法读取账户列表：{error}"))
    })
}

fn account_count(state: &AppState) -> Result<usize, String> {
    state.database.with_connection(|connection| {
        connection
            .query_row("SELECT COUNT(*) FROM auth_accounts", [], |row| {
                row.get::<_, i64>(0)
            })
            .map(|value| value.max(0) as usize)
            .map_err(|error| format!("无法统计账户：{error}"))
    })
}

fn create_account(
    state: &AppState,
    username: &str,
    password_hash: &str,
    role: &str,
    preset: &str,
    only_if_empty: bool,
) -> Result<Account, String> {
    let id = new_id("u");
    let timestamp = now_iso();
    let permission = normalize_book_permission(&json!({}));
    state.database.with_connection(|connection| {
        let transaction = connection
            .transaction()
            .map_err(|error| format!("无法开始账户事务：{error}"))?;
        let count: i64 = transaction
            .query_row("SELECT COUNT(*) FROM auth_accounts", [], |row| row.get(0))
            .map_err(|error| format!("无法统计账户：{error}"))?;
        if only_if_empty && count > 0 {
            return Err("已有管理员账户，公开注册已关闭".into());
        }
        let effective_role = if count == 0 { "admin" } else { role };
        transaction
            .execute(
                r#"
                INSERT INTO auth_accounts(
                    id, username, password_hash, role, disabled, avatar, preset,
                    learner_profile_json, book_permission_json, created_at, updated_at
                ) VALUES(?1, ?2, ?3, ?4, 0, '', ?5, '{}', ?6, ?7, ?7)
                "#,
                params![
                    id,
                    username,
                    password_hash,
                    effective_role,
                    preset,
                    permission.to_string(),
                    timestamp
                ],
            )
            .map_err(|error| {
                if error.to_string().contains("UNIQUE") {
                    "用户名已被使用".to_string()
                } else {
                    format!("无法创建账户：{error}")
                }
            })?;
        transaction
            .commit()
            .map_err(|error| format!("无法提交账户事务：{error}"))?;
        Ok(())
    })?;
    account_by_username(state, username)?.ok_or_else(|| "账户创建后无法读取".to_string())
}

fn create_session(
    state: &AppState,
    user_id: &str,
    device_credential_id: &str,
    device_session_nonce_hash: &str,
) -> Result<(String, i64), String> {
    let token = random_secret(32);
    let token_hash = hash_secret(&token);
    let max_age = token_hours(state) * 3600;
    let created = now_iso();
    state.database.with_connection(|connection| {
        connection
            .execute(
                r#"
                INSERT INTO auth_sessions(
                    token_hash, user_id, created_at, expires_at, last_seen_at,
                    device_credential_id, device_session_nonce_hash
                ) VALUES(?1, ?2, ?3, ?4, ?3, ?5, ?6)
                "#,
                params![
                    token_hash,
                    user_id,
                    created,
                    unix_now() + max_age,
                    device_credential_id,
                    device_session_nonce_hash
                ],
            )
            .map_err(|error| format!("无法创建登录会话：{error}"))?;
        Ok(())
    })?;
    Ok((token, max_age))
}

fn principal_for_token(state: &AppState, token: &str) -> Result<Option<Principal>, String> {
    let token_hash = hash_secret(token);
    let row = state.database.with_connection(|connection| {
        connection
            .query_row(
                r#"
                SELECT a.id, a.username, a.role, a.disabled, a.avatar, a.preset,
                       s.device_credential_id, s.device_session_nonce_hash
                  FROM auth_sessions s
                  JOIN auth_accounts a ON a.id = s.user_id
                 WHERE s.token_hash = ?1 AND s.expires_at > ?2 AND a.disabled = 0
                "#,
                params![token_hash, unix_now()],
                |row| {
                    Ok(Principal {
                        user_id: row.get(0)?,
                        username: row.get(1)?,
                        role: row.get(2)?,
                        disabled: false,
                        avatar: row.get(4)?,
                        preset: row.get(5)?,
                        device_credential_id: row.get(6)?,
                        device_session_nonce_hash: row.get(7)?,
                    })
                },
            )
            .optional()
            .map_err(|error| format!("无法校验登录会话：{error}"))
    })?;
    let Some(principal) = row else {
        return Ok(None);
    };
    if !principal.device_credential_id.is_empty() && !device_session_active(state, &principal)? {
        return Ok(None);
    }
    let _ = state.database.with_connection(|connection| {
        connection
            .execute(
                "UPDATE auth_sessions SET last_seen_at = ?1 WHERE token_hash = ?2",
                params![now_iso(), hash_secret(token)],
            )
            .map_err(|error| format!("无法更新登录会话：{error}"))
    });
    Ok(Some(principal))
}

pub fn require_admin(principal: &Principal) -> ApiResult<()> {
    if principal.is_admin() {
        Ok(())
    } else {
        Err(ApiError::forbidden("需要管理员权限"))
    }
}

fn session_response(
    state: &AppState,
    token: &str,
    max_age: i64,
    body: Value,
) -> ApiResult<Response> {
    let secure = cookie_secure(state);
    let cookie = format!(
        "{COOKIE_NAME}={token}; Path=/; Max-Age={max_age}; HttpOnly; SameSite=Lax{}",
        if secure { "; Secure" } else { "" }
    );
    let mut response = Json(body).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie)
            .map_err(|error| ApiError::internal(format!("无法生成登录 Cookie：{error}")))?,
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

fn clear_session_response(body: Value) -> Response {
    let cookie = format!(
        "{COOKIE_NAME}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax; Expires=Thu, 01 Jan 1970 00:00:00 GMT"
    );
    let mut response = Json(body).into_response();
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

fn learning_policy(state: &AppState, user_id: &str) -> Result<Option<Value>, String> {
    let grant = state.database.with_connection(|connection| {
        connection
            .query_row(
                "SELECT payload_json FROM user_grants WHERE user_id = ?1",
                params![user_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| format!("无法读取资源授予：{error}"))
    })?;
    Ok(grant
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|value| value.get("learning_policy").cloned())
        .filter(|value| !value.is_null()))
}

async fn auth_status(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let enabled = auth_enabled(&state)?;
    if !enabled {
        return Ok(Json(json!({
            "enabled": false,
            "authenticated": true,
            "user_id": "local-admin",
            "username": "local",
            "role": "admin",
            "is_admin": true,
            "preset": "standard",
            "avatar": "",
            "learning_policy": null
        })));
    }
    let principal = token_from_headers(&headers)
        .map(|token| principal_for_token(&state, &token))
        .transpose()?
        .flatten();
    let Some(principal) = principal else {
        return Ok(Json(json!({
            "enabled": true,
            "authenticated": false,
            "is_admin": false,
            "avatar": "",
            "preset": null,
            "learning_policy": null
        })));
    };
    let policy = learning_policy(&state, &principal.user_id)?;
    let preset = if policy.is_some() && principal.preset != "learner" {
        "learner"
    } else {
        principal.preset.as_str()
    };
    Ok(Json(json!({
        "enabled": true,
        "authenticated": true,
        "user_id": principal.user_id,
        "username": principal.username,
        "role": principal.role,
        "is_admin": principal.is_admin(),
        "avatar": principal.avatar,
        "preset": preset,
        "learning_policy": policy
    })))
}

async fn auth_login(
    State(state): State<AppState>,
    Json(body): Json<LoginRequest>,
) -> ApiResult<Response> {
    if !auth_enabled(&state)? {
        return Ok(Json(json!({"ok": true, "message": "认证已关闭，无需登录"})).into_response());
    }
    let account = account_by_username(&state, body.username.trim())?
        .ok_or_else(|| ApiError::unauthorized("用户名或密码错误"))?;
    if account.disabled || !verify_password(&body.password, &account.password_hash).await? {
        return Err(ApiError::unauthorized("用户名或密码错误"));
    }
    let (token, max_age) = create_session(&state, &account.id, "", "")?;
    session_response(
        &state,
        &token,
        max_age,
        json!({
            "ok": true,
            "user_id": account.id,
            "username": account.username,
            "role": account.role,
            "is_admin": account.role == "admin"
        }),
    )
}

async fn auth_logout(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Response> {
    if let Some(token) = token_from_headers(&headers) {
        let token_hash = hash_secret(&token);
        state.database.with_connection(|connection| {
            connection
                .execute(
                    "DELETE FROM auth_sessions WHERE token_hash = ?1",
                    params![token_hash],
                )
                .map_err(|error| format!("无法注销登录会话：{error}"))?;
            Ok(())
        })?;
    }
    Ok(clear_session_response(json!({"ok": true})))
}

async fn auth_register(
    State(state): State<AppState>,
    Json(body): Json<RegisterRequest>,
) -> ApiResult<Response> {
    if !auth_enabled(&state)? {
        return Err(ApiError::bad_request("认证未启用，无法注册账户"));
    }
    let username = validate_username(&body.username)?;
    validate_password(&body.password)?;
    if account_count(&state)? > 0 {
        return Err(ApiError::forbidden("公开注册已关闭，请由管理员创建账户"));
    }
    let password_hash = hash_password(&body.password).await?;
    let account = create_account(&state, &username, &password_hash, "user", "standard", true)
        .map_err(|error| {
            if error.contains("用户名") {
                ApiError::conflict(error)
            } else if error.contains("公开注册") {
                ApiError::forbidden(error)
            } else {
                ApiError::internal(error)
            }
        })?;
    let (token, max_age) = create_session(&state, &account.id, "", "")?;
    session_response(
        &state,
        &token,
        max_age,
        json!({
            "ok": true,
            "user_id": account.id,
            "username": account.username,
            "role": account.role,
            "is_first_user": true,
            "is_admin": account.role == "admin"
        }),
    )
}

async fn auth_is_first_user(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let enabled = auth_enabled(&state)?;
    Ok(Json(json!({
        "is_first_user": enabled && account_count(&state)? == 0
    })))
}

async fn auth_profile(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
) -> ApiResult<Json<Value>> {
    if !auth_enabled(&state)? {
        return Ok(Json(json!({
            "id": "local-admin",
            "username": "local",
            "role": "admin",
            "created_at": "",
            "disabled": false,
            "avatar": "",
            "preset": "standard"
        })));
    }
    let account = account_by_id(&state, &principal.user_id)?
        .ok_or_else(|| ApiError::not_found("用户不存在"))?;
    Ok(Json(account.public()))
}

async fn update_auth_profile(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Json(body): Json<UpdateProfileRequest>,
) -> ApiResult<Json<Value>> {
    if !auth_enabled(&state)? {
        return Err(ApiError::bad_request("认证未启用，个人资料不可用"));
    }
    let avatar = body.avatar.trim();
    if !avatar.is_empty() && !valid_icon_marker(avatar) {
        return Err(ApiError::unprocessable(
            "头像标记必须为空或 icon:<名称>:<颜色>",
        ));
    }
    update_account_fields(&state, &principal.user_id, &[("avatar", avatar.into())])?;
    delete_avatar_file(&state, &principal.user_id);
    Ok(Json(json!({"ok": true, "avatar": avatar})))
}

fn valid_icon_marker(value: &str) -> bool {
    let mut parts = value.split(':');
    let Some("icon") = parts.next() else {
        return false;
    };
    let Some(name) = parts.next() else {
        return false;
    };
    let Some(color) = parts.next() else {
        return false;
    };
    parts.next().is_none()
        && (1..=32).contains(&name.len())
        && (1..=32).contains(&color.len())
        && name.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
        && color.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
}

pub(super) fn update_account_fields(
    state: &AppState,
    user_id: &str,
    fields: &[(&str, Value)],
) -> Result<(), String> {
    let allowed = [
        "avatar",
        "role",
        "preset",
        "disabled",
        "learner_profile_json",
        "book_permission_json",
        "password_hash",
    ];
    if fields.iter().any(|(field, _)| !allowed.contains(field)) {
        return Err("拒绝写入未知账户字段".into());
    }
    state.database.with_connection(|connection| {
        for (field, value) in fields {
            let value = match value {
                Value::String(value) => value.clone(),
                Value::Bool(value) => i64::from(*value).to_string(),
                other => other.to_string(),
            };
            connection
                .execute(
                    &format!(
                        "UPDATE auth_accounts SET {field} = ?1, updated_at = ?2 WHERE id = ?3"
                    ),
                    params![value, now_iso(), user_id],
                )
                .map_err(|error| format!("无法更新账户：{error}"))?;
        }
        Ok(())
    })
}

fn avatar_root(state: &AppState) -> PathBuf {
    state.paths.data.join("auth").join("avatars")
}

fn avatar_path(state: &AppState, user_id: &str) -> Option<PathBuf> {
    if user_id.is_empty()
        || user_id.len() > 64
        || !user_id.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '_' || character == '-'
        })
    {
        return None;
    }
    let root = avatar_root(state);
    for extension in ["png", "jpg", "webp"] {
        let candidate = root.join(format!("{user_id}.{extension}"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn delete_avatar_file(state: &AppState, user_id: &str) {
    let root = avatar_root(state);
    for extension in ["png", "jpg", "webp"] {
        let _ = std::fs::remove_file(root.join(format!("{user_id}.{extension}")));
    }
}

fn sniff_image(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if data.starts_with(b"\xff\xd8\xff") {
        Some("jpg")
    } else if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        Some("webp")
    } else {
        None
    }
}

async fn upload_avatar(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    mut multipart: Multipart,
) -> ApiResult<Json<Value>> {
    if !auth_enabled(&state)? {
        return Err(ApiError::bad_request("认证未启用，头像不可用"));
    }
    let account = account_by_id(&state, &principal.user_id)?
        .ok_or_else(|| ApiError::not_found("用户不存在"))?;
    let mut bytes = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|error| ApiError::bad_request(format!("无法读取头像上传：{error}")))?
    {
        if field.name() == Some("file") {
            bytes = field
                .bytes()
                .await
                .map_err(|error| ApiError::bad_request(format!("无法读取头像内容：{error}")))?
                .to_vec();
            break;
        }
    }
    if bytes.is_empty() {
        return Err(ApiError::bad_request("未收到头像文件"));
    }
    if bytes.len() > MAX_AVATAR_BYTES {
        return Err(ApiError {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            code: "payload_too_large",
            message: "头像图片不能超过 1 MB".into(),
            retryable: false,
        });
    }
    let extension = sniff_image(&bytes).ok_or_else(|| ApiError {
        status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
        code: "unsupported_media_type",
        message: "头像必须是 PNG、JPEG 或 WebP 图片".into(),
        retryable: false,
    })?;
    let version = account
        .avatar
        .strip_prefix("img:")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0)
        + 1;
    let marker = format!("img:{version}");
    let root = avatar_root(&state);
    tokio::fs::create_dir_all(&root)
        .await
        .map_err(|error| ApiError::internal(format!("无法创建头像目录：{error}")))?;
    delete_avatar_file(&state, &principal.user_id);
    tokio::fs::write(
        root.join(format!("{}.{}", principal.user_id, extension)),
        &bytes,
    )
    .await
    .map_err(|error| ApiError::internal(format!("无法保存头像：{error}")))?;
    update_account_fields(
        &state,
        &principal.user_id,
        &[("avatar", Value::String(marker.clone()))],
    )?;
    Ok(Json(json!({"ok": true, "avatar": marker})))
}

async fn remove_avatar(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
) -> ApiResult<Json<Value>> {
    if !auth_enabled(&state)? {
        return Err(ApiError::bad_request("认证未启用，头像不可用"));
    }
    delete_avatar_file(&state, &principal.user_id);
    update_account_fields(
        &state,
        &principal.user_id,
        &[("avatar", Value::String(String::new()))],
    )?;
    Ok(Json(json!({"ok": true, "avatar": ""})))
}

async fn get_avatar(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
) -> ApiResult<Response> {
    let target = avatar_path(&state, &user_id).ok_or_else(|| ApiError::not_found("头像不存在"))?;
    let bytes = tokio::fs::read(&target)
        .await
        .map_err(|error| ApiError::internal(format!("无法读取头像：{error}")))?;
    let content_type = match target.extension().and_then(|value| value.to_str()) {
        Some("png") => "image/png",
        Some("jpg") => "image/jpeg",
        Some("webp") => "image/webp",
        _ => "application/octet-stream",
    };
    let mut response = bytes.into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=86400"),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    Ok(response)
}

async fn list_users(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
) -> ApiResult<Json<Value>> {
    require_admin(&principal)?;
    let users = list_accounts(&state)?
        .iter()
        .map(Account::public)
        .collect::<Vec<_>>();
    Ok(Json(Value::Array(users)))
}

async fn admin_create_user(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Json(body): Json<AdminCreateUserRequest>,
) -> ApiResult<Response> {
    if !auth_enabled(&state)? {
        return Err(ApiError::bad_request("认证未启用，无法创建用户"));
    }
    require_admin(&principal)?;
    create_ordinary_user(&state, &body).await
}

async fn create_ordinary_user(
    state: &AppState,
    body: &AdminCreateUserRequest,
) -> ApiResult<Response> {
    let username = validate_username(&body.username)?;
    validate_password(&body.password)?;
    let preset = validate_preset(&body.preset)?;
    if account_by_username(state, &username)?.is_some() {
        return Err(ApiError::conflict("用户名已被使用"));
    }
    let password_hash = hash_password(&body.password).await?;
    let account = create_account(state, &username, &password_hash, "user", &preset, false)
        .map_err(|error| {
            if error.contains("用户名") {
                ApiError::conflict(error)
            } else {
                ApiError::internal(error)
            }
        })?;
    if account.role == "admin" {
        let _ = delete_account(state, &account.id);
        return Err(ApiError::internal("管理员端点只能创建普通用户"));
    }
    if preset == "learner" {
        save_grant(state, &account.id, &learner_grant(&account.id))?;
    }
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "ok": true,
            "user_id": account.id,
            "username": account.username,
            "role": account.role,
            "is_admin": false,
            "preset": account.preset
        })),
    )
        .into_response())
}

async fn import_users(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    mut multipart: Multipart,
) -> ApiResult<Json<Value>> {
    if !auth_enabled(&state)? {
        return Err(ApiError::bad_request("认证未启用，无法导入用户"));
    }
    require_admin(&principal)?;
    let mut bytes = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|error| ApiError::bad_request(format!("无法读取 CSV 上传：{error}")))?
    {
        if field.name() == Some("file") {
            bytes = field
                .bytes()
                .await
                .map_err(|error| ApiError::bad_request(format!("无法读取 CSV 内容：{error}")))?
                .to_vec();
            break;
        }
    }
    if bytes.is_empty() {
        return Err(ApiError::bad_request("未收到 CSV 文件"));
    }
    if bytes.len() > MAX_IMPORT_BYTES {
        return Err(ApiError {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            code: "payload_too_large",
            message: "用户导入文件不能超过 2 MB".into(),
            retryable: false,
        });
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| ApiError::bad_request("用户导入文件必须使用 UTF-8 编码"))?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let mut reader = csv::ReaderBuilder::new()
        .trim(csv::Trim::All)
        .from_reader(text.as_bytes());
    let headers = reader
        .headers()
        .map_err(|error| ApiError::unprocessable(format!("无法读取 CSV 表头：{error}")))?
        .iter()
        .map(|value| value.trim().to_ascii_lowercase())
        .collect::<Vec<_>>();
    if headers != ["username", "password", "preset"] {
        return Err(ApiError::unprocessable(
            "CSV 表头必须严格为：username,password,preset",
        ));
    }
    let records = reader
        .records()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| ApiError::unprocessable(format!("无法解析 CSV 行：{error}")))?;
    if records.is_empty() {
        return Err(ApiError::unprocessable("CSV 中没有用户行"));
    }
    if records.len() > MAX_IMPORT_ROWS {
        return Err(ApiError {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            code: "payload_too_large",
            message: format!("单次最多导入 {MAX_IMPORT_ROWS} 个用户"),
            retryable: false,
        });
    }
    let mut results = Vec::new();
    for (index, record) in records.iter().enumerate() {
        let row = index + 2;
        let body = AdminCreateUserRequest {
            username: record.get(0).unwrap_or_default().trim().to_string(),
            password: record.get(1).unwrap_or_default().to_string(),
            preset: record
                .get(2)
                .unwrap_or_default()
                .trim()
                .to_string()
                .if_empty("standard"),
        };
        match create_ordinary_user(&state, &body).await {
            Ok(_) => {
                let account = account_by_username(&state, &body.username)?;
                results.push(json!({
                    "row": row,
                    "username": body.username,
                    "ok": true,
                    "user": account.map(|account| json!({
                        "user_id": account.id,
                        "username": account.username,
                        "role": account.role,
                        "is_admin": false,
                        "preset": account.preset
                    }))
                }));
            }
            Err(error) => results.push(json!({
                "row": row,
                "username": body.username,
                "ok": false,
                "error": error.message
            })),
        }
    }
    let created = results
        .iter()
        .filter(|item| item.get("ok").and_then(Value::as_bool) == Some(true))
        .count();
    let failed = results.len() - created;
    Ok(Json(json!({
        "ok": failed == 0,
        "created_count": created,
        "failed_count": failed,
        "results": results
    })))
}

trait IfEmpty {
    fn if_empty(self, fallback: &str) -> String;
}

impl IfEmpty for String {
    fn if_empty(self, fallback: &str) -> String {
        if self.is_empty() {
            fallback.to_string()
        } else {
            self
        }
    }
}

async fn delete_user(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(username): Path<String>,
) -> ApiResult<Json<Value>> {
    require_admin(&principal)?;
    if principal.username.eq_ignore_ascii_case(&username) {
        return Err(ApiError::bad_request("不能删除当前登录账户"));
    }
    delete_user_by_username(&state, &username)?;
    Ok(Json(json!({"ok": true})))
}

fn delete_user_by_username(state: &AppState, username: &str) -> ApiResult<Account> {
    let account =
        account_by_username(state, username)?.ok_or_else(|| ApiError::not_found("用户不存在"))?;
    delete_account(state, &account.id)?;
    delete_avatar_file(state, &account.id);
    Ok(account)
}

fn delete_account(state: &AppState, user_id: &str) -> Result<bool, String> {
    state.database.with_connection(|connection| {
        let changed = connection
            .execute("DELETE FROM auth_accounts WHERE id = ?1", params![user_id])
            .map_err(|error| format!("无法删除账户：{error}"))?;
        Ok(changed > 0)
    })
}

async fn batch_delete_users(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Json(body): Json<BatchDeleteRequest>,
) -> ApiResult<Json<Value>> {
    require_admin(&principal)?;
    if body.usernames.is_empty() || body.usernames.len() > MAX_IMPORT_ROWS {
        return Err(ApiError::unprocessable("批量删除需要 1-500 个用户名"));
    }
    let mut seen = std::collections::HashSet::new();
    let mut results = Vec::new();
    for username in body.usernames {
        let username = username.trim().to_string();
        if username.is_empty() || !seen.insert(username.to_ascii_lowercase()) {
            continue;
        }
        let error = if principal.username.eq_ignore_ascii_case(&username) {
            Some("不能删除当前登录账户".to_string())
        } else {
            delete_user_by_username(&state, &username)
                .err()
                .map(|error| error.message)
        };
        results.push(json!({
            "username": username,
            "ok": error.is_none(),
            "error": error
        }));
    }
    let deleted = results
        .iter()
        .filter(|item| item.get("ok").and_then(Value::as_bool) == Some(true))
        .count();
    let failed = results.len() - deleted;
    Ok(Json(json!({
        "ok": failed == 0,
        "deleted_count": deleted,
        "failed_count": failed,
        "results": results
    })))
}

async fn update_user_role(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(username): Path<String>,
    Json(body): Json<SetRoleRequest>,
) -> ApiResult<Json<Value>> {
    require_admin(&principal)?;
    if principal.username.eq_ignore_ascii_case(&username) {
        return Err(ApiError::bad_request("不能修改当前登录账户的角色"));
    }
    if !matches!(body.role.as_str(), "admin" | "user") {
        return Err(ApiError::unprocessable("角色必须是 admin 或 user"));
    }
    let account =
        account_by_username(&state, &username)?.ok_or_else(|| ApiError::not_found("用户不存在"))?;
    update_account_fields(
        &state,
        &account.id,
        &[("role", Value::String(body.role.clone()))],
    )?;
    Ok(Json(
        json!({"ok": true, "username": username, "role": body.role}),
    ))
}

fn require_learner(account: &Account) -> ApiResult<()> {
    if account.role == "admin" || account.preset != "learner" {
        Err(ApiError::forbidden("需要学习者账户"))
    } else {
        Ok(())
    }
}

fn normalize_learner_profile(value: &Value) -> ApiResult<Value> {
    let Some(input) = value.as_object() else {
        return Err(ApiError::unprocessable("学习者资料必须是对象"));
    };
    let mut output = Map::new();
    if let Some(age) = input.get("age") {
        if !age.is_null() {
            let age = age
                .as_i64()
                .ok_or_else(|| ApiError::unprocessable("年龄必须是整数"))?;
            if !(3..=120).contains(&age) {
                return Err(ApiError::unprocessable("年龄必须在 3-120 之间"));
            }
            output.insert("age".into(), json!(age));
        }
    }
    for key in [
        "grade_level",
        "curriculum",
        "language",
        "reading_level",
        "explanation_style",
    ] {
        if let Some(value) = input.get(key) {
            if !value.is_null() {
                let value = value
                    .as_str()
                    .ok_or_else(|| ApiError::unprocessable(format!("{key} 必须是字符串")))?;
                if value.chars().count() > 80 {
                    return Err(ApiError::unprocessable(format!("{key} 不能超过 80 个字符")));
                }
                output.insert(key.into(), Value::String(value.trim().to_string()));
            }
        }
    }
    Ok(Value::Object(output))
}

async fn get_own_learner_profile(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
) -> ApiResult<Json<Value>> {
    let account = account_by_id(&state, &principal.user_id)?
        .ok_or_else(|| ApiError::not_found("用户不存在"))?;
    require_learner(&account)?;
    Ok(Json(json!({"learner_profile": account.learner_profile})))
}

async fn put_own_learner_profile(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let account = account_by_id(&state, &principal.user_id)?
        .ok_or_else(|| ApiError::not_found("用户不存在"))?;
    require_learner(&account)?;
    let profile = normalize_learner_profile(&body)?;
    update_account_fields(
        &state,
        &account.id,
        &[("learner_profile_json", Value::String(profile.to_string()))],
    )?;
    Ok(Json(json!({"learner_profile": profile})))
}

async fn get_learner_profile(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(username): Path<String>,
) -> ApiResult<Json<Value>> {
    require_admin(&principal)?;
    let account =
        account_by_username(&state, &username)?.ok_or_else(|| ApiError::not_found("用户不存在"))?;
    require_learner(&account)?;
    Ok(Json(json!({"learner_profile": account.learner_profile})))
}

async fn put_learner_profile(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(username): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    require_admin(&principal)?;
    let account =
        account_by_username(&state, &username)?.ok_or_else(|| ApiError::not_found("用户不存在"))?;
    require_learner(&account)?;
    let profile = normalize_learner_profile(&body)?;
    update_account_fields(
        &state,
        &account.id,
        &[("learner_profile_json", Value::String(profile.to_string()))],
    )?;
    Ok(Json(json!({"learner_profile": profile})))
}

pub(super) fn empty_grant(user_id: &str) -> Value {
    json!({
        "version": 2,
        "user_id": user_id,
        "models": {"llm": []},
        "knowledge_bases": [],
        "skills": [],
        "partners": [],
        "enabled_tools": null,
        "mcp_tools": null,
        "cli_apps": null,
        "exec_enabled": null,
        "learning_policy": null
    })
}

fn normalize_string_list(value: Option<&Value>) -> Option<Vec<Value>> {
    value.and_then(Value::as_array).map(|items| {
        items
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(|item| Value::String(item.to_string()))
            .collect()
    })
}

fn normalize_learning_policy(value: Option<&Value>) -> Value {
    let Some(input) = value else {
        return Value::Null;
    };
    if !input.is_object() {
        return json!({});
    }
    let capabilities = normalize_string_list(input.get("allowed_capabilities")).unwrap_or_default();
    let has_reading = input.get("reading").is_some_and(Value::is_object);
    let reading = input.get("reading").unwrap_or(&Value::Null);
    let material_ids = normalize_string_list(reading.get("material_ids")).unwrap_or_else(|| {
        if has_reading {
            Vec::new()
        } else {
            vec![Value::String("*".into())]
        }
    });
    let extensions = normalize_string_list(reading.get("extensions")).unwrap_or_else(|| {
        if has_reading {
            Vec::new()
        } else {
            [
                "read_aloud",
                "guided_learning",
                "vocabulary",
                "quiz",
                "translation",
            ]
            .into_iter()
            .map(|value| Value::String(value.into()))
            .collect()
        }
    });
    let mut output = Map::from_iter([
        (
            "age_band".into(),
            Value::String(
                input
                    .get("age_band")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
            ),
        ),
        (
            "locked_persona".into(),
            Value::String(
                input
                    .get("locked_persona")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
            ),
        ),
        ("allowed_capabilities".into(), Value::Array(capabilities)),
        (
            "default_capability".into(),
            Value::String(
                input
                    .get("default_capability")
                    .and_then(Value::as_str)
                    .unwrap_or("chat")
                    .trim()
                    .to_string(),
            ),
        ),
    ]);
    if let Some(surfaces) = normalize_string_list(input.get("allowed_surfaces")) {
        output.insert("allowed_surfaces".into(), Value::Array(surfaces));
    }
    output.insert(
        "reading".into(),
        json!({
            "allow_upload": reading
                .get("allow_upload")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            "material_ids": material_ids,
            "extensions": extensions
        }),
    );
    Value::Object(output)
}

pub(super) fn normalize_grant(user_id: &str, payload: &Value) -> Value {
    let mut grant = empty_grant(user_id);
    if !payload.is_object() {
        return grant;
    }
    grant["user_id"] = Value::String(user_id.to_string());
    let profiles = payload
        .pointer("/models/llm")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| item.is_object())
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    grant["models"] = json!({"llm": profiles});
    for key in ["knowledge_bases", "skills", "partners"] {
        let values = payload
            .get(key)
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter(|item| item.is_object())
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        grant[key] = Value::Array(values);
    }
    for key in ["enabled_tools", "mcp_tools", "cli_apps"] {
        grant[key] = normalize_string_list(payload.get(key))
            .map(Value::Array)
            .unwrap_or(Value::Null);
    }
    grant["exec_enabled"] = payload
        .get("exec_enabled")
        .and_then(Value::as_bool)
        .map(Value::Bool)
        .unwrap_or(Value::Null);
    grant["learning_policy"] = normalize_learning_policy(payload.get("learning_policy"));
    grant
}

pub(super) fn learner_grant(user_id: &str) -> Value {
    json!({
        "version": 2,
        "user_id": user_id,
        "models": {"llm": []},
        "knowledge_bases": [],
        "skills": [],
        "partners": [],
        "enabled_tools": [],
        "mcp_tools": [],
        "cli_apps": [],
        "exec_enabled": false,
        "learning_policy": {
            "age_band": "9-12",
            "locked_persona": "teacher",
            "allowed_capabilities": ["chat", "immersive_reading"],
            "default_capability": "immersive_reading",
            "allowed_surfaces": ["chat", "reading"],
            "reading": {
                "allow_upload": false,
                "material_ids": [],
                "extensions": []
            }
        }
    })
}

pub(super) fn save_grant(state: &AppState, user_id: &str, grant: &Value) -> ApiResult<()> {
    validate_grant(grant)?;
    state
        .database
        .with_connection(|connection| {
            connection
                .execute(
                    r#"
                INSERT INTO user_grants(user_id, payload_json, updated_at)
                VALUES(?1, ?2, ?3)
                ON CONFLICT(user_id) DO UPDATE SET
                    payload_json = excluded.payload_json,
                    updated_at = excluded.updated_at
                "#,
                    params![user_id, grant.to_string(), now_iso()],
                )
                .map_err(|error| format!("无法保存资源授予：{error}"))?;
            Ok(())
        })
        .map_err(ApiError::internal)
}

pub(super) fn validate_grant(grant: &Value) -> ApiResult<()> {
    fn walk(value: &Value, trail: &str) -> Result<(), String> {
        match value {
            Value::Object(object) => {
                for (key, child) in object {
                    let lowered = key.to_ascii_lowercase();
                    if ["api_key", "secret", "password", "token", "path", "base_url"]
                        .contains(&lowered.as_str())
                        || lowered.ends_with("_key")
                    {
                        return Err(format!("资源授予不能包含敏感或路径字段：{trail}.{key}"));
                    }
                    walk(child, &format!("{trail}.{key}"))?;
                }
            }
            Value::Array(items) => {
                for (index, child) in items.iter().enumerate() {
                    walk(child, &format!("{trail}[{index}]"))?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    walk(grant, "grant").map_err(ApiError::unprocessable)?;
    if let Some(policy) = grant
        .get("learning_policy")
        .filter(|value| !value.is_null())
    {
        let age = policy.get("age_band").and_then(Value::as_str);
        if !matches!(age, Some("6-8" | "9-12" | "13-15")) {
            return Err(ApiError::unprocessable("学习策略年龄区间无效"));
        }
        if !matches!(
            policy.get("locked_persona").and_then(Value::as_str),
            Some("teacher")
        ) {
            return Err(ApiError::unprocessable("学习策略锁定角色必须是 teacher"));
        }
        let capabilities = policy
            .get("allowed_capabilities")
            .and_then(Value::as_array)
            .filter(|items| !items.is_empty())
            .ok_or_else(|| ApiError::unprocessable("学习能力列表不能为空"))?;
        if capabilities
            .iter()
            .any(|item| !matches!(item.as_str(), Some("chat") | Some("immersive_reading")))
        {
            return Err(ApiError::unprocessable("学习能力列表包含不支持的值"));
        }
        let default = policy.get("default_capability").and_then(Value::as_str);
        if !default
            .is_some_and(|value| capabilities.iter().any(|item| item.as_str() == Some(value)))
        {
            return Err(ApiError::unprocessable("默认学习能力必须包含在许可列表中"));
        }
        let surfaces = policy
            .get("allowed_surfaces")
            .and_then(Value::as_array)
            .filter(|items| !items.is_empty())
            .ok_or_else(|| ApiError::unprocessable("学习入口列表不能为空"))?;
        if surfaces.iter().any(|item| {
            !matches!(
                item.as_str(),
                Some("books") | Some("chat") | Some("reading")
            )
        }) {
            return Err(ApiError::unprocessable("学习入口列表包含不支持的值"));
        }
        let reading = policy
            .get("reading")
            .and_then(Value::as_object)
            .ok_or_else(|| ApiError::unprocessable("阅读策略必须是对象"))?;
        if reading
            .get("material_ids")
            .is_some_and(|value| !value.is_array())
        {
            return Err(ApiError::unprocessable("阅读材料列表必须是数组"));
        }
        if reading
            .get("extensions")
            .is_some_and(|value| !value.is_array())
        {
            return Err(ApiError::unprocessable("阅读扩展列表必须是数组"));
        }
        if let Some(extensions) = reading.get("extensions").and_then(Value::as_array) {
            let extension_ok = |value: &Value| {
                value.as_str().is_some_and(|value| {
                    let mut characters = value.chars();
                    value.len() <= 64
                        && characters
                            .next()
                            .is_some_and(|character| character.is_ascii_lowercase())
                        && characters.all(|character| {
                            character.is_ascii_lowercase()
                                || character.is_ascii_digit()
                                || matches!(character, '_' | '-')
                        })
                })
            };
            if extensions.iter().any(|value| !extension_ok(value)) {
                return Err(ApiError::unprocessable("阅读扩展标识无效"));
            }
        }
    }
    Ok(())
}

pub(super) fn load_grant_value(state: &AppState, user_id: &str) -> Result<Value, String> {
    let raw = state.database.with_connection(|connection| {
        connection
            .query_row(
                "SELECT payload_json FROM user_grants WHERE user_id = ?1",
                params![user_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| format!("无法读取资源授予：{error}"))
    })?;
    Ok(raw
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .map(|value| normalize_grant(user_id, &value))
        .unwrap_or_else(|| empty_grant(user_id)))
}

async fn list_devices(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Query(query): Query<DeviceListQuery>,
) -> ApiResult<Json<Value>> {
    require_admin(&principal)?;
    if !auth_enabled(&state)? {
        return Err(ApiError::bad_request("设备凭据需要启用本地认证"));
    }
    let devices = list_device_values(&state, query.user_id.as_deref(), query.include_revoked)?;
    let users = list_accounts(&state)?;
    let names = users
        .into_iter()
        .map(|user| (user.id, user.username))
        .collect::<std::collections::HashMap<_, _>>();
    let devices = devices
        .into_iter()
        .map(|mut device| {
            let user_id = device
                .get("user_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            device["username"] = Value::String(names.get(user_id).cloned().unwrap_or_default());
            device
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({"devices": devices})))
}

async fn create_device(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Json(body): Json<DeviceCreateRequest>,
) -> ApiResult<Json<Value>> {
    require_admin(&principal)?;
    if !auth_enabled(&state)? {
        return Err(ApiError::bad_request("设备凭据需要启用本地认证"));
    }
    let account =
        account_by_id(&state, &body.user_id)?.ok_or_else(|| ApiError::not_found("用户不存在"))?;
    require_learner(&account)?;
    if !(1..=MAX_DEVICE_DAYS).contains(&body.expires_in_days) {
        return Err(ApiError::unprocessable("有效期必须在 1-365 天之间"));
    }
    if !(5..=1440).contains(&body.daily_limit_minutes) {
        return Err(ApiError::unprocessable("每日时长必须在 5-1440 分钟之间"));
    }
    let device_name = body.device_name.trim();
    if device_name.is_empty() {
        return Err(ApiError::unprocessable("设备名称不能为空"));
    }
    if device_name.chars().count() > 80 {
        return Err(ApiError::unprocessable("设备名称不能超过 80 个字符"));
    }
    let id = new_id("dc");
    let pairing_code = format!("dc_{}", random_secret(24));
    let pin = format!("{:06}", rand::thread_rng().gen_range(0..1_000_000_u32));
    let pin_hash = hash_password(&pin).await?;
    let created = now_iso();
    let expires = chrono::Utc::now() + chrono::Duration::days(body.expires_in_days);
    state.database.with_connection(|connection| {
        connection
            .execute(
                r#"
                INSERT INTO auth_device_credentials(
                    id, user_id, device_name, pairing_code_hash, pin_hash,
                    created_at, expires_at, daily_limit_minutes
                ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                "#,
                params![
                    id,
                    account.id,
                    device_name,
                    hash_secret(&pairing_code),
                    pin_hash,
                    created,
                    expires.to_rfc3339(),
                    body.daily_limit_minutes
                ],
            )
            .map_err(|error| format!("无法创建设备凭据：{error}"))?;
        Ok(())
    })?;
    let device = device_by_id(&state, &id)?
        .map(public_device)
        .ok_or_else(|| ApiError::internal("设备凭据创建后无法读取"))?;
    Ok(Json(json!({
        "device": device,
        "pairing_code": pairing_code,
        "pin": pin
    })))
}

async fn revoke_device(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(device_id): Path<String>,
) -> ApiResult<Json<Value>> {
    require_admin(&principal)?;
    let changed = state.database.with_connection(|connection| {
        connection
            .execute(
                r#"
                UPDATE auth_device_credentials
                   SET revoked_at = ?1, revoked_by = ?2
                 WHERE id = ?3 AND revoked_at IS NULL
                "#,
                params![now_iso(), principal.user_id, device_id],
            )
            .map_err(|error| format!("无法撤销设备凭据：{error}"))
    })?;
    if changed == 0 && device_by_id(&state, &device_id)?.is_none() {
        return Err(ApiError::not_found("设备凭据不存在"));
    }
    let device = device_by_id(&state, &device_id)?
        .map(public_device)
        .ok_or_else(|| ApiError::not_found("设备凭据不存在"))?;
    state.database.with_connection(|connection| {
        connection
            .execute(
                "DELETE FROM auth_sessions WHERE device_credential_id = ?1",
                params![device_id],
            )
            .map_err(|error| format!("无法清理设备会话：{error}"))?;
        Ok(())
    })?;
    Ok(Json(json!({"device": device, "ok": true})))
}

async fn device_login(
    State(state): State<AppState>,
    Json(body): Json<DeviceLoginRequest>,
) -> ApiResult<Response> {
    if !auth_enabled(&state)? {
        return Err(ApiError::bad_request("设备凭据需要启用本地认证"));
    }
    if body.pin.len() != 6 || body.pairing_code.len() < 8 {
        return Err(ApiError::unauthorized("设备凭据错误"));
    }
    let pairing_hash = hash_secret(&body.pairing_code);
    let device = state.database.with_connection(|connection| {
        connection
            .query_row(
                &format!("{DEVICE_SELECT} WHERE pairing_code_hash = ?1"),
                params![pairing_hash],
                device_from_row,
            )
            .optional()
            .map_err(|error| format!("无法读取设备凭据：{error}"))
    })?;
    let Some(device) = device else {
        return Err(ApiError::unauthorized("设备凭据错误"));
    };
    if !device_active_for_login(&state, &device)? {
        return Err(ApiError::unauthorized("设备凭据已失效"));
    }
    if !verify_password(&body.pin, &device.pin_hash).await? {
        record_failed_pin(&state, &device.id)?;
        return Err(ApiError::unauthorized("设备凭据错误"));
    }
    let account = account_by_id(&state, &device.user_id)?
        .ok_or_else(|| ApiError::unauthorized("设备账户不可用"))?;
    require_learner(&account).map_err(|_| ApiError::unauthorized("设备账户不可用"))?;
    let now = chrono::Utc::now();
    let session_nonce = random_secret(32);
    let nonce_hash = hash_secret(&session_nonce);
    state.database.with_connection(|connection| {
        connection
            .execute(
                r#"
                UPDATE auth_device_credentials
                   SET failed_pin_attempts = 0, pin_locked_until = '',
                       session_nonce_hash = ?1, last_login_at = ?2,
                       last_heartbeat_at = ?2
                 WHERE id = ?3
                "#,
                params![nonce_hash, now.to_rfc3339(), device.id],
            )
            .map_err(|error| format!("无法开始设备会话：{error}"))?;
        Ok(())
    })?;
    let (token, max_age) = create_session(&state, &account.id, &device.id, &nonce_hash)?;
    session_response(
        &state,
        &token,
        max_age,
        json!({
            "ok": true,
            "user_id": account.id,
            "username": account.username,
            "role": account.role,
            "is_admin": false,
            "device_credential_id": device.id
        }),
    )
}

#[derive(Clone, Debug)]
struct DeviceCredential {
    id: String,
    user_id: String,
    device_name: String,
    pin_hash: String,
    created_at: String,
    expires_at: String,
    daily_limit_minutes: i64,
    last_login_at: String,
    last_heartbeat_at: String,
    usage_day: String,
    used_seconds: i64,
    session_nonce_hash: String,
    failed_pin_attempts: i64,
    pin_locked_until: String,
    revoked_at: Option<String>,
    revoked_by: String,
}

const DEVICE_SELECT: &str = r#"
    SELECT id, user_id, device_name, pin_hash, created_at, expires_at,
           daily_limit_minutes, last_login_at, last_heartbeat_at, usage_day,
           used_seconds, session_nonce_hash, failed_pin_attempts,
           pin_locked_until, revoked_at, revoked_by
      FROM auth_device_credentials
"#;

fn device_from_row(row: &Row<'_>) -> rusqlite::Result<DeviceCredential> {
    Ok(DeviceCredential {
        id: row.get(0)?,
        user_id: row.get(1)?,
        device_name: row.get(2)?,
        pin_hash: row.get(3)?,
        created_at: row.get(4)?,
        expires_at: row.get(5)?,
        daily_limit_minutes: row.get(6)?,
        last_login_at: row.get(7)?,
        last_heartbeat_at: row.get(8)?,
        usage_day: row.get(9)?,
        used_seconds: row.get(10)?,
        session_nonce_hash: row.get(11)?,
        failed_pin_attempts: row.get(12)?,
        pin_locked_until: row.get(13)?,
        revoked_at: row.get(14)?,
        revoked_by: row.get(15)?,
    })
}

fn device_by_id(state: &AppState, device_id: &str) -> Result<Option<DeviceCredential>, String> {
    state.database.with_connection(|connection| {
        connection
            .query_row(
                &format!("{DEVICE_SELECT} WHERE id = ?1"),
                params![device_id],
                device_from_row,
            )
            .optional()
            .map_err(|error| format!("无法读取设备凭据：{error}"))
    })
}

fn list_device_values(
    state: &AppState,
    user_id: Option<&str>,
    include_revoked: bool,
) -> Result<Vec<Value>, String> {
    let devices = state.database.with_connection(|connection| {
        let mut statement = connection
            .prepare(&format!("{DEVICE_SELECT} ORDER BY created_at DESC"))
            .map_err(|error| format!("无法查询设备凭据：{error}"))?;
        let rows = statement
            .query_map([], device_from_row)
            .map_err(|error| format!("无法查询设备凭据：{error}"))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| format!("无法读取设备凭据：{error}"))
    })?;
    Ok(devices
        .into_iter()
        .filter(|device| {
            (include_revoked || device.revoked_at.is_none())
                && user_id.is_none_or(|user_id| device.user_id == user_id)
        })
        .map(public_device)
        .collect())
}

fn public_device(device: DeviceCredential) -> Value {
    json!({
        "id": device.id,
        "user_id": device.user_id,
        "device_name": device.device_name,
        "created_at": device.created_at,
        "expires_at": device.expires_at,
        "daily_limit_minutes": device.daily_limit_minutes,
        "last_login_at": device.last_login_at,
        "last_heartbeat_at": device.last_heartbeat_at,
        "usage_day": device.usage_day,
        "used_seconds": device.used_seconds,
        "revoked_at": device.revoked_at,
        "revoked_by": device.revoked_by
    })
}

fn parse_utc(value: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|value| value.with_timezone(&chrono::Utc))
}

fn day_string(now: chrono::DateTime<chrono::Utc>) -> String {
    now.date_naive().format("%Y-%m-%d").to_string()
}

fn daily_usage(device: &DeviceCredential, now: chrono::DateTime<chrono::Utc>) -> (String, i64) {
    let day = day_string(now);
    if device.usage_day == day {
        (day, device.used_seconds.max(0))
    } else {
        (day, 0)
    }
}

fn device_active_for_login(state: &AppState, device: &DeviceCredential) -> Result<bool, String> {
    if device.revoked_at.is_some()
        || parse_utc(&device.expires_at).is_none_or(|value| value <= chrono::Utc::now())
    {
        return Ok(false);
    }
    if let Some(locked) = parse_utc(&device.pin_locked_until) {
        if locked > chrono::Utc::now() {
            return Ok(false);
        }
    }
    let Some(account) = account_by_id(state, &device.user_id)? else {
        return Ok(false);
    };
    Ok(account.role != "admin" && !account.disabled && account.preset == "learner")
}

fn device_session_active(state: &AppState, principal: &Principal) -> Result<bool, String> {
    let Some(device) = device_by_id(state, &principal.device_credential_id)? else {
        return Ok(false);
    };
    if !device_active_for_login(state, &device)? {
        return Ok(false);
    }
    if device.session_nonce_hash != principal.device_session_nonce_hash {
        return Ok(false);
    }
    let now = chrono::Utc::now();
    let Some(last) = parse_utc(&device.last_heartbeat_at) else {
        return Ok(false);
    };
    let elapsed = now.signed_duration_since(last).num_seconds();
    if !(0..=DEVICE_HEARTBEAT_GRACE_SECONDS).contains(&elapsed) {
        return Ok(false);
    }
    let (_, used) = daily_usage(&device, now);
    Ok(used < device.daily_limit_minutes * 60)
}

fn record_failed_pin(state: &AppState, device_id: &str) -> Result<(), String> {
    let now = chrono::Utc::now();
    state.database.with_connection(|connection| {
        connection
            .execute(
                r#"
                UPDATE auth_device_credentials
                   SET failed_pin_attempts = failed_pin_attempts + 1,
                       pin_locked_until = CASE
                         WHEN failed_pin_attempts + 1 >= 5
                         THEN ?1 ELSE pin_locked_until END
                 WHERE id = ?2
                "#,
                params![
                    (now + chrono::Duration::minutes(15)).to_rfc3339(),
                    device_id
                ],
            )
            .map_err(|error| format!("无法记录设备 PIN 尝试：{error}"))?;
        Ok(())
    })
}

async fn device_heartbeat(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
) -> ApiResult<Json<Value>> {
    if principal.device_credential_id.is_empty() {
        return Err(ApiError::bad_request("当前会话未使用设备凭据"));
    }
    let device = device_by_id(&state, &principal.device_credential_id)?
        .ok_or_else(|| ApiError::unauthorized("设备会话已失效"))?;
    if !device_active_for_login(&state, &device)? {
        return Err(ApiError::unauthorized("设备会话已失效"));
    }
    let now = chrono::Utc::now();
    let last = parse_utc(&device.last_heartbeat_at)
        .ok_or_else(|| ApiError::unauthorized("设备会话已失效"))?;
    let (day, used) = daily_usage(&device, now);
    let used = (used
        + now
            .signed_duration_since(last)
            .num_seconds()
            .clamp(0, DEVICE_HEARTBEAT_GRACE_SECONDS))
    .min(device.daily_limit_minutes * 60);
    state.database.with_connection(|connection| {
        connection
            .execute(
                r#"
                UPDATE auth_device_credentials
                   SET usage_day = ?1, used_seconds = ?2, last_heartbeat_at = ?3
                 WHERE id = ?4
                "#,
                params![day, used, now.to_rfc3339(), device.id],
            )
            .map_err(|error| format!("无法刷新设备租约：{error}"))?;
        Ok(())
    })?;
    let remaining = (device.daily_limit_minutes * 60 - used).max(0);
    Ok(Json(json!({
        "ok": remaining > 0,
        "device_credential_id": device.id,
        "device_name": device.device_name,
        "daily_limit_minutes": device.daily_limit_minutes,
        "used_seconds": used,
        "remaining_seconds": remaining,
        "limit_reached": remaining == 0,
        "last_heartbeat_at": now.to_rfc3339()
    })))
}

fn public_origin(value: &str) -> ApiResult<(String, String)> {
    let parsed = url::Url::parse(value.trim())
        .map_err(|_| ApiError::bad_request("请输入有效的 HTTPS 公网地址"))?;
    if parsed.scheme() != "https" || parsed.host_str().is_none() {
        return Err(ApiError::bad_request("请输入有效的 HTTPS 公网地址"));
    }
    if parsed.path() != "/" && !parsed.path().is_empty() {
        return Err(ApiError::bad_request("会话移交地址只能包含站点根地址"));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(ApiError::bad_request("会话移交地址不能包含查询参数或片段"));
    }
    let host = parsed
        .host_str()
        .unwrap_or_default()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    Ok((value.trim().trim_end_matches('/').to_string(), host))
}

async fn create_session_handoff(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Json(body): Json<SessionHandoffCreateRequest>,
) -> ApiResult<Json<Value>> {
    if !auth_enabled(&state)? {
        return Err(ApiError::bad_request("会话移交需要启用认证"));
    }
    let (origin, host) = public_origin(&body.public_origin)?;
    let code = format!("dt_{}", random_secret(24));
    let ticket = random_secret(48);
    let expires = unix_now() + 5 * 60;
    state.database.with_connection(|connection| {
        connection
            .execute(
                r#"
                INSERT INTO auth_handoffs(
                    id, code_hash, ticket_hash, user_id, public_host,
                    created_at, expires_at
                ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)
                "#,
                params![
                    new_id("handoff"),
                    hash_secret(&code),
                    hash_secret(&ticket),
                    principal.user_id,
                    host,
                    now_iso(),
                    expires
                ],
            )
            .map_err(|error| format!("无法创建会话移交：{error}"))?;
        Ok(())
    })?;
    Ok(Json(json!({
        "ok": true,
        "code": code,
        "handoff_url": format!("{origin}/handoff?code={code}"),
        "expires_at": expires,
        "expires_in": 300
    })))
}

#[derive(Debug)]
struct HandoffRow {
    user_id: String,
    public_host: String,
}

fn request_host(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers
        .get("x-deeptutor-frontend-host")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let value = value.trim_end_matches('.');
        let without_port = if value.starts_with('[') {
            value
        } else {
            value.split(':').next().unwrap_or(value)
        };
        return Some(without_port.to_ascii_lowercase());
    }
    headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            if value.starts_with('[') {
                value.to_string()
            } else {
                value
                    .split(':')
                    .next()
                    .unwrap_or(value)
                    .to_ascii_lowercase()
            }
        })
}

async fn exchange_session_handoff(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SessionHandoffExchangeRequest>,
) -> ApiResult<Json<Value>> {
    if !auth_enabled(&state)? {
        return Err(ApiError::bad_request("会话移交需要启用认证"));
    }
    if body.code.len() < 16 {
        return Err(ApiError::bad_request("配对码无效、过期或已使用"));
    }
    let host =
        request_host(&headers).ok_or_else(|| ApiError::bad_request("配对请求缺少有效前端主机"))?;
    let row = state.database.with_connection(|connection| {
        let row = connection
            .query_row(
                r#"
                SELECT user_id, public_host
                  FROM auth_handoffs
                 WHERE code_hash = ?1 AND expires_at > ?2 AND completed_at IS NULL
                "#,
                params![hash_secret(&body.code), unix_now()],
                |row| {
                    Ok(HandoffRow {
                        user_id: row.get(0)?,
                        public_host: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(|error| format!("无法校验配对码：{error}"))?;
        if let Some(row) = row.as_ref() {
            if row.public_host != host
                && !(is_loopback_host(&row.public_host) && is_loopback_host(&host))
            {
                return Ok(None);
            }
            connection
                .execute(
                    "UPDATE auth_handoffs SET exchanged_at = ?1 WHERE code_hash = ?2",
                    params![now_iso(), hash_secret(&body.code)],
                )
                .map_err(|error| format!("无法消费配对码：{error}"))?;
        }
        let ticket = connection
            .query_row(
                "SELECT ticket_hash FROM auth_handoffs WHERE code_hash = ?1",
                params![hash_secret(&body.code)],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| format!("无法读取移交票据：{error}"))?;
        Ok(row.map(|_| ticket.unwrap_or_default()))
    })?;
    let ticket_hash = row.ok_or_else(|| ApiError::bad_request("配对码无效、过期或已使用"))?;
    // 票据只返回本次请求；数据库中只保存摘要，因此重新生成同值票据不现实。
    // 将原始票据与摘要绑定在同一个 one-time 记录中会要求可逆存储；这里改用
    // 票据本身作为携带摘要的 HMAC 免签名短票，服务端仍只凭 code 表校验。
    let ticket = format!(
        "{}.{}",
        ticket_hash,
        hash_secret(&format!("{}:{}", body.code, host))
    );
    Ok(Json(json!({"ok": true, "ticket": ticket})))
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]")
}

async fn complete_session_handoff(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SessionHandoffCompleteRequest>,
) -> ApiResult<Response> {
    if !auth_enabled(&state)? {
        return Err(ApiError::bad_request("会话移交需要启用认证"));
    }
    let host =
        request_host(&headers).ok_or_else(|| ApiError::bad_request("移交请求缺少有效前端主机"))?;
    let Some((ticket_hash, _)) = body.ticket.split_once('.') else {
        return Err(ApiError::bad_request("移交票据无效、过期或已使用"));
    };
    let row = state.database.with_connection(|connection| {
        let row = connection
            .query_row(
                r#"
                SELECT user_id, public_host
                  FROM auth_handoffs
                 WHERE ticket_hash = ?1 AND exchanged_at IS NOT NULL
                   AND completed_at IS NULL AND expires_at > ?2
                "#,
                params![ticket_hash, unix_now()],
                |row| {
                    Ok(HandoffRow {
                        user_id: row.get(0)?,
                        public_host: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(|error| format!("无法校验移交票据：{error}"))?;
        let Some(row) = row.as_ref() else {
            return Ok(None);
        };
        if row.public_host != host
            && !(is_loopback_host(&row.public_host) && is_loopback_host(&host))
        {
            return Ok(None);
        }
        connection
            .execute(
                "UPDATE auth_handoffs SET completed_at = ?1 WHERE ticket_hash = ?2",
                params![now_iso(), ticket_hash],
            )
            .map_err(|error| format!("无法消费移交票据：{error}"))?;
        Ok(Some(row.user_id.clone()))
    })?;
    let user_id = row.ok_or_else(|| ApiError::bad_request("移交票据无效、过期或已使用"))?;
    let account =
        account_by_id(&state, &user_id)?.ok_or_else(|| ApiError::bad_request("移交账户不存在"))?;
    if account.disabled {
        return Err(ApiError::bad_request("移交账户已停用"));
    }
    let (token, max_age) = create_session(&state, &account.id, "", "")?;
    session_response(&state, &token, max_age, json!({"ok": true}))
}

async fn codex_oauth_callback(
    State(state): State<AppState>,
    Query(query): Query<CodexCallbackQuery>,
) -> Html<&'static str> {
    let success = super::provider_auth::handle_codex_oauth_callback(
        &state,
        query.code,
        query.state,
        query.error,
    )
    .await
    .is_ok();
    if success {
        Html(
            "<!doctype html><title>DeepTutor Codex</title><p>Authentication received. You can return to DeepTutor.</p>",
        )
    } else {
        Html(
            "<!doctype html><title>DeepTutor Codex</title><p>Authentication could not be received. Return to DeepTutor and try again.</p>",
        )
    }
}

pub fn current_user_id(request: &Request) -> Option<String> {
    request
        .extensions()
        .get::<Principal>()
        .map(|principal| principal.user_id.clone())
}

pub fn is_authenticated_admin(request: &Request) -> bool {
    request
        .extensions()
        .get::<Principal>()
        .is_some_and(Principal::is_admin)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 图标头像标记会拒绝图片标记与路径() {
        assert!(valid_icon_marker("icon:book-blue:amber-500"));
        assert!(!valid_icon_marker("img:2"));
        assert!(!valid_icon_marker("icon:../x:blue"));
    }

    #[test]
    fn 密码规则至少八个字符() {
        assert!(validate_password("12345678").is_ok());
        assert!(validate_password("1234567").is_err());
    }

    #[test]
    fn 用户名规则接受邮箱和普通名称() {
        assert_eq!(validate_username(" learner_01 ").unwrap(), "learner_01");
        assert!(validate_username("student@example.com").is_ok());
        assert!(validate_username("x").is_err());
    }

    #[test]
    fn 书籍权限规范化会丢弃未知级别() {
        let value = normalize_book_permission(&json!({
            "create": false,
            "default": "write",
            "books": {"a": "read", "b": "delete", "c": "unknown"}
        }));
        assert_eq!(value["create"], false);
        assert_eq!(value["default"], "none");
        assert_eq!(value["books"]["a"], "read");
        assert_eq!(value["books"]["b"], "edit");
        assert!(value["books"].get("c").is_none());
    }
}
