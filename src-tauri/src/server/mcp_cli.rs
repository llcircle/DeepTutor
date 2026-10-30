use super::auth::{require_admin, Principal};
use super::error::{ApiError, ApiResult};
use super::{AppState, ScopedState};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Extension, Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use futures_util::future::join_all;
use futures_util::StreamExt;
use rand::RngCore;
use regex::Regex;
use rusqlite::OptionalExtension;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path as FilePath, PathBuf};
use std::process::Stdio;
use std::sync::LazyLock;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::time::timeout;
use url::Url;
use uuid::Uuid;

const ADMIN_MCP_KEY: &str = "mcp";
const USER_MCP_KEY: &str = "mcp_user";
const CLI_STATE_DOMAIN: &str = "cli_app_installation";
const CLI_DISABLED_KEY: &str = "cli_apps_disabled";
const MAX_USER_MCP_SERVERS: usize = 32;
const MAX_CATALOG_PAGE: usize = 100;
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_TOOL_LIST_BYTES: usize = 4 * 1024 * 1024;

const MCP_CATALOG_JSON: &str =
    include_str!("../../../deeptutor/services/mcp/catalog/vendor/curated.json");
const CLI_CATALOG_JSON: &str =
    include_str!("../../../deeptutor/services/cli_apps/vendor/catalog.json");

static SERVER_NAME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-zA-Z0-9][a-zA-Z0-9_-]{0,63}$").expect("MCP 服务名正则有效"));
static APP_ID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z0-9][a-z0-9._-]{0,95}$").expect("CLI 应用标识正则有效"));

#[derive(Debug, Deserialize)]
struct CatalogQuery {
    q: Option<String>,
    category: Option<String>,
    tier: Option<String>,
    cursor: Option<String>,
    limit: Option<usize>,
    installable_only: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct UserServerPayload {
    config: Value,
    #[serde(default)]
    secrets: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct CatalogInstallPayload {
    #[serde(default)]
    name: String,
    #[serde(default)]
    secrets: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct EnabledPayload {
    enabled: bool,
}

#[derive(Debug, Deserialize)]
struct OAuthCallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/settings/mcp",
            get(admin_mcp_settings).put(update_admin_mcp_settings),
        )
        .route(
            "/settings/mcp/servers/{name}",
            put(admin_upsert_mcp_server).delete(admin_delete_mcp_server),
        )
        .route("/settings/mcp/test", post(admin_test_mcp_server))
        .route("/space/mcp/servers", get(list_user_mcp_servers))
        .route(
            "/space/mcp/servers/{name}",
            put(user_upsert_mcp_server).delete(user_delete_mcp_server),
        )
        .route(
            "/space/mcp/servers/{name}/authorize",
            post(authorize_user_mcp_server),
        )
        .route("/space/mcp/servers/{name}/test", post(user_test_mcp_server))
        .route("/space/mcp/oauth/callback", get(mcp_oauth_callback))
        .route("/space/mcp/catalog", get(mcp_catalog))
        .route(
            "/space/mcp/catalog/{entry_id}/install",
            post(install_mcp_catalog_entry),
        )
        .route("/space/cli-apps/apps", get(cli_apps))
        .route(
            "/space/cli-apps/apps/{app_id}/enabled",
            put(set_cli_app_enabled),
        )
        .route("/space/cli-apps/apps/{app_id}", delete(uninstall_cli_app))
        .route("/space/cli-apps/catalog", get(cli_catalog))
        .route(
            "/space/cli-apps/catalog/{app_id}/install",
            post(install_cli_app),
        )
}

#[derive(Clone, Debug)]
enum ProbeFailure {
    AuthRequired(String),
    Failed(String),
}

impl From<String> for ProbeFailure {
    fn from(value: String) -> Self {
        Self::Failed(value)
    }
}

#[derive(Clone, Debug)]
struct ProbeResult {
    ok: bool,
    tools: Vec<Value>,
    error: String,
    needs_auth: bool,
}

impl ProbeResult {
    fn success(tools: Vec<Value>) -> Self {
        Self {
            ok: true,
            tools,
            error: String::new(),
            needs_auth: false,
        }
    }

    fn failure(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            tools: Vec::new(),
            error: error.into(),
            needs_auth: false,
        }
    }

    fn auth_required(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            tools: Vec::new(),
            error: error.into(),
            needs_auth: true,
        }
    }
}

async fn admin_mcp_settings(
    ScopedState(state): ScopedState,
    Extension(principal): Extension<Principal>,
) -> ApiResult<Json<Value>> {
    require_admin(&principal)?;
    Ok(Json(admin_state(&state).await))
}

async fn update_admin_mcp_settings(
    ScopedState(state): ScopedState,
    Extension(principal): Extension<Principal>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    require_admin(&principal)?;
    let servers = body
        .get("servers")
        .and_then(Value::as_object)
        .ok_or_else(|| ApiError::bad_request("servers 必须是对象"))?;
    let mut normalized = Map::new();
    for (name, config) in servers {
        let config = normalize_server_config(name, config, false)?;
        normalized.insert(name.clone(), config);
    }
    state.database.set_setting(
        ADMIN_MCP_KEY,
        &json!({"servers": Value::Object(normalized)}),
    )?;
    Ok(Json(admin_state(&state).await))
}

async fn admin_upsert_mcp_server(
    ScopedState(state): ScopedState,
    Extension(principal): Extension<Principal>,
    Path(name): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    require_admin(&principal)?;
    validate_server_name(&name)?;
    let config = normalize_server_config(&name, &body, false)?;
    let mut servers = admin_servers(&state);
    servers.insert(name.clone(), config);
    state
        .database
        .set_setting(ADMIN_MCP_KEY, &json!({"servers": Value::Object(servers)}))?;
    if let Ok(config) = materialize_config(
        &state,
        &name,
        admin_servers(&state)
            .get(&name)
            .cloned()
            .unwrap_or_else(|| json!({})),
    ) {
        let result = timeout(PROBE_TIMEOUT, probe_config(&config))
            .await
            .unwrap_or_else(|_| ProbeResult::failure("MCP 连接测试超过 8 秒"));
        if result.ok {
            let _ = super::deferred::store_mcp_tools(&state, &name, &result.tools);
        }
    }
    Ok(Json(admin_state(&state).await))
}

async fn admin_delete_mcp_server(
    ScopedState(state): ScopedState,
    Extension(principal): Extension<Principal>,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    require_admin(&principal)?;
    let mut servers = admin_servers(&state);
    servers.remove(&name);
    state
        .database
        .set_setting(ADMIN_MCP_KEY, &json!({"servers": Value::Object(servers)}))?;
    delete_secret_file(&state, &name);
    let _ = super::deferred::remove_mcp_server_tools(&state, &name);
    Ok(Json(admin_state(&state).await))
}

async fn admin_test_mcp_server(
    ScopedState(state): ScopedState,
    Extension(principal): Extension<Principal>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    require_admin(&principal)?;
    let config = normalize_server_config("probe", &body, false)?;
    let config = materialize_config(&state, "probe", config)?;
    let result = probe_config(&config).await;
    Ok(Json(test_result_json(result)))
}

async fn list_user_mcp_servers(
    State(central): State<AppState>,
    ScopedState(state): ScopedState,
    Extension(principal): Extension<Principal>,
) -> ApiResult<Json<Value>> {
    Ok(Json(user_state(&central, &state).await))
}

async fn user_upsert_mcp_server(
    State(central): State<AppState>,
    ScopedState(state): ScopedState,
    Extension(principal): Extension<Principal>,
    Path(name): Path<String>,
    Json(payload): Json<UserServerPayload>,
) -> ApiResult<Json<Value>> {
    validate_server_name(&name)?;
    if admin_servers(&central).contains_key(&name) {
        return Err(ApiError::conflict(format!(
            "MCP 服务名 {name} 已被管理员注册表占用"
        )));
    }
    let mut servers = user_servers(&state);
    if !servers.contains_key(&name) && servers.len() >= MAX_USER_MCP_SERVERS {
        return Err(ApiError::bad_request(format!(
            "每个账户最多可添加 {MAX_USER_MCP_SERVERS} 个 MCP 服务"
        )));
    }
    let (config, secrets) = normalize_user_payload(&name, &payload)?;
    servers.insert(name.clone(), config);
    state
        .database
        .set_setting(USER_MCP_KEY, &json!({"servers": Value::Object(servers)}))?;
    merge_secret_values(&state, &name, &secrets)?;
    Ok(Json(user_state(&central, &state).await))
}

async fn user_delete_mcp_server(
    State(central): State<AppState>,
    ScopedState(state): ScopedState,
    Extension(_principal): Extension<Principal>,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    let mut servers = user_servers(&state);
    servers.remove(&name);
    state
        .database
        .set_setting(USER_MCP_KEY, &json!({"servers": Value::Object(servers)}))?;
    delete_secret_file(&state, &name);
    let _ = super::deferred::remove_mcp_server_tools(&state, &name);
    Ok(Json(user_state(&central, &state).await))
}

async fn user_test_mcp_server(
    State(_central): State<AppState>,
    ScopedState(state): ScopedState,
    Extension(_principal): Extension<Principal>,
    Path(name): Path<String>,
    Json(payload): Json<UserServerPayload>,
) -> ApiResult<Json<Value>> {
    validate_server_name(&name)?;
    let (mut config, draft_secrets) = normalize_user_payload(&name, &payload)?;
    config = merge_draft_secrets(&state, &name, config, &draft_secrets)?;
    let result = probe_config(&config).await;
    if result.ok {
        if let Err(error) = super::deferred::store_mcp_tools(&state, &name, &result.tools) {
            tracing::warn!(server = %name, %error, "MCP 工具目录保存失败");
        }
    }
    Ok(Json(test_result_json(result)))
}

async fn authorize_user_mcp_server(
    ScopedState(state): ScopedState,
    Extension(_principal): Extension<Principal>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let config = user_servers(&state)
        .remove(&name)
        .ok_or_else(|| ApiError::not_found(format!("MCP 服务 {name} 不存在")))?;
    if config.get("auth").and_then(Value::as_str) != Some("oauth") {
        return Err(ApiError::bad_request("该 MCP 服务没有配置 OAuth"));
    }
    let origin = request_origin(&headers);
    if origin.is_empty() {
        return Err(ApiError::bad_request("无法确定 MCP OAuth 回调地址"));
    }
    let authorize_url = begin_oauth(&state, &name, &config, &origin).await?;
    Ok(Json(json!({"authorize_url": authorize_url})))
}

async fn mcp_oauth_callback(
    ScopedState(state): ScopedState,
    Query(query): Query<OAuthCallbackQuery>,
) -> Response {
    if let Some(error) = query.error {
        let detail = query.error_description.unwrap_or(error);
        return oauth_callback_page(false, &detail).into_response();
    }
    let Some(code) = query.code.filter(|value| !value.trim().is_empty()) else {
        return oauth_callback_page(false, "OAuth 回调缺少授权码").into_response();
    };
    let Some(state_token) = query.state.filter(|value| !value.trim().is_empty()) else {
        return oauth_callback_page(false, "OAuth 回调缺少 state").into_response();
    };
    match complete_oauth(&state, &state_token, &code).await {
        Ok(()) => oauth_callback_page(true, "").into_response(),
        Err(error) => oauth_callback_page(false, &error.message).into_response(),
    }
}

async fn mcp_catalog(
    ScopedState(state): ScopedState,
    Query(query): Query<CatalogQuery>,
) -> ApiResult<Json<Value>> {
    let installed = user_servers(&state);
    Ok(Json(mcp_catalog_page(&query, &installed)))
}

async fn install_mcp_catalog_entry(
    State(central): State<AppState>,
    ScopedState(state): ScopedState,
    Extension(_principal): Extension<Principal>,
    Path(entry_id): Path<String>,
    Json(payload): Json<CatalogInstallPayload>,
) -> ApiResult<Json<Value>> {
    let name = if payload.name.trim().is_empty() {
        entry_id.clone()
    } else {
        payload.name.trim().to_string()
    };
    validate_server_name(&name)?;
    if admin_servers(&central).contains_key(&name) {
        return Err(ApiError::conflict(format!(
            "MCP 服务名 {name} 已被管理员注册表占用"
        )));
    }
    let mut servers = user_servers(&state);
    if !servers.contains_key(&name) && servers.len() >= MAX_USER_MCP_SERVERS {
        return Err(ApiError::bad_request(format!(
            "每个账户最多可添加 {MAX_USER_MCP_SERVERS} 个 MCP 服务"
        )));
    }
    let (config, secrets) = build_catalog_server(&entry_id, &name, &payload.secrets)?;
    servers.insert(name.clone(), config);
    state
        .database
        .set_setting(USER_MCP_KEY, &json!({"servers": Value::Object(servers)}))?;
    merge_secret_values(&state, &name, &secrets)?;
    Ok(Json(user_state(&central, &state).await))
}

async fn cli_apps(
    State(central): State<AppState>,
    Extension(principal): Extension<Principal>,
) -> ApiResult<Json<Value>> {
    Ok(Json(cli_state(&central, &principal)?))
}

async fn set_cli_app_enabled(
    State(central): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(app_id): Path<String>,
    Json(payload): Json<EnabledPayload>,
) -> ApiResult<Json<Value>> {
    validate_app_id(&app_id)?;
    let installed = central
        .database
        .get_resource(CLI_STATE_DOMAIN, &app_id)?
        .ok_or_else(|| ApiError::not_found(format!("CLI 应用 {app_id} 尚未安装")))?;
    let _ = installed;
    let allowed = cli_allowed_apps(&central, &principal)?;
    if allowed
        .as_ref()
        .is_some_and(|values| !values.contains(&app_id))
    {
        return Err(ApiError::forbidden("当前账户未获准使用这个 CLI 应用"));
    }
    let mut disabled = cli_disabled_apps(&central, &principal)?;
    if payload.enabled {
        disabled.remove(&app_id);
    } else {
        disabled.insert(app_id);
    }
    save_cli_disabled_apps(&central, &principal, disabled)?;
    Ok(Json(cli_state(&central, &principal)?))
}

async fn install_cli_app(
    State(central): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(app_id): Path<String>,
) -> Response {
    if let Err(error) = require_admin(&principal) {
        return error.into_response();
    }
    if let Err(error) = validate_app_id(&app_id) {
        return error.into_response();
    }
    let Some(entry) = cli_catalog_entries()
        .into_iter()
        .find(|entry| entry.get("id").and_then(Value::as_str) == Some(app_id.as_str()))
    else {
        return business_error(
            StatusCode::NOT_FOUND,
            "cli.not_in_catalog",
            format!("CLI 应用 {app_id} 不在目录中"),
            "",
        );
    };
    let outcome = install_portable_cli_app(&central, &app_id, &entry).await;
    match outcome {
        Ok(log) => {
            let mut state = match cli_state(&central, &principal) {
                Ok(state) => state,
                Err(error) => return error.into_response(),
            };
            state["log"] = Value::String(log);
            Json(state).into_response()
        }
        Err((code, message, log)) => business_error(StatusCode::BAD_REQUEST, &code, message, &log),
    }
}

async fn uninstall_cli_app(
    State(central): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(app_id): Path<String>,
) -> ApiResult<Json<Value>> {
    require_admin(&principal)?;
    validate_app_id(&app_id)?;
    let root = central.paths.data.join("cli-apps");
    let app_dir = safe_child(&root, &app_id)?;
    if app_dir.exists() {
        std::fs::remove_dir_all(&app_dir)
            .map_err(|error| ApiError::internal(format!("无法删除 CLI 应用目录：{error}")))?;
    }
    central
        .database
        .delete_resource(CLI_STATE_DOMAIN, &app_id)?;
    let mut disabled = cli_disabled_apps(&central, &principal)?;
    disabled.remove(&app_id);
    save_cli_disabled_apps(&central, &principal, disabled)?;
    Ok(Json(cli_state(&central, &principal)?))
}

async fn cli_catalog(
    State(central): State<AppState>,
    Extension(principal): Extension<Principal>,
    Query(query): Query<CatalogQuery>,
) -> ApiResult<Json<Value>> {
    require_authentication(&principal)?;
    Ok(Json(cli_catalog_page(&central, &query)?))
}

async fn admin_state(state: &AppState) -> Value {
    let servers = admin_servers(state);
    let status = probe_status_rows(state, &servers).await;
    json!({
        "servers": Value::Object(servers),
        "status": status
    })
}

async fn user_state(central: &AppState, state: &AppState) -> Value {
    let config = user_servers(state);
    let status = probe_status_rows(state, &config).await;
    let deployment = admin_servers(central);
    let deployment_status = probe_status_rows(central, &deployment).await;
    let configured_secrets = config
        .keys()
        .map(|name| {
            let fields = read_secret_map(state, name)
                .map(|values| {
                    values
                        .keys()
                        .filter(|key| !key.starts_with("__oauth_"))
                        .cloned()
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            (
                name.clone(),
                Value::Array(fields.into_iter().map(Value::String).collect()),
            )
        })
        .collect::<Map<_, _>>();
    let oauth = config
        .iter()
        .filter(|(_, config)| config.get("auth").and_then(Value::as_str) == Some("oauth"))
        .map(|(name, _)| {
            (
                name.clone(),
                json!({
                    "required": true,
                    "authorized": oauth_authorized(state, name)
                }),
            )
        })
        .collect::<Map<_, _>>();
    json!({
        "servers": Value::Object(config),
        "status": status,
        "configured_secrets": Value::Object(configured_secrets),
        "rejected": [],
        "deployment": {
            "servers": deployment.keys().cloned().collect::<Vec<_>>(),
            "status": deployment_status
        },
        "limits": {"max_servers": MAX_USER_MCP_SERVERS},
        "oauth": Value::Object(oauth)
    })
}

fn admin_servers(state: &AppState) -> Map<String, Value> {
    state
        .database
        .get_setting(ADMIN_MCP_KEY)
        .ok()
        .flatten()
        .and_then(|value| {
            value
                .get("servers")
                .and_then(Value::as_object)
                .cloned()
                .or_else(|| value.as_object().cloned())
        })
        .unwrap_or_default()
}

fn user_servers(state: &AppState) -> Map<String, Value> {
    state
        .database
        .get_setting(USER_MCP_KEY)
        .ok()
        .flatten()
        .and_then(|value| value.get("servers").and_then(Value::as_object).cloned())
        .unwrap_or_default()
}

async fn probe_status_rows(state: &AppState, servers: &Map<String, Value>) -> Vec<Value> {
    let tasks = servers.iter().map(|(name, raw)| {
        let name = name.clone();
        let raw = raw.clone();
        let state = state.clone();
        async move {
            let enabled = raw.get("enabled").and_then(Value::as_bool).unwrap_or(true);
            let transport = resolved_transport(&raw).unwrap_or("").to_string();
            if !enabled {
                return json!({
                    "name": name,
                    "transport": transport,
                    "status": "disabled",
                    "error": "",
                    "tools": []
                });
            }
            let result = match materialize_config(&state, &name, raw) {
                Ok(config) => timeout(PROBE_TIMEOUT, probe_config(&config))
                    .await
                    .unwrap_or_else(|_| ProbeResult::failure("MCP 连接测试超过 8 秒")),
                Err(error) => ProbeResult::failure(error.message),
            };
            if result.ok {
                if let Err(error) = super::deferred::store_mcp_tools(&state, &name, &result.tools) {
                    tracing::warn!(server = %name, %error, "MCP 工具目录保存失败");
                }
            }
            json!({
                "name": name,
                "transport": transport,
                "status": if result.ok {
                    "connected"
                } else if result.needs_auth {
                    "needs_auth"
                } else {
                    "error"
                },
                "error": result.error,
                "tools": result.tools
            })
        }
    });
    let mut rows = join_all(tasks).await;
    rows.sort_by(|left, right| {
        left.get("name")
            .and_then(Value::as_str)
            .cmp(&right.get("name").and_then(Value::as_str))
    });
    rows
}

fn test_result_json(result: ProbeResult) -> Value {
    json!({
        "ok": result.ok,
        "tools": result.tools,
        "error": result.error
    })
}

fn validate_server_name(name: &str) -> ApiResult<()> {
    if SERVER_NAME_RE.is_match(name) {
        Ok(())
    } else {
        Err(ApiError::bad_request(
            "MCP 服务名必须以字母或数字开头，只能包含字母、数字、下划线和短横线，最长 64 个字符",
        ))
    }
}

fn validate_app_id(app_id: &str) -> ApiResult<()> {
    if APP_ID_RE.is_match(app_id) {
        Ok(())
    } else {
        Err(ApiError::bad_request("CLI 应用标识不合法"))
    }
}

fn require_authentication(principal: &Principal) -> ApiResult<()> {
    if principal.disabled {
        Err(ApiError::forbidden("账户已停用"))
    } else {
        Ok(())
    }
}

fn normalize_server_config(name: &str, raw: &Value, user_scope: bool) -> ApiResult<Value> {
    validate_server_name(name)?;
    let object = raw
        .as_object()
        .ok_or_else(|| ApiError::bad_request("MCP 服务配置必须是对象"))?;
    let string = |key: &str, fallback: &str| {
        object
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or(fallback)
            .trim()
            .to_string()
    };
    let request_type = object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if !matches!(request_type, "" | "stdio" | "sse" | "streamableHttp") {
        return Err(ApiError::bad_request("MCP transport 类型不受支持"));
    }
    let command = string("command", "");
    let url = string("url", "");
    let transport = if !request_type.is_empty() {
        request_type
    } else if !command.is_empty() {
        "stdio"
    } else if url.trim_end_matches('/').ends_with("/sse") {
        "sse"
    } else if !url.is_empty() {
        "streamableHttp"
    } else {
        return Err(ApiError::bad_request(format!(
            "MCP 服务 {name} 必须配置 command 或 url"
        )));
    };
    if user_scope && transport == "stdio" {
        return Err(ApiError::forbidden(
            "普通账户只能配置远程 MCP 服务；stdio 服务由管理员注册",
        ));
    }
    if transport == "stdio" {
        if command.is_empty() || !url.is_empty() {
            return Err(ApiError::bad_request(
                "stdio MCP 服务必须提供 command，且不能提供 url",
            ));
        }
    } else {
        if url.is_empty() || !command.is_empty() {
            return Err(ApiError::bad_request(
                "远程 MCP 服务必须提供 url，且不能提供 command",
            ));
        }
        validate_remote_url(&url)?;
    }
    let args = string_array(object.get("args"));
    let env = string_map(object.get("env"));
    let headers = string_map(object.get("headers"));
    let enabled_tools = object
        .get("enabled_tools")
        .map(|value| string_array(Some(value)))
        .filter(|values| !values.is_empty())
        .unwrap_or_else(|| vec!["*".to_string()]);
    let disabled_tools = string_array(object.get("disabled_tools"));
    let tool_timeout = object
        .get("tool_timeout")
        .and_then(Value::as_u64)
        .unwrap_or(30)
        .clamp(1, 600);
    let enabled = object
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let allow_private = object
        .get("allow_private_network")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if user_scope && is_private_origin(&url) && !allow_private {
        return Err(ApiError::forbidden(
            "远程 MCP 服务指向私有网络。请先让管理员批准该来源，或启用 allow_private_network",
        ));
    }
    let auth = match object.get("auth").and_then(Value::as_str).unwrap_or("") {
        "" => "",
        "oauth" => "oauth",
        _ => return Err(ApiError::bad_request("MCP auth 只能为空或 oauth")),
    };
    Ok(json!({
        "type": transport,
        "command": command,
        "args": args,
        "env": env,
        "cwd": string("cwd", ""),
        "url": url,
        "headers": headers,
        "tool_timeout": tool_timeout,
        "enabled_tools": enabled_tools,
        "disabled_tools": disabled_tools,
        "enabled": enabled,
        "allow_private_network": allow_private,
        "auth": auth,
        "catalog_entry": string("catalog_entry", "")
    }))
}

fn normalize_user_payload(
    name: &str,
    payload: &UserServerPayload,
) -> ApiResult<(Value, HashMap<String, String>)> {
    let mut config = normalize_server_config(name, &payload.config, true)?;
    let mut secrets = payload
        .secrets
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<HashMap<_, _>>();
    for section in ["headers", "env"] {
        let Some(values) = config.get(section).and_then(Value::as_object).cloned() else {
            continue;
        };
        let mut rewritten = Map::new();
        for (key, value) in values {
            let Some(text) = value.as_str() else {
                continue;
            };
            if text.is_empty() || is_secret_reference(text) {
                rewritten.insert(key, Value::String(text.to_string()));
                continue;
            }
            let field = format!(
                "{}.{}",
                if section == "headers" {
                    "header"
                } else {
                    "env"
                },
                key
            );
            secrets.insert(field.clone(), text.to_string());
            rewritten.insert(key, Value::String(secret_reference(name, &field)));
        }
        config[section] = Value::Object(rewritten);
    }
    let literals = secrets
        .iter()
        .filter(|(_, value)| !value.is_empty())
        .map(|(key, value)| (value.clone(), key.clone()))
        .collect::<HashMap<_, _>>();
    if !literals.is_empty() {
        replace_secret_literals(&mut config, &literals, name);
    }
    Ok((config, secrets))
}

fn replace_secret_literals(value: &mut Value, literals: &HashMap<String, String>, server: &str) {
    match value {
        Value::String(text) => {
            if let Some(field) = literals.get(text) {
                *text = secret_reference(server, field);
            }
        }
        Value::Array(items) => {
            for item in items {
                replace_secret_literals(item, literals, server);
            }
        }
        Value::Object(items) => {
            for item in items.values_mut() {
                replace_secret_literals(item, literals, server);
            }
        }
        _ => {}
    }
}

fn resolved_transport(config: &Value) -> Option<&str> {
    if let Some(value) = config.get("type").and_then(Value::as_str) {
        if !value.is_empty() {
            return Some(value);
        }
    }
    if config
        .get("command")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty())
    {
        return Some("stdio");
    }
    let url = config.get("url").and_then(Value::as_str)?;
    if url.trim_end_matches('/').ends_with("/sse") {
        Some("sse")
    } else {
        Some("streamableHttp")
    }
}

fn validate_remote_url(raw: &str) -> ApiResult<Url> {
    let url =
        Url::parse(raw).map_err(|error| ApiError::bad_request(format!("MCP URL 无效：{error}")))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(ApiError::bad_request(
            "MCP URL 必须是 http/https 地址，且不能包含账号、密码或片段",
        ));
    }
    Ok(url)
}

fn is_private_origin(raw: &str) -> bool {
    let Ok(url) = Url::parse(raw) else {
        return true;
    };
    let Some(host) = url.host_str() else {
        return true;
    };
    if host.eq_ignore_ascii_case("localhost") || host.ends_with(".localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .map(|ip| match ip {
            std::net::IpAddr::V4(ip) => {
                ip.is_loopback() || ip.is_private() || ip.is_link_local() || ip.is_unspecified()
            }
            std::net::IpAddr::V6(ip) => {
                ip.is_loopback()
                    || ip.is_unique_local()
                    || ip.is_unicast_link_local()
                    || ip.is_unspecified()
            }
        })
        .unwrap_or(false)
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn string_map(value: Option<&Value>) -> Map<String, Value> {
    value
        .and_then(Value::as_object)
        .map(|items| {
            items
                .iter()
                .filter_map(|(key, value)| {
                    value
                        .as_str()
                        .map(|text| (key.clone(), Value::String(text.to_string())))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn secret_reference(server: &str, field: &str) -> String {
    format!("${{secret:{server}/{field}}}")
}

fn is_secret_reference(value: &str) -> bool {
    value.starts_with("${secret:") && value.ends_with('}')
}

fn secret_path(state: &AppState, server: &str) -> PathBuf {
    state
        .paths
        .data
        .join("private")
        .join("mcp")
        .join(format!("{server}.json"))
}

fn oauth_path(state: &AppState, server: &str) -> PathBuf {
    state
        .paths
        .data
        .join("private")
        .join("mcp")
        .join(format!("{server}.oauth.json"))
}

fn read_secret_map(state: &AppState, server: &str) -> Result<BTreeMap<String, String>, String> {
    read_json_file(secret_path(state, server))
}

fn read_json_file(path: PathBuf) -> Result<BTreeMap<String, String>, String> {
    if !path.is_file() {
        return Ok(BTreeMap::new());
    }
    let raw = std::fs::read_to_string(&path)
        .map_err(|error| format!("无法读取凭据文件 {}：{error}", path.display()))?;
    let value: Value = serde_json::from_str(&raw)
        .map_err(|error| format!("凭据文件 {} 已损坏：{error}", path.display()))?;
    Ok(value
        .as_object()
        .map(|items| {
            items
                .iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|text| (key.clone(), text.to_string()))
                })
                .collect()
        })
        .unwrap_or_default())
}

fn write_json_file(path: &FilePath, value: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("无法创建凭据目录 {}：{error}", parent.display()))?;
    }
    let temporary = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    let data =
        serde_json::to_vec_pretty(value).map_err(|error| format!("无法序列化凭据：{error}"))?;
    std::fs::write(&temporary, data)
        .map_err(|error| format!("无法写入凭据文件 {}：{error}", temporary.display()))?;
    if path.exists() {
        std::fs::remove_file(path)
            .map_err(|error| format!("无法替换凭据文件 {}：{error}", path.display()))?;
    }
    std::fs::rename(&temporary, path)
        .map_err(|error| format!("无法保存凭据文件 {}：{error}", path.display()))
}

fn merge_secret_values(
    state: &AppState,
    server: &str,
    values: &HashMap<String, String>,
) -> ApiResult<()> {
    let mut current = read_secret_map(state, server)?;
    for (key, value) in values {
        if value.is_empty() {
            current.remove(key);
        } else {
            current.insert(key.clone(), value.clone());
        }
    }
    let value = current
        .into_iter()
        .map(|(key, value)| (key, Value::String(value)))
        .collect::<Map<String, Value>>();
    write_json_file(&secret_path(state, server), &Value::Object(value))?;
    Ok(())
}

fn delete_secret_file(state: &AppState, server: &str) {
    let _ = std::fs::remove_file(secret_path(state, server));
    let _ = std::fs::remove_file(oauth_path(state, server));
}

fn materialize_config(state: &AppState, server: &str, mut config: Value) -> ApiResult<Value> {
    let secrets = read_secret_map(state, server)?;
    resolve_secrets(&mut config, server, &secrets);
    if config.get("auth").and_then(Value::as_str) == Some("oauth") {
        let token = read_oauth_token(state, server)?
            .and_then(|value| {
                value
                    .get("access_token")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ApiError::unauthorized("MCP 服务需要浏览器 OAuth 授权"))?;
        if !config
            .get("headers")
            .and_then(Value::as_object)
            .is_some_and(|headers| {
                headers
                    .keys()
                    .any(|key| key.eq_ignore_ascii_case("authorization"))
            })
        {
            config["headers"]["Authorization"] = Value::String(format!("Bearer {token}"));
        }
    }
    Ok(config)
}

fn merge_draft_secrets(
    state: &AppState,
    server: &str,
    mut config: Value,
    draft: &HashMap<String, String>,
) -> ApiResult<Value> {
    let mut secrets = read_secret_map(state, server)?;
    for (key, value) in draft {
        secrets.insert(key.clone(), value.clone());
    }
    resolve_secrets(&mut config, server, &secrets);
    if config.get("auth").and_then(Value::as_str) == Some("oauth") {
        if let Some(token) = read_oauth_token(state, server)?.and_then(|value| {
            value
                .get("access_token")
                .and_then(Value::as_str)
                .map(str::to_string)
        }) {
            config["headers"]["Authorization"] = Value::String(format!("Bearer {token}"));
        }
    }
    Ok(config)
}

fn resolve_secrets(value: &mut Value, server: &str, secrets: &BTreeMap<String, String>) {
    match value {
        Value::String(text) => {
            for (key, secret) in secrets {
                let reference = format!("${{secret:{server}/{key}}}");
                if text.contains(&reference) {
                    *text = text.replace(&reference, secret);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                resolve_secrets(item, server, secrets);
            }
        }
        Value::Object(items) => {
            for item in items.values_mut() {
                resolve_secrets(item, server, secrets);
            }
        }
        _ => {}
    }
}

fn mcp_catalog_page(query: &CatalogQuery, installed: &Map<String, Value>) -> Value {
    let needle = query.q.as_deref().unwrap_or("").trim().to_lowercase();
    let category = query.category.as_deref().unwrap_or("").trim();
    let tier = query.tier.as_deref().unwrap_or("").trim();
    let mut counts = BTreeMap::<String, usize>::new();
    let mut matches = Vec::new();
    for entry in raw_mcp_catalog_entries() {
        if !entry
            .get("self_service")
            .and_then(Value::as_bool)
            .unwrap_or_else(|| entry.get("transport").and_then(Value::as_str) != Some("stdio"))
        {
            continue;
        }
        if !tier.is_empty()
            && entry
                .get("tier")
                .and_then(Value::as_str)
                .unwrap_or("curated")
                != tier
        {
            continue;
        }
        if !needle.is_empty() && !mcp_entry_matches(&entry, &needle) {
            continue;
        }
        let entry_category = entry
            .get("category")
            .and_then(Value::as_str)
            .unwrap_or("other");
        *counts.entry(entry_category.to_string()).or_default() += 1;
        if category.is_empty() || entry_category == category {
            matches.push(entry);
        }
    }
    matches.sort_by(|left, right| {
        let left_rank = tier_rank(
            left.get("tier")
                .and_then(Value::as_str)
                .unwrap_or("curated"),
        );
        let right_rank = tier_rank(
            right
                .get("tier")
                .and_then(Value::as_str)
                .unwrap_or("curated"),
        );
        left_rank.cmp(&right_rank).then_with(|| {
            left.get("display_name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_lowercase()
                .cmp(
                    &right
                        .get("display_name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_lowercase(),
                )
        })
    });
    let total = matches.len();
    let limit = query.limit.unwrap_or(30).clamp(1, MAX_CATALOG_PAGE);
    let start = query
        .cursor
        .as_deref()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0)
        .min(total);
    let end = (start + limit).min(total);
    let entries = matches[start..end]
        .iter()
        .map(|entry| mcp_catalog_row(entry, installed))
        .collect::<Vec<_>>();
    json!({
        "entries": entries,
        "next_cursor": if end < total { end.to_string() } else { String::new() },
        "total": total,
        "categories": counts
            .into_iter()
            .map(|(category, count)| (category, Value::from(count)))
            .collect::<Map<_, _>>()
    })
}

fn raw_mcp_catalog_entries() -> Vec<Value> {
    serde_json::from_str::<Value>(MCP_CATALOG_JSON)
        .ok()
        .and_then(|value| value.get("entries").and_then(Value::as_array).cloned())
        .unwrap_or_default()
}

fn mcp_entry_matches(entry: &Value, needle: &str) -> bool {
    let mut haystack = vec![
        entry.get("id").and_then(Value::as_str).unwrap_or(""),
        entry
            .get("display_name")
            .and_then(Value::as_str)
            .unwrap_or(""),
        entry.get("category").and_then(Value::as_str).unwrap_or(""),
        entry
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or(""),
    ];
    for key in ["description", "requires"] {
        if let Some(values) = entry.get(key).and_then(Value::as_object) {
            haystack.extend(values.values().filter_map(Value::as_str));
        }
    }
    haystack.join(" ").to_lowercase().contains(needle)
}

fn tier_rank(tier: &str) -> usize {
    match tier {
        "verified" => 0,
        "curated" => 1,
        _ => 2,
    }
}

fn mcp_catalog_row(entry: &Value, installed: &Map<String, Value>) -> Value {
    let entry_id = entry.get("id").and_then(Value::as_str).unwrap_or("");
    let installed_as = installed
        .iter()
        .filter(|(name, config)| {
            config
                .get("catalog_entry")
                .and_then(Value::as_str)
                .is_some_and(|value| value == entry_id)
                || (config
                    .get("catalog_entry")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .is_empty()
                    && name.as_str() == entry_id)
        })
        .map(|(name, _)| Value::String(name.clone()))
        .collect::<Vec<_>>();
    let fields = entry
        .get("fields")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|field| {
                    let object = field.as_object()?;
                    let target = object.get("target")?.as_array()?;
                    Some(json!({
                        "key": object.get("key").cloned().unwrap_or(Value::Null),
                        "label_i18n": object.get("label").cloned().unwrap_or_else(|| json!({})),
                        "secret": object.get("secret").and_then(Value::as_bool).unwrap_or(true),
                        "required": object.get("required").and_then(Value::as_bool).unwrap_or(true),
                        "placeholder": object.get("placeholder").cloned().unwrap_or_else(|| json!("")),
                        "target": target
                    }))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    json!({
        "id": entry_id,
        "display_name": entry.get("display_name").cloned().unwrap_or(Value::Null),
        "description_i18n": entry.get("description").cloned().unwrap_or_else(|| json!({})),
        "category": entry.get("category").cloned().unwrap_or(Value::Null),
        "tier": entry.get("tier").cloned().unwrap_or_else(|| json!("curated")),
        "transport": normalize_catalog_transport(
            entry.get("transport").and_then(Value::as_str).unwrap_or("")
        ),
        "homepage": entry.get("homepage").cloned().unwrap_or_else(|| json!("")),
        "docs_url": entry.get("docs_url").cloned().unwrap_or_else(|| json!("")),
        "requires_i18n": entry.get("requires").cloned().unwrap_or_else(|| json!({})),
        "logo_url": entry.get("logo_url").cloned().unwrap_or_else(|| json!("")),
        "trust": entry.get("trust").cloned().unwrap_or_else(|| json!("verified")),
        "self_service": entry.get("self_service").and_then(Value::as_bool).unwrap_or(true),
        "installed": !installed_as.is_empty(),
        "installed_as": installed_as,
        "fields": fields
    })
}

fn normalize_catalog_transport(value: &str) -> &str {
    match value {
        "http" => "streamableHttp",
        "streamable_http" => "streamableHttp",
        "stdio" => "stdio",
        "sse" => "sse",
        "streamableHttp" => "streamableHttp",
        _ => value,
    }
}

fn build_catalog_server(
    entry_id: &str,
    name: &str,
    values: &HashMap<String, String>,
) -> ApiResult<(Value, HashMap<String, String>)> {
    let entry = raw_mcp_catalog_entries()
        .into_iter()
        .find(|entry| entry.get("id").and_then(Value::as_str) == Some(entry_id))
        .ok_or_else(|| ApiError::not_found(format!("MCP 目录条目 {entry_id} 不存在")))?;
    let self_service = entry
        .get("self_service")
        .and_then(Value::as_bool)
        .unwrap_or_else(|| entry.get("transport").and_then(Value::as_str) != Some("stdio"));
    if !self_service {
        return Err(ApiError::forbidden("该 MCP 目录条目仅供管理员安装"));
    }
    let transport =
        normalize_catalog_transport(entry.get("transport").and_then(Value::as_str).unwrap_or(""));
    if transport == "stdio" {
        return Err(ApiError::forbidden(
            "普通账户不能安装 stdio MCP 服务：那会在主机上执行命令",
        ));
    }
    let mut server = entry
        .get("server")
        .and_then(Value::as_object)
        .cloned()
        .ok_or_else(|| ApiError::internal("MCP 目录条目的 server 模板无效"))?;
    server.insert("type".into(), Value::String(transport.to_string()));
    server.insert("catalog_entry".into(), Value::String(entry_id.to_string()));
    let mut config = Value::Object(server);
    let mut secrets = HashMap::new();
    if let Some(fields) = entry.get("fields").and_then(Value::as_array) {
        for field in fields {
            let Some(field) = field.as_object() else {
                continue;
            };
            let key = field.get("key").and_then(Value::as_str).unwrap_or("");
            let required = field
                .get("required")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let raw = values.get(key).map(|value| value.trim()).unwrap_or("");
            if raw.is_empty() {
                if required {
                    return Err(ApiError::bad_request(format!(
                        "安装 {entry_id} 缺少必填凭据 {key}"
                    )));
                }
                continue;
            }
            let secret = field.get("secret").and_then(Value::as_bool).unwrap_or(true);
            let template = field
                .get("value_template")
                .and_then(Value::as_str)
                .unwrap_or("{value}");
            let rendered = template.replace("{value}", raw);
            let injected = if secret {
                secrets.insert(key.to_string(), rendered);
                secret_reference(name, key)
            } else {
                rendered
            };
            let Some(target) = field.get("target").and_then(Value::as_array) else {
                return Err(ApiError::internal(format!(
                    "MCP 目录条目 {entry_id} 的凭据目标无效"
                )));
            };
            let kind = target.first().and_then(Value::as_str).unwrap_or("");
            let target_name = target.get(1).and_then(Value::as_str).unwrap_or("");
            match kind {
                "header" => {
                    config["headers"][target_name] = Value::String(injected);
                }
                "env" => {
                    config["env"][target_name] = Value::String(injected);
                }
                "arg" => {
                    set_arg(&mut config, target_name, injected);
                }
                "url_param" => {
                    let url = config
                        .get("url")
                        .and_then(Value::as_str)
                        .ok_or_else(|| ApiError::internal("目录条目缺少远程 URL"))?;
                    config["url"] = Value::String(set_url_param(url, target_name, &injected)?);
                }
                _ => {
                    return Err(ApiError::internal(format!(
                        "MCP 目录条目 {entry_id} 的凭据目标 {kind} 不受支持"
                    )));
                }
            }
        }
    }
    let config = normalize_server_config(name, &config, true)?;
    Ok((config, secrets))
}

fn set_arg(config: &mut Value, flag: &str, value: String) {
    let mut args = config
        .get("args")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let prefix = format!("{flag}=");
    let mut output = Vec::new();
    let mut skip = false;
    for arg in args.drain(..) {
        if skip {
            skip = false;
            continue;
        }
        if arg == flag {
            skip = true;
            continue;
        }
        if arg.starts_with(&prefix) {
            continue;
        }
        output.push(arg);
    }
    output.push(flag.to_string());
    output.push(value);
    config["args"] = Value::Array(output.into_iter().map(Value::String).collect());
}

fn set_url_param(raw: &str, key: &str, value: &str) -> ApiResult<String> {
    let mut url =
        Url::parse(raw).map_err(|error| ApiError::bad_request(format!("MCP URL 无效：{error}")))?;
    let pairs = url
        .query_pairs()
        .filter(|(name, _)| name != key)
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    url.set_query(None);
    {
        let mut query = url.query_pairs_mut();
        for (name, value) in pairs {
            query.append_pair(&name, &value);
        }
        query.append_pair(key, value);
    }
    Ok(url.to_string())
}

async fn probe_config(config: &Value) -> ProbeResult {
    match resolved_transport(config) {
        Some("stdio") => probe_stdio(config).await,
        Some("sse") => probe_sse(config).await,
        Some("streamableHttp") => probe_streamable_http(config).await,
        _ => ProbeResult::failure("MCP 服务没有可用的 transport"),
    }
}

/// 按扩展工具目录中的服务器和原始工具名执行一次 MCP `tools/call`。
pub async fn call_tool(
    state: &AppState,
    qualified_name: &str,
    arguments: &Value,
) -> Result<Value, String> {
    let entry = super::deferred::provider_entry(state, qualified_name)?
        .ok_or_else(|| format!("扩展工具 {qualified_name} 不在工具目录中。"))?;
    if entry.get("provider_kind").and_then(Value::as_str) == Some("prompt") {
        return Err("该扩展工具不是 MCP 服务工具。".to_string());
    }
    let server = entry
        .get("server")
        .and_then(Value::as_str)
        .ok_or_else(|| "MCP 工具目录缺少服务器名称。".to_string())?;
    let server_tool = entry
        .get("server_tool")
        .and_then(Value::as_str)
        .ok_or_else(|| "MCP 工具目录缺少原始工具名称。".to_string())?;
    let raw = user_servers(state)
        .remove(server)
        .or_else(|| admin_servers(state).remove(server))
        .ok_or_else(|| format!("MCP 服务 {server} 已被移除。"))?;
    if !raw.get("enabled").and_then(Value::as_bool).unwrap_or(true) {
        return Err(format!("MCP 服务 {server} 已停用。"));
    }
    let enabled_tools = string_array(raw.get("enabled_tools"));
    let disabled_tools = string_array(raw.get("disabled_tools"));
    if (!enabled_tools.is_empty()
        && !enabled_tools
            .iter()
            .any(|item| item == "*" || item == server_tool))
        || disabled_tools.iter().any(|item| item == server_tool)
    {
        return Err(format!("MCP 服务 {server} 不允许调用工具 {server_tool}。"));
    }
    let config = materialize_config(state, server, raw).map_err(|error| error.message)?;
    let tool_timeout = Duration::from_secs(
        config
            .get("tool_timeout")
            .and_then(Value::as_u64)
            .unwrap_or(30)
            .clamp(1, 600),
    );
    let call = timeout(tool_timeout, async {
        match resolved_transport(&config) {
            Some("stdio") => call_stdio(&config, server_tool, arguments, state).await,
            Some("streamableHttp") => call_streamable_http(&config, server_tool, arguments).await,
            Some("sse") => call_sse(&config, server_tool, arguments).await,
            _ => Err(ProbeFailure::Failed(
                "MCP 服务没有可用的 transport".to_string(),
            )),
        }
    })
    .await
    .map_err(|_| format!("MCP 工具调用超过 {} 秒。", tool_timeout.as_secs()))?
    .map_err(|error| match error {
        ProbeFailure::AuthRequired(message) | ProbeFailure::Failed(message) => message,
    })?;
    let is_error = call
        .get("isError")
        .or_else(|| call.get("is_error"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if is_error {
        return Err(mcp_result_text(&call));
    }
    Ok(call)
}

async fn call_streamable_http(
    config: &Value,
    tool: &str,
    arguments: &Value,
) -> Result<Value, ProbeFailure> {
    let url = config.get("url").and_then(Value::as_str).unwrap_or("");
    let client = mcp_http_client()?;
    let headers = mcp_headers(config)?;
    let initialize = json_rpc_request(
        1,
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "DeepTutor", "version": env!("CARGO_PKG_VERSION")}
        }),
    );
    let (initialized, session_id) = post_rpc(&client, url, &headers, &initialize, None, 1).await?;
    ensure_rpc_ok(&initialized)?;
    let notification = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    let _ = post_rpc(
        &client,
        url,
        &headers,
        &notification,
        session_id.as_deref(),
        -1,
    )
    .await;
    let request = json_rpc_request(
        2,
        "tools/call",
        json!({"name": tool, "arguments": arguments}),
    );
    let (response, _) =
        post_rpc(&client, url, &headers, &request, session_id.as_deref(), 2).await?;
    ensure_rpc_ok(&response)?;
    response
        .get("result")
        .cloned()
        .ok_or_else(|| ProbeFailure::Failed("MCP tools/call 响应缺少 result。".to_string()))
}

async fn call_sse(config: &Value, tool: &str, arguments: &Value) -> Result<Value, ProbeFailure> {
    let raw_url = config.get("url").and_then(Value::as_str).unwrap_or("");
    let client = mcp_http_client()?;
    let headers = mcp_headers(config)?;
    let mut request = client
        .get(raw_url)
        .header(header::ACCEPT, "text/event-stream");
    for (name, value) in &headers {
        request = request.header(name, value);
    }
    let response = request
        .send()
        .await
        .map_err(|error| ProbeFailure::Failed(format!("MCP SSE 连接失败：{error}")))?;
    if !response.status().is_success() {
        return Err(ProbeFailure::Failed(format!(
            "MCP SSE 服务返回 HTTP {}",
            response.status()
        )));
    }
    let mut stream = response.bytes_stream();
    let mut buffer = Vec::new();
    let endpoint = timeout(Duration::from_secs(8), async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk
                .map_err(|error| ProbeFailure::Failed(format!("读取 MCP SSE 事件失败：{error}")))?;
            if buffer.len() + chunk.len() > 1024 * 1024 {
                return Err(ProbeFailure::Failed("MCP SSE 初始事件过大。".to_string()));
            }
            buffer.extend_from_slice(&chunk);
            if let Some(endpoint) = parse_sse_endpoint(&buffer) {
                return Ok(endpoint);
            }
        }
        Err(ProbeFailure::Failed(
            "MCP SSE 服务没有返回消息端点".to_string(),
        ))
    })
    .await
    .map_err(|_| ProbeFailure::Failed("等待 MCP SSE 端点超时。".to_string()))??;
    let endpoint_url = Url::parse(raw_url)
        .and_then(|base| base.join(&endpoint))
        .map_err(|error| ProbeFailure::Failed(format!("MCP SSE 端点无效：{error}")))?
        .to_string();
    let initialize = json_rpc_request(
        1,
        "initialize",
        json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "DeepTutor", "version": env!("CARGO_PKG_VERSION")}
        }),
    );
    post_rpc_no_response(&client, &endpoint_url, &headers, &initialize).await?;
    post_rpc_no_response(
        &client,
        &endpoint_url,
        &headers,
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await?;
    let call = json_rpc_request(
        2,
        "tools/call",
        json!({"name": tool, "arguments": arguments}),
    );
    post_rpc_no_response(&client, &endpoint_url, &headers, &call).await?;
    let response = timeout(Duration::from_secs(30), async {
        loop {
            if let Some(value) = parse_sse_rpc_events(&buffer, 2) {
                return Ok(value);
            }
            let Some(chunk) = stream.next().await else {
                return Err(ProbeFailure::Failed(
                    "MCP SSE 连接在工具响应前结束。".to_string(),
                ));
            };
            let chunk = chunk
                .map_err(|error| ProbeFailure::Failed(format!("读取 MCP SSE 响应失败：{error}")))?;
            if buffer.len() + chunk.len() > MAX_TOOL_LIST_BYTES {
                return Err(ProbeFailure::Failed("MCP SSE 响应过大。".to_string()));
            }
            buffer.extend_from_slice(&chunk);
        }
    })
    .await
    .map_err(|_| ProbeFailure::Failed("等待 MCP SSE 工具响应超时。".to_string()))??;
    ensure_rpc_ok(&response)?;
    response
        .get("result")
        .cloned()
        .ok_or_else(|| ProbeFailure::Failed("MCP tools/call 响应缺少 result。".to_string()))
}

async fn call_stdio(
    config: &Value,
    tool: &str,
    arguments: &Value,
    state: &AppState,
) -> Result<Value, ProbeFailure> {
    let command = config
        .get("command")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| ProbeFailure::Failed("stdio MCP 缺少 command。".to_string()))?;
    let args = string_array(config.get("args"));
    let mut command_builder = Command::new(command);
    command_builder
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .env_clear();
    for key in [
        "PATH",
        "PATHEXT",
        "SYSTEMROOT",
        "WINDIR",
        "COMSPEC",
        "LD_LIBRARY_PATH",
        "DYLD_LIBRARY_PATH",
    ] {
        if let Ok(value) = std::env::var(key) {
            command_builder.env(key, value);
        }
    }
    let workspace = state
        .paths
        .data
        .join("mcp-workspaces")
        .join(safe_stdio_name(
            config
                .get("catalog_entry")
                .and_then(Value::as_str)
                .unwrap_or(command),
        ));
    std::fs::create_dir_all(&workspace)
        .map_err(|error| ProbeFailure::Failed(format!("无法创建 MCP 工作目录：{error}")))?;
    command_builder
        .current_dir(&workspace)
        .env("HOME", state.paths.home.as_os_str())
        .env("USERPROFILE", state.paths.home.as_os_str())
        .env("TMP", state.paths.temp.as_os_str())
        .env("TEMP", state.paths.temp.as_os_str());
    if let Some(environment) = config.get("env").and_then(Value::as_object) {
        for (key, value) in environment {
            if let Some(value) = value.as_str() {
                command_builder.env(key, value);
            }
        }
    }
    let mut child = command_builder
        .spawn()
        .map_err(|error| ProbeFailure::Failed(format!("无法启动 stdio MCP：{error}")))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| ProbeFailure::Failed("无法打开 stdio MCP 输入。".to_string()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ProbeFailure::Failed("无法打开 stdio MCP 输出。".to_string()))?;
    let initialize = json_rpc_request(
        1,
        "initialize",
        json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "DeepTutor", "version": env!("CARGO_PKG_VERSION")}
        }),
    );
    let call = json_rpc_request(
        2,
        "tools/call",
        json!({"name": tool, "arguments": arguments}),
    );
    let payload = format!(
        "{}\n{}\n{}\n",
        initialize,
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        call
    );
    stdin
        .write_all(payload.as_bytes())
        .await
        .map_err(|error| ProbeFailure::Failed(format!("写入 stdio MCP 失败：{error}")))?;
    stdin
        .flush()
        .await
        .map_err(|error| ProbeFailure::Failed(format!("刷新 stdio MCP 输入失败：{error}")))?;
    let mut lines = BufReader::new(stdout).lines();
    let response = timeout(Duration::from_secs(30), async {
        loop {
            let Some(line) = lines
                .next_line()
                .await
                .map_err(|error| ProbeFailure::Failed(format!("读取 stdio MCP 失败：{error}")))?
            else {
                return Err(ProbeFailure::Failed("stdio MCP 在响应前结束。".to_string()));
            };
            let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            if rpc_id_matches(&value, 2) {
                return Ok(value);
            }
            if rpc_id_matches(&value, 1) {
                ensure_rpc_ok(&value)?;
            }
        }
    })
    .await
    .map_err(|_| ProbeFailure::Failed("stdio MCP 工具调用超过 30 秒。".to_string()))??;
    let _ = child.kill().await;
    ensure_rpc_ok(&response)?;
    response
        .get("result")
        .cloned()
        .ok_or_else(|| ProbeFailure::Failed("MCP tools/call 响应缺少 result。".to_string()))
}

fn parse_sse_rpc_events(buffer: &[u8], expected_id: i64) -> Option<Value> {
    let text = String::from_utf8_lossy(buffer);
    for event in text.split("\n\n") {
        let data = event
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim)
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(&data) {
            if rpc_id_matches(&value, expected_id) {
                return Some(value);
            }
        }
    }
    None
}

fn mcp_result_text(result: &Value) -> String {
    if let Some(content) = result.get("content").and_then(Value::as_array) {
        let text = content
            .iter()
            .filter_map(|item| {
                item.get("text")
                    .and_then(Value::as_str)
                    .or_else(|| item.get("content").and_then(Value::as_str))
            })
            .collect::<Vec<_>>()
            .join("\n");
        if !text.is_empty() {
            return text;
        }
    }
    result
        .get("structuredContent")
        .or_else(|| result.get("structured_content"))
        .map(|value| serde_json::to_string_pretty(value).unwrap_or_default())
        .unwrap_or_else(|| serde_json::to_string_pretty(result).unwrap_or_default())
}

fn safe_stdio_name(value: &str) -> String {
    let value = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    value.trim_matches('_').chars().take(64).collect::<String>()
}

async fn probe_streamable_http(config: &Value) -> ProbeResult {
    let url = config.get("url").and_then(Value::as_str).unwrap_or("");
    let client = match mcp_http_client() {
        Ok(client) => client,
        Err(error) => return ProbeResult::failure(error),
    };
    match streamable_session(&client, url, config).await {
        Ok(tools) => ProbeResult::success(tools),
        Err(ProbeFailure::AuthRequired(error)) => ProbeResult::auth_required(error),
        Err(ProbeFailure::Failed(error)) => ProbeResult::failure(error),
    }
}

async fn streamable_session(
    client: &reqwest::Client,
    url: &str,
    config: &Value,
) -> Result<Vec<Value>, ProbeFailure> {
    let headers = mcp_headers(config)?;
    let initialize = json_rpc_request(
        1,
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {
                "name": "DeepTutor",
                "version": env!("CARGO_PKG_VERSION")
            }
        }),
    );
    let (initialize, session_id) = post_rpc(client, url, &headers, &initialize, None, 1).await?;
    ensure_rpc_ok(&initialize)?;
    let notification = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    let _ = post_rpc(
        client,
        url,
        &headers,
        &notification,
        session_id.as_deref(),
        -1,
    )
    .await;
    let list = json_rpc_request(2, "tools/list", json!({}));
    let (list, _) = post_rpc(client, url, &headers, &list, session_id.as_deref(), 2).await?;
    ensure_rpc_ok(&list)?;
    Ok(parse_tools(&list))
}

async fn probe_sse(config: &Value) -> ProbeResult {
    let raw_url = config.get("url").and_then(Value::as_str).unwrap_or("");
    let client = match mcp_http_client() {
        Ok(client) => client,
        Err(error) => return ProbeResult::failure(error),
    };
    let headers = match mcp_headers(config) {
        Ok(headers) => headers,
        Err(ProbeFailure::AuthRequired(error)) | Err(ProbeFailure::Failed(error)) => {
            return ProbeResult::failure(error)
        }
    };
    let mut request = client
        .get(raw_url)
        .header(header::ACCEPT, "text/event-stream")
        .timeout(Duration::from_secs(6));
    for (name, value) in &headers {
        request = request.header(name, value);
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => return ProbeResult::failure(format!("MCP SSE 连接失败：{error}")),
    };
    if response.status() == StatusCode::UNAUTHORIZED || response.status() == StatusCode::FORBIDDEN {
        return ProbeResult::auth_required(format!(
            "MCP 服务要求授权（HTTP {}）",
            response.status()
        ));
    }
    if !response.status().is_success() {
        return ProbeResult::failure(format!("MCP SSE 服务返回 HTTP {}", response.status()));
    }
    let mut stream = response.bytes_stream();
    let mut buffer = Vec::new();
    let endpoint = timeout(Duration::from_secs(6), async {
        while let Some(chunk) = stream.next().await {
            let Ok(chunk) = chunk else {
                break;
            };
            if buffer.len() + chunk.len() > 256 * 1024 {
                break;
            }
            buffer.extend_from_slice(&chunk);
            if let Some(endpoint) = parse_sse_endpoint(&buffer) {
                return Some(endpoint);
            }
        }
        None
    })
    .await
    .ok()
    .flatten();
    let Some(endpoint) = endpoint else {
        return ProbeResult::failure("MCP SSE 服务没有返回消息端点");
    };
    let endpoint_url = match Url::parse(raw_url).and_then(|base| base.join(&endpoint)) {
        Ok(url) => url.to_string(),
        Err(error) => return ProbeResult::failure(format!("MCP SSE 端点无效：{error}")),
    };
    let initialize = json_rpc_request(
        1,
        "initialize",
        json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "DeepTutor", "version": env!("CARGO_PKG_VERSION")}
        }),
    );
    let posted = post_rpc_no_response(&client, &endpoint_url, &headers, &initialize).await;
    if let Err(ProbeFailure::AuthRequired(error)) = posted {
        return ProbeResult::auth_required(error);
    }
    if let Err(ProbeFailure::Failed(error)) = posted {
        return ProbeResult::failure(error);
    }
    ProbeResult::failure("旧版 MCP SSE 端点已接受请求，但当前连接未保持事件流，无法完成 tools/list；请优先使用 streamableHttp")
}

fn mcp_http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(20))
        .user_agent(format!("DeepTutor/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| format!("无法创建 MCP 网络客户端：{error}"))
}

fn mcp_headers(config: &Value) -> Result<Vec<(HeaderName, HeaderValue)>, ProbeFailure> {
    let mut output = Vec::new();
    if let Some(headers) = config.get("headers").and_then(Value::as_object) {
        for (name, value) in headers {
            let Some(value) = value.as_str() else {
                continue;
            };
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| ProbeFailure::Failed(format!("MCP 请求头名称不合法：{name}")))?;
            let value = HeaderValue::from_str(value)
                .map_err(|_| ProbeFailure::Failed(format!("MCP 请求头 {name} 的值不合法")))?;
            output.push((name, value));
        }
    }
    Ok(output)
}

fn json_rpc_request(id: i64, method: &str, params: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params
    })
}

async fn post_rpc_no_response(
    client: &reqwest::Client,
    url: &str,
    headers: &[(HeaderName, HeaderValue)],
    payload: &Value,
) -> Result<(), ProbeFailure> {
    let mut request = client
        .post(url)
        .header(header::ACCEPT, "application/json, text/event-stream")
        .json(payload);
    for (name, value) in headers {
        request = request.header(name, value);
    }
    let response = request
        .send()
        .await
        .map_err(|error| ProbeFailure::Failed(format!("MCP 请求失败：{error}")))?;
    if matches!(
        response.status(),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
    ) {
        return Err(ProbeFailure::AuthRequired(format!(
            "MCP 服务要求授权（HTTP {}）",
            response.status()
        )));
    }
    if !response.status().is_success() {
        return Err(ProbeFailure::Failed(format!(
            "MCP 服务返回 HTTP {}",
            response.status()
        )));
    }
    Ok(())
}

async fn post_rpc(
    client: &reqwest::Client,
    url: &str,
    headers: &[(HeaderName, HeaderValue)],
    payload: &Value,
    session_id: Option<&str>,
    expected_id: i64,
) -> Result<(Value, Option<String>), ProbeFailure> {
    let mut request = client
        .post(url)
        .header(header::ACCEPT, "application/json, text/event-stream")
        .json(payload);
    for (name, value) in headers {
        request = request.header(name, value);
    }
    if let Some(session_id) = session_id {
        request = request.header("Mcp-Session-Id", session_id);
    }
    let response = request
        .send()
        .await
        .map_err(|error| ProbeFailure::Failed(format!("MCP 请求失败：{error}")))?;
    let status = response.status();
    let response_session = response
        .headers()
        .get("Mcp-Session-Id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return Err(ProbeFailure::AuthRequired(format!(
            "MCP 服务要求授权（HTTP {status}）"
        )));
    }
    if !status.is_success() && status != StatusCode::ACCEPTED {
        let body = response
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(500)
            .collect::<String>();
        return Err(ProbeFailure::Failed(format!(
            "MCP 服务返回 HTTP {status}：{body}"
        )));
    }
    if expected_id < 0 || status == StatusCode::ACCEPTED {
        return Ok((Value::Null, response_session));
    }
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = response
        .bytes()
        .await
        .map_err(|error| ProbeFailure::Failed(format!("无法读取 MCP 响应：{error}")))?;
    if body.len() > MAX_TOOL_LIST_BYTES {
        return Err(ProbeFailure::Failed("MCP 响应过大".to_string()));
    }
    let text = String::from_utf8_lossy(&body);
    let value = parse_rpc_body(&text, &content_type, expected_id)
        .ok_or_else(|| ProbeFailure::Failed("MCP 返回了无法解析的 JSON-RPC 响应".to_string()))?;
    Ok((value, response_session))
}

fn parse_rpc_body(body: &str, content_type: &str, expected_id: i64) -> Option<Value> {
    if content_type.contains("text/event-stream") || body.trim_start().starts_with("event:") {
        for line in body.lines() {
            let Some(raw) = line.strip_prefix("data:") else {
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(raw.trim()) else {
                continue;
            };
            if rpc_id_matches(&value, expected_id) {
                return Some(value);
            }
        }
        return None;
    }
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        if value.is_array() {
            return value.as_array().and_then(|items| {
                items
                    .iter()
                    .find(|item| rpc_id_matches(item, expected_id))
                    .cloned()
            });
        }
        return Some(value);
    }
    for line in body.lines() {
        if let Ok(value) = serde_json::from_str::<Value>(line.trim()) {
            if rpc_id_matches(&value, expected_id) {
                return Some(value);
            }
        }
    }
    None
}

fn rpc_id_matches(value: &Value, expected_id: i64) -> bool {
    value.get("id").and_then(Value::as_i64) == Some(expected_id)
}

fn ensure_rpc_ok(value: &Value) -> Result<(), ProbeFailure> {
    if let Some(error) = value.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("MCP JSON-RPC 调用失败");
        return Err(ProbeFailure::Failed(message.to_string()));
    }
    Ok(())
}

fn parse_tools(value: &Value) -> Vec<Value> {
    value
        .pointer("/result/tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| {
                    let name = tool.get("name").and_then(Value::as_str)?;
                    Some(json!({
                        "name": name,
                        "description": tool.get("description").and_then(Value::as_str).unwrap_or(""),
                        "inputSchema": tool
                            .get("inputSchema")
                            .or_else(|| tool.get("input_schema"))
                            .cloned()
                            .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                        "annotations": tool.get("annotations").cloned().unwrap_or_else(|| json!({}))
                    }))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_sse_endpoint(buffer: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(buffer);
    let mut event = "";
    for line in text.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("event:") {
            event = value.trim();
            continue;
        }
        if event == "endpoint" {
            if let Some(value) = line.strip_prefix("data:") {
                let value = value.trim();
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }
    None
}

async fn probe_stdio(config: &Value) -> ProbeResult {
    let command = config.get("command").and_then(Value::as_str).unwrap_or("");
    if command.is_empty() {
        return ProbeResult::failure("stdio MCP 服务缺少 command");
    }
    let args = config
        .get("args")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut command_builder = Command::new(command);
    command_builder
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(cwd) = config.get("cwd").and_then(Value::as_str) {
        if !cwd.trim().is_empty() {
            let cwd = PathBuf::from(cwd);
            if !cwd.is_absolute() {
                return ProbeResult::failure("stdio MCP 的 cwd 必须是绝对路径");
            }
            command_builder.current_dir(cwd);
        }
    }
    if let Some(environment) = config.get("env").and_then(Value::as_object) {
        for (key, value) in environment {
            if let Some(value) = value.as_str() {
                command_builder.env(key, value);
            }
        }
    }
    let mut child = match command_builder.spawn() {
        Ok(child) => child,
        Err(error) => {
            return ProbeResult::failure(format!("无法启动 stdio MCP 命令：{error}"));
        }
    };
    let Some(mut stdin) = child.stdin.take() else {
        return ProbeResult::failure("无法打开 stdio MCP 进程输入");
    };
    let Some(stdout) = child.stdout.take() else {
        return ProbeResult::failure("无法打开 stdio MCP 进程输出");
    };
    let initialize = json_rpc_request(
        1,
        "initialize",
        json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "DeepTutor", "version": env!("CARGO_PKG_VERSION")}
        }),
    );
    let initialized = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
    let list = json_rpc_request(2, "tools/list", json!({}));
    let payload = format!("{initialize}\n{initialized}\n{list}\n");
    if let Err(error) = stdin.write_all(payload.as_bytes()).await {
        let _ = child.kill().await;
        return ProbeResult::failure(format!("无法写入 stdio MCP 进程：{error}"));
    }
    let _ = stdin.flush().await;
    let mut lines = BufReader::new(stdout).lines();
    let read = timeout(Duration::from_secs(8), async {
        loop {
            let line = lines.next_line().await.ok().flatten()?;
            let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            if rpc_id_matches(&value, 2) {
                return Some(value);
            }
            if rpc_id_matches(&value, 1) {
                if let Some(error) = value.get("error") {
                    return Some(json!({"error": error}));
                }
            }
        }
    })
    .await;
    let _ = child.kill().await;
    match read {
        Ok(Some(value)) => match ensure_rpc_ok(&value) {
            Ok(()) => ProbeResult::success(parse_tools(&value)),
            Err(ProbeFailure::AuthRequired(error)) | Err(ProbeFailure::Failed(error)) => {
                ProbeResult::failure(error)
            }
        },
        Ok(None) => ProbeResult::failure("stdio MCP 进程提前结束"),
        Err(_) => ProbeResult::failure("stdio MCP 初始化和 tools/list 超过 8 秒"),
    }
}

fn read_oauth_token(state: &AppState, server: &str) -> ApiResult<Option<Value>> {
    let path = oauth_path(state, server);
    if !path.is_file() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path)
        .map_err(|error| ApiError::internal(format!("无法读取 MCP OAuth 令牌：{error}")))?;
    let value = serde_json::from_str(&raw)
        .map_err(|error| ApiError::internal(format!("MCP OAuth 令牌文件已损坏：{error}")))?;
    Ok(Some(value))
}

fn oauth_authorized(state: &AppState, server: &str) -> bool {
    read_oauth_token(state, server)
        .ok()
        .flatten()
        .and_then(|value| {
            value
                .get("access_token")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .is_some_and(|token| !token.is_empty())
}

async fn begin_oauth(
    state: &AppState,
    name: &str,
    config: &Value,
    origin: &str,
) -> ApiResult<String> {
    let resource_url = config
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad_request("OAuth MCP 服务缺少 URL"))?;
    let metadata = discover_oauth_metadata(&state.client, resource_url).await?;
    let authorization_endpoint = metadata
        .get("authorization_endpoint")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::provider("OAuth 元数据缺少 authorization_endpoint"))?;
    let token_endpoint = metadata
        .get("token_endpoint")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::provider("OAuth 元数据缺少 token_endpoint"))?;
    let redirect_uri = format!(
        "{}/api/space/mcp/oauth/callback",
        origin.trim_end_matches('/')
    );
    let (client_id, client_secret) = register_oauth_client(state, &metadata, &redirect_uri).await?;
    let mut verifier_bytes = [0u8; 48];
    rand::thread_rng().fill_bytes(&mut verifier_bytes);
    let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut state_bytes = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut state_bytes);
    let oauth_state = URL_SAFE_NO_PAD.encode(state_bytes);
    let pending_key = format!("mcp_oauth_pending:{oauth_state}");
    state.database.set_setting(
        &pending_key,
        &json!({
            "server": name,
            "verifier": verifier,
            "client_id": client_id,
            "client_secret": client_secret,
            "token_endpoint": token_endpoint,
            "redirect_uri": redirect_uri,
            "resource": resource_url,
            "created_at": chrono::Utc::now().to_rfc3339()
        }),
    )?;
    let mut authorize = Url::parse(authorization_endpoint)
        .map_err(|error| ApiError::provider(format!("OAuth 授权地址无效：{error}")))?;
    {
        let mut query = authorize.query_pairs_mut();
        query.append_pair("response_type", "code");
        query.append_pair("client_id", &client_id);
        query.append_pair("redirect_uri", &redirect_uri);
        query.append_pair("state", &oauth_state);
        query.append_pair("code_challenge", &challenge);
        query.append_pair("code_challenge_method", "S256");
        query.append_pair("resource", resource_url);
        if let Some(scope) = metadata.get("scopes_supported").and_then(Value::as_array) {
            let values = scope.iter().filter_map(Value::as_str).collect::<Vec<_>>();
            if !values.is_empty() {
                query.append_pair("scope", &values.join(" "));
            }
        }
    }
    Ok(authorize.to_string())
}

async fn discover_oauth_metadata(client: &reqwest::Client, resource_url: &str) -> ApiResult<Value> {
    let resource = Url::parse(resource_url)
        .map_err(|error| ApiError::bad_request(format!("MCP URL 无效：{error}")))?;
    let origin = format!(
        "{}://{}",
        resource.scheme(),
        resource
            .host_str()
            .ok_or_else(|| ApiError::bad_request("MCP URL 缺少主机名"))?
    );
    let mut issuer = origin.clone();
    let mut protected_locations = Vec::new();
    let path = resource.path().trim_end_matches('/');
    protected_locations.push(format!(
        "{origin}/.well-known/oauth-protected-resource{path}"
    ));
    protected_locations.push(format!("{origin}/.well-known/oauth-protected-resource"));
    for location in protected_locations {
        if let Ok(response) = client.get(&location).send().await {
            if response.status().is_success() {
                if let Ok(value) = response.json::<Value>().await {
                    if let Some(server) = value
                        .get("authorization_servers")
                        .and_then(Value::as_array)
                        .and_then(|items| items.first())
                        .and_then(Value::as_str)
                    {
                        issuer = server.trim_end_matches('/').to_string();
                    }
                    break;
                }
            }
        }
    }
    let issuer_url = Url::parse(&issuer)
        .map_err(|error| ApiError::provider(format!("OAuth issuer 无效：{error}")))?;
    let issuer_path = issuer_url.path().trim_end_matches('/');
    let mut metadata_locations = if issuer_path.is_empty() {
        vec![
            format!(
                "{}://{}/.well-known/oauth-authorization-server",
                issuer_url.scheme(),
                issuer_url.host_str().unwrap_or_default()
            ),
            format!(
                "{}://{}/.well-known/openid-configuration",
                issuer_url.scheme(),
                issuer_url.host_str().unwrap_or_default()
            ),
        ]
    } else {
        vec![
            format!(
                "{}://{}/.well-known/oauth-authorization-server{issuer_path}",
                issuer_url.scheme(),
                issuer_url.host_str().unwrap_or_default()
            ),
            format!(
                "{}://{}/.well-known/openid-configuration{issuer_path}",
                issuer_url.scheme(),
                issuer_url.host_str().unwrap_or_default()
            ),
            format!("{issuer}/.well-known/openid-configuration"),
        ]
    };
    if metadata_locations.is_empty() {
        metadata_locations.push(format!("{issuer}/.well-known/openid-configuration"));
    }
    let mut last_error = String::new();
    for location in metadata_locations {
        match client.get(&location).send().await {
            Ok(response) if response.status().is_success() => {
                if let Ok(value) = response.json::<Value>().await {
                    if value.get("authorization_endpoint").is_some()
                        && value.get("token_endpoint").is_some()
                    {
                        return Ok(value);
                    }
                }
            }
            Ok(response) => last_error = format!("{location} 返回 HTTP {}", response.status()),
            Err(error) => last_error = format!("{location} 请求失败：{error}"),
        }
    }
    Err(ApiError::provider(if last_error.is_empty() {
        "没有发现可用的 OAuth 授权服务器元数据".to_string()
    } else {
        format!("无法发现 MCP OAuth 元数据：{last_error}")
    }))
}

async fn register_oauth_client(
    state: &AppState,
    metadata: &Value,
    redirect_uri: &str,
) -> ApiResult<(String, String)> {
    let Some(registration_endpoint) = metadata
        .get("registration_endpoint")
        .and_then(Value::as_str)
    else {
        return Err(ApiError::provider(
            "OAuth 服务不支持动态客户端注册，无法开始授权",
        ));
    };
    let response = state
        .client
        .post(registration_endpoint)
        .json(&json!({
            "client_name": "DeepTutor Desktop",
            "redirect_uris": [redirect_uri],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
            "application_type": "native"
        }))
        .send()
        .await
        .map_err(|error| ApiError::provider(format!("OAuth 客户端注册失败：{error}")))?;
    if !response.status().is_success() {
        return Err(ApiError::provider(format!(
            "OAuth 客户端注册返回 HTTP {}",
            response.status()
        )));
    }
    let value = response
        .json::<Value>()
        .await
        .map_err(|error| ApiError::provider(format!("OAuth 注册响应无效：{error}")))?;
    let client_id = value
        .get("client_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiError::provider("OAuth 注册响应缺少 client_id"))?
        .to_string();
    let client_secret = value
        .get("client_secret")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Ok((client_id, client_secret))
}

async fn complete_oauth(state: &AppState, oauth_state: &str, code: &str) -> ApiResult<()> {
    let pending_key = format!("mcp_oauth_pending:{oauth_state}");
    let pending = state
        .database
        .get_setting(&pending_key)?
        .ok_or_else(|| ApiError::bad_request("OAuth state 不存在或已过期"))?;
    let server = pending
        .get("server")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::internal("OAuth 待处理状态缺少服务名"))?;
    let verifier = pending
        .get("verifier")
        .and_then(Value::as_str)
        .unwrap_or("");
    let client_id = pending
        .get("client_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let client_secret = pending
        .get("client_secret")
        .and_then(Value::as_str)
        .unwrap_or("");
    let token_endpoint = pending
        .get("token_endpoint")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::internal("OAuth 待处理状态缺少 token_endpoint"))?;
    let redirect_uri = pending
        .get("redirect_uri")
        .and_then(Value::as_str)
        .unwrap_or("");
    let resource = pending
        .get("resource")
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
        ("code_verifier", verifier),
    ];
    if !resource.is_empty() {
        form.push(("resource", resource));
    }
    if !client_secret.is_empty() {
        form.push(("client_secret", client_secret));
    }
    let response = state
        .client
        .post(token_endpoint)
        .form(&form)
        .send()
        .await
        .map_err(|error| ApiError::provider(format!("OAuth 令牌交换失败：{error}")))?;
    if !response.status().is_success() {
        return Err(ApiError::provider(format!(
            "OAuth 令牌交换返回 HTTP {}",
            response.status()
        )));
    }
    let mut token = response
        .json::<Value>()
        .await
        .map_err(|error| ApiError::provider(format!("OAuth 令牌响应无效：{error}")))?;
    if token
        .get("access_token")
        .and_then(Value::as_str)
        .unwrap_or("")
        .is_empty()
    {
        return Err(ApiError::provider("OAuth 令牌响应缺少 access_token"));
    }
    token["obtained_at"] = Value::String(chrono::Utc::now().to_rfc3339());
    token["client_id"] = Value::String(client_id.to_string());
    token["client_secret"] = Value::String(client_secret.to_string());
    token["token_endpoint"] = Value::String(token_endpoint.to_string());
    write_json_file(&oauth_path(state, server), &token)?;
    state.database.set_setting(&pending_key, &Value::Null)?;
    Ok(())
}

fn request_origin(headers: &HeaderMap) -> String {
    let forwarded_proto = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let host = headers
        .get("x-forwarded-host")
        .and_then(|value| value.to_str().ok())
        .or_else(|| {
            headers
                .get(header::HOST)
                .and_then(|value| value.to_str().ok())
        })
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    match (forwarded_proto, host) {
        (Some(proto), Some(host)) if matches!(proto, "http" | "https") => {
            format!("{proto}://{host}")
        }
        (_, Some(host)) => format!("http://{host}"),
        _ => String::new(),
    }
}

fn oauth_callback_page(ok: bool, detail: &str) -> Html<String> {
    let title = if ok {
        "DeepTutor 授权完成"
    } else {
        "DeepTutor 授权失败"
    };
    let escaped = html_escape(detail);
    let detail_html = if ok {
        String::new()
    } else {
        format!("<p class=\"detail\">{escaped}</p>")
    };
    let delay = if ok { 1200 } else { 6000 };
    Html(format!(
        r#"<!doctype html>
<html lang="zh-CN"><head><meta charset="utf-8"><title>{title}</title>
<style>
:root{{color-scheme:light dark}}body{{margin:0;min-height:100vh;display:grid;place-items:center;
font:15px/1.7 ui-sans-serif,system-ui,sans-serif;background:#f5f3ec;color:#1f2922}}
main{{max-width:38rem;padding:2rem 2.5rem;border:1px solid #b9c3b9;border-radius:1rem;background:#fff}}
.detail{{color:#8b3a2e;word-break:break-word}}@media(prefers-color-scheme:dark){{
body{{background:#151915;color:#edf2ea}}main{{background:#202720;border-color:#465246}}}}
</style></head><body><main><h1>{title}</h1>{detail_html}
<p>窗口将在几秒后自动关闭。</p></main>
<script>setTimeout(function(){{window.close()}},{delay})</script></body></html>"#
    ))
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn cli_catalog_entries() -> Vec<Value> {
    let raw = serde_json::from_str::<Value>(CLI_CATALOG_JSON).unwrap_or_else(|_| json!({}));
    let pin = raw
        .pointer("/meta/commit")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    raw.get("apps")
        .and_then(Value::as_array)
        .map(|apps| {
            apps.iter()
                .filter_map(|app| normalize_cli_catalog_entry(app, &pin))
                .collect()
        })
        .unwrap_or_default()
}

fn normalize_cli_catalog_entry(raw: &Value, pin: &str) -> Option<Value> {
    let id = raw
        .get("name")
        .or_else(|| raw.get("id"))
        .and_then(Value::as_str)?
        .trim()
        .to_string();
    if APP_ID_RE.is_match(&id) == false {
        return None;
    }
    let install_cmd = raw
        .get("install_cmd")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let (runtime, kind, target) = cli_install_plan(install_cmd);
    let installable = !target.is_empty();
    let reason = if installable {
        String::new()
    } else if install_cmd.is_empty() {
        "该应用没有可自动安装的包；请按安装说明手动部署".to_string()
    } else {
        format!("DeepTutor 便携版不会执行系统级安装命令或 shell 管道：{install_cmd}")
    };
    Some(json!({
        "id": id,
        "display_name": raw.get("display_name").or_else(|| raw.get("name")).cloned().unwrap_or(Value::Null),
        "description": raw.get("description").cloned().unwrap_or_else(|| json!("")),
        "category": raw.get("category").cloned().unwrap_or_else(|| json!("other")),
        "origin": raw.get("origin").cloned().unwrap_or_else(|| json!("public")),
        "trust": if matches!(raw.get("origin").and_then(Value::as_str), Some("harness" | "first-party")) {
            "first-party"
        } else {
            "third-party"
        },
        "requires": raw.get("requires").cloned().unwrap_or_else(|| json!("")),
        "homepage": raw.get("homepage").cloned().unwrap_or_else(|| json!("")),
        "source_url": raw.get("source_url").or_else(|| raw.get("homepage")).cloned().unwrap_or_else(|| json!("")),
        "entry_point": raw.get("entry_point").cloned().unwrap_or_else(|| json!("")),
        "runtime": runtime,
        "install_kind": kind,
        "install_target": target,
        "installable": installable,
        "pinned": !pin.is_empty() && install_cmd.contains(&pin),
        "unsupported_reason": reason,
        "install_notes": raw.get("install_notes").cloned().unwrap_or_else(|| json!("")),
        "version": raw.get("version").cloned().unwrap_or_else(|| json!("")),
        "catalog_pin": pin
    }))
}

fn cli_install_plan(command: &str) -> (&'static str, &'static str, String) {
    let command = command.trim();
    let lower = command.to_ascii_lowercase();
    if lower.starts_with("pip install ") {
        let target = command["pip install ".len()..]
            .split_whitespace()
            .filter(|value| !value.starts_with('-'))
            .collect::<Vec<_>>()
            .join(" ");
        if !target.is_empty() && cli_target_is_safe(&target) {
            return ("python", "pip", target);
        }
    }
    if lower.starts_with("npm install -g ") || lower.starts_with("npm install --global ") {
        let offset = if lower.starts_with("npm install -g ") {
            "npm install -g ".len()
        } else {
            "npm install --global ".len()
        };
        let target = command[offset..]
            .split_whitespace()
            .filter(|value| !value.starts_with('-'))
            .collect::<Vec<_>>()
            .join(" ");
        if !target.is_empty() && cli_target_is_safe(&target) {
            return ("node", "npm", target);
        }
    }
    ("none", "", String::new())
}

fn cli_target_is_safe(target: &str) -> bool {
    !target.is_empty()
        && !target
            .chars()
            .any(|character| matches!(character, '\0' | '\r' | '\n'))
        && ![";", "&&", "||", "|", ">", "<", "`", "$("]
            .iter()
            .any(|token| target.contains(token))
}

fn cli_catalog_pin() -> String {
    serde_json::from_str::<Value>(CLI_CATALOG_JSON)
        .ok()
        .and_then(|value| {
            value
                .pointer("/meta/commit")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default()
}

fn cli_catalog_page(state: &AppState, query: &CatalogQuery) -> ApiResult<Value> {
    let installed = state
        .database
        .list_resources(CLI_STATE_DOMAIN)?
        .into_iter()
        .filter_map(|row| {
            let id = row.get("id").and_then(Value::as_str)?.to_string();
            Some((id, row))
        })
        .collect::<HashMap<_, _>>();
    let needle = query.q.as_deref().unwrap_or("").trim().to_lowercase();
    let category = query.category.as_deref().unwrap_or("").trim();
    let installable_only = query.installable_only.unwrap_or(true);
    let mut counts = BTreeMap::<String, usize>::new();
    let mut entries = Vec::new();
    for entry in cli_catalog_entries() {
        if installable_only
            && !entry
                .get("installable")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        {
            continue;
        }
        let entry_category = entry
            .get("category")
            .and_then(Value::as_str)
            .unwrap_or("other");
        let haystack = [
            entry.get("id").and_then(Value::as_str).unwrap_or(""),
            entry
                .get("display_name")
                .and_then(Value::as_str)
                .unwrap_or(""),
            entry
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or(""),
            entry_category,
        ]
        .join(" ")
        .to_lowercase();
        if !needle.is_empty() && !haystack.contains(&needle) {
            continue;
        }
        *counts.entry(entry_category.to_string()).or_default() += 1;
        if !category.is_empty() && entry_category != category {
            continue;
        }
        entries.push(entry);
    }
    entries.sort_by(|left, right| {
        left.get("display_name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_lowercase()
            .cmp(
                &right
                    .get("display_name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_lowercase(),
            )
    });
    let total = entries.len();
    let limit = query.limit.unwrap_or(24).clamp(1, MAX_CATALOG_PAGE);
    let start = query
        .cursor
        .as_deref()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0)
        .min(total);
    let end = (start + limit).min(total);
    let rows = entries[start..end]
        .iter()
        .map(|entry| {
            let id = entry.get("id").and_then(Value::as_str).unwrap_or("");
            let mut row = entry.clone();
            row["installed"] = Value::Bool(installed.contains_key(id));
            row
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "entries": rows,
        "next_cursor": if end < total { end.to_string() } else { String::new() },
        "total": total,
        "categories": counts
            .into_iter()
            .map(|(category, count)| (category, Value::from(count)))
            .collect::<Map<_, _>>(),
        "catalog_pin": cli_catalog_pin()
    }))
}

fn cli_state(state: &AppState, principal: &Principal) -> ApiResult<Value> {
    let catalog = cli_catalog_entries()
        .into_iter()
        .map(|entry| {
            (
                entry
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                entry,
            )
        })
        .collect::<HashMap<_, _>>();
    let disabled = cli_disabled_apps(state, principal)?;
    let allowed = cli_allowed_apps(state, principal)?;
    let exec_denied = cli_exec_denied(state, principal)?;
    let mut apps = state
        .database
        .list_resources(CLI_STATE_DOMAIN)?
        .into_iter()
        .filter_map(|installed| {
            let id = installed.get("id").and_then(Value::as_str)?.to_string();
            let entry = catalog.get(&id);
            let granted = allowed
                .as_ref()
                .map(|values| values.contains(&id))
                .unwrap_or(true);
            Some(json!({
                "id": id,
                "display_name": entry
                    .and_then(|entry| entry.get("display_name"))
                    .cloned()
                    .unwrap_or_else(|| Value::String(id.clone())),
                "description": entry
                    .and_then(|entry| entry.get("description"))
                    .cloned()
                    .unwrap_or_else(|| json!("")),
                "category": entry
                    .and_then(|entry| entry.get("category"))
                    .cloned()
                    .unwrap_or_else(|| json!("")),
                "tool_name": format!("cli_{}", id.replace(['.', '-'], "_")),
                "entry_point": installed.get("entry_point").cloned().unwrap_or_else(|| json!("")),
                "runtime": installed.get("runtime").cloned().unwrap_or_else(|| json!("none")),
                "installed_at": installed.get("installed_at").cloned().unwrap_or_else(|| json!("")),
                "version": installed.get("version").cloned().unwrap_or_else(|| json!("")),
                "pin": installed.get("pin").cloned().unwrap_or_else(|| json!("")),
                "trust": entry
                    .and_then(|entry| entry.get("trust"))
                    .cloned()
                    .unwrap_or_else(|| json!("third-party")),
                "granted": granted,
                "enabled": granted && !disabled.contains(&id),
                "in_catalog": entry.is_some()
            }))
        })
        .collect::<Vec<_>>();
    apps.sort_by(|left, right| {
        left.get("id")
            .and_then(Value::as_str)
            .cmp(&right.get("id").and_then(Value::as_str))
    });
    Ok(json!({
        "apps": apps,
        "access": {
            "unrestricted": principal.is_admin(),
            "exec_denied": exec_denied
        },
        "catalog_pin": cli_catalog_pin()
    }))
}

fn cli_allowed_apps(
    state: &AppState,
    principal: &Principal,
) -> ApiResult<Option<BTreeSet<String>>> {
    if principal.is_admin() {
        return Ok(None);
    }
    let grant = user_grant(state, &principal.user_id)?;
    let values = grant
        .get("cli_apps")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    Ok(Some(values))
}

fn cli_exec_denied(state: &AppState, principal: &Principal) -> ApiResult<bool> {
    if principal.is_admin() {
        return Ok(false);
    }
    let grant = user_grant(state, &principal.user_id)?;
    Ok(grant.get("exec_enabled").and_then(Value::as_bool) == Some(false))
}

fn user_grant(state: &AppState, user_id: &str) -> ApiResult<Value> {
    Ok(state
        .database
        .with_connection(|connection| {
            connection
                .query_row(
                    "SELECT payload_json FROM user_grants WHERE user_id = ?1",
                    rusqlite::params![user_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(|error| format!("无法读取用户授权：{error}"))
        })?
        .map(|raw| serde_json::from_str(&raw).map_err(|error| format!("用户授权数据损坏：{error}")))
        .transpose()
        .map(|value| value.unwrap_or_else(|| json!({})))?)
}

fn cli_disabled_apps(state: &AppState, principal: &Principal) -> ApiResult<BTreeSet<String>> {
    let key = format!("{CLI_DISABLED_KEY}:{}", principal.user_id);
    Ok(state
        .database
        .get_setting(&key)?
        .and_then(|value| value.as_array().cloned())
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default())
}

fn save_cli_disabled_apps(
    state: &AppState,
    principal: &Principal,
    disabled: BTreeSet<String>,
) -> ApiResult<()> {
    let key = format!("{CLI_DISABLED_KEY}:{}", principal.user_id);
    state.database.set_setting(
        &key,
        &Value::Array(disabled.into_iter().map(Value::String).collect()),
    )?;
    Ok(())
}

async fn install_portable_cli_app(
    state: &AppState,
    app_id: &str,
    entry: &Value,
) -> Result<String, (String, String, String)> {
    let runtime = entry
        .get("runtime")
        .and_then(Value::as_str)
        .unwrap_or("none");
    let target = entry
        .get("install_target")
        .and_then(Value::as_str)
        .unwrap_or("");
    let entry_point = entry
        .get("entry_point")
        .and_then(Value::as_str)
        .unwrap_or("");
    if target.is_empty() || !matches!(runtime, "python" | "node") {
        return Err((
            "cli.not_installable".to_string(),
            entry
                .get("unsupported_reason")
                .and_then(Value::as_str)
                .unwrap_or("该 CLI 应用不支持便携式安装")
                .to_string(),
            String::new(),
        ));
    }
    let app_root = safe_child(&state.paths.data.join("cli-apps"), app_id).map_err(|error| {
        (
            "cli.install_failed".to_string(),
            error.message,
            String::new(),
        )
    })?;
    let runtime_dir = app_root.join("runtime");
    let staging = app_root.join(format!("runtime.staging-{}", Uuid::new_v4()));
    if staging.exists() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    std::fs::create_dir_all(&staging).map_err(|error| {
        (
            "cli.install_failed".to_string(),
            format!("无法创建 CLI 应用目录：{error}"),
            String::new(),
        )
    })?;
    let environment = clean_install_environment(state);
    let (steps, executable): (Vec<(Vec<String>, HashMap<String, String>)>, PathBuf) =
        if runtime == "python" {
            let python = find_command("python")
                .or_else(|| find_command("python3"))
                .ok_or_else(|| {
                    (
                        "cli.python_missing".to_string(),
                        "该应用需要外部 Python 解释器，但 PATH 中未找到 python".to_string(),
                        String::new(),
                    )
                })?;
            let mut venv_env = environment.clone();
            venv_env.insert("PYTHONDONTWRITEBYTECODE".into(), "1".into());
            let venv = (
                vec![
                    python.display().to_string(),
                    "-m".into(),
                    "venv".into(),
                    staging.display().to_string(),
                ],
                venv_env.clone(),
            );
            let python_staged = if cfg!(windows) {
                staging.join("Scripts").join("python.exe")
            } else {
                staging.join("bin").join("python")
            };
            let mut pip_env = venv_env;
            pip_env.insert("PIP_DISABLE_PIP_VERSION_CHECK".into(), "1".into());
            let pip = (
                vec![
                    python_staged.display().to_string(),
                    "-m".into(),
                    "pip".into(),
                    "install".into(),
                    "--no-input".into(),
                    "--disable-pip-version-check".into(),
                    target.to_string(),
                ],
                pip_env,
            );
            let executable = if cfg!(windows) {
                staging.join("Scripts").join(format!("{entry_point}.exe"))
            } else {
                staging.join("bin").join(entry_point)
            };
            (vec![venv, pip], executable)
        } else {
            let npm = find_command("npm").ok_or_else(|| {
                (
                    "cli.node_missing".to_string(),
                    "该应用需要外部 Node.js/npm，但 PATH 中未找到 npm".to_string(),
                    String::new(),
                )
            })?;
            let mut npm_env = environment;
            npm_env.insert("npm_config_ignore_scripts".into(), "1".into());
            npm_env.insert("npm_config_fund".into(), "false".into());
            npm_env.insert("npm_config_audit".into(), "false".into());
            npm_env.insert("npm_config_update_notifier".into(), "false".into());
            let step = (
                vec![
                    npm.display().to_string(),
                    "install".into(),
                    "--global".into(),
                    "--prefix".into(),
                    staging.display().to_string(),
                    target.to_string(),
                ],
                npm_env,
            );
            let executable = if cfg!(windows) {
                staging
                    .join("node_modules")
                    .join(".bin")
                    .join(format!("{entry_point}.cmd"))
            } else {
                staging.join("bin").join(entry_point)
            };
            (vec![step], executable)
        };
    let mut log = String::new();
    let mut failed = None;
    for (argv, env) in steps {
        let display = argv.join(" ");
        log.push_str(&format!("$ {display}\n"));
        match run_install_command(&argv, &env, Duration::from_secs(900)).await {
            Ok(output) => {
                log.push_str(&output);
                if !log.ends_with('\n') {
                    log.push('\n');
                }
            }
            Err(error) => {
                failed = Some(error);
                break;
            }
        }
    }
    if failed.is_none() && !executable.is_file() {
        failed = Some(format!(
            "安装完成但入口命令不存在：{}。该包可能发布为其他命令名",
            executable.display()
        ));
    }
    if let Some(error) = failed {
        let _ = std::fs::remove_dir_all(&staging);
        let tail = tail_chars(&log, 20_000);
        let error_log = tail_chars(&format!("{log}\nFAILED: {error}"), 20_000);
        write_cli_install_log(state, app_id, &format!("{tail}\nFAILED: {error}\n"));
        return Err(("cli.install_failed".to_string(), error, error_log));
    }
    if runtime_dir.exists() {
        let backup = app_root.join(format!("runtime.backup-{}", Uuid::new_v4()));
        std::fs::rename(&runtime_dir, &backup).map_err(|error| {
            (
                "cli.install_failed".to_string(),
                format!("无法备份旧 CLI 运行时：{error}"),
                String::new(),
            )
        })?;
        if let Err(error) = std::fs::rename(&staging, &runtime_dir) {
            let _ = std::fs::rename(&backup, &runtime_dir);
            return Err((
                "cli.install_failed".to_string(),
                format!("无法启用新 CLI 运行时：{error}"),
                String::new(),
            ));
        }
        let _ = std::fs::remove_dir_all(backup);
    } else if let Err(error) = std::fs::rename(&staging, &runtime_dir) {
        return Err((
            "cli.install_failed".to_string(),
            format!("无法启用 CLI 运行时：{error}"),
            String::new(),
        ));
    }
    let final_executable = executable.strip_prefix(&staging).unwrap_or(&executable);
    let final_executable = runtime_dir.join(final_executable);
    let installed_at = chrono::Utc::now().to_rfc3339();
    state
        .database
        .put_resource(
            CLI_STATE_DOMAIN,
            app_id,
            &json!({
                "id": app_id,
                "entry_point": entry_point,
                "executable": final_executable.display().to_string(),
                "runtime": runtime,
                "kind": entry.get("install_kind").cloned().unwrap_or_else(|| json!("")),
                "target": target,
                "pin": entry.get("catalog_pin").cloned().unwrap_or_else(|| json!("")),
                "installed_at": installed_at,
                "version": entry.get("version").cloned().unwrap_or_else(|| json!(""))
            }),
        )
        .map_err(|error| ("cli.install_failed".to_string(), error, String::new()))?;
    let log = tail_chars(&log, 20_000);
    write_cli_install_log(state, app_id, &log);
    Ok(log)
}

fn clean_install_environment(state: &AppState) -> HashMap<String, String> {
    let mut output = HashMap::new();
    for key in ["PATH", "SystemRoot", "ComSpec", "PATHEXT", "LANG", "LC_ALL"] {
        if let Some(value) = std::env::var_os(key) {
            output.insert(key.to_string(), value.to_string_lossy().into_owned());
        }
    }
    for (key, value) in state.paths.environment() {
        output.insert(key, value);
    }
    output
}

fn find_command(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let extensions = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
            .split(';')
            .map(str::to_string)
            .collect::<Vec<_>>()
    } else {
        vec![String::new()]
    };
    for directory in std::env::split_paths(&path) {
        for extension in &extensions {
            let candidate = directory.join(format!("{name}{extension}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

async fn run_install_command(
    argv: &[String],
    environment: &HashMap<String, String>,
    limit: Duration,
) -> Result<String, String> {
    if argv.is_empty() {
        return Err("安装命令为空".to_string());
    }
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (key, value) in environment {
        command.env(key, value);
    }
    let child = command
        .spawn()
        .map_err(|error| format!("无法启动 `{}`：{error}", argv[0]))?;
    let output = timeout(limit, child.wait_with_output())
        .await
        .map_err(|_| format!("安装命令 `{}` 超时", argv[0]))?
        .map_err(|error| format!("安装命令 `{}` 执行失败：{error}", argv[0]))?;
    let mut log = String::from_utf8_lossy(&output.stdout).into_owned();
    log.push_str(&String::from_utf8_lossy(&output.stderr));
    if !output.status.success() {
        return Err(format!(
            "安装命令 `{}` 退出码 {}",
            argv[0],
            output.status.code().unwrap_or(-1)
        ));
    }
    Ok(tail_chars(&log, 100_000))
}

fn write_cli_install_log(state: &AppState, app_id: &str, log: &str) {
    let path = state
        .paths
        .data
        .join("cli-apps")
        .join(app_id)
        .join("install.log");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, log);
}

fn tail_chars(value: &str, limit: usize) -> String {
    let count = value.chars().count();
    if count <= limit {
        return value.to_string();
    }
    value.chars().skip(count - limit).collect()
}

fn safe_child(root: &FilePath, component: &str) -> ApiResult<PathBuf> {
    if component.is_empty()
        || component == "."
        || component == ".."
        || component.contains('/')
        || component.contains('\\')
    {
        return Err(ApiError::bad_request("路径组件不合法"));
    }
    Ok(root.join(component))
}

fn business_error(status: StatusCode, code: &str, message: String, log: &str) -> Response {
    (
        status,
        Json(json!({
            "detail": {
                "code": code,
                "message": message,
                "log": log
            },
            "error_code": code,
            "message": message,
            "retryable": false
        })),
    )
        .into_response()
}
