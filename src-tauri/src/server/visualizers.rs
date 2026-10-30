use super::error::{ApiError, ApiResult};
use super::{AppState, ScopedState};
use axum::body::Body;
use axum::extract::{Multipart, Path};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::{Path as FilePath, PathBuf};
use uuid::Uuid;
use zip::ZipArchive;

const MAX_ARCHIVE_BYTES: u64 = 25 * 1024 * 1024;
const MAX_ENTRY_BYTES: u64 = 10 * 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 128 * 1024;
const MAX_ENTRIES: usize = 200;
const MAX_RATIO: u64 = 100;
const STATE_SETTING: &str = "visualizers_state";
const IFRAME_CSP: &str = "default-src 'self'; script-src 'self' 'unsafe-inline' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self' data:; media-src 'self' data: blob:; connect-src 'none'; frame-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'self'";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/visualizers/list", get(list_visualizers))
        .route(
            "/visualizers/bundled/{visualizer_id}/install",
            post(install_bundled),
        )
        .route("/visualizers/import", post(import_visualizer))
        .route(
            "/visualizers/{visualizer_id}/enable",
            post(enable_visualizer),
        )
        .route(
            "/visualizers/{visualizer_id}/disable",
            post(disable_visualizer),
        )
        .route("/visualizers/{visualizer_id}", delete(uninstall_visualizer))
        .route(
            "/visualizers/{visualizer_id}/assets/{*asset_path}",
            get(serve_asset),
        )
}

#[derive(Clone, Debug, Default)]
struct VisualizerState {
    installed: BTreeSet<String>,
    disabled: BTreeSet<String>,
    uninstalled: BTreeSet<String>,
}

#[derive(Clone, Debug)]
struct CatalogItem {
    manifest: Value,
    origin: &'static str,
    core: bool,
    default_installed: bool,
    root: Option<PathBuf>,
}

async fn list_visualizers(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    let catalog = load_catalog(&state)?;
    let saved = load_state(&state)?;
    Ok(Json(json!({
        "schema_version": "deeptutor.visualizer-catalog/v1",
        "visualizers": public_catalog(catalog, &saved)
    })))
}

async fn install_bundled(
    ScopedState(state): ScopedState,
    Path(visualizer_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let id = normalize_id(&visualizer_id)?;
    let catalog = load_catalog(&state)?;
    let item = catalog
        .iter()
        .find(|item| item.manifest["id"].as_str() == Some(id.as_str()))
        .ok_or_else(|| ApiError::not_found(format!("未找到捆绑可视化器：{id}")))?;
    if item.origin != "bundled" {
        return Err(ApiError::bad_request("只有捆绑可视化器需要执行安装"));
    }

    let mut saved = load_state(&state)?;
    saved.installed.insert(id.clone());
    saved.uninstalled.remove(&id);
    saved.disabled.remove(&id);
    save_state(&state, &saved)?;
    Ok(Json(json!({"status": "installed", "visualizer": id})))
}

async fn enable_visualizer(
    ScopedState(state): ScopedState,
    Path(visualizer_id): Path<String>,
) -> ApiResult<Json<Value>> {
    set_enabled(state, visualizer_id, true)
}

async fn disable_visualizer(
    ScopedState(state): ScopedState,
    Path(visualizer_id): Path<String>,
) -> ApiResult<Json<Value>> {
    set_enabled(state, visualizer_id, false)
}

fn set_enabled(state: AppState, visualizer_id: String, enabled: bool) -> ApiResult<Json<Value>> {
    let id = normalize_id(&visualizer_id)?;
    let catalog = load_catalog(&state)?;
    let item = catalog
        .iter()
        .find(|item| item.manifest["id"].as_str() == Some(id.as_str()))
        .ok_or_else(|| ApiError::not_found(format!("可视化器不存在：{id}")))?;
    let installed = is_installed(item, &load_state(&state)?);
    if !installed {
        return Err(ApiError::conflict(format!("可视化器尚未安装：{id}")));
    }
    let mut saved = load_state(&state)?;
    if enabled {
        saved.disabled.remove(&id);
    } else {
        saved.disabled.insert(id.clone());
    }
    save_state(&state, &saved)?;
    Ok(Json(json!({
        "status": if enabled { "enabled" } else { "disabled" },
        "visualizer": id
    })))
}

async fn uninstall_visualizer(
    ScopedState(state): ScopedState,
    Path(visualizer_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let id = normalize_id(&visualizer_id)?;
    let catalog = load_catalog(&state)?;
    let item = catalog
        .iter()
        .find(|item| item.manifest["id"].as_str() == Some(id.as_str()))
        .ok_or_else(|| ApiError::not_found(format!("可视化器不存在：{id}")))?;
    if item.core {
        return Err(ApiError::bad_request("内置可视化器不能卸载，请改为禁用"));
    }

    let mut saved = load_state(&state)?;
    match item.origin {
        "bundled" => {
            saved.installed.remove(&id);
            saved.uninstalled.insert(id.clone());
            saved.disabled.remove(&id);
        }
        "user" => {
            let package = item
                .root
                .as_ref()
                .ok_or_else(|| ApiError::internal("用户可视化器缺少安装目录"))?;
            let root = visualizer_root(&state)?;
            let safe_package = package
                .canonicalize()
                .map_err(|error| ApiError::internal(format!("无法解析可视化器目录：{error}")))?;
            if !safe_package.starts_with(&root) || safe_package == root {
                return Err(ApiError::forbidden("拒绝删除便携数据目录外的可视化器"));
            }
            fs::remove_dir_all(&safe_package)
                .map_err(|error| ApiError::internal(format!("无法卸载可视化器：{error}")))?;
            saved.disabled.remove(&id);
            saved.installed.remove(&id);
        }
        "plugin" => {
            return Err(ApiError::bad_request(
                "插件可视化器随插件安装，请在插件管理中卸载插件",
            ));
        }
        _ => return Err(ApiError::bad_request("该可视化器不可卸载")),
    }
    save_state(&state, &saved)?;
    Ok(Json(json!({"status": "uninstalled", "visualizer": id})))
}

async fn import_visualizer(
    ScopedState(state): ScopedState,
    mut multipart: Multipart,
) -> ApiResult<Json<Value>> {
    let mut package = None;
    let mut filename = String::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|error| ApiError::bad_request(format!("无法读取导入表单：{error}")))?
    {
        if field.name() == Some("file") {
            filename = field.file_name().unwrap_or("").to_string();
            package = Some(
                field
                    .bytes()
                    .await
                    .map_err(|error| ApiError::bad_request(format!("无法读取安装包：{error}")))?,
            );
        }
    }
    let bytes = package.ok_or_else(|| ApiError::bad_request("导入表单缺少 file"))?;
    if !filename.to_ascii_lowercase().ends_with(".zip") {
        return Err(ApiError::bad_request("可视化器安装包必须是 ZIP 文件"));
    }
    if bytes.is_empty() || bytes.len() as u64 > MAX_ARCHIVE_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            "可视化器安装包不能超过 25 MB",
            false,
        ));
    }

    let id = install_archive(&state, &bytes)?;
    let catalog = load_catalog(&state)?;
    let manifest = public_item(
        catalog
            .iter()
            .find(|item| item.manifest["id"].as_str() == Some(id.as_str()))
            .ok_or_else(|| ApiError::internal("安装后的可视化器无法重新发现"))?,
        &load_state(&state)?,
    );
    Ok(Json(json!({
        "status": "installed",
        "visualizer": id,
        "manifest": manifest
    })))
}

async fn serve_asset(
    ScopedState(state): ScopedState,
    Path((visualizer_id, asset_path)): Path<(String, String)>,
) -> ApiResult<Response> {
    let id = normalize_id(&visualizer_id)?;
    let catalog = load_catalog(&state)?;
    let item = catalog
        .iter()
        .find(|item| item.manifest["id"].as_str() == Some(id.as_str()))
        .ok_or_else(|| ApiError::not_found("可视化器不存在"))?;
    if !matches!(item.origin, "user" | "plugin") {
        return Err(ApiError::not_found("可视化器资产不存在"));
    }
    if !is_enabled(item, &load_state(&state)?) {
        return Err(ApiError::not_found("可视化器资产不存在"));
    }
    let package = item
        .root
        .as_ref()
        .ok_or_else(|| ApiError::not_found("可视化器资产不存在"))?;
    let relative = safe_relative_path(&asset_path)?;
    let candidate = package.join(relative);
    let canonical = candidate
        .canonicalize()
        .map_err(|_| ApiError::not_found("可视化器资产不存在"))?;
    if !canonical.starts_with(package) || !canonical.is_file() {
        return Err(ApiError::not_found("可视化器资产不存在"));
    }
    let data = tokio::fs::read(&canonical)
        .await
        .map_err(|error| ApiError::internal(format!("无法读取可视化器资产：{error}")))?;
    let mime = mime_guess::from_path(&canonical).first_or_octet_stream();
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime.as_ref())
        .header(header::CACHE_CONTROL, "private, max-age=300")
        .header(header::CONTENT_SECURITY_POLICY, IFRAME_CSP)
        .header("cross-origin-resource-policy", "same-origin")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(header::CONTENT_LENGTH, data.len())
        .body(Body::from(data))
        .map_err(|error| ApiError::internal(format!("无法构建资产响应：{error}")))?;
    if let Some(name) = canonical.file_name().and_then(|name| name.to_str()) {
        if let Ok(value) = HeaderValue::from_str(&format!("inline; filename=\"{name}\"")) {
            response
                .headers_mut()
                .insert(header::CONTENT_DISPOSITION, value);
        }
    }
    Ok(response.into_response())
}

fn load_catalog(state: &AppState) -> ApiResult<Vec<CatalogItem>> {
    let mut catalog = builtins();
    let root = visualizer_root(state)?;
    if root.is_dir() {
        let mut entries = fs::read_dir(&root)
            .map_err(|error| ApiError::internal(format!("无法读取可视化器目录：{error}")))?
            .filter_map(Result::ok)
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            if !entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                continue;
            }
            let package = entry.path();
            let manifest_path = package.join("visualizer.json");
            if !manifest_path.is_file() {
                continue;
            }
            let Ok(metadata) = fs::metadata(&manifest_path) else {
                continue;
            };
            if metadata.len() > MAX_MANIFEST_BYTES {
                continue;
            }
            let Ok(raw) = fs::read_to_string(&manifest_path) else {
                continue;
            };
            let Ok(mut manifest) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            if validate_imported_manifest(&mut manifest, &package).is_err() {
                continue;
            }
            let id = manifest["id"].as_str().unwrap_or("").to_string();
            if id.is_empty() || catalog.iter().any(|item| item.manifest["id"] == id) {
                continue;
            }
            catalog.push(CatalogItem {
                manifest,
                origin: "user",
                core: false,
                default_installed: true,
                root: Some(package),
            });
        }
    }
    for (mut manifest, package) in
        super::plugins::plugin_visualizers(state).map_err(ApiError::internal)?
    {
        if validate_imported_manifest(&mut manifest, &package).is_err() {
            continue;
        }
        let id = manifest["id"].as_str().unwrap_or("").to_string();
        if id.is_empty() || catalog.iter().any(|item| item.manifest["id"] == id) {
            continue;
        }
        catalog.push(CatalogItem {
            manifest,
            origin: "plugin",
            core: false,
            default_installed: true,
            root: Some(package),
        });
    }
    catalog.sort_by_key(|item| {
        (
            item.manifest["priority"].as_i64().unwrap_or(100),
            item.manifest["id"].as_str().unwrap_or("").to_string(),
        )
    });
    Ok(catalog)
}

fn public_catalog(catalog: Vec<CatalogItem>, state: &VisualizerState) -> Vec<Value> {
    catalog
        .iter()
        .map(|item| public_item(item, state))
        .collect()
}

fn public_item(item: &CatalogItem, state: &VisualizerState) -> Value {
    let installed = is_installed(item, state);
    let enabled = installed
        && !state
            .disabled
            .contains(item.manifest["id"].as_str().unwrap_or(""));
    let mut object = item.manifest.as_object().cloned().unwrap_or_else(Map::new);
    object.remove("prompt");
    object.insert("origin".to_string(), json!(item.origin));
    object.insert("installed".to_string(), json!(installed));
    object.insert("enabled".to_string(), json!(enabled));
    object.insert(
        "uninstallable".to_string(),
        json!(!item.core && item.origin != "plugin"),
    );
    Value::Object(object)
}

fn is_installed(item: &CatalogItem, state: &VisualizerState) -> bool {
    let id = item.manifest["id"].as_str().unwrap_or("");
    if item.core {
        return true;
    }
    if matches!(item.origin, "user" | "plugin") {
        return true;
    }
    state.installed.contains(id) || (item.default_installed && !state.uninstalled.contains(id))
}

fn is_enabled(item: &CatalogItem, state: &VisualizerState) -> bool {
    let id = item.manifest["id"].as_str().unwrap_or("");
    is_installed(item, state) && !state.disabled.contains(id)
}

fn load_state(state: &AppState) -> ApiResult<VisualizerState> {
    let raw = state
        .database
        .get_setting(STATE_SETTING)?
        .unwrap_or_else(|| json!({}));
    Ok(VisualizerState {
        installed: string_set(raw.get("installed")),
        disabled: string_set(raw.get("disabled")),
        uninstalled: string_set(raw.get("uninstalled")),
    })
}

fn save_state(state: &AppState, value: &VisualizerState) -> ApiResult<()> {
    state.database.set_setting(
        STATE_SETTING,
        &json!({
            "installed": value.installed,
            "disabled": value.disabled,
            "uninstalled": value.uninstalled
        }),
    )?;
    Ok(())
}

fn string_set(value: Option<&Value>) -> BTreeSet<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn visualizer_root(state: &AppState) -> ApiResult<PathBuf> {
    let root = state.paths.data.join("visualizers");
    fs::create_dir_all(&root)
        .map_err(|error| ApiError::internal(format!("无法创建可视化器目录：{error}")))?;
    root.canonicalize()
        .map_err(|error| ApiError::internal(format!("无法解析可视化器目录：{error}")))
}

fn install_archive(state: &AppState, bytes: &[u8]) -> ApiResult<String> {
    let root = visualizer_root(state)?;
    let staging = root.join(format!(".import-{}", Uuid::new_v4().simple()));
    fs::create_dir(&staging)
        .map_err(|error| ApiError::internal(format!("无法创建导入暂存目录：{error}")))?;

    let result = (|| {
        extract_archive(bytes, &staging)?;
        let package_root = locate_package_root(&staging)?;
        let manifest_path = package_root.join("visualizer.json");
        let metadata = fs::metadata(&manifest_path)
            .map_err(|error| ApiError::bad_request(format!("无法读取 visualizer.json：{error}")))?;
        if metadata.len() > MAX_MANIFEST_BYTES {
            return Err(ApiError::bad_request("visualizer.json 不能超过 128 KB"));
        }
        let raw = fs::read_to_string(&manifest_path)
            .map_err(|error| ApiError::bad_request(format!("无法读取 visualizer.json：{error}")))?;
        let mut manifest: Value = serde_json::from_str(&raw)
            .map_err(|error| ApiError::bad_request(format!("visualizer.json 无效：{error}")))?;
        validate_imported_manifest(&mut manifest, &package_root)?;
        let id = manifest["id"].as_str().unwrap_or("").to_string();
        let catalog = load_catalog(state)?;
        if catalog
            .iter()
            .any(|item| item.manifest["id"].as_str() == Some(id.as_str()))
        {
            return Err(ApiError::conflict(format!(
                "可视化器标识已被宿主保留：{id}"
            )));
        }
        let destination = root.join(&id);
        if !destination.starts_with(&root) || destination.exists() {
            return Err(ApiError::conflict(format!("可视化器已安装：{id}")));
        }
        fs::rename(&package_root, &destination)
            .map_err(|error| ApiError::internal(format!("无法安装可视化器到便携目录：{error}")))?;
        let mut saved = load_state(state)?;
        saved.disabled.remove(&id);
        saved.installed.remove(&id);
        save_state(state, &saved)?;
        Ok(id)
    })();

    let _ = fs::remove_dir_all(&staging);
    result
}

fn extract_archive(bytes: &[u8], destination: &FilePath) -> ApiResult<()> {
    let mut archive = ZipArchive::new(Cursor::new(bytes))
        .map_err(|error| ApiError::bad_request(format!("安装包不是有效的 ZIP：{error}")))?;
    let mut files = 0;
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|error| ApiError::bad_request(format!("无法读取 ZIP 条目：{error}")))?;
        if !entry.is_dir() {
            files += 1;
        }
    }
    if files > MAX_ENTRIES {
        return Err(ApiError::bad_request("安装包包含过多文件"));
    }

    let root = destination
        .canonicalize()
        .map_err(|error| ApiError::internal(format!("无法解析导入目录：{error}")))?;
    let mut total = 0_u64;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| ApiError::bad_request(format!("无法读取 ZIP 条目：{error}")))?;
        if entry.is_dir() {
            continue;
        }
        let raw = entry.name().replace('\\', "/");
        let relative = safe_relative_path(&raw)?;
        if raw.starts_with("__MACOSX/")
            || relative
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with('.'))
        {
            continue;
        }
        let suffix = relative
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !matches!(
            suffix.as_str(),
            "json"
                | "html"
                | "js"
                | "mjs"
                | "css"
                | "svg"
                | "png"
                | "jpg"
                | "jpeg"
                | "webp"
                | "woff2"
                | "wasm"
        ) {
            return Err(ApiError::bad_request(format!(
                "安装包包含不支持的文件类型：{raw}"
            )));
        }
        let size = entry.size();
        let compressed = entry.compressed_size();
        if size > MAX_ENTRY_BYTES {
            return Err(ApiError::bad_request(format!("安装包条目过大：{raw}")));
        }
        if compressed > 0 && size / compressed > MAX_RATIO {
            return Err(ApiError::bad_request(format!(
                "安装包条目压缩比异常：{raw}"
            )));
        }
        total = total.saturating_add(size);
        if total > MAX_ARCHIVE_BYTES {
            return Err(ApiError::bad_request("安装包解压后不能超过 25 MB"));
        }
        let target = root.join(relative);
        if !target.starts_with(&root) {
            return Err(ApiError::bad_request(format!("安装包路径非法：{raw}")));
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| ApiError::internal(format!("无法创建安装子目录：{error}")))?;
        }
        let mut output = fs::File::create(&target)
            .map_err(|error| ApiError::internal(format!("无法创建安装文件：{error}")))?;
        let mut limited = entry.by_ref().take(MAX_ENTRY_BYTES + 1);
        let copied = std::io::copy(&mut limited, &mut output)
            .map_err(|error| ApiError::bad_request(format!("无法解压安装包：{error}")))?;
        output
            .flush()
            .map_err(|error| ApiError::internal(format!("无法写入安装文件：{error}")))?;
        if copied > MAX_ENTRY_BYTES {
            return Err(ApiError::bad_request(format!("安装包条目过大：{raw}")));
        }
    }
    Ok(())
}

fn locate_package_root(staging: &FilePath) -> ApiResult<PathBuf> {
    if staging.join("visualizer.json").is_file() {
        return Ok(staging.to_path_buf());
    }
    let directories = fs::read_dir(staging)
        .map_err(|error| ApiError::bad_request(format!("无法读取安装包目录：{error}")))?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false))
        .collect::<Vec<_>>();
    if directories.len() == 1 && directories[0].path().join("visualizer.json").is_file() {
        return Ok(directories[0].path());
    }
    Err(ApiError::bad_request(
        "安装包根目录必须包含 visualizer.json",
    ))
}

fn validate_imported_manifest(manifest: &mut Value, package_root: &FilePath) -> ApiResult<()> {
    let object = manifest
        .as_object_mut()
        .ok_or_else(|| ApiError::bad_request("visualizer.json 必须是 JSON 对象"))?;
    let id = object
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if !valid_id(&id) {
        return Err(ApiError::bad_request("可视化器标识格式无效"));
    }
    object.insert("id".to_string(), json!(id));
    object.insert(
        "version".to_string(),
        json!(object
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or("1.0.0")),
    );
    object.insert(
        "author".to_string(),
        json!(object
            .get("author")
            .and_then(Value::as_str)
            .unwrap_or("DeepTutor")),
    );
    object.insert(
        "subjects".to_string(),
        array_or_empty(object.get("subjects")),
    );
    object.insert("intents".to_string(), array_or_empty(object.get("intents")));
    object.insert(
        "payload_schema".to_string(),
        object
            .get("payload_schema")
            .cloned()
            .unwrap_or_else(|| json!({})),
    );
    object.insert(
        "payload_kind".to_string(),
        json!(
            if object.get("payload_kind").and_then(Value::as_str) == Some("json") {
                "json"
            } else {
                "text"
            }
        ),
    );
    object.insert(
        "language_tag".to_string(),
        json!(object
            .get("language_tag")
            .and_then(Value::as_str)
            .unwrap_or("text")),
    );
    object.insert(
        "agentic".to_string(),
        json!(object
            .get("agentic")
            .and_then(Value::as_bool)
            .unwrap_or(true)),
    );
    object.insert("core".to_string(), json!(false));
    object.insert("default_installed".to_string(), json!(false));
    object.insert(
        "priority".to_string(),
        json!(object
            .get("priority")
            .and_then(Value::as_i64)
            .unwrap_or(100)),
    );
    object.insert(
        "native_renderer".to_string(),
        json!(object
            .get("native_renderer")
            .and_then(Value::as_str)
            .unwrap_or("")),
    );
    object.insert("render_target".to_string(), json!("iframe"));

    let display_name = object
        .get("display_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    if display_name.trim().is_empty() || display_name.chars().count() > 80 {
        return Err(ApiError::bad_request(
            "display_name 必须在 1 到 80 字符之间",
        ));
    }
    let description = object
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("");
    if description.trim().is_empty() || description.chars().count() > 1000 {
        return Err(ApiError::bad_request(
            "description 必须在 1 到 1000 字符之间",
        ));
    }
    let payload_format = object
        .get("payload_format")
        .and_then(Value::as_str)
        .unwrap_or("");
    if payload_format.trim().is_empty() || payload_format.chars().count() > 160 {
        return Err(ApiError::bad_request(
            "payload_format 必须在 1 到 160 字符之间",
        ));
    }
    let prompt = object.get("prompt").and_then(Value::as_str).unwrap_or("");
    if prompt.trim().is_empty() || prompt.chars().count() > 12_000 {
        return Err(ApiError::bad_request("prompt 必须在 1 到 12000 字符之间"));
    }
    let entry = object
        .get("renderer_entry")
        .and_then(Value::as_str)
        .unwrap_or("")
        .replace('\\', "/");
    let relative = safe_relative_path(&entry)?;
    if relative.extension().and_then(|value| value.to_str()) != Some("html") {
        return Err(ApiError::bad_request("renderer_entry 必须指向 HTML 文件"));
    }
    let entry_path = package_root
        .join(&relative)
        .canonicalize()
        .map_err(|_| ApiError::bad_request("renderer_entry 指向的文件不存在"))?;
    let package = package_root
        .canonicalize()
        .map_err(|error| ApiError::internal(format!("无法解析安装包目录：{error}")))?;
    if !entry_path.starts_with(&package) || !entry_path.is_file() {
        return Err(ApiError::bad_request("renderer_entry 指向的文件不存在"));
    }
    object.insert("renderer_entry".to_string(), json!(entry));
    Ok(())
}

fn array_or_empty(value: Option<&Value>) -> Value {
    let array = value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .take(32)
                .map(|item| json!(item))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Value::Array(array)
}

fn safe_relative_path(value: &str) -> ApiResult<PathBuf> {
    let normalized = value.trim().replace('\\', "/");
    if normalized.is_empty()
        || normalized.starts_with('/')
        || normalized.contains('\0')
        || normalized.contains('%')
    {
        return Err(ApiError::bad_request("文件路径非法"));
    }
    let mut path = PathBuf::new();
    for segment in normalized.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." || segment.contains(':') {
            return Err(ApiError::bad_request("文件路径包含非法片段"));
        }
        path.push(segment);
    }
    Ok(path)
}

fn normalize_id(value: &str) -> ApiResult<String> {
    let id = value.trim().to_ascii_lowercase();
    if !valid_id(&id) {
        return Err(ApiError::bad_request("可视化器标识格式无效"));
    }
    Ok(id)
}

fn valid_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    (2..=64).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'.' | b'-')
        })
}

fn builtins() -> Vec<CatalogItem> {
    vec![
        builtin(
            json!({
                "id": "svg", "version": "1.0.0", "display_name": "SVG",
                "description": "用于精确解释插图与自定义图表。",
                "author": "DeepTutor", "subjects": ["general"],
                "intents": ["explain", "illustrate", "compare"],
                "render_target": "native", "native_renderer": "svg",
                "renderer_entry": "", "payload_format": "image/svg+xml",
                "payload_kind": "text", "payload_schema": {},
                "language_tag": "svg", "prompt": "生成可直接渲染的 SVG。",
                "agentic": true, "core": true, "default_installed": true, "priority": 10
            }),
            "core",
            true,
            true,
        ),
        builtin(
            json!({
                "id": "mermaid", "version": "1.0.0", "display_name": "Mermaid",
                "description": "用于流程图、时序图、状态图、类图和实体关系图。",
                "author": "DeepTutor", "subjects": ["general", "computer_science"],
                "intents": ["structure", "flow", "sequence"],
                "render_target": "native", "native_renderer": "mermaid",
                "renderer_entry": "", "payload_format": "text/vnd.mermaid",
                "payload_kind": "text", "payload_schema": {},
                "language_tag": "mermaid", "prompt": "生成有效的 Mermaid DSL。",
                "agentic": true, "core": true, "default_installed": true, "priority": 20
            }),
            "core",
            true,
            true,
        ),
        builtin(
            json!({
                "id": "mindmap", "version": "1.0.0", "display_name": "思维导图",
                "description": "使用 Mermaid 绘制的层级主题与关系。",
                "author": "DeepTutor", "subjects": ["general"],
                "intents": ["structure", "outline", "explain"],
                "render_target": "native", "native_renderer": "mermaid",
                "renderer_entry": "", "payload_format": "text/vnd.mermaid",
                "payload_kind": "text", "payload_schema": {},
                "language_tag": "mermaid", "prompt": "生成有效的 Mermaid mindmap DSL。",
                "agentic": true, "core": true, "default_installed": true, "priority": 25
            }),
            "core",
            true,
            true,
        ),
        builtin(
            json!({
                "id": "chartjs", "version": "1.0.0", "display_name": "Chart.js",
                "description": "根据严格 JSON 渲染标准定量图表。",
                "author": "DeepTutor", "subjects": ["statistics", "general"],
                "intents": ["compare", "trend", "distribution"],
                "render_target": "native", "native_renderer": "chartjs",
                "renderer_entry": "", "payload_format": "application/vnd.chartjs+json",
                "payload_kind": "json", "payload_schema": {},
                "language_tag": "json", "prompt": "生成严格 JSON 格式的 Chart.js 配置。",
                "agentic": true, "core": true, "default_installed": true, "priority": 30
            }),
            "core",
            true,
            true,
        ),
        builtin(
            json!({
                "id": "html", "version": "1.0.0", "display_name": "交互式 HTML",
                "description": "自包含的交互课程、分步讲解和模拟。",
                "author": "DeepTutor", "subjects": ["general"],
                "intents": ["interact", "simulate", "practice"],
                "render_target": "native", "native_renderer": "html",
                "renderer_entry": "", "payload_format": "text/html",
                "payload_kind": "text", "payload_schema": {},
                "language_tag": "html", "prompt": "生成完整的单文件 HTML。",
                "agentic": true, "core": true, "default_installed": true, "priority": 40
            }),
            "core",
            true,
            true,
        ),
        builtin(
            json!({
                "id": "manim_video", "version": "1.0.0", "display_name": "Manim 动画",
                "description": "由数学动画流水线渲染的视频。",
                "author": "DeepTutor", "subjects": ["mathematics"],
                "intents": ["animate", "derive"],
                "render_target": "artifact", "native_renderer": "math_animator",
                "renderer_entry": "", "payload_format": "video/mp4",
                "payload_kind": "text", "payload_schema": {},
                "language_tag": "text", "prompt": "由专用 Manim 流水线渲染。",
                "agentic": false, "core": true, "default_installed": true, "priority": 200
            }),
            "core",
            true,
            true,
        ),
        builtin(
            json!({
                "id": "manim_image", "version": "1.0.0", "display_name": "Manim 分镜",
                "description": "由数学动画流水线渲染的分镜图片。",
                "author": "DeepTutor", "subjects": ["mathematics"],
                "intents": ["storyboard", "derive"],
                "render_target": "artifact", "native_renderer": "math_animator",
                "renderer_entry": "", "payload_format": "image/png",
                "payload_kind": "text", "payload_schema": {},
                "language_tag": "text", "prompt": "由专用 Manim 流水线渲染。",
                "agentic": false, "core": true, "default_installed": true, "priority": 210
            }),
            "core",
            true,
            true,
        ),
        builtin(
            json!({
                "id": "geogebra", "version": "1.0.0", "display_name": "GeoGebra",
                "description": "交互式几何、函数、微积分、向量和坐标模型。",
                "author": "DeepTutor", "subjects": ["mathematics", "physics"],
                "intents": ["construct", "explore", "prove", "graph"],
                "render_target": "native", "native_renderer": "geogebra",
                "renderer_entry": "", "payload_format": "application/vnd.geogebra.commands+json",
                "payload_kind": "json", "payload_schema": {},
                "language_tag": "json", "prompt": "生成包含 commands 和 view 的严格 JSON。",
                "agentic": true, "core": false, "default_installed": false, "priority": 5
            }),
            "bundled",
            false,
            false,
        ),
    ]
}

fn builtin(
    manifest: Value,
    origin: &'static str,
    core: bool,
    default_installed: bool,
) -> CatalogItem {
    CatalogItem {
        manifest,
        origin,
        core,
        default_installed,
        root: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 可视化器标识只接受安全字符() {
        assert!(valid_id("demo.chart"));
        assert!(valid_id("a1"));
        assert!(!valid_id("A"));
        assert!(!valid_id("../escape"));
        assert!(!valid_id("x"));
    }

    #[test]
    fn 相对路径拒绝目录穿越() {
        assert!(safe_relative_path("assets/main.js").is_ok());
        assert!(safe_relative_path("../secret").is_err());
        assert!(safe_relative_path("C:/secret").is_err());
        assert!(safe_relative_path("assets\\..\\secret").is_err());
    }
}
