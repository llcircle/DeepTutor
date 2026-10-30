use super::auth::{require_admin, Principal};
use super::error::{ApiError, ApiResult};
use super::plugin_wasm;
use super::{AppState, ScopedState};
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, RawQuery, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{delete, get, post};
use axum::{Extension, Json, Router};
use base64::Engine as _;
use chrono::Utc;
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Component, Path as FilePath, PathBuf};
use std::sync::LazyLock;
use uuid::Uuid;
use walkdir::WalkDir;
use zip::ZipArchive;

const STATE_KEY: &str = "plugins_state";
const MANIFEST_FILENAME: &str = "deeptutor.plugin.json";
const SCHEMA_VERSION: &str = "deeptutor.plugin/v1";
const PAGE_SCHEMA_VERSION: &str = "deeptutor.plugin-frontend-page/v1";
const CATALOG_JSON: &str = include_str!("../../../deeptutor/plugins/catalog.json");
const MAX_MANIFEST_BYTES: u64 = 2 * 1024 * 1024;
const MAX_PAGE_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_PAGE_ASSET_BYTES: u64 = 5 * 1024 * 1024;
const MAX_PAGE_ASSETS: usize = 64;
const MAX_PACKAGE_FILES: usize = 20_000;
const PAGE_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self' data:; media-src 'self' data: blob:; connect-src 'none'; frame-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'self'; sandbox allow-scripts";

static PLUGIN_ID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z0-9][a-z0-9._-]{1,127}$").expect("插件 ID 正则有效"));
static EXTENSION_ID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z0-9][a-z0-9._-]{0,63}$").expect("插件扩展 ID 正则有效"));
static VERSION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[0-9]+(?:\.[0-9]+){1,3}(?:[-+][A-Za-z0-9.-]+)?$").expect("插件版本正则有效")
});
static ASSET_NAME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._~-]{0,127}$").expect("页面资源正则有效"));
const RESERVED_CAPABILITY_IDS: &[&str] = &[
    "chat",
    "ask_questions",
    "audio_overview",
    "deep_solve",
    "deep_question",
    "deep_research",
    "visualize",
    "math_animator",
    "mastery_path",
    "immersive_reading",
    "video_lesson",
];

#[derive(Debug, Deserialize)]
struct ApprovalRequest {
    snapshot_digest: String,
}

#[derive(Debug, Default, Deserialize)]
struct InstallRequest {
    #[serde(default = "default_version")]
    version: String,
    #[serde(default)]
    allow_deprecated: bool,
    #[serde(default)]
    source_path: String,
}

fn default_version() -> String {
    "latest".to_string()
}

#[derive(Clone, Debug)]
struct Manifest {
    raw: Value,
    id: String,
    name: String,
    version: String,
    description: String,
    extensions: Vec<PluginExtension>,
}

#[derive(Clone, Debug)]
struct PluginExtension {
    extension_type: String,
    id: String,
    entry_point: String,
    manifest: String,
    path: String,
    methods: Vec<String>,
    auth: String,
    description: String,
    parameters: Value,
    schema: String,
    operations: Vec<String>,
}

#[derive(Clone, Debug)]
struct Installation {
    version: String,
    package_path: PathBuf,
    artifact_sha256: String,
    installed_at: String,
    source: String,
}

#[derive(Clone, Debug)]
struct PluginRecord {
    id: String,
    name: String,
    version: String,
    status: String,
    description: String,
    manifest: Option<Manifest>,
    installation: Option<Installation>,
    approval_digest: String,
    error: String,
}

pub fn router() -> Router<AppState> {
    let dispatch_router = Router::new()
        .route(
            "/plugins/{plugin_id}/{*plugin_path}",
            get(dispatch_plugin_route)
                .post(dispatch_plugin_route)
                .put(dispatch_plugin_route)
                .patch(dispatch_plugin_route)
                .delete(dispatch_plugin_route),
        )
        .layer(DefaultBodyLimit::max(plugin_wasm::MAX_REQUEST_BYTES));
    Router::new()
        .route("/plugins/state", get(plugin_state))
        .route("/plugins/extensions", get(plugin_extensions))
        .route("/plugins/catalog", get(plugin_catalog))
        .route("/plugins/{plugin_id}/info", get(plugin_info))
        .route("/plugins/{plugin_id}/approve", post(approve_plugin))
        .route("/plugins/{plugin_id}/enable", post(enable_plugin))
        .route("/plugins/{plugin_id}/disable", post(disable_plugin))
        .route("/plugins/{plugin_id}/install", post(install_plugin))
        .route("/plugins/{plugin_id}/rollback", post(rollback_plugin))
        .route(
            "/plugins/{plugin_id}/installation",
            delete(uninstall_plugin),
        )
        .route(
            "/plugins/{plugin_id}/pages/{extension_id}",
            get(redirect_plugin_page),
        )
        .route(
            "/plugins/{plugin_id}/pages/{extension_id}/",
            get(get_plugin_page),
        )
        .route(
            "/plugins/{plugin_id}/pages/{extension_id}/assets/{*asset_path}",
            get(get_plugin_page_asset),
        )
        .merge(dispatch_router)
}

async fn plugin_state(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(json!({
        "plugins": plugin_records(&state)?
            .iter()
            .map(record_json)
            .collect::<Vec<_>>()
    })))
}

async fn plugin_extensions(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    let plugins = plugin_records(&state)?
        .into_iter()
        .filter(|record| record.status == "enabled")
        .filter_map(|record| {
            let manifest = record.manifest.as_ref()?;
            let extensions = manifest
                .extensions
                .iter()
                .map(|extension| {
                    let mut value = extension_json(extension);
                    if extension.extension_type == "frontend_page" && !extension.manifest.is_empty()
                    {
                        value["entry_url"] = Value::String(format!(
                            "/api/plugins/{}/pages/{}/",
                            record.id, extension.id
                        ));
                    }
                    value
                })
                .collect::<Vec<_>>();
            Some(json!({
                "id": record.id,
                "name": record.name,
                "version": record.version,
                "extensions": extensions
            }))
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({"plugins": plugins})))
}

async fn plugin_catalog() -> ApiResult<Json<Value>> {
    Ok(Json(json!({"entries": catalog_entries()})))
}

async fn plugin_info(
    ScopedState(state): ScopedState,
    Path(plugin_id): Path<String>,
) -> ApiResult<Json<Value>> {
    validate_plugin_id(&plugin_id)?;
    let record = plugin_records(&state)?
        .into_iter()
        .find(|record| record.id == plugin_id)
        .ok_or_else(|| ApiError::not_found("插件不存在"))?;
    Ok(Json(record_json(&record)))
}

async fn approve_plugin(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(plugin_id): Path<String>,
    Json(payload): Json<ApprovalRequest>,
) -> ApiResult<Json<Value>> {
    require_admin(&principal)?;
    validate_plugin_id(&plugin_id)?;
    if payload.snapshot_digest.len() != 64
        || !payload
            .snapshot_digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(ApiError::bad_request("插件快照摘要无效"));
    }
    let mut plugin_state = load_state(&state)?;
    let row = plugin_state
        .get_mut("plugins")
        .and_then(Value::as_object_mut)
        .and_then(|plugins| plugins.get_mut(&plugin_id))
        .ok_or_else(|| ApiError::not_found("插件尚未安装"))?;
    let manifest = parse_manifest_value(
        row.get("manifest")
            .cloned()
            .ok_or_else(|| ApiError::conflict("插件清单缺失"))?,
    )?;
    let installation = parse_installation(
        row.get("installation")
            .cloned()
            .ok_or_else(|| ApiError::conflict("插件安装信息缺失"))?,
    )?;
    let actual = approval_digest(&manifest, &installation.artifact_sha256);
    if actual != payload.snapshot_digest {
        return Err(ApiError::conflict(
            "插件快照已变化，请重新核对当前清单后再批准",
        ));
    }
    row["approval"] = json!({
        "digest": actual,
        "approved_at": Utc::now().to_rfc3339()
    });
    save_state(&state, &plugin_state)?;
    let record = find_record(&plugin_records(&state)?, &plugin_id)?;
    Ok(Json(record_json(&record)))
}

async fn enable_plugin(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(plugin_id): Path<String>,
) -> ApiResult<Json<Value>> {
    set_enabled(state, principal, plugin_id, true).await
}

async fn disable_plugin(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(plugin_id): Path<String>,
) -> ApiResult<Json<Value>> {
    set_enabled(state, principal, plugin_id, false).await
}

async fn set_enabled(
    state: AppState,
    principal: Principal,
    plugin_id: String,
    enabled: bool,
) -> ApiResult<Json<Value>> {
    require_admin(&principal)?;
    validate_plugin_id(&plugin_id)?;
    let records = plugin_records(&state)?;
    let record = find_record(&records, &plugin_id)?;
    if matches!(
        record.status.as_str(),
        "broken" | "incompatible" | "available"
    ) {
        return Err(ApiError::conflict(format!(
            "插件当前状态为 {}，不能启用或停用",
            record.status
        )));
    }
    let mut plugin_state = load_state(&state)?;
    let disabled = plugin_state
        .get_mut("disabled")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| ApiError::internal("插件状态已损坏"))?;
    let mut ids = disabled
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect::<std::collections::BTreeSet<_>>();
    if enabled {
        ids.remove(&plugin_id);
    } else {
        ids.insert(plugin_id.clone());
    }
    *disabled = ids.into_iter().map(Value::String).collect();
    save_state(&state, &plugin_state)?;
    let record = find_record(&plugin_records(&state)?, &plugin_id)?;
    Ok(Json(record_json(&record)))
}

async fn install_plugin(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(plugin_id): Path<String>,
    Json(payload): Json<InstallRequest>,
) -> ApiResult<Json<Value>> {
    require_admin(&principal)?;
    validate_plugin_id(&plugin_id)?;
    let source = if payload.source_path.trim().is_empty() {
        install_source_from_catalog(&state, &plugin_id, &payload).await?
    } else {
        let source = PathBuf::from(payload.source_path.trim());
        confined_path(&state.paths.root, &source)?
    };
    let staging_root = state
        .paths
        .data
        .join("plugins")
        .join(".staging")
        .join(Uuid::new_v4().to_string());
    std::fs::create_dir_all(&staging_root)
        .map_err(|error| ApiError::internal(format!("无法创建插件暂存目录：{error}")))?;
    let prepared = prepare_package(&source, &staging_root)?;
    let manifest = load_manifest(&prepared.join(MANIFEST_FILENAME))?;
    if manifest.id != plugin_id {
        let _ = std::fs::remove_dir_all(&staging_root);
        return Err(ApiError::conflict(format!(
            "安装包声明的插件 ID 为 {}，与请求的 {plugin_id} 不一致",
            manifest.id
        )));
    }
    if payload.version != "latest" && payload.version != manifest.version {
        let _ = std::fs::remove_dir_all(&staging_root);
        return Err(ApiError::conflict("请求版本与安装包清单版本不一致"));
    }
    if !payload.allow_deprecated {
        let entry = catalog_entries().into_iter().find(|entry| {
            entry.get("id").and_then(Value::as_str) == Some(&plugin_id)
                && entry.get("version").and_then(Value::as_str) == Some(manifest.version.as_str())
        });
        if entry.and_then(|entry| entry.get("status").cloned())
            == Some(Value::String("deprecated".to_string()))
        {
            let _ = std::fs::remove_dir_all(&staging_root);
            return Err(ApiError::conflict("该插件版本已弃用，请显式允许弃用版本"));
        }
    }
    let version_dir = state
        .paths
        .data
        .join("plugins")
        .join(&plugin_id)
        .join("versions")
        .join(&manifest.version);
    if version_dir.exists() {
        let _ = std::fs::remove_dir_all(&staging_root);
        return Err(ApiError::conflict("该插件版本已经安装"));
    }
    if let Some(parent) = version_dir.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| ApiError::internal(format!("无法创建插件版本目录：{error}")))?;
    }
    std::fs::rename(&prepared, &version_dir)
        .map_err(|error| ApiError::internal(format!("无法发布插件安装目录：{error}")))?;
    let _ = std::fs::remove_dir_all(&staging_root);
    let artifact_sha256 = package_digest(&version_dir)?;
    let mut plugin_state = load_state(&state)?;
    let plugins = plugin_state
        .get_mut("plugins")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| ApiError::internal("插件状态已损坏"))?;
    let previous = plugins.get(&plugin_id).cloned();
    let mut history = previous
        .as_ref()
        .and_then(|row| row.get("history"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if let Some(previous) = previous.as_ref().filter(|row| {
        row.get("installation")
            .and_then(|value| value.get("package_path"))
            .and_then(Value::as_str)
            .is_some()
    }) {
        history.push(json!({
            "manifest": previous.get("manifest").cloned().unwrap_or(Value::Null),
            "installation": previous.get("installation").cloned().unwrap_or(Value::Null)
        }));
    }
    history.truncate(5);
    plugins.insert(
        plugin_id.clone(),
        json!({
            "manifest": manifest.raw,
            "installation": {
                "version": manifest.version,
                "package_path": version_dir.display().to_string(),
                "artifact_sha256": artifact_sha256,
                "installed_at": Utc::now().to_rfc3339(),
                "source": source.display().to_string()
            },
            "approval": Value::Null,
            "history": history
        }),
    );
    save_state(&state, &plugin_state)?;
    Ok(Json(json!({
        "id": plugin_id,
        "version": manifest.version,
        "action": if previous.is_some() { "updated" } else { "installed" },
        "approval_required": true
    })))
}

async fn rollback_plugin(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(plugin_id): Path<String>,
) -> ApiResult<Json<Value>> {
    require_admin(&principal)?;
    validate_plugin_id(&plugin_id)?;
    let mut plugin_state = load_state(&state)?;
    let row = plugin_state
        .get_mut("plugins")
        .and_then(Value::as_object_mut)
        .and_then(|plugins| plugins.get_mut(&plugin_id))
        .ok_or_else(|| ApiError::not_found("插件尚未安装"))?;
    let history = row
        .get_mut("history")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| ApiError::internal("插件回滚历史已损坏"))?;
    let previous = history
        .pop()
        .ok_or_else(|| ApiError::conflict("没有可回滚的插件版本"))?;
    let manifest = parse_manifest_value(
        previous
            .get("manifest")
            .cloned()
            .ok_or_else(|| ApiError::internal("历史插件清单缺失"))?,
    )?;
    let installation = parse_installation(
        previous
            .get("installation")
            .cloned()
            .ok_or_else(|| ApiError::internal("历史插件安装信息缺失"))?,
    )?;
    row["manifest"] = previous["manifest"].clone();
    row["installation"] = previous["installation"].clone();
    row["approval"] = Value::Null;
    save_state(&state, &plugin_state)?;
    Ok(Json(json!({
        "id": plugin_id,
        "version": manifest.version,
        "package_path": installation.package_path.display().to_string(),
        "approval_required": true
    })))
}

async fn uninstall_plugin(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(plugin_id): Path<String>,
) -> ApiResult<Json<Value>> {
    require_admin(&principal)?;
    validate_plugin_id(&plugin_id)?;
    let mut plugin_state = load_state(&state)?;
    let plugins = plugin_state
        .get_mut("plugins")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| ApiError::internal("插件状态已损坏"))?;
    let row = plugins
        .remove(&plugin_id)
        .ok_or_else(|| ApiError::not_found("插件尚未安装"))?;
    if let Some(path) = row
        .get("installation")
        .and_then(|value| value.get("package_path"))
        .and_then(Value::as_str)
    {
        let package = confined_path(&state.paths.root, FilePath::new(path))?;
        let plugin_root = state.paths.data.join("plugins").join(&plugin_id);
        if package.starts_with(&plugin_root) && plugin_root.exists() {
            std::fs::remove_dir_all(&plugin_root)
                .map_err(|error| ApiError::internal(format!("无法删除插件文件：{error}")))?;
        }
    }
    if let Some(disabled) = plugin_state
        .get_mut("disabled")
        .and_then(Value::as_array_mut)
    {
        disabled.retain(|value| value.as_str() != Some(plugin_id.as_str()));
    }
    save_state(&state, &plugin_state)?;
    Ok(Json(json!({"id": plugin_id, "action": "uninstalled"})))
}

async fn redirect_plugin_page(
    ScopedState(state): ScopedState,
    Extension(principal): Extension<Principal>,
    Path((plugin_id, extension_id)): Path<(String, String)>,
) -> ApiResult<Redirect> {
    let _ = resolve_frontend_page(&state, &principal, &plugin_id, &extension_id)?;
    Ok(Redirect::temporary(&format!(
        "/api/plugins/{plugin_id}/pages/{extension_id}/"
    )))
}

async fn get_plugin_page(
    ScopedState(state): ScopedState,
    Extension(principal): Extension<Principal>,
    Path((plugin_id, extension_id)): Path<(String, String)>,
) -> ApiResult<Response> {
    let (manifest, installation, extension) =
        resolve_frontend_page(&state, &principal, &plugin_id, &extension_id)?;
    let page = load_page_manifest(&installation.package_path.join(&extension.manifest))?;
    let asset = package_asset(&installation.package_path, &page.entry)?;
    file_response(asset, false, &plugin_id, &manifest.version, &extension.id).await
}

async fn get_plugin_page_asset(
    ScopedState(state): ScopedState,
    Extension(principal): Extension<Principal>,
    Path((plugin_id, extension_id, asset_path)): Path<(String, String, String)>,
) -> ApiResult<Response> {
    let (manifest, installation, extension) =
        resolve_frontend_page(&state, &principal, &plugin_id, &extension_id)?;
    let page = load_page_manifest(&installation.package_path.join(&extension.manifest))?;
    if !page.assets.iter().any(|asset| asset == &asset_path) {
        return Err(ApiError::not_found("插件页面资源未声明"));
    }
    validate_asset_path(&asset_path)?;
    let asset = package_asset(&installation.package_path, &asset_path)?;
    file_response(asset, true, &plugin_id, &manifest.version, &extension.id).await
}

async fn dispatch_plugin_route(
    ScopedState(state): ScopedState,
    Extension(principal): Extension<Principal>,
    method: Method,
    Path((plugin_id, plugin_path)): Path<(String, String)>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> ApiResult<Response> {
    validate_plugin_id(&plugin_id)?;
    let normalized = format!("/{}", plugin_path.trim_matches('/'));
    let records = plugin_records(&state)?;
    let record = records
        .into_iter()
        .find(|record| record.id == plugin_id && record.status == "enabled")
        .ok_or_else(|| ApiError::not_found("插件未安装或未启用"))?;
    let manifest = record
        .manifest
        .as_ref()
        .ok_or_else(|| ApiError::not_found("插件清单不存在"))?;
    let extension = manifest
        .extensions
        .iter()
        .find(|extension| extension.extension_type == "http_route" && extension.path == normalized)
        .ok_or_else(|| ApiError::not_found("插件路由不存在"))?;
    if !extension
        .methods
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(method.as_str()))
    {
        return Err(ApiError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "插件路由不支持该 HTTP 方法",
            false,
        ));
    }
    if extension.auth == "admin" {
        if !principal.is_admin() {
            return Err(ApiError::forbidden("该插件路由仅限管理员"));
        }
    }
    let installation = record
        .installation
        .as_ref()
        .ok_or_else(|| ApiError::not_found("插件安装信息不存在"))?;
    if let Some(relative) = extension.entry_point.strip_prefix("static:") {
        validate_asset_path(relative)?;
        let path = package_asset(&installation.package_path, relative)?;
        return file_response(path, true, &record.id, &record.version, &extension.id).await;
    }
    if let Some(relative) = extension.entry_point.strip_prefix("json:") {
        validate_asset_path(relative)?;
        let path = package_asset(&installation.package_path, relative)?;
        let data = tokio::fs::read(&path)
            .await
            .map_err(|error| ApiError::internal(format!("无法读取插件 JSON：{error}")))?;
        let value: Value = serde_json::from_slice(&data)
            .map_err(|error| ApiError::internal(format!("插件 JSON 无效：{error}")))?;
        return Ok(Json(value).into_response());
    }
    if let Some(relative) = extension.entry_point.strip_prefix("wasm:") {
        validate_wasm_path(relative)?;
        let path = package_asset(&installation.package_path, relative)?;
        let metadata = tokio::fs::metadata(&path)
            .await
            .map_err(|error| ApiError::not_found(format!("插件 WASM 不存在：{error}")))?;
        if metadata.len() > plugin_wasm::MAX_MODULE_BYTES as u64 {
            return Err(ApiError::bad_request("插件 WASM 模块不能超过 16 MiB"));
        }
        let module = tokio::fs::read(&path)
            .await
            .map_err(|error| ApiError::internal(format!("无法读取插件 WASM：{error}")))?;
        let request = json!({
            "version": 1,
            "plugin_id": record.id.as_str(),
            "extension_id": extension.id.as_str(),
            "method": method.as_str(),
            "path": extension.path.as_str(),
            "query": query.unwrap_or_default(),
            "headers": wasm_request_headers(&headers),
            "body_base64": base64::engine::general_purpose::STANDARD.encode(&body)
        });
        let request_bytes = serde_json::to_vec(&request)
            .map_err(|error| ApiError::internal(format!("无法编码插件请求：{error}")))?;
        let response = plugin_wasm::execute(module, request_bytes)
            .await
            .map_err(|error| {
                ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    "plugin_runtime_error",
                    error,
                    false,
                )
            })?;
        return wasm_response(&response, &record.id, &record.version, &extension.id);
    }
    Err(ApiError::new(
        StatusCode::NOT_IMPLEMENTED,
        "plugin_entrypoint_unsupported",
        "插件入口不受支持；便携版仅允许 static:、json: 与 wasm: 路由。",
        false,
    ))
}

fn wasm_request_headers(headers: &HeaderMap) -> Value {
    let mut selected = Map::new();
    for name in ["accept", "accept-language", "content-type"] {
        let values = headers
            .get_all(name)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .map(str::to_string)
            .collect::<Vec<_>>();
        if !values.is_empty() {
            selected.insert(name.to_string(), json!(values));
        }
    }
    Value::Object(selected)
}

fn wasm_response(
    bytes: &[u8],
    plugin_id: &str,
    version: &str,
    extension_id: &str,
) -> ApiResult<Response> {
    if bytes.len() > plugin_wasm::MAX_RESPONSE_BYTES {
        return Err(ApiError::bad_gateway_plugin("插件响应超过 4 MiB 限制"));
    }
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|error| ApiError::bad_gateway_plugin(format!("插件响应 JSON 无效：{error}")))?;
    let object = value
        .as_object()
        .ok_or_else(|| ApiError::bad_gateway_plugin("插件响应必须是 JSON 对象"))?;
    let status = object.get("status").and_then(Value::as_u64).unwrap_or(200);
    if !(200..=599).contains(&status) {
        return Err(ApiError::bad_gateway_plugin(
            "插件响应 status 必须位于 200 到 599",
        ));
    }
    let status = StatusCode::from_u16(status as u16)
        .map_err(|error| ApiError::bad_gateway_plugin(format!("插件响应状态码无效：{error}")))?;

    let body = if let Some(encoded) = object.get("body_base64") {
        let encoded = encoded
            .as_str()
            .ok_or_else(|| ApiError::bad_gateway_plugin("body_base64 必须是字符串"))?;
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|error| {
                ApiError::bad_gateway_plugin(format!("插件响应 base64 无效：{error}"))
            })?
    } else if let Some(text) = object.get("body") {
        text.as_str()
            .ok_or_else(|| ApiError::bad_gateway_plugin("body 必须是字符串"))?
            .as_bytes()
            .to_vec()
    } else {
        Vec::new()
    };
    if body.len() > plugin_wasm::MAX_RESPONSE_BYTES {
        return Err(ApiError::bad_gateway_plugin("插件响应正文超过 4 MiB 限制"));
    }

    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    let mut has_content_type = false;
    if let Some(headers) = object.get("headers") {
        let headers = headers
            .as_object()
            .ok_or_else(|| ApiError::bad_gateway_plugin("插件响应 headers 必须是对象"))?;
        for (name, value) in headers {
            let normalized = name.to_ascii_lowercase();
            if !matches!(
                normalized.as_str(),
                "cache-control" | "content-language" | "content-type" | "etag" | "last-modified"
            ) {
                return Err(ApiError::bad_gateway_plugin(format!(
                    "插件响应头 {name} 不在允许列表中"
                )));
            }
            let values = match value {
                Value::String(value) => vec![value.as_str()],
                Value::Array(values) => values
                    .iter()
                    .map(|value| {
                        value
                            .as_str()
                            .ok_or_else(|| ApiError::bad_gateway_plugin("响应头值必须是字符串"))
                    })
                    .collect::<ApiResult<Vec<_>>>()?,
                _ => {
                    return Err(ApiError::bad_gateway_plugin(
                        "响应头值必须是字符串或字符串数组",
                    ))
                }
            };
            let header_name =
                header::HeaderName::from_bytes(normalized.as_bytes()).map_err(|error| {
                    ApiError::bad_gateway_plugin(format!("响应头名称无效：{error}"))
                })?;
            for value in values {
                let header_value = HeaderValue::from_str(value).map_err(|error| {
                    ApiError::bad_gateway_plugin(format!("响应头内容无效：{error}"))
                })?;
                response.headers_mut().append(&header_name, header_value);
            }
            if normalized == "content-type" {
                has_content_type = true;
            }
        }
    }
    if !has_content_type {
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        );
    }
    if !response.headers().contains_key(header::CACHE_CONTROL) {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response.headers_mut().insert(
        header::HeaderName::from_static("x-deeptutor-plugin"),
        HeaderValue::from_str(&format!("{plugin_id}/{version}/{extension_id}"))
            .map_err(|error| ApiError::internal(error.to_string()))?,
    );
    Ok(response)
}

fn resolve_frontend_page(
    state: &AppState,
    principal: &Principal,
    plugin_id: &str,
    extension_id: &str,
) -> ApiResult<(Manifest, Installation, PluginExtension)> {
    validate_plugin_id(plugin_id)?;
    if EXTENSION_ID_RE.find(extension_id).is_none() {
        return Err(ApiError::not_found("插件页面不存在"));
    }
    let record = plugin_records(state)?
        .into_iter()
        .find(|record| record.id == plugin_id && record.status == "enabled")
        .ok_or_else(|| ApiError::not_found("插件未安装或未启用"))?;
    let manifest = record
        .manifest
        .ok_or_else(|| ApiError::not_found("插件清单不存在"))?;
    let extension = manifest
        .extensions
        .iter()
        .find(|extension| {
            extension.extension_type == "frontend_page"
                && extension.id == extension_id
                && !extension.manifest.is_empty()
        })
        .cloned()
        .ok_or_else(|| ApiError::not_found("插件页面不存在"))?;
    if extension.auth == "admin" && !principal.is_admin() {
        return Err(ApiError::forbidden("管理员页面需要经过管理员会话访问"));
    }
    let installation = record
        .installation
        .ok_or_else(|| ApiError::not_found("插件安装信息不存在"))?;
    Ok((manifest, installation, extension))
}

fn plugin_records(state: &AppState) -> ApiResult<Vec<PluginRecord>> {
    let plugin_state = load_state(state)?;
    let disabled = plugin_state
        .get("disabled")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    let mut records = BTreeMap::new();
    if let Some(rows) = plugin_state.get("plugins").and_then(Value::as_object) {
        for (plugin_id, row) in rows {
            let manifest = row
                .get("manifest")
                .cloned()
                .and_then(|value| parse_manifest_value(value).ok());
            let installation = row
                .get("installation")
                .cloned()
                .and_then(|value| parse_installation(value).ok());
            let Some(manifest) = manifest else {
                records.insert(
                    plugin_id.clone(),
                    PluginRecord {
                        id: plugin_id.clone(),
                        name: plugin_id.clone(),
                        version: String::new(),
                        status: "broken".to_string(),
                        description: String::new(),
                        manifest: None,
                        installation: None,
                        approval_digest: String::new(),
                        error: "插件清单缺失或无效".to_string(),
                    },
                );
                continue;
            };
            let Some(installation) = installation else {
                records.insert(
                    plugin_id.clone(),
                    PluginRecord {
                        id: plugin_id.clone(),
                        name: manifest.name.clone(),
                        version: manifest.version.clone(),
                        status: "broken".to_string(),
                        description: manifest.description.clone(),
                        manifest: Some(manifest),
                        installation: None,
                        approval_digest: String::new(),
                        error: "插件安装信息缺失".to_string(),
                    },
                );
                continue;
            };
            let digest = approval_digest(&manifest, &installation.artifact_sha256);
            let approved = row
                .get("approval")
                .and_then(|approval| approval.get("digest"))
                .and_then(Value::as_str)
                == Some(digest.as_str());
            let mut status = if approved {
                "enabled"
            } else {
                "approval-required"
            };
            if disabled.contains(plugin_id.as_str()) && approved {
                status = "disabled";
            }
            let error = if !installation.package_path.is_dir() {
                status = "broken";
                "插件安装目录不存在".to_string()
            } else {
                match package_digest(&installation.package_path) {
                    Ok(actual) if actual == installation.artifact_sha256 => String::new(),
                    Ok(_) => {
                        status = "broken";
                        "插件安装包校验失败，文件可能已被修改".to_string()
                    }
                    Err(error) => {
                        status = "broken";
                        format!("无法校验插件安装包：{error}")
                    }
                }
            };
            records.insert(
                plugin_id.clone(),
                PluginRecord {
                    id: plugin_id.clone(),
                    name: manifest.name.clone(),
                    version: manifest.version.clone(),
                    status: status.to_string(),
                    description: manifest.description.clone(),
                    manifest: Some(manifest),
                    installation: Some(installation),
                    approval_digest: digest,
                    error,
                },
            );
        }
    }
    for entry in catalog_entries() {
        let Some(id) = entry.get("id").and_then(Value::as_str) else {
            continue;
        };
        records
            .entry(id.to_string())
            .or_insert_with(|| PluginRecord {
                id: id.to_string(),
                name: entry
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(id)
                    .to_string(),
                version: entry
                    .get("version")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                status: entry
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("available")
                    .to_string(),
                description: entry
                    .get("description_i18n")
                    .and_then(|value| value.get("en"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                manifest: None,
                installation: None,
                approval_digest: String::new(),
                error: String::new(),
            });
    }
    Ok(records.into_values().collect())
}

fn find_record(records: &[PluginRecord], plugin_id: &str) -> ApiResult<PluginRecord> {
    records
        .iter()
        .find(|record| record.id == plugin_id)
        .cloned()
        .ok_or_else(|| ApiError::not_found("插件不存在"))
}

fn record_json(record: &PluginRecord) -> Value {
    let mut value = json!({
        "id": record.id,
        "name": record.name,
        "version": record.version,
        "status": record.status,
        "description": record.description,
        "distribution": "managed"
    });
    if let Some(manifest) = &record.manifest {
        value["manifest"] = manifest.raw.clone();
        value["approval_digest"] = Value::String(record.approval_digest.clone());
    }
    if let Some(installation) = &record.installation {
        value["installation"] = json!({
            "version": installation.version,
            "package_path": installation.package_path.display().to_string(),
            "artifact_path": installation.package_path.display().to_string(),
            "artifact_sha256": installation.artifact_sha256,
            "installed_at": installation.installed_at,
            "dependencies": [],
            "source": installation.source
        });
    }
    if !record.error.is_empty() {
        value["error"] = Value::String(record.error.clone());
    }
    value
}

fn extension_json(extension: &PluginExtension) -> Value {
    let mut value = json!({
        "type": extension.extension_type,
        "id": extension.id
    });
    if !extension.entry_point.is_empty() {
        value["entry_point"] = Value::String(extension.entry_point.clone());
    }
    if !extension.manifest.is_empty() {
        value["manifest"] = Value::String(extension.manifest.clone());
    }
    if !extension.path.is_empty() {
        value["path"] = Value::String(extension.path.clone());
    }
    if !extension.methods.is_empty() {
        value["methods"] = json!(extension.methods);
    }
    if !extension.auth.is_empty() {
        value["auth"] = Value::String(extension.auth.clone());
    }
    if !extension.description.is_empty() {
        value["description"] = Value::String(extension.description.clone());
    }
    if matches!(
        extension.extension_type.as_str(),
        "tool" | "capability" | "reading_extension"
    ) {
        value["parameters"] = extension.parameters.clone();
    }
    if !extension.schema.is_empty() {
        value["schema"] = Value::String(extension.schema.clone());
    }
    if !extension.operations.is_empty() {
        value["operations"] = json!(extension.operations);
    }
    value
}

#[derive(Clone, Debug)]
pub(super) struct PluginToolResult {
    pub(super) content: String,
    pub(super) metadata: Value,
    pub(super) success: bool,
    pub(super) terminate_turn: bool,
}

#[derive(Clone, Debug)]
pub(super) struct PluginCapabilityResult {
    pub(super) response: String,
    pub(super) events: Vec<Value>,
    pub(super) result: Value,
    pub(super) attachments: Vec<Value>,
}

pub(super) fn plugin_tool_definitions(state: &AppState) -> Result<Vec<Value>, String> {
    let mut definitions = Vec::new();
    for record in plugin_records(state)
        .map_err(|error| error.to_string())?
        .into_iter()
        .filter(|record| record.status == "enabled")
    {
        let Some(manifest) = record.manifest.as_ref() else {
            continue;
        };
        for extension in manifest
            .extensions
            .iter()
            .filter(|extension| extension.extension_type == "tool")
        {
            definitions.push(plugin_tool_definition(&record.id, extension));
        }
    }
    Ok(definitions)
}

fn plugin_tool_definition(plugin_id: &str, extension: &PluginExtension) -> Value {
    let name = plugin_tool_name(plugin_id, &extension.id);
    let description = if extension.description.is_empty() {
        format!("DeepTutor 插件工具：{plugin_id}/{}", extension.id)
    } else {
        format!(
            "DeepTutor 插件工具：{plugin_id}/{}。{}",
            extension.id, extension.description
        )
    };
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": extension.parameters
        }
    })
}

fn plugin_tool_name(plugin_id: &str, extension_id: &str) -> String {
    let digest = Sha256::digest(format!("{plugin_id}\0{extension_id}"));
    let short_extension = extension_id.chars().take(40).collect::<String>();
    format!("plugin_{}_{}", hex::encode(&digest[..4]), short_extension)
}

pub(super) async fn execute_plugin_tool(
    state: &AppState,
    name: &str,
    arguments: &Value,
    turn_id: &str,
    session_id: &str,
    language: &str,
) -> Result<Option<PluginToolResult>, String> {
    if !name.starts_with("plugin_") {
        return Ok(None);
    }
    if !arguments.is_object() {
        return Err("插件工具 arguments 必须是 JSON 对象".to_string());
    }
    let records = plugin_records(state).map_err(|error| error.to_string())?;
    let mut selected = None;
    for record in records
        .into_iter()
        .filter(|record| record.status == "enabled")
    {
        let Some(manifest) = record.manifest.as_ref() else {
            continue;
        };
        for extension in manifest
            .extensions
            .iter()
            .filter(|extension| extension.extension_type == "tool")
        {
            if plugin_tool_name(&record.id, &extension.id) == name {
                let installation = record
                    .installation
                    .as_ref()
                    .ok_or_else(|| "插件安装信息不存在".to_string())?;
                let relative = extension
                    .entry_point
                    .strip_prefix("wasm:")
                    .ok_or_else(|| "插件 Tool 必须使用 wasm: entry_point".to_string())?;
                validate_wasm_path(relative).map_err(|error| error.to_string())?;
                let module_path = package_asset(&installation.package_path, relative)
                    .map_err(|error| error.to_string())?;
                selected = Some((
                    module_path,
                    record.id.clone(),
                    record.version.clone(),
                    extension.id.clone(),
                ));
                break;
            }
        }
        if selected.is_some() {
            break;
        }
    }
    let Some((module_path, plugin_id, version, extension_id)) = selected else {
        return Err(format!("插件工具 `{name}` 未安装、未批准或未启用"));
    };
    let metadata = tokio::fs::metadata(&module_path)
        .await
        .map_err(|error| format!("插件 Tool WASM 不存在：{error}"))?;
    if metadata.len() > plugin_wasm::MAX_MODULE_BYTES as u64 {
        return Err("插件 Tool WASM 不能超过 16 MiB".to_string());
    }
    let module = tokio::fs::read(&module_path)
        .await
        .map_err(|error| format!("无法读取插件 Tool WASM：{error}"))?;
    let request = json!({
        "version": 1,
        "kind": "tool",
        "plugin_id": plugin_id,
        "plugin_version": version,
        "extension_id": extension_id,
        "tool_name": name,
        "arguments": arguments,
        "context": {
            "turn_id": turn_id,
            "session_id": session_id,
            "language": language
        }
    });
    let request_bytes =
        serde_json::to_vec(&request).map_err(|error| format!("无法编码插件 Tool 请求：{error}"))?;
    let response_bytes = plugin_wasm::execute(module, request_bytes).await?;
    let response: Value = serde_json::from_slice(&response_bytes)
        .map_err(|error| format!("插件 Tool 响应 JSON 无效：{error}"))?;
    let object = response
        .as_object()
        .ok_or_else(|| "插件 Tool 响应必须是 JSON 对象".to_string())?;
    let content = object
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| "插件 Tool 响应缺少字符串 content".to_string())?
        .to_string();
    let metadata = object.get("metadata").cloned().unwrap_or_else(|| json!({}));
    if !metadata.is_object() {
        return Err("插件 Tool 响应 metadata 必须是 JSON 对象".to_string());
    }
    let success = object
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let terminate_turn = object
        .get("terminate_turn")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(Some(PluginToolResult {
        content,
        metadata,
        success,
        terminate_turn,
    }))
}

pub(super) fn plugin_capability_catalog(state: &AppState) -> Result<Vec<Value>, String> {
    let mut capabilities = Vec::new();
    for record in plugin_records(state)
        .map_err(|error| error.to_string())?
        .into_iter()
        .filter(|record| record.status == "enabled")
    {
        let Some(manifest) = record.manifest.as_ref() else {
            continue;
        };
        for extension in manifest.extensions.iter().filter(|extension| {
            matches!(
                extension.extension_type.as_str(),
                "capability" | "loop_capability"
            )
        }) {
            let mut value = extension_json(extension);
            value["id"] = Value::String(extension.id.clone());
            value["kind"] = Value::String(
                if extension.extension_type == "loop_capability" {
                    "loop"
                } else {
                    "turn"
                }
                .to_string(),
            );
            value["available"] = Value::Bool(true);
            value["plugin_id"] = Value::String(record.id.clone());
            value["plugin_name"] = Value::String(record.name.clone());
            value["plugin_version"] = Value::String(record.version.clone());
            value["manifest"] = json!({
                "name": extension.id,
                "display_name": if extension.description.is_empty() {
                    record.name.clone()
                } else {
                    extension.description.clone()
                },
                "description": extension.description,
                "config_schema": extension.parameters,
                "runtime": "wasm"
            });
            capabilities.push(value);
        }
    }
    Ok(capabilities)
}

pub(super) fn plugin_capability_names(state: &AppState) -> Result<Vec<String>, String> {
    Ok(plugin_capability_catalog(state)?
        .into_iter()
        .filter_map(|value| value.get("id").and_then(Value::as_str).map(str::to_string))
        .collect())
}

pub(super) async fn execute_plugin_capability(
    state: &AppState,
    capability: &str,
    request: &Value,
    session_id: &str,
    turn_id: &str,
    language: &str,
) -> Result<Option<PluginCapabilityResult>, String> {
    let records = plugin_records(state).map_err(|error| error.to_string())?;
    let mut selected = None;
    for record in records
        .into_iter()
        .filter(|record| record.status == "enabled")
    {
        let Some(manifest) = record.manifest.as_ref() else {
            continue;
        };
        for extension in manifest.extensions.iter().filter(|extension| {
            matches!(
                extension.extension_type.as_str(),
                "capability" | "loop_capability"
            ) && extension.id == capability
        }) {
            let installation = record
                .installation
                .as_ref()
                .ok_or_else(|| "插件安装信息不存在".to_string())?;
            let relative = extension
                .entry_point
                .strip_prefix("wasm:")
                .ok_or_else(|| "capability 扩展必须使用 wasm: entry_point".to_string())?;
            validate_wasm_path(relative).map_err(|error| error.to_string())?;
            selected = Some((
                package_asset(&installation.package_path, relative)
                    .map_err(|error| error.to_string())?,
                record.id.clone(),
                record.version.clone(),
                extension.id.clone(),
                extension.extension_type.clone(),
            ));
            break;
        }
        if selected.is_some() {
            break;
        }
    }
    let Some((module_path, plugin_id, plugin_version, extension_id, extension_type)) = selected
    else {
        return Ok(None);
    };
    let metadata = tokio::fs::metadata(&module_path)
        .await
        .map_err(|error| format!("capability WASM 不存在：{error}"))?;
    if metadata.len() > plugin_wasm::MAX_MODULE_BYTES as u64 {
        return Err("capability WASM 不能超过 16 MiB".to_string());
    }
    let module = tokio::fs::read(&module_path)
        .await
        .map_err(|error| format!("无法读取 capability WASM：{error}"))?;
    let context = json!({
        "session_id": session_id,
        "turn_id": turn_id,
        "language": language,
        "user_message": request.get("content").cloned().unwrap_or_else(|| json!("")),
        "config": request.get("config").cloned().unwrap_or_else(|| json!({})),
        "knowledge_bases": request.get("knowledge_bases").cloned().unwrap_or_else(|| json!([])),
        "attachments": request.get("attachments").cloned().unwrap_or_else(|| json!([]))
    });
    let request = json!({
        "version": 1,
        "kind": extension_type,
        "plugin_id": plugin_id,
        "plugin_version": plugin_version,
        "extension_id": extension_id,
        "capability": capability,
        "context": context
    });
    let request_bytes = serde_json::to_vec(&request)
        .map_err(|error| format!("无法编码 capability 请求：{error}"))?;
    let response_bytes = plugin_wasm::execute(module, request_bytes).await?;
    let response: Value = serde_json::from_slice(&response_bytes)
        .map_err(|error| format!("capability 响应 JSON 无效：{error}"))?;
    let object = response
        .as_object()
        .ok_or_else(|| "capability 响应必须是 JSON 对象".to_string())?;
    let response_text = object
        .get("response")
        .and_then(Value::as_str)
        .ok_or_else(|| "capability 响应缺少字符串 response".to_string())?
        .to_string();
    let mut events = Vec::new();
    if let Some(items) = object.get("events").and_then(Value::as_array) {
        if items.len() > 256 {
            return Err("capability 响应事件不能超过 256 条".to_string());
        }
        for item in items {
            let event = item
                .as_object()
                .ok_or_else(|| "capability 事件必须是 JSON 对象".to_string())?;
            let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
            if !matches!(
                event_type,
                "progress"
                    | "content"
                    | "thinking"
                    | "stage_start"
                    | "stage_end"
                    | "artifact"
                    | "workspace_item"
                    | "source"
            ) {
                return Err(format!("capability 事件类型不受支持：{event_type}"));
            }
            let content = event
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let event_metadata = event.get("metadata").cloned().unwrap_or_else(|| json!({}));
            if !event_metadata.is_object() {
                return Err("capability 事件 metadata 必须是 JSON 对象".to_string());
            }
            events.push(json!({
                "type": event_type,
                "content": content,
                "metadata": event_metadata
            }));
        }
    }
    let mut result = object.get("result").cloned().unwrap_or_else(|| json!({}));
    if !result.is_object() {
        return Err("capability 响应 result 必须是 JSON 对象".to_string());
    }
    if result.get("response").is_none() {
        result["response"] = Value::String(response_text.clone());
    }
    let attachments = object
        .get("attachments")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if attachments.len() > 128 {
        return Err("capability 响应附件不能超过 128 项".to_string());
    }
    if attachments.iter().any(|item| !item.is_object()) {
        return Err("capability 响应附件必须是 JSON 对象".to_string());
    }
    Ok(Some(PluginCapabilityResult {
        response: response_text,
        events,
        result,
        attachments,
    }))
}

pub(super) fn plugin_reading_extensions(state: &AppState) -> Result<Vec<Value>, String> {
    let mut extensions = Vec::new();
    for record in plugin_records(state)
        .map_err(|error| error.to_string())?
        .into_iter()
        .filter(|record| record.status == "enabled")
    {
        let Some(manifest) = record.manifest.as_ref() else {
            continue;
        };
        for extension in manifest
            .extensions
            .iter()
            .filter(|extension| extension.extension_type == "reading_extension")
        {
            let mut value = extension.parameters.clone();
            if let Some(object) = value.as_object_mut() {
                object
                    .entry("name")
                    .or_insert_with(|| Value::String(record.name.clone()));
                object
                    .entry("version")
                    .or_insert_with(|| Value::String(record.version.clone()));
                object.insert("kind".into(), Value::String("plugin".into()));
                object.insert("plugin_id".into(), Value::String(record.id.clone()));
            }
            extensions.push(value);
        }
    }
    Ok(extensions)
}

pub(super) fn plugin_visualizers(state: &AppState) -> Result<Vec<(Value, PathBuf)>, String> {
    let mut visualizers = Vec::new();
    for record in plugin_records(state)
        .map_err(|error| error.to_string())?
        .into_iter()
        .filter(|record| record.status == "enabled")
    {
        let Some(manifest) = record.manifest.as_ref() else {
            continue;
        };
        let Some(installation) = record.installation.as_ref() else {
            continue;
        };
        for extension in manifest
            .extensions
            .iter()
            .filter(|extension| extension.extension_type == "visualizer")
        {
            let manifest_path = package_asset(&installation.package_path, &extension.manifest)
                .map_err(|error| error.to_string())?;
            let metadata = std::fs::metadata(&manifest_path)
                .map_err(|error| format!("无法读取插件可视化器清单：{error}"))?;
            if metadata.len() > 128 * 1024 {
                return Err("插件 visualizer.json 不能超过 128 KiB".to_string());
            }
            let raw = std::fs::read_to_string(&manifest_path)
                .map_err(|error| format!("无法读取插件 visualizer.json：{error}"))?;
            let visualizer_manifest = serde_json::from_str(&raw)
                .map_err(|error| format!("插件 visualizer.json 无效：{error}"))?;
            visualizers.push((visualizer_manifest, installation.package_path.clone()));
        }
    }
    Ok(visualizers)
}

pub(super) async fn execute_plugin_reading_action(
    state: &AppState,
    extension_id: &str,
    action: &str,
    context: &Value,
) -> Result<Option<Value>, String> {
    let records = plugin_records(state).map_err(|error| error.to_string())?;
    let mut selected = None;
    for record in records
        .into_iter()
        .filter(|record| record.status == "enabled")
    {
        let Some(manifest) = record.manifest.as_ref() else {
            continue;
        };
        for extension in manifest.extensions.iter().filter(|extension| {
            extension.extension_type == "reading_extension" && extension.id == extension_id
        }) {
            let action_manifest = extension
                .parameters
                .get("actions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .find(|row| row.get("id").and_then(Value::as_str) == Some(action))
                .cloned()
                .ok_or_else(|| "阅读插件动作不存在".to_string())?;
            let installation = record
                .installation
                .as_ref()
                .ok_or_else(|| "插件安装信息不存在".to_string())?;
            let relative = extension
                .entry_point
                .strip_prefix("wasm:")
                .ok_or_else(|| "reading_extension 必须使用 wasm: entry_point".to_string())?;
            validate_wasm_path(relative).map_err(|error| error.to_string())?;
            selected = Some((
                package_asset(&installation.package_path, relative)
                    .map_err(|error| error.to_string())?,
                extension.parameters.clone(),
                action_manifest,
            ));
            break;
        }
        if selected.is_some() {
            break;
        }
    }
    let Some((module_path, extension_manifest, action_manifest)) = selected else {
        return Ok(None);
    };
    for requirement in action_manifest
        .get("requires")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        let field = if requirement == "selection" {
            "selection"
        } else {
            "visible_text"
        };
        if context
            .get(field)
            .and_then(Value::as_str)
            .is_none_or(|value| value.trim().is_empty())
        {
            return Err(format!("阅读插件动作需要非空 {field}"));
        }
    }
    let metadata = tokio::fs::metadata(&module_path)
        .await
        .map_err(|error| format!("阅读插件 WASM 不存在：{error}"))?;
    if metadata.len() > plugin_wasm::MAX_MODULE_BYTES as u64 {
        return Err("阅读插件 WASM 不能超过 16 MiB".to_string());
    }
    let module = tokio::fs::read(&module_path)
        .await
        .map_err(|error| format!("无法读取阅读插件 WASM：{error}"))?;
    let request = json!({
        "version": 1,
        "kind": "reading_extension",
        "extension_id": extension_id,
        "action": action,
        "context": context
    });
    let request_bytes =
        serde_json::to_vec(&request).map_err(|error| format!("无法编码阅读插件请求：{error}"))?;
    let response_bytes = plugin_wasm::execute(module, request_bytes).await?;
    let result: Value = serde_json::from_slice(&response_bytes)
        .map_err(|error| format!("阅读插件响应 JSON 无效：{error}"))?;
    validate_reading_extension_result(&extension_manifest, &result)?;
    Ok(Some(result))
}

fn validate_reading_extension_result(manifest: &Value, result: &Value) -> Result<(), String> {
    let object = result
        .as_object()
        .ok_or_else(|| "阅读插件响应必须是 JSON 对象".to_string())?;
    let result_type = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| "阅读插件响应缺少 type".to_string())?;
    let declared = manifest
        .get("result_types")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .any(|value| value == result_type);
    if !declared {
        return Err(format!("阅读插件返回未声明的结果类型：{result_type}"));
    }
    let title = object.get("title").and_then(Value::as_str).unwrap_or("");
    let message = object.get("message").and_then(Value::as_str).unwrap_or("");
    if title.chars().count() > 160 || message.chars().count() > 4000 {
        return Err("阅读插件结果标题或正文过长".to_string());
    }
    let payload = object.get("payload").cloned().unwrap_or_else(|| json!({}));
    if !payload.is_object() {
        return Err("阅读插件 payload 必须是 JSON 对象".to_string());
    }
    if serde_json::to_string(&payload)
        .map(|value| value.len() > 64 * 1024)
        .unwrap_or(true)
    {
        return Err("阅读插件 payload 不能超过 64 KiB".to_string());
    }
    if result_type == "browser_speech" {
        let text = payload
            .get("text")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty() && value.chars().count() <= 60_000)
            .ok_or_else(|| "browser_speech payload.text 无效")?;
        let _ = text;
    }
    if result_type == "quiz" {
        let questions = payload
            .get("questions")
            .and_then(Value::as_array)
            .ok_or_else(|| "quiz payload.questions 必须是数组")?;
        if questions.is_empty() || questions.len() > 20 {
            return Err("quiz 必须包含 1 到 20 道题".to_string());
        }
        for question in questions {
            if question
                .get("prompt")
                .and_then(Value::as_str)
                .is_none_or(|value| value.trim().is_empty())
            {
                return Err("quiz 题目缺少 prompt".to_string());
            }
            let choices = question
                .get("choices")
                .and_then(Value::as_array)
                .ok_or_else(|| "quiz 题目缺少 choices")?;
            if choices.len() < 2
                || choices.len() > 8
                || choices.iter().any(|value| !value.is_string())
            {
                return Err("quiz choices 必须包含 2 到 8 个字符串".to_string());
            }
        }
    }
    Ok(())
}

fn load_state(state: &AppState) -> ApiResult<Value> {
    let value = state
        .database
        .get_setting(STATE_KEY)?
        .unwrap_or_else(|| json!({"version": 2, "disabled": [], "plugins": {}}));
    if value.get("version").and_then(Value::as_i64) != Some(2)
        || !value.get("disabled").is_some_and(Value::is_array)
        || !value.get("plugins").is_some_and(Value::is_object)
    {
        return Err(ApiError::conflict("插件状态文件结构无效"));
    }
    Ok(value)
}

fn save_state(state: &AppState, value: &Value) -> ApiResult<()> {
    state.database.set_setting(STATE_KEY, value)?;
    Ok(())
}

fn parse_manifest_value(value: Value) -> ApiResult<Manifest> {
    let object = value
        .as_object()
        .ok_or_else(|| ApiError::bad_request("插件清单必须是 JSON 对象"))?;
    let schema = object
        .get("schema_version")
        .and_then(Value::as_str)
        .unwrap_or("");
    if schema != SCHEMA_VERSION {
        return Err(ApiError::bad_request(format!(
            "不支持的插件清单版本：{schema}"
        )));
    }
    let id = required_string(object, "id", 128)?;
    validate_plugin_id(&id)?;
    let name = required_string(object, "name", 120)?;
    let version = required_string(object, "version", 80)?;
    if VERSION_RE.find(&version).is_none() {
        return Err(ApiError::bad_request("插件版本号格式无效"));
    }
    let description = object
        .get("description_i18n")
        .and_then(Value::as_object)
        .and_then(|value| value.get("en"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let extensions = object
        .get("extensions")
        .and_then(Value::as_array)
        .ok_or_else(|| ApiError::bad_request("插件清单 extensions 必须是数组"))?
        .iter()
        .enumerate()
        .map(|(index, value)| parse_extension(value, index))
        .collect::<ApiResult<Vec<_>>>()?;
    let mut seen_ids = std::collections::BTreeSet::new();
    let mut seen_paths = std::collections::BTreeSet::new();
    for extension in &extensions {
        if !seen_ids.insert(extension.id.clone()) {
            return Err(ApiError::bad_request("插件扩展 ID 重复"));
        }
        if extension.extension_type == "http_route" && !seen_paths.insert(extension.path.clone()) {
            return Err(ApiError::bad_request("插件 HTTP 路由路径重复"));
        }
    }
    Ok(Manifest {
        raw: value,
        id,
        name,
        version,
        description,
        extensions,
    })
}

fn parse_extension(value: &Value, index: usize) -> ApiResult<PluginExtension> {
    let object = value
        .as_object()
        .ok_or_else(|| ApiError::bad_request(format!("extensions[{index}] 必须是对象")))?;
    let extension_type = required_string(object, "type", 64)?;
    if !matches!(
        extension_type.as_str(),
        "app_connector"
            | "capability"
            | "frontend_page"
            | "http_route"
            | "loop_capability"
            | "persistence_schema"
            | "tool"
            | "reading_extension"
            | "visualizer"
    ) {
        return Err(ApiError::bad_request(format!(
            "不支持的插件扩展类型：{extension_type}"
        )));
    }
    let id = required_string(object, "id", 64)?;
    if EXTENSION_ID_RE.find(&id).is_none() {
        return Err(ApiError::bad_request("插件扩展 ID 无效"));
    }
    let entry_point = optional_string(object, "entry_point")?;
    let manifest = optional_string(object, "manifest")?;
    let path = optional_string(object, "path")?;
    let auth = optional_string(object, "auth")?;
    let description = optional_string(object, "description")?;
    if description.chars().count() > 500 {
        return Err(ApiError::bad_request("插件扩展 description 过长"));
    }
    let parameters = object
        .get("parameters")
        .cloned()
        .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
    if extension_type == "tool"
        && (parameters.get("type").and_then(Value::as_str) != Some("object")
            || !parameters.get("properties").is_some_and(Value::is_object))
    {
        return Err(ApiError::bad_request(
            "tool parameters 必须是 JSON Schema object",
        ));
    }
    if extension_type == "reading_extension" {
        validate_reading_extension_manifest(&parameters, &id)?;
    }
    let schema = optional_string(object, "schema")?;
    let operations = object
        .get("operations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|value| {
            value
                .as_str()
                .map(str::trim)
                .filter(|value| {
                    !value.is_empty()
                        && value.len() <= 64
                        && value.chars().all(|character| {
                            character.is_ascii_alphanumeric()
                                || matches!(character, '_' | '-' | '.')
                        })
                })
                .map(str::to_string)
                .ok_or_else(|| ApiError::bad_request("插件 operations 包含无效名称"))
        })
        .collect::<ApiResult<Vec<_>>>()?;
    if operations.len() > 64 {
        return Err(ApiError::bad_request("插件 operations 不能超过 64 项"));
    }
    if !manifest.is_empty() {
        validate_relative_path(&manifest)?;
    }
    if !schema.is_empty() {
        validate_relative_path(&schema)?;
        if !schema.to_ascii_lowercase().ends_with(".json") {
            return Err(ApiError::bad_request("插件 schema 必须指向 .json 文件"));
        }
    }
    if !path.is_empty() && !path.starts_with('/') {
        return Err(ApiError::bad_request("插件路由 path 必须以 / 开头"));
    }
    if extension_type == "tool"
        && !id.chars().all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '_' | '-')
        })
    {
        return Err(ApiError::bad_request("tool 扩展 ID 必须是安全函数名"));
    }
    if extension_type == "capability" && RESERVED_CAPABILITY_IDS.contains(&id.as_str()) {
        return Err(ApiError::bad_request("capability 扩展 ID 不能覆盖内置能力"));
    }
    if matches!(
        extension_type.as_str(),
        "capability" | "loop_capability" | "tool" | "reading_extension"
    ) {
        if entry_point.is_empty() {
            return Err(ApiError::bad_request(format!(
                "{extension_type} 扩展必须声明 wasm: entry_point"
            )));
        }
        let Some(relative) = entry_point.strip_prefix("wasm:") else {
            return Err(ApiError::bad_request(format!(
                "{extension_type} entry_point 必须使用 wasm: 前缀"
            )));
        };
        validate_wasm_path(relative)?;
    }
    if extension_type == "visualizer" && manifest.is_empty() {
        return Err(ApiError::bad_request("visualizer 扩展必须声明 manifest"));
    }
    if extension_type == "frontend_page" {
        if manifest.is_empty() || path.is_empty() {
            return Err(ApiError::bad_request(
                "frontend_page 扩展必须声明 manifest 和 path",
            ));
        }
    }
    if extension_type == "http_route" {
        if entry_point.is_empty() || path.is_empty() {
            return Err(ApiError::bad_request(
                "http_route 扩展必须声明 entry_point 和 path",
            ));
        }
        if let Some(relative) = entry_point.strip_prefix("static:") {
            validate_asset_path(relative)?;
        } else if let Some(relative) = entry_point.strip_prefix("json:") {
            validate_json_path(relative)?;
        } else if let Some(relative) = entry_point.strip_prefix("wasm:") {
            validate_wasm_path(relative)?;
        } else {
            return Err(ApiError::bad_request(
                "http_route entry_point 必须使用 static:、json: 或 wasm: 前缀",
            ));
        }
    }
    if matches!(extension_type.as_str(), "http_route" | "frontend_page")
        && !matches!(auth.as_str(), "public" | "authenticated" | "admin")
    {
        return Err(ApiError::bad_request("插件路由认证策略无效"));
    }
    if extension_type == "persistence_schema" && (schema.is_empty() || operations.is_empty()) {
        return Err(ApiError::bad_request(
            "persistence_schema 扩展必须声明 schema 和 operations",
        ));
    }
    if extension_type == "app_connector" && operations.is_empty() {
        return Err(ApiError::bad_request(
            "app_connector 扩展必须声明 operations",
        ));
    }
    if !matches!(
        extension_type.as_str(),
        "capability" | "loop_capability" | "tool" | "reading_extension" | "http_route"
    ) && !entry_point.is_empty()
    {
        return Err(ApiError::bad_request(format!(
            "{extension_type} 扩展不允许 entry_point"
        )));
    }
    let methods = object
        .get("methods")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|method| method.to_ascii_uppercase())
        .collect::<Vec<_>>();
    for method in &methods {
        if !matches!(method.as_str(), "GET" | "POST" | "PUT" | "PATCH" | "DELETE") {
            return Err(ApiError::bad_request("插件路由 HTTP 方法无效"));
        }
    }
    if extension_type == "http_route" && methods.is_empty() {
        return Err(ApiError::bad_request("http_route 扩展必须声明 methods"));
    }
    Ok(PluginExtension {
        extension_type,
        id,
        entry_point,
        manifest,
        path,
        methods,
        auth,
        description,
        parameters,
        schema,
        operations,
    })
}

fn parse_installation(value: Value) -> ApiResult<Installation> {
    let object = value
        .as_object()
        .ok_or_else(|| ApiError::internal("插件安装信息必须是对象"))?;
    let version = required_string(object, "version", 80)?;
    let package_path = PathBuf::from(
        object
            .get("package_path")
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::internal("插件安装信息缺少目录"))?,
    );
    let artifact_sha256 = required_string(object, "artifact_sha256", 64)?;
    let installed_at = required_string(object, "installed_at", 80)?;
    let source = object
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Ok(Installation {
        version,
        package_path,
        artifact_sha256,
        installed_at,
        source,
    })
}

fn load_manifest(path: &FilePath) -> ApiResult<Manifest> {
    let metadata = std::fs::metadata(path)
        .map_err(|error| ApiError::bad_request(format!("无法读取插件清单：{error}")))?;
    if metadata.len() > MAX_MANIFEST_BYTES {
        return Err(ApiError::bad_request("插件清单不能超过 2 MiB"));
    }
    let data = std::fs::read(path)
        .map_err(|error| ApiError::bad_request(format!("无法读取插件清单：{error}")))?;
    let value = serde_json::from_slice(&data)
        .map_err(|error| ApiError::bad_request(format!("插件清单 JSON 无效：{error}")))?;
    parse_manifest_value(value)
}

fn prepare_package(source: &FilePath, staging_root: &FilePath) -> ApiResult<PathBuf> {
    if source.is_dir() && !source.is_symlink() {
        let target = staging_root.join("package");
        copy_package_tree(source, &target)?;
        return locate_package_root(&target)
            .ok_or_else(|| ApiError::bad_request("目录中缺少 deeptutor.plugin.json"));
    }
    if source.is_file() {
        let bytes = std::fs::read(source)
            .map_err(|error| ApiError::bad_request(format!("无法读取插件安装包：{error}")))?;
        let cursor = std::io::Cursor::new(bytes);
        let mut archive = ZipArchive::new(cursor)
            .map_err(|error| ApiError::bad_request(format!("插件安装包不是有效 ZIP：{error}")))?;
        if archive.len() > MAX_PACKAGE_FILES {
            return Err(ApiError::bad_request("插件安装包文件数量过多"));
        }
        let target = staging_root.join("package");
        for index in 0..archive.len() {
            let mut entry = archive
                .by_index(index)
                .map_err(|error| ApiError::bad_request(format!("无法读取插件归档：{error}")))?;
            let name = entry.name().replace('\\', "/");
            validate_relative_path(&name)?;
            let destination = target.join(&name);
            if entry.is_dir() {
                std::fs::create_dir_all(&destination)
                    .map_err(|error| ApiError::internal(format!("无法创建插件目录：{error}")))?;
                continue;
            }
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| ApiError::internal(format!("无法创建插件目录：{error}")))?;
            }
            let mut file = std::fs::File::create(&destination)
                .map_err(|error| ApiError::internal(format!("无法创建插件文件：{error}")))?;
            std::io::copy(&mut entry, &mut file)
                .map_err(|error| ApiError::internal(format!("无法解压插件文件：{error}")))?;
        }
        return locate_package_root(&target)
            .ok_or_else(|| ApiError::bad_request("安装包中缺少 deeptutor.plugin.json"));
    }
    Err(ApiError::bad_request("插件来源必须是目录或 ZIP 文件"))
}

fn locate_package_root(root: &FilePath) -> Option<PathBuf> {
    if root.join(MANIFEST_FILENAME).is_file() {
        return Some(root.to_path_buf());
    }
    let entries = std::fs::read_dir(root)
        .ok()?
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    if entries.len() == 1 && entries[0].path().is_dir() {
        let nested = entries[0].path();
        if nested.join(MANIFEST_FILENAME).is_file() {
            return Some(nested);
        }
    }
    None
}

async fn install_source_from_catalog(
    state: &AppState,
    plugin_id: &str,
    payload: &InstallRequest,
) -> ApiResult<PathBuf> {
    let entry = catalog_entries()
        .into_iter()
        .find(|entry| {
            entry.get("id").and_then(Value::as_str) == Some(plugin_id)
                && (payload.version == "latest"
                    || entry.get("version").and_then(Value::as_str)
                        == Some(payload.version.as_str()))
        })
        .ok_or_else(|| ApiError::not_found("官方插件目录中没有该版本"))?;
    let kind = entry
        .get("artifact")
        .and_then(|artifact| artifact.get("kind"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if kind != "portable-package" {
        return Err(ApiError::conflict(
            "当前便携版仅支持 WASM 或声明式便携插件包",
        ));
    }
    let url = entry
        .get("artifact")
        .and_then(|artifact| artifact.get("url"))
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::conflict("插件目录缺少下载地址"))?;
    let parsed = url::Url::parse(url)
        .map_err(|error| ApiError::bad_request(format!("插件下载地址无效：{error}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ApiError::bad_request("插件下载地址必须使用 HTTP(S)"));
    }
    let directory = state
        .paths
        .temp
        .join("plugin-downloads")
        .join(Uuid::new_v4().to_string());
    std::fs::create_dir_all(&directory)
        .map_err(|error| ApiError::internal(format!("无法创建插件下载目录：{error}")))?;
    let file = directory.join("plugin.zip");
    let response = state
        .client
        .get(parsed)
        .send()
        .await
        .map_err(|error| ApiError::provider(format!("插件下载失败：{error}")))?;
    if !response.status().is_success() {
        return Err(ApiError::provider(format!(
            "插件下载返回 HTTP {}",
            response.status()
        )));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|error| ApiError::provider(format!("无法读取插件下载内容：{error}")))?;
    std::fs::write(&file, &bytes)
        .map_err(|error| ApiError::internal(format!("无法保存插件下载：{error}")))?;
    Ok(file)
}

fn copy_package_tree(source: &FilePath, target: &FilePath) -> ApiResult<()> {
    std::fs::create_dir_all(target)
        .map_err(|error| ApiError::internal(format!("无法创建插件复制目录：{error}")))?;
    let mut count = 0usize;
    for entry in WalkDir::new(source).follow_links(false) {
        let entry =
            entry.map_err(|error| ApiError::bad_request(format!("无法遍历插件目录：{error}")))?;
        if entry.file_type().is_symlink() {
            return Err(ApiError::bad_request("插件安装包不能包含符号链接"));
        }
        let relative = entry
            .path()
            .strip_prefix(source)
            .map_err(|error| ApiError::internal(error.to_string()))?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        count += 1;
        if count > MAX_PACKAGE_FILES {
            return Err(ApiError::bad_request("插件安装包文件数量过多"));
        }
        let destination = target.join(relative);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&destination)
                .map_err(|error| ApiError::internal(format!("无法创建插件目录：{error}")))?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| ApiError::internal(format!("无法创建插件目录：{error}")))?;
            }
            std::fs::copy(entry.path(), &destination)
                .map_err(|error| ApiError::internal(format!("无法复制插件文件：{error}")))?;
        } else {
            return Err(ApiError::bad_request("插件安装包包含特殊文件"));
        }
    }
    Ok(())
}

fn package_digest(root: &FilePath) -> ApiResult<String> {
    let mut digest = Sha256::new();
    for entry in WalkDir::new(root).follow_links(false).sort_by_file_name() {
        let entry =
            entry.map_err(|error| ApiError::internal(format!("无法读取插件目录：{error}")))?;
        if entry.file_type().is_symlink() {
            return Err(ApiError::conflict("插件目录包含符号链接"));
        }
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|error| ApiError::internal(error.to_string()))?
            .to_string_lossy()
            .replace('\\', "/");
        digest.update(relative.as_bytes());
        let mut file = std::fs::File::open(entry.path())
            .map_err(|error| ApiError::internal(format!("无法读取插件文件：{error}")))?;
        let mut buffer = [0u8; 1024 * 1024];
        loop {
            let count = file
                .read(&mut buffer)
                .map_err(|error| ApiError::internal(format!("无法读取插件文件：{error}")))?;
            if count == 0 {
                break;
            }
            digest.update(&buffer[..count]);
        }
    }
    Ok(hex::encode(digest.finalize()))
}

fn approval_digest(manifest: &Manifest, artifact: &str) -> String {
    let mut digest = Sha256::new();
    canonical_json(&manifest.raw, &mut digest);
    digest.update(artifact.as_bytes());
    hex::encode(digest.finalize())
}

fn canonical_json(value: &Value, digest: &mut Sha256) {
    match value {
        Value::Object(object) => {
            digest.update(b"{");
            let mut keys = object.keys().collect::<Vec<_>>();
            keys.sort();
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    digest.update(b",");
                }
                digest.update(serde_json::to_string(key).unwrap_or_default().as_bytes());
                digest.update(b":");
                canonical_json(&object[key], digest);
            }
            digest.update(b"}");
        }
        Value::Array(values) => {
            digest.update(b"[");
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    digest.update(b",");
                }
                canonical_json(value, digest);
            }
            digest.update(b"]");
        }
        _ => {
            digest.update(value.to_string().as_bytes());
        }
    }
}

fn catalog_entries() -> Vec<Value> {
    serde_json::from_str::<Value>(CATALOG_JSON)
        .ok()
        .and_then(|value| value.get("entries").and_then(Value::as_array).cloned())
        .unwrap_or_default()
}

#[derive(Debug, Deserialize)]
struct PageManifest {
    schema_version: String,
    entry: String,
    assets: Vec<String>,
}

fn load_page_manifest(path: &FilePath) -> ApiResult<PageManifest> {
    let metadata = std::fs::metadata(path)
        .map_err(|error| ApiError::not_found(format!("插件页面清单不存在：{error}")))?;
    if metadata.len() > MAX_PAGE_MANIFEST_BYTES {
        return Err(ApiError::bad_request("插件页面清单不能超过 64 KiB"));
    }
    let data = std::fs::read(path)
        .map_err(|error| ApiError::internal(format!("无法读取插件页面清单：{error}")))?;
    let page: PageManifest = serde_json::from_slice(&data)
        .map_err(|error| ApiError::bad_request(format!("插件页面清单无效：{error}")))?;
    if page.schema_version != PAGE_SCHEMA_VERSION
        || page.assets.is_empty()
        || page.assets.len() > MAX_PAGE_ASSETS
        || !page.assets.contains(&page.entry)
    {
        return Err(ApiError::bad_request("插件页面清单结构无效"));
    }
    for asset in &page.assets {
        validate_asset_path(asset)?;
    }
    Ok(page)
}

async fn file_response(
    path: PathBuf,
    asset: bool,
    plugin_id: &str,
    version: &str,
    extension_id: &str,
) -> ApiResult<Response> {
    let metadata = tokio::fs::metadata(&path)
        .await
        .map_err(|error| ApiError::not_found(format!("插件页面资源不存在：{error}")))?;
    if metadata.len() > MAX_PAGE_ASSET_BYTES {
        return Err(ApiError::bad_request("插件页面资源不能超过 5 MiB"));
    }
    let data = tokio::fs::read(&path)
        .await
        .map_err(|error| ApiError::internal(format!("无法读取插件页面资源：{error}")))?;
    let mime = mime_guess::from_path(&path)
        .first_or_octet_stream()
        .essence_str()
        .to_string();
    let mut response = Response::new(Body::from(data));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(if mime == "text/html" {
            "text/html; charset=utf-8"
        } else {
            &mime
        })
        .map_err(|error| ApiError::internal(error.to_string()))?,
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(PAGE_CSP),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response.headers_mut().insert(
        header::HeaderName::from_static("cross-origin-resource-policy"),
        HeaderValue::from_static(if asset { "cross-origin" } else { "same-origin" }),
    );
    response.headers_mut().insert(
        header::HeaderName::from_static("x-deeptutor-plugin"),
        HeaderValue::from_str(&format!("{plugin_id}/{version}/{extension_id}"))
            .map_err(|error| ApiError::internal(error.to_string()))?,
    );
    Ok(response)
}

fn package_asset(root: &FilePath, relative: &str) -> ApiResult<PathBuf> {
    validate_relative_path(relative)?;
    let path = root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
    let path = confined_path(root, &path)?;
    if !path.is_file() {
        return Err(ApiError::not_found("插件资源不存在"));
    }
    Ok(path)
}

fn validate_asset_path(value: &str) -> ApiResult<()> {
    validate_relative_path(value)?;
    let path = FilePath::new(value);
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !matches!(
        extension.as_str(),
        "css"
            | "html"
            | "ico"
            | "jpeg"
            | "jpg"
            | "js"
            | "json"
            | "png"
            | "svg"
            | "txt"
            | "webp"
            | "woff2"
    ) {
        return Err(ApiError::bad_request("插件页面资源类型不受支持"));
    }
    for component in path.components() {
        match component {
            Component::Normal(part)
                if ASSET_NAME_RE.find(part.to_str().unwrap_or("")).is_some() => {}
            _ => return Err(ApiError::bad_request("插件页面资源路径无效")),
        }
    }
    Ok(())
}

fn validate_json_path(value: &str) -> ApiResult<()> {
    validate_asset_path(value)?;
    if !value.to_ascii_lowercase().ends_with(".json") {
        return Err(ApiError::bad_request("JSON 插件入口必须指向 .json 文件"));
    }
    Ok(())
}

fn validate_wasm_path(value: &str) -> ApiResult<()> {
    validate_relative_path(value)?;
    if !value.to_ascii_lowercase().ends_with(".wasm") {
        return Err(ApiError::bad_request("WASM 插件入口必须指向 .wasm 文件"));
    }
    Ok(())
}

fn validate_relative_path(value: &str) -> ApiResult<()> {
    if value.is_empty()
        || value.len() > 512
        || value.contains(['\0', '\\'])
        || value.starts_with('/')
    {
        return Err(ApiError::bad_request("插件资源路径无效"));
    }
    let path = FilePath::new(value);
    for component in path.components() {
        match component {
            Component::Normal(part) if !part.is_empty() => {}
            _ => return Err(ApiError::bad_request("插件资源路径不能包含上级目录")),
        }
    }
    Ok(())
}

fn validate_plugin_id(plugin_id: &str) -> ApiResult<()> {
    if PLUGIN_ID_RE.find(plugin_id).is_none() {
        return Err(ApiError::bad_request("插件 ID 无效"));
    }
    Ok(())
}

fn required_string(object: &Map<String, Value>, key: &str, max_length: usize) -> ApiResult<String> {
    let value = object
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiError::bad_request(format!("插件清单缺少 {key}")))?;
    if value.chars().count() > max_length {
        return Err(ApiError::bad_request(format!("插件清单字段 {key} 过长")));
    }
    Ok(value.to_string())
}

fn optional_string(object: &Map<String, Value>, key: &str) -> ApiResult<String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(value)) => Ok(value.trim().to_string()),
        _ => Err(ApiError::bad_request(format!(
            "插件清单字段 {key} 必须是字符串"
        ))),
    }
}

fn validate_reading_extension_manifest(value: &Value, extension_id: &str) -> ApiResult<()> {
    let object = value
        .as_object()
        .ok_or_else(|| ApiError::bad_request("reading_extension parameters 必须是对象"))?;
    if object.get("protocol_version").and_then(Value::as_str) != Some("1") {
        return Err(ApiError::bad_request(
            "reading_extension protocol_version 必须是 1",
        ));
    }
    if object.get("id").and_then(Value::as_str) != Some(extension_id) {
        return Err(ApiError::bad_request(
            "reading_extension parameters.id 必须与扩展 ID 一致",
        ));
    }
    let actions = object
        .get("actions")
        .and_then(Value::as_array)
        .ok_or_else(|| ApiError::bad_request("reading_extension actions 必须是数组"))?;
    if actions.is_empty() || actions.len() > 12 {
        return Err(ApiError::bad_request(
            "reading_extension actions 必须包含 1 到 12 项",
        ));
    }
    let mut action_ids = std::collections::BTreeSet::new();
    for action in actions {
        let action = action
            .as_object()
            .ok_or_else(|| ApiError::bad_request("reading_extension action 必须是对象"))?;
        let id = action
            .get("id")
            .and_then(Value::as_str)
            .filter(|value| {
                value.len() <= 64
                    && value.chars().all(|character| {
                        character.is_ascii_lowercase()
                            || character.is_ascii_digit()
                            || character == '_'
                            || character == '-'
                    })
            })
            .ok_or_else(|| ApiError::bad_request("reading_extension action.id 无效"))?;
        if !action_ids.insert(id) {
            return Err(ApiError::bad_request("reading_extension action.id 重复"));
        }
        let label = action
            .get("label")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty() && value.chars().count() <= 80)
            .ok_or_else(|| ApiError::bad_request("reading_extension action.label 无效"))?;
        let _ = label;
        if action.get("trigger").and_then(Value::as_str) != Some("toolbar") {
            return Err(ApiError::bad_request(
                "reading_extension action.trigger 必须是 toolbar",
            ));
        }
        let requires = action
            .get("requires")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if requires.len() > 2
            || requires
                .iter()
                .any(|value| !matches!(value.as_str(), Some("selection") | Some("visible_text")))
        {
            return Err(ApiError::bad_request(
                "reading_extension action.requires 只支持 selection 或 visible_text",
            ));
        }
    }
    let result_types = object
        .get("result_types")
        .and_then(Value::as_array)
        .ok_or_else(|| ApiError::bad_request("reading_extension result_types 必须是数组"))?;
    if result_types.is_empty()
        || result_types.len() > 4
        || result_types.iter().any(|value| {
            !matches!(
                value.as_str(),
                Some("card") | Some("quiz") | Some("feedback") | Some("browser_speech")
            )
        })
    {
        return Err(ApiError::bad_request("reading_extension result_types 无效"));
    }
    Ok(())
}

fn confined_path(root: &FilePath, candidate: &FilePath) -> ApiResult<PathBuf> {
    let root = std::fs::canonicalize(root)
        .map_err(|error| ApiError::internal(format!("无法解析安装目录：{error}")))?;
    let absolute = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };
    if absolute
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(ApiError::bad_request("路径不能包含上级目录引用"));
    }
    let mut ancestor = absolute.as_path();
    while !ancestor.exists() {
        ancestor = ancestor
            .parent()
            .ok_or_else(|| ApiError::bad_request("路径无有效父目录"))?;
    }
    if ancestor.is_symlink() && ancestor != root {
        return Err(ApiError::bad_request("路径不能经过符号链接"));
    }
    let original_ancestor = ancestor;
    let ancestor = std::fs::canonicalize(ancestor)
        .map_err(|error| ApiError::bad_request(format!("无法解析路径：{error}")))?;
    if !ancestor.starts_with(&root) {
        return Err(ApiError::forbidden(
            "为保持便携性，插件数据必须位于安装目录内",
        ));
    }
    let suffix = absolute
        .strip_prefix(original_ancestor)
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    let resolved = if suffix.as_os_str().is_empty() {
        ancestor
    } else {
        ancestor.join(suffix)
    };
    if resolved.exists() && resolved.is_symlink() {
        return Err(ApiError::bad_request("目标路径不能是符号链接"));
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::portable::PortablePaths;
    use crate::storage::Database;
    use tempfile::TempDir;

    fn test_state() -> (AppState, TempDir, PathBuf) {
        let temporary = tempfile::tempdir().expect("创建测试目录");
        let root = temporary.path().to_path_buf();
        let paths = PortablePaths {
            home: root.join("home"),
            data: root.join("data"),
            cache: root.join("cache"),
            temp: root.join("temp"),
            logs: root.join("logs"),
            webview: root.join("cache/webview2"),
            uploads: root.join("data/uploads"),
            outputs: root.join("data/outputs"),
            database: root.join("data/deeptutor.sqlite3"),
            root,
        };
        paths.prepare().expect("创建便携测试目录");
        let database = Database::new(&paths.database);
        database.initialize().expect("初始化测试数据库");
        let package = paths
            .data
            .join("plugins")
            .join("demo.plugin")
            .join("versions")
            .join("1.0.0");
        std::fs::create_dir_all(&package).expect("创建测试插件目录");
        let state = AppState::new(database, paths).expect("创建测试应用状态");
        (state, temporary, package)
    }

    fn tool_manifest(entry_point: &str) -> Value {
        json!({
            "schema_version": SCHEMA_VERSION,
            "id": "demo.plugin",
            "name": "演示插件",
            "version": "1.0.0",
            "description_i18n": {"en": "Demo plugin"},
            "extensions": [{
                "type": "tool",
                "id": "echo_tool",
                "entry_point": entry_point,
                "description": "返回固定测试结果",
                "parameters": {
                    "type": "object",
                    "properties": {"message": {"type": "string"}},
                    "required": ["message"]
                }
            }, {
                "type": "capability",
                "id": "demo_capability",
                "entry_point": entry_point,
                "description": "固定测试能力",
                "parameters": {"type": "object", "properties": {}}
            }, {
                "type": "loop_capability",
                "id": "demo_loop_capability",
                "entry_point": entry_point,
                "description": "固定循环测试能力",
                "parameters": {"type": "object", "properties": {}}
            }]
        })
    }

    fn install_test_package(state: &AppState, package: &PathBuf, raw: Value, response: &str) {
        std::fs::write(
            package.join(MANIFEST_FILENAME),
            serde_json::to_vec_pretty(&raw).expect("编码测试清单"),
        )
        .expect("写入测试清单");
        let escaped = response.replace('\\', "\\\\").replace('"', "\\\"");
        let source = format!(
            r#"
            (module
                (memory (export "memory") 80)
                (global $heap (mut i32) (i32.const 1024))
                (func (export "alloc") (param $len i32) (result i32)
                    (local $pointer i32)
                    (local.set $pointer (global.get $heap))
                    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
                    (local.get $pointer)
                )
                (data (i32.const 128) "{escaped}")
                (func (export "deeptutor_route")
                    (param $request_pointer i32)
                    (param $request_length i32)
                    (param $output_pointer i32)
                    (param $output_capacity i32)
                    (result i32)
                    (memory.copy
                        (local.get $output_pointer)
                        (i32.const 128)
                        (i32.const {length}))
                    (i32.const {length})
                )
            )
            "#,
            length = response.len()
        );
        let wasm = wat::parse_str(source).expect("测试 WAT 有效");
        std::fs::write(package.join("tool.wasm"), wasm).expect("写入测试 WASM");
        let manifest = parse_manifest_value(raw).expect("测试清单有效");
        let artifact_sha256 = package_digest(package).expect("计算测试插件摘要");
        let approval = approval_digest(&manifest, &artifact_sha256);
        state
            .database
            .set_setting(
                STATE_KEY,
                &json!({
                    "version": 2,
                    "disabled": [],
                    "plugins": {
                        "demo.plugin": {
                            "manifest": manifest.raw,
                            "installation": {
                                "version": "1.0.0",
                                "package_path": package.display().to_string(),
                                "artifact_sha256": artifact_sha256,
                                "installed_at": "2026-01-01T00:00:00Z",
                                "source": "test"
                            },
                            "approval": {"digest": approval},
                            "history": []
                        }
                    }
                }),
            )
            .expect("写入测试插件状态");
    }

    fn install_test_tool(state: &AppState, package: &PathBuf) -> String {
        let response = r#"{"content":"ok","metadata":{"source":"plugin"},"success":true,"terminate_turn":true,"response":"ok","events":[],"result":{"kind":"plugin_capability"},"attachments":[]}"#;
        install_test_package(state, package, tool_manifest("wasm:tool.wasm"), response);
        plugin_tool_name("demo.plugin", "echo_tool")
    }

    fn reading_manifest() -> Value {
        json!({
            "schema_version": SCHEMA_VERSION,
            "id": "demo.plugin",
            "name": "阅读演示插件",
            "version": "1.0.0",
            "description_i18n": {"en": "Demo reading plugin"},
            "extensions": [{
                "type": "reading_extension",
                "id": "plugin_explain",
                "entry_point": "wasm:tool.wasm",
                "description": "解释选中文本",
                "parameters": {
                    "id": "plugin_explain",
                    "protocol_version": "1",
                    "actions": [{
                        "id": "explain",
                        "label": "解释",
                        "trigger": "toolbar",
                        "requires": ["selection"]
                    }],
                    "result_types": ["card"]
                }
            }]
        })
    }

    fn visualizer_manifest() -> Value {
        json!({
            "schema_version": SCHEMA_VERSION,
            "id": "demo.plugin",
            "name": "可视化演示插件",
            "version": "1.0.0",
            "description_i18n": {"en": "Demo visualizer plugin"},
            "extensions": [{
                "type": "visualizer",
                "id": "demo_visualizer",
                "manifest": "visualizers/demo/visualizer.json"
            }]
        })
    }

    #[test]
    fn reject_package_traversal() {
        assert!(validate_relative_path("../outside").is_err());
        assert!(validate_relative_path("assets/app.js").is_ok());
    }

    #[test]
    fn approval_digest_is_stable_across_key_order() {
        let first = parse_manifest_value(json!({
            "schema_version": SCHEMA_VERSION,
            "id": "com.example.demo",
            "name": "Demo",
            "version": "1.0.0",
            "description_i18n": {"en": "Demo"},
            "extensions": []
        }))
        .expect("清单有效");
        let second = parse_manifest_value(json!({
            "extensions": [],
            "description_i18n": {"en": "Demo"},
            "version": "1.0.0",
            "name": "Demo",
            "id": "com.example.demo",
            "schema_version": SCHEMA_VERSION
        }))
        .expect("清单有效");
        assert_eq!(
            approval_digest(&first, "artifact"),
            approval_digest(&second, "artifact")
        );
    }

    #[test]
    fn python_tool_entrypoint_is_rejected() {
        let error = parse_extension(
            &json!({
                "type": "tool",
                "id": "legacy_tool",
                "entry_point": "legacy.worker",
                "parameters": {"type": "object", "properties": {}}
            }),
            0,
        )
        .expect_err("Python Tool 入口必须被拒绝");
        assert!(error.to_string().contains("wasm:"));
    }

    #[tokio::test]
    async fn wasm_tool_is_registered_and_executed() {
        let (state, _temporary, package) = test_state();
        let name = install_test_tool(&state, &package);
        let definitions = plugin_tool_definitions(&state).expect("读取插件 Tool 定义");
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0]["function"]["name"], name);

        let surface = crate::server::tool_policy::compose(&state, &json!({}), "chat", &[], false)
            .expect("组合工具表");
        assert!(surface.names.contains(&name));
        assert_eq!(surface.deferred_definitions.len(), 1);

        let result = execute_plugin_tool(
            &state,
            &name,
            &json!({"message": "你好"}),
            "turn-test",
            "session-test",
            "zh",
        )
        .await
        .expect("插件 Tool 应执行成功")
        .expect("插件 Tool 应被识别");
        assert_eq!(result.content, "ok");
        assert_eq!(result.metadata["source"], "plugin");
        assert!(result.success);
        assert!(result.terminate_turn);

        let capabilities = plugin_capability_catalog(&state).expect("读取插件能力目录");
        assert_eq!(capabilities.len(), 2);
        assert_eq!(capabilities[0]["id"], "demo_capability");
        let capability_result = execute_plugin_capability(
            &state,
            "demo_capability",
            &json!({"content": "测试"}),
            "session-test",
            "turn-test",
            "zh",
        )
        .await
        .expect("插件能力应执行成功")
        .expect("插件能力应被识别");
        assert_eq!(capability_result.response, "ok");
        assert_eq!(capability_result.result["response"], "ok");

        assert!(capabilities
            .iter()
            .any(|item| item["id"] == "demo_loop_capability" && item["kind"] == "loop"));
        let loop_result = execute_plugin_capability(
            &state,
            "demo_loop_capability",
            &json!({"content": "循环测试"}),
            "session-test",
            "turn-loop",
            "zh",
        )
        .await
        .expect("插件循环能力应执行成功")
        .expect("插件循环能力应被识别");
        assert_eq!(loop_result.response, "ok");
    }

    #[test]
    fn plugin_visualizer_manifest_is_discovered() {
        let (state, _temporary, package) = test_state();
        let visualizer = package.join("visualizers/demo");
        std::fs::create_dir_all(&visualizer).expect("创建测试可视化器目录");
        std::fs::write(
            visualizer.join("visualizer.json"),
            serde_json::to_vec_pretty(&json!({
                "id": "demo_visualizer",
                "display_name": "演示可视化",
                "description": "测试插件可视化器",
                "payload_format": "JSON",
                "prompt": "生成演示可视化",
                "renderer_entry": "index.html"
            }))
            .expect("编码测试可视化器清单"),
        )
        .expect("写入测试可视化器清单");
        std::fs::write(visualizer.join("index.html"), "<html></html>")
            .expect("写入测试可视化器页面");
        install_test_package(
            &state,
            &package,
            visualizer_manifest(),
            r#"{"content":"unused"}"#,
        );

        let visualizers = plugin_visualizers(&state).expect("读取插件可视化器");
        assert_eq!(visualizers.len(), 1);
        assert_eq!(visualizers[0].0["id"], "demo_visualizer");
        assert!(visualizers[0]
            .1
            .join("visualizers/demo/index.html")
            .is_file());
    }

    #[tokio::test]
    async fn wasm_reading_extension_is_registered_and_executed() {
        let (state, _temporary, package) = test_state();
        let response = r#"{"type":"card","title":"解释","message":"固定解释结果","payload":{"kind":"explanation"}}"#;
        install_test_package(&state, &package, reading_manifest(), response);

        let extensions = plugin_reading_extensions(&state).expect("读取插件阅读扩展");
        assert_eq!(extensions.len(), 1);
        assert_eq!(extensions[0]["id"], "plugin_explain");
        assert_eq!(extensions[0]["name"], "阅读演示插件");
        assert_eq!(extensions[0]["kind"], "plugin");

        let result = execute_plugin_reading_action(
            &state,
            "plugin_explain",
            "explain",
            &json!({"selection": "已选内容"}),
        )
        .await
        .expect("插件阅读动作应执行成功")
        .expect("插件阅读动作应被识别");
        assert_eq!(result["type"], "card");
        assert_eq!(result["payload"]["kind"], "explanation");

        let error = execute_plugin_reading_action(
            &state,
            "plugin_explain",
            "explain",
            &json!({"selection": "   "}),
        )
        .await
        .expect_err("缺少选文时必须失败");
        assert!(error.contains("selection"));
    }

    #[test]
    fn reading_result_payloads_are_validated() {
        let manifest = json!({"result_types": ["browser_speech", "quiz"]});
        assert!(validate_reading_extension_result(
            &manifest,
            &json!({"type": "browser_speech", "payload": {"text": "朗读内容"}})
        )
        .is_ok());
        assert!(validate_reading_extension_result(
            &manifest,
            &json!({
                "type": "quiz",
                "payload": {
                    "questions": [{
                        "prompt": "题目",
                        "choices": ["甲", "乙"]
                    }]
                }
            })
        )
        .is_ok());
        assert!(validate_reading_extension_result(
            &manifest,
            &json!({"type": "quiz", "payload": {"questions": []}})
        )
        .is_err());
        assert!(validate_reading_extension_result(
            &manifest,
            &json!({"type": "card", "payload": {}})
        )
        .is_err());
    }

    #[test]
    fn existing_package_asset_resolves_to_a_file() {
        let temporary = tempfile::tempdir().expect("创建测试目录");
        let path = temporary.path().join("asset.txt");
        std::fs::write(&path, "ok").expect("写入测试资源");
        let resolved = package_asset(temporary.path(), "asset.txt").expect("解析测试资源");
        assert!(resolved.is_file());
    }

    #[test]
    fn modified_plugin_package_is_not_registered() {
        let (state, _temporary, package) = test_state();
        install_test_tool(&state, &package);
        std::fs::write(package.join("tampered.txt"), "modified").expect("修改测试插件包");
        let definitions = plugin_tool_definitions(&state).expect("读取插件 Tool 定义");
        assert!(definitions.is_empty(), "安装包变化后不得继续注册 Tool");
    }
}
