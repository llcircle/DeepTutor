use super::error::{ApiError, ApiResult};
use super::knowledge::{kb_raw_root, normalized_kb_name, start_index_task};
use super::{AppState, ScopedState};
use axum::extract::{Path, Query};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path as FilePath, PathBuf};
use url::Url;
use uuid::Uuid;
use walkdir::WalkDir;

const LINKED_DOMAIN: &str = "kb_linked_folder";
const LINKED_MANIFEST_DOMAIN: &str = "kb_linked_manifest";
const GITHUB_DOMAIN: &str = "kb_github_source";
const GITHUB_MANIFEST_DOMAIN: &str = "kb_github_manifest";
const WEB_DOMAIN: &str = "kb_web_source";
const WEB_MANIFEST_DOMAIN: &str = "kb_web_manifest";
const WEB_PAIRING_DOMAIN: &str = "kb_web_pairing";
const WEB_JOB_DOMAIN: &str = "kb_web_sync_job";
const MAX_SOURCE_FILE_BYTES: u64 = 20 * 1024 * 1024;
const MAX_WEB_PAGES: usize = 200;
const MAX_WEB_BYTES: usize = 3 * 1024 * 1024;

#[derive(Debug, Deserialize)]
struct FolderPayload {
    folder_path: String,
}

#[derive(Debug, Deserialize)]
struct AddGithubPayload {
    repo: String,
    #[serde(default = "default_branch")]
    branch: String,
    #[serde(default)]
    path: String,
    #[serde(default = "default_glob")]
    glob: String,
}

#[derive(Debug, Deserialize)]
struct AddWebPayload {
    url: String,
    #[serde(default = "default_depth")]
    max_depth: usize,
    #[serde(default = "default_pages")]
    max_pages: usize,
}

#[derive(Debug, Deserialize)]
struct SchedulePayload {
    auto_sync_enabled: bool,
    #[serde(default = "default_interval")]
    sync_interval_hours: u32,
}

#[derive(Debug, Deserialize)]
struct ConnectObsidianPayload {
    name: String,
    vault_path: String,
}

#[derive(Debug, Deserialize)]
struct ConnectMarginNotePayload {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    db_path: String,
}

#[derive(Debug, Deserialize)]
struct ProbeFolderPayload {
    folder_path: String,
    #[serde(default = "default_provider")]
    rag_provider: String,
}

#[derive(Debug, Deserialize)]
struct ConnectFolderPayload {
    name: String,
    folder_path: String,
    #[serde(default = "default_provider")]
    rag_provider: String,
}

#[derive(Debug, Deserialize)]
struct ProbeServerPayload {
    server_url: String,
    #[serde(default)]
    api_key: String,
    #[serde(default)]
    use_saved_api_key: bool,
    #[serde(default)]
    knowledge_base_id: String,
}

#[derive(Debug, Deserialize)]
struct ConnectServerPayload {
    name: String,
    server_url: String,
    #[serde(default)]
    api_key: String,
    #[serde(default)]
    search_mode: String,
    #[serde(default)]
    knowledge_base_id: String,
}

#[derive(Debug, Deserialize)]
struct ImaListPayload {
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    api_key: String,
    #[serde(default)]
    cursor: String,
    #[serde(default = "default_ima_limit")]
    limit: usize,
}

#[derive(Debug, Deserialize)]
struct ImaProbePayload {
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    api_key: String,
    knowledge_base_id: String,
}

#[derive(Debug, Deserialize)]
struct ImaConnectPayload {
    name: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    api_key: String,
    knowledge_base_id: String,
}

#[derive(Debug, Deserialize)]
struct KiwixProbePayload {
    server_url: String,
    zim_name: String,
}

#[derive(Debug, Deserialize)]
struct KiwixConnectPayload {
    name: String,
    server_url: String,
    zim_name: String,
}

#[derive(Debug, Deserialize)]
struct KiwixCatalogQuery {
    server_url: String,
    #[serde(default)]
    q: String,
}

#[derive(Debug, Deserialize)]
struct KiwixArticlesQuery {
    kb_ref: String,
    q: String,
}

fn default_branch() -> String {
    "main".to_string()
}
fn default_glob() -> String {
    "*.md".to_string()
}
fn default_depth() -> usize {
    3
}
fn default_pages() -> usize {
    200
}
fn default_interval() -> u32 {
    24
}
fn default_provider() -> String {
    "native".to_string()
}
fn default_ima_limit() -> usize {
    20
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/knowledge-bases/connect-obsidian", post(connect_obsidian))
        .route(
            "/knowledge-bases/connect-marginnote4",
            post(connect_marginnote4),
        )
        .route("/knowledge-bases/probe-folder", post(probe_folder))
        .route("/knowledge-bases/connect-folder", post(connect_folder))
        .route(
            "/knowledge-bases/probe-lightrag-server",
            post(probe_lightrag_server),
        )
        .route(
            "/knowledge-bases/connect-lightrag-server",
            post(connect_lightrag_server),
        )
        .route("/knowledge-bases/probe-weknora", post(probe_weknora))
        .route("/knowledge-bases/connect-weknora", post(connect_weknora))
        .route("/knowledge-bases/list-ima", post(list_ima))
        .route("/knowledge-bases/probe-ima", post(probe_ima))
        .route("/knowledge-bases/connect-ima", post(connect_ima))
        .route("/knowledge-bases/kiwix-catalog", get(kiwix_catalog))
        .route("/knowledge-bases/probe-kiwix", post(probe_kiwix))
        .route("/knowledge-bases/connect-kiwix", post(connect_kiwix))
        .route("/knowledge-bases/kiwix-articles", get(kiwix_articles))
        .route(
            "/knowledge-bases/{kb_name}/linked-folders",
            get(list_linked_folders),
        )
        .route("/knowledge-bases/{kb_name}/link-folder", post(link_folder))
        .route(
            "/knowledge-bases/{kb_name}/linked-folders/{folder_id}",
            delete(unlink_folder),
        )
        .route(
            "/knowledge-bases/{kb_name}/sync-folder/{folder_id}",
            post(sync_folder),
        )
        .route(
            "/knowledge-bases/{kb_name}/github-source",
            post(add_github_source),
        )
        .route(
            "/knowledge-bases/{kb_name}/github-sources",
            get(list_github_sources),
        )
        .route(
            "/knowledge-bases/{kb_name}/github-source/{source_id}",
            delete(remove_github_source),
        )
        .route(
            "/knowledge-bases/{kb_name}/sync-github",
            post(sync_github_sources),
        )
        .route(
            "/knowledge-bases/{kb_name}/web-source",
            post(add_web_source),
        )
        .route(
            "/knowledge-bases/{kb_name}/web-sources",
            get(list_web_sources),
        )
        .route(
            "/knowledge-bases/{kb_name}/web-source/{source_id}",
            delete(remove_web_source),
        )
        .route(
            "/knowledge-bases/{kb_name}/web-source-sync",
            get(web_sync_jobs),
        )
        .route(
            "/knowledge-bases/{kb_name}/web-source/{source_id}/pairings",
            get(web_pairings),
        )
        .route(
            "/knowledge-bases/{kb_name}/web-source/{source_id}/schedule",
            put(update_web_schedule),
        )
        .route(
            "/knowledge-bases/{kb_name}/web-source/{source_id}/cancel",
            post(cancel_web_sync),
        )
        .route(
            "/knowledge-bases/{kb_name}/web-source/{source_id}/retry",
            post(retry_web_sync),
        )
        .route(
            "/knowledge-bases/{kb_name}/sync-web",
            post(sync_web_sources),
        )
}

async fn list_linked_folders(
    ScopedState(state): ScopedState,
    Path(kb_name): Path<String>,
) -> ApiResult<Json<Value>> {
    let name = normalized_kb_name(&kb_name)?;
    ensure_kb(&state, &name)?;
    Ok(Json(Value::Array(list_for_kb(
        &state,
        LINKED_DOMAIN,
        &name,
    )?)))
}

async fn link_folder(
    ScopedState(state): ScopedState,
    Path(kb_name): Path<String>,
    Json(body): Json<FolderPayload>,
) -> ApiResult<Json<Value>> {
    let name = normalized_kb_name(&kb_name)?;
    let raw_root = kb_raw_root(&state, &name)?;
    let source = existing_directory(&body.folder_path, "链接文件夹")?;
    let source_key = source.to_string_lossy().to_string();
    if list_for_kb(&state, LINKED_DOMAIN, &name)?
        .iter()
        .any(|item| item.get("path").and_then(Value::as_str) == Some(source_key.as_str()))
    {
        return Err(ApiError::conflict("该文件夹已经链接到此知识库。"));
    }
    let id = format!("folder-{}", Uuid::new_v4());
    let destination = raw_root.join("linked-folders").join(&id);
    let manifest = copy_directory(&source, &destination)?;
    let count = manifest.len();
    let value = json!({
        "id": id,
        "kb_name": name,
        "path": source_key,
        "added_at": now_iso(),
        "file_count": count,
        "last_sync": Value::Null,
        "managed_files": manifest
    });
    state.database.put_resource(LINKED_DOMAIN, &id, &value)?;
    Ok(Json(value))
}

async fn unlink_folder(
    ScopedState(state): ScopedState,
    Path((kb_name, folder_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let name = normalized_kb_name(&kb_name)?;
    ensure_kb(&state, &name)?;
    let source = state
        .database
        .get_resource(LINKED_DOMAIN, &folder_id)?
        .filter(|item| item.get("kb_name").and_then(Value::as_str) == Some(name.as_str()))
        .ok_or_else(|| ApiError::not_found("链接文件夹不存在。"))?;
    let raw_root = kb_raw_root(&state, &name)?;
    let destination = raw_root.join("linked-folders").join(&folder_id);
    if destination.starts_with(&state.paths.data) && destination.exists() {
        std::fs::remove_dir_all(&destination)
            .map_err(|error| ApiError::internal(format!("无法删除链接文件夹副本：{error}")))?;
    }
    state.database.delete_resource(LINKED_DOMAIN, &folder_id)?;
    state
        .database
        .delete_resource(LINKED_MANIFEST_DOMAIN, &folder_id)?;
    let _ = source;
    Ok(Json(
        json!({"message": "已解除链接", "folder_id": folder_id}),
    ))
}

async fn sync_folder(
    ScopedState(state): ScopedState,
    Path((kb_name, folder_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let name = normalized_kb_name(&kb_name)?;
    let raw_root = kb_raw_root(&state, &name)?;
    let mut folder = state
        .database
        .get_resource(LINKED_DOMAIN, &folder_id)?
        .filter(|item| item.get("kb_name").and_then(Value::as_str) == Some(name.as_str()))
        .ok_or_else(|| ApiError::not_found("链接文件夹不存在。"))?;
    let source = existing_directory(
        folder
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        "链接文件夹",
    )?;
    let previous = state
        .database
        .get_resource(LINKED_MANIFEST_DOMAIN, &folder_id)?
        .and_then(|value| value.get("files").cloned())
        .unwrap_or_else(|| json!({}));
    let previous_map = string_map(&previous);
    let destination = raw_root.join("linked-folders").join(&folder_id);
    let current = copy_directory(&source, &destination)?;
    let mut added = 0usize;
    let mut modified = 0usize;
    for (relative, digest) in &current {
        match previous_map.get(relative) {
            None => added += 1,
            Some(old) if old != digest => modified += 1,
            _ => {}
        }
    }
    let removed = previous_map
        .keys()
        .filter(|key| !current.contains_key(*key))
        .count();
    for relative in previous_map.keys() {
        if !current.contains_key(relative) {
            let path = destination.join(relative);
            if path.starts_with(&destination) {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    state.database.put_resource(
        LINKED_MANIFEST_DOMAIN,
        &folder_id,
        &json!({"folder_id": folder_id, "files": current}),
    )?;
    folder["file_count"] = Value::from(current.len());
    folder["last_sync"] = Value::String(now_iso());
    folder["managed_files"] = Value::Object(
        current
            .iter()
            .map(|(key, value)| (key.clone(), Value::String(value.clone())))
            .collect(),
    );
    state
        .database
        .put_resource(LINKED_DOMAIN, &folder_id, &folder)?;
    if added + modified + removed > 0 {
        let task_id = format!("kb-task-{}", Uuid::new_v4());
        start_index_task(&state, &name, &task_id)?;
        Ok(Json(json!({
            "message": format!("已同步 {} 个新增、{} 个修改、{} 个删除文件。", added, modified, removed),
            "folder_path": source.display().to_string(),
            "files": current.keys().cloned().collect::<Vec<_>>(),
            "new_files": added,
            "modified_files": modified,
            "file_count": current.len(),
            "task_id": task_id
        })))
    } else {
        Ok(Json(json!({
            "message": "没有需要同步的文件。",
            "folder_path": source.display().to_string(),
            "files": [],
            "new_files": 0,
            "modified_files": 0,
            "file_count": current.len(),
            "task_id": Value::Null
        })))
    }
}

async fn add_github_source(
    ScopedState(state): ScopedState,
    Path(kb_name): Path<String>,
    Json(body): Json<AddGithubPayload>,
) -> ApiResult<Json<Value>> {
    let name = normalized_kb_name(&kb_name)?;
    ensure_kb(&state, &name)?;
    let (owner, repo) = parse_repository(&body.repo)?;
    let id = format!("github-{}", Uuid::new_v4());
    let value = json!({
        "id": id,
        "kb_name": name,
        "repo": format!("{owner}/{repo}"),
        "branch": if body.branch.trim().is_empty() { "main" } else { body.branch.trim() },
        "path": body.path.trim_matches('/'),
        "glob": if body.glob.trim().is_empty() { "*.md" } else { body.glob.trim() },
        "enabled": true,
        "last_synced_sha": "",
        "last_synced_at": "",
        "last_sync_status": "pending",
        "last_sync_error": Value::Null,
        "files_synced": 0,
        "added_at": now_iso()
    });
    state.database.put_resource(GITHUB_DOMAIN, &id, &value)?;
    Ok(Json(value))
}

async fn list_github_sources(
    ScopedState(state): ScopedState,
    Path(kb_name): Path<String>,
) -> ApiResult<Json<Value>> {
    let name = normalized_kb_name(&kb_name)?;
    ensure_kb(&state, &name)?;
    Ok(Json(Value::Array(list_for_kb(
        &state,
        GITHUB_DOMAIN,
        &name,
    )?)))
}

async fn remove_github_source(
    ScopedState(state): ScopedState,
    Path((kb_name, source_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let name = normalized_kb_name(&kb_name)?;
    ensure_kb(&state, &name)?;
    state
        .database
        .get_resource(GITHUB_DOMAIN, &source_id)?
        .filter(|item| item.get("kb_name").and_then(Value::as_str) == Some(name.as_str()))
        .ok_or_else(|| ApiError::not_found("GitHub 来源不存在。"))?;
    let raw_root = kb_raw_root(&state, &name)?;
    let directory = raw_root.join("github").join(&source_id);
    if directory.starts_with(&state.paths.data) && directory.exists() {
        std::fs::remove_dir_all(&directory)
            .map_err(|error| ApiError::internal(format!("无法删除 GitHub 来源副本：{error}")))?;
    }
    state.database.delete_resource(GITHUB_DOMAIN, &source_id)?;
    state
        .database
        .delete_resource(GITHUB_MANIFEST_DOMAIN, &source_id)?;
    Ok(Json(json!({"message": "已删除", "source_id": source_id})))
}

async fn sync_github_sources(
    ScopedState(state): ScopedState,
    Path(kb_name): Path<String>,
) -> ApiResult<Json<Value>> {
    let name = normalized_kb_name(&kb_name)?;
    ensure_kb(&state, &name)?;
    let sources = list_for_kb(&state, GITHUB_DOMAIN, &name)?;
    if sources.is_empty() {
        return Ok(Json(json!({"message": "没有 GitHub 来源", "results": []})));
    }
    let mut results = Vec::new();
    let mut changed = false;
    for source in sources {
        if source.get("enabled").and_then(Value::as_bool) == Some(false) {
            continue;
        }
        let result = sync_one_github(&state, &name, source).await;
        changed |= result
            .get("files_added")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            > 0
            || result
                .get("files_updated")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                > 0
            || result
                .get("files_removed")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                > 0;
        results.push(result);
    }
    if changed {
        let task_id = format!("kb-task-{}", Uuid::new_v4());
        start_index_task(&state, &name, &task_id)?;
    }
    Ok(Json(json!({
        "message": format!("已同步 {} 个来源", results.len()),
        "results": results
    })))
}

async fn add_web_source(
    ScopedState(state): ScopedState,
    Path(kb_name): Path<String>,
    Json(body): Json<AddWebPayload>,
) -> ApiResult<Json<Value>> {
    let name = normalized_kb_name(&kb_name)?;
    ensure_kb(&state, &name)?;
    let url = normalize_http_url(&body.url, "网页来源")?;
    let id = format!("web-{}", Uuid::new_v4());
    let value = json!({
        "id": id,
        "kb_name": name,
        "url": url,
        "max_depth": body.max_depth.clamp(1, 5),
        "max_pages": body.max_pages.clamp(1, MAX_WEB_PAGES),
        "enabled": true,
        "auto_sync_enabled": true,
        "sync_interval_hours": 24,
        "page_count": 0,
        "last_synced_at": "",
        "last_sync_status": "pending",
        "last_sync_error": Value::Null,
        "added_at": now_iso(),
        "bilingual_pairings": []
    });
    state.database.put_resource(WEB_DOMAIN, &id, &value)?;
    ensure_web_job(&state, &name, &value)?;
    Ok(Json(value))
}

async fn list_web_sources(
    ScopedState(state): ScopedState,
    Path(kb_name): Path<String>,
) -> ApiResult<Json<Value>> {
    let name = normalized_kb_name(&kb_name)?;
    ensure_kb(&state, &name)?;
    Ok(Json(Value::Array(list_for_kb(&state, WEB_DOMAIN, &name)?)))
}

async fn remove_web_source(
    ScopedState(state): ScopedState,
    Path((kb_name, source_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let name = normalized_kb_name(&kb_name)?;
    ensure_kb(&state, &name)?;
    state
        .database
        .get_resource(WEB_DOMAIN, &source_id)?
        .filter(|item| item.get("kb_name").and_then(Value::as_str) == Some(name.as_str()))
        .ok_or_else(|| ApiError::not_found("网页来源不存在。"))?;
    let raw_root = kb_raw_root(&state, &name)?;
    let directory = raw_root.join("web").join(&source_id);
    if directory.starts_with(&state.paths.data) && directory.exists() {
        std::fs::remove_dir_all(&directory)
            .map_err(|error| ApiError::internal(format!("无法删除网页来源副本：{error}")))?;
    }
    state.database.delete_resource(WEB_DOMAIN, &source_id)?;
    state
        .database
        .delete_resource(WEB_MANIFEST_DOMAIN, &source_id)?;
    state.database.delete_resource(WEB_JOB_DOMAIN, &source_id)?;
    Ok(Json(json!({"message": "已删除", "source_id": source_id})))
}

async fn web_pairings(
    ScopedState(state): ScopedState,
    Path((kb_name, source_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let name = normalized_kb_name(&kb_name)?;
    ensure_kb(&state, &name)?;
    let source = state
        .database
        .get_resource(WEB_DOMAIN, &source_id)?
        .filter(|item| item.get("kb_name").and_then(Value::as_str) == Some(name.as_str()))
        .ok_or_else(|| ApiError::not_found("网页来源不存在。"))?;
    let repo_pairings = state
        .database
        .list_resources(WEB_PAIRING_DOMAIN)?
        .into_iter()
        .filter(|item| {
            item.get("kb_name").and_then(Value::as_str) == Some(name.as_str())
                && item.get("source_id").and_then(Value::as_str) == Some(source_id.as_str())
        })
        .collect::<Vec<_>>();
    if !repo_pairings.is_empty() {
        return Ok(Json(Value::Array(repo_pairings)));
    }
    Ok(Json(
        source
            .get("bilingual_pairings")
            .cloned()
            .unwrap_or_else(|| json!([])),
    ))
}

async fn update_web_schedule(
    ScopedState(state): ScopedState,
    Path((kb_name, source_id)): Path<(String, String)>,
    Json(body): Json<SchedulePayload>,
) -> ApiResult<Json<Value>> {
    let name = normalized_kb_name(&kb_name)?;
    ensure_kb(&state, &name)?;
    let mut source = state
        .database
        .get_resource(WEB_DOMAIN, &source_id)?
        .filter(|item| item.get("kb_name").and_then(Value::as_str) == Some(name.as_str()))
        .ok_or_else(|| ApiError::not_found("网页来源不存在。"))?;
    source["auto_sync_enabled"] = Value::Bool(body.auto_sync_enabled);
    source["sync_interval_hours"] = Value::from(body.sync_interval_hours.clamp(1, 168));
    state
        .database
        .put_resource(WEB_DOMAIN, &source_id, &source)?;
    if body.auto_sync_enabled {
        ensure_web_job(&state, &name, &source)?;
    } else {
        state.database.delete_resource(WEB_JOB_DOMAIN, &source_id)?;
    }
    Ok(Json(source))
}

async fn web_sync_jobs(
    ScopedState(state): ScopedState,
    Path(kb_name): Path<String>,
) -> ApiResult<Json<Value>> {
    let name = normalized_kb_name(&kb_name)?;
    ensure_kb(&state, &name)?;
    let sources = list_for_kb(&state, WEB_DOMAIN, &name)?;
    for source in &sources {
        if source
            .get("auto_sync_enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true)
        {
            ensure_web_job(&state, &name, source)?;
        }
    }
    let jobs = state
        .database
        .list_resources(WEB_JOB_DOMAIN)?
        .into_iter()
        .filter(|item| item.get("kb_name").and_then(Value::as_str) == Some(name.as_str()))
        .collect::<Vec<_>>();
    Ok(Json(Value::Array(jobs)))
}

async fn cancel_web_sync(
    ScopedState(state): ScopedState,
    Path((kb_name, source_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let name = normalized_kb_name(&kb_name)?;
    ensure_kb(&state, &name)?;
    let mut job = state
        .database
        .get_resource(WEB_JOB_DOMAIN, &source_id)?
        .filter(|item| item.get("kb_name").and_then(Value::as_str) == Some(name.as_str()))
        .ok_or_else(|| ApiError::conflict("同步任务不可取消。"))?;
    job["cancel_requested"] = Value::Bool(true);
    job["state"] = Value::String("cancelled".into());
    job["error"] = Value::Null;
    state
        .database
        .put_resource(WEB_JOB_DOMAIN, &source_id, &job)?;
    Ok(Json(
        json!({"message": "已请求取消", "source_id": source_id}),
    ))
}

async fn retry_web_sync(
    ScopedState(state): ScopedState,
    Path((kb_name, source_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let name = normalized_kb_name(&kb_name)?;
    ensure_kb(&state, &name)?;
    let source = state
        .database
        .get_resource(WEB_DOMAIN, &source_id)?
        .filter(|item| item.get("kb_name").and_then(Value::as_str) == Some(name.as_str()))
        .ok_or_else(|| ApiError::not_found("网页来源不存在。"))?;
    if !source
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true)
        || !source
            .get("auto_sync_enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true)
    {
        return Err(ApiError::conflict("请先启用来源和自动同步。"));
    }
    let mut job = state
        .database
        .get_resource(WEB_JOB_DOMAIN, &source_id)?
        .unwrap_or_else(|| json!({}));
    job["kb_name"] = Value::String(name);
    job["source_id"] = Value::String(source_id.clone());
    job["state"] = Value::String("queued".into());
    job["cancel_requested"] = Value::Bool(false);
    job["error"] = Value::Null;
    job["attempt"] = Value::from(job.get("attempt").and_then(Value::as_u64).unwrap_or(0) + 1);
    job["next_run_at"] = Value::from(crate::storage::now());
    state
        .database
        .put_resource(WEB_JOB_DOMAIN, &source_id, &job)?;
    Ok(Json(
        json!({"message": "已重新排入队列", "source_id": source_id}),
    ))
}

async fn sync_web_sources(
    ScopedState(state): ScopedState,
    Path(kb_name): Path<String>,
) -> ApiResult<Json<Value>> {
    let name = normalized_kb_name(&kb_name)?;
    ensure_kb(&state, &name)?;
    let sources = list_for_kb(&state, WEB_DOMAIN, &name)?;
    if sources.is_empty() {
        return Ok(Json(
            json!({"ok": true, "message": "没有网页来源", "results": []}),
        ));
    }
    let mut results = Vec::new();
    for mut source in sources {
        if !source
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true)
        {
            continue;
        }
        let source_id = source
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let result = sync_one_web(&state, &name, &mut source).await;
        state
            .database
            .put_resource(WEB_DOMAIN, &source_id, &source)?;
        results.push(result);
    }
    let ok = results
        .iter()
        .all(|result| result.get("ok").and_then(Value::as_bool) == Some(true));
    Ok(Json(json!({
        "ok": ok,
        "message": format!("已同步 {} 个网页来源", results.len()),
        "results": results
    })))
}

async fn connect_obsidian(
    ScopedState(state): ScopedState,
    Json(body): Json<ConnectObsidianPayload>,
) -> ApiResult<Json<Value>> {
    let name = normalized_kb_name(&body.name)?;
    let vault = existing_directory(&body.vault_path, "Obsidian 库")?;
    ensure_kb_absent(&state, &name)?;
    let record = create_kb_record(
        &state,
        &name,
        json!({
            "type": "obsidian",
            "rag_provider": "obsidian",
            "vault_path": vault.display().to_string(),
            "external_path": vault.display().to_string(),
            "read_only": true,
            "status": "ready",
            "description": "Obsidian 库实时读取"
        }),
    )?;
    Ok(Json(json!({
        "status": "connected",
        "name": name,
        "vault_path": record.get("vault_path").cloned().unwrap_or(Value::Null)
    })))
}

async fn connect_marginnote4(
    ScopedState(state): ScopedState,
    Json(body): Json<ConnectMarginNotePayload>,
) -> ApiResult<Json<Value>> {
    let name = normalized_kb_name(&body.name)?;
    ensure_kb_absent(&state, &name)?;
    let record = create_kb_record(
        &state,
        &name,
        json!({
            "type": "marginnote4",
            "rag_provider": "marginnote4",
            "db_path": body.db_path.trim(),
            "description": body.description,
            "read_only": true,
            "status": "ready"
        }),
    )?;
    let mut result = json!({"status": "connected", "name": name});
    if let Some(path) = record.get("db_path").and_then(Value::as_str) {
        if !path.is_empty() {
            result["db_path"] = Value::String(path.to_string());
        }
    }
    Ok(Json(result))
}

async fn probe_folder(Json(body): Json<ProbeFolderPayload>) -> Json<Value> {
    match probe_local_folder(&body.folder_path) {
        Ok((path, doc_count)) => Json(json!({
            "ok": doc_count > 0,
            "provider": body.rag_provider,
            "external_path": path.display().to_string(),
            "version": Value::Null,
            "doc_count": doc_count,
            "embedding": {
                "compatible": Value::Null,
                "index_model": Value::Null,
                "current_model": Value::Null
            },
            "warnings": if doc_count == 0 { vec![json!("文件夹中没有可索引文件")] } else { Vec::<Value>::new() },
            "error": if doc_count == 0 { Some("文件夹中没有可索引文件") } else { None }
        })),
        Err(error) => Json(json!({
            "ok": false,
            "provider": body.rag_provider,
            "external_path": body.folder_path,
            "version": Value::Null,
            "doc_count": Value::Null,
            "embedding": {
                "compatible": Value::Null,
                "index_model": Value::Null,
                "current_model": Value::Null
            },
            "warnings": [],
            "error": error.message
        })),
    }
}

async fn connect_folder(
    ScopedState(state): ScopedState,
    Json(body): Json<ConnectFolderPayload>,
) -> ApiResult<Json<Value>> {
    let name = normalized_kb_name(&body.name)?;
    let (path, count) = probe_local_folder(&body.folder_path)?;
    if count == 0 {
        return Err(ApiError::bad_request("文件夹中没有可索引文件。"));
    }
    ensure_kb_absent(&state, &name)?;
    create_kb_record(
        &state,
        &name,
        json!({
            "type": "linked",
            "rag_provider": body.rag_provider,
            "external_path": path.display().to_string(),
            "read_only": true,
            "status": "ready",
            "file_count": count
        }),
    )?;
    Ok(Json(json!({
        "status": "connected",
        "name": name,
        "external_path": path.display().to_string(),
        "rag_provider": body.rag_provider,
        "warnings": []
    })))
}

async fn probe_lightrag_server(
    ScopedState(state): ScopedState,
    Json(body): Json<ProbeServerPayload>,
) -> Json<Value> {
    let result = probe_lightrag(&state, &body).await;
    Json(result)
}

async fn connect_lightrag_server(
    ScopedState(state): ScopedState,
    Json(body): Json<ConnectServerPayload>,
) -> ApiResult<Json<Value>> {
    let name = normalized_kb_name(&body.name)?;
    let probe = ProbeServerPayload {
        server_url: body.server_url.clone(),
        api_key: body.api_key.clone(),
        use_saved_api_key: false,
        knowledge_base_id: String::new(),
    };
    let result = probe_lightrag(&state, &probe).await;
    if result.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(ApiError::bad_request(
            result
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("无法连接 LightRAG 服务。"),
        ));
    }
    ensure_kb_absent(&state, &name)?;
    let server_url = result
        .get("base_url")
        .and_then(Value::as_str)
        .unwrap_or(&body.server_url);
    create_kb_record(
        &state,
        &name,
        json!({
            "type": "lightrag-server",
            "rag_provider": "lightrag-server",
            "server_url": server_url,
            "search_mode": body.search_mode.trim().to_ascii_lowercase(),
            "read_only": true,
            "status": "ready"
        }),
    )?;
    Ok(Json(json!({
        "status": "connected",
        "name": name,
        "server_url": server_url,
        "rag_provider": "lightrag-server"
    })))
}

async fn probe_weknora(
    ScopedState(state): ScopedState,
    Json(body): Json<ProbeServerPayload>,
) -> Json<Value> {
    let result = probe_weknora_server(&state, &body).await;
    Json(result)
}

async fn connect_weknora(
    ScopedState(state): ScopedState,
    Json(body): Json<ConnectServerPayload>,
) -> ApiResult<Json<Value>> {
    let name = normalized_kb_name(&body.name)?;
    let probe = ProbeServerPayload {
        server_url: body.server_url.clone(),
        api_key: body.api_key.clone(),
        use_saved_api_key: false,
        knowledge_base_id: body.knowledge_base_id.clone(),
    };
    let result = probe_weknora_server(&state, &probe).await;
    if result.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(ApiError::bad_request(
            result
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("无法连接 WeKnora 知识库。"),
        ));
    }
    ensure_kb_absent(&state, &name)?;
    let server_url = result
        .get("base_url")
        .and_then(Value::as_str)
        .unwrap_or(&body.server_url);
    create_kb_record(
        &state,
        &name,
        json!({
            "type": "weknora",
            "rag_provider": "weknora",
            "server_url": server_url,
            "knowledge_base_id": body.knowledge_base_id,
            "read_only": true,
            "status": "ready"
        }),
    )?;
    Ok(Json(json!({
        "status": "connected",
        "name": name,
        "server_url": server_url,
        "knowledge_base_id": body.knowledge_base_id,
        "rag_provider": "weknora"
    })))
}

async fn list_ima(
    ScopedState(state): ScopedState,
    Json(body): Json<ImaListPayload>,
) -> ApiResult<Json<Value>> {
    let credentials = resolve_ima_credentials(&state, &body.client_id, &body.api_key)?;
    let limit = body.limit.clamp(1, 50);
    let payload = ima_request(
        &state,
        "search_knowledge_base",
        &credentials,
        json!({"query": "", "cursor": body.cursor.trim(), "limit": limit}),
    )
    .await?;
    let items = payload
        .get("info_list")
        .or_else(|| payload.get("data"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let knowledge_bases = items
        .iter()
        .filter_map(|item| {
            let id = item
                .get("id")
                .or_else(|| item.get("knowledge_base_id"))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())?;
            Some(json!({
                "id": id,
                "name": item.get("name").and_then(Value::as_str).unwrap_or(id),
                "description": item.get("description").cloned().unwrap_or(Value::Null)
            }))
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({
        "knowledge_bases": knowledge_bases,
        "next_cursor": payload.get("next_cursor").cloned().unwrap_or_else(|| json!("")),
        "is_end": payload.get("is_end").cloned().unwrap_or(Value::Bool(true))
    })))
}

async fn probe_ima(
    ScopedState(state): ScopedState,
    Json(body): Json<ImaProbePayload>,
) -> Json<Value> {
    let result = probe_ima_knowledge_base(&state, &body).await;
    Json(result)
}

async fn connect_ima(
    ScopedState(state): ScopedState,
    Json(body): Json<ImaConnectPayload>,
) -> ApiResult<Json<Value>> {
    let name = normalized_kb_name(&body.name)?;
    let result = probe_ima_knowledge_base(
        &state,
        &ImaProbePayload {
            client_id: body.client_id.clone(),
            api_key: body.api_key.clone(),
            knowledge_base_id: body.knowledge_base_id.clone(),
        },
    )
    .await;
    if result.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(ApiError::bad_request(
            result
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("无法连接 IMA 知识库。"),
        ));
    }
    ensure_kb_absent(&state, &name)?;
    create_kb_record(
        &state,
        &name,
        json!({
            "type": "ima",
            "rag_provider": "ima",
            "knowledge_base_id": body.knowledge_base_id,
            "client_id": body.client_id,
            "api_key": body.api_key,
            "description": result.get("description").cloned().unwrap_or(Value::Null),
            "read_only": true,
            "status": "ready"
        }),
    )?;
    Ok(Json(json!({
        "status": "connected",
        "name": name,
        "knowledge_base_id": body.knowledge_base_id,
        "rag_provider": "ima"
    })))
}

async fn kiwix_catalog(
    ScopedState(state): ScopedState,
    Query(query): Query<KiwixCatalogQuery>,
) -> ApiResult<Json<Value>> {
    let base = normalize_http_url(&query.server_url, "Kiwix 服务地址")?;
    let mut request = state.client.get(format!("{base}/catalog/search")).query(&[
        ("count", "50"),
        ("start", "0"),
        ("q", query.q.trim()),
    ]);
    if query.q.trim().is_empty() {
        request = state
            .client
            .get(format!("{base}/catalog/search?count=50&start=0"));
    }
    let response = request
        .send()
        .await
        .map_err(|error| ApiError::provider(format!("无法读取 Kiwix 目录：{error}")))?;
    if !response.status().is_success() {
        return Err(ApiError::provider(format!(
            "kiwix-serve 返回 HTTP {}",
            response.status()
        )));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|error| ApiError::provider(format!("无法读取 Kiwix 目录：{error}")))?;
    let text = String::from_utf8_lossy(&bytes);
    let archives = parse_kiwix_catalog(&text, &base);
    Ok(Json(json!({"archives": archives})))
}

async fn probe_kiwix(
    ScopedState(state): ScopedState,
    Json(body): Json<KiwixProbePayload>,
) -> ApiResult<Json<Value>> {
    let title = fetch_kiwix_title(&state, &body.server_url, &body.zim_name).await?;
    Ok(Json(
        json!({"ok": true, "zim_name": body.zim_name, "title": title}),
    ))
}

async fn connect_kiwix(
    ScopedState(state): ScopedState,
    Json(body): Json<KiwixConnectPayload>,
) -> ApiResult<Json<Value>> {
    let name = normalized_kb_name(&body.name)?;
    let title = fetch_kiwix_title(&state, &body.server_url, &body.zim_name).await?;
    ensure_kb_absent(&state, &name)?;
    create_kb_record(
        &state,
        &name,
        json!({
            "type": "kiwix",
            "rag_provider": "kiwix",
            "server_url": normalize_http_url(&body.server_url, "Kiwix 服务地址")?,
            "zim_name": body.zim_name,
            "zim_title": title,
            "read_only": true,
            "status": "ready"
        }),
    )?;
    Ok(Json(json!({
        "status": "connected",
        "name": name,
        "zim_name": body.zim_name,
        "title": title,
        "rag_provider": "kiwix"
    })))
}

async fn kiwix_articles(
    ScopedState(state): ScopedState,
    Query(query): Query<KiwixArticlesQuery>,
) -> ApiResult<Json<Value>> {
    let record = find_kb(&state, &query.kb_ref)?;
    if record.get("type").and_then(Value::as_str) != Some("kiwix") {
        return Err(ApiError::not_found("Kiwix 知识库不存在。"));
    }
    let base = record
        .get("server_url")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::internal("Kiwix 知识库缺少服务地址。"))?;
    let zim = record
        .get("zim_name")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::internal("Kiwix 知识库缺少 ZIM 名称。"))?;
    let response = state
        .client
        .get(format!("{}/search", base.trim_end_matches('/')))
        .query(&[
            ("books.name", zim),
            ("pattern", query.q.as_str()),
            ("format", "xml"),
            ("pageLength", "10"),
        ])
        .send()
        .await
        .map_err(|error| ApiError::provider(format!("Kiwix 搜索失败：{error}")))?;
    if !response.status().is_success() {
        return Err(ApiError::provider(format!(
            "Kiwix 搜索返回 HTTP {}",
            response.status()
        )));
    }
    let body = response
        .text()
        .await
        .map_err(|error| ApiError::provider(format!("无法读取 Kiwix 搜索结果：{error}")))?;
    Ok(Json(json!({"articles": parse_kiwix_items(&body, zim)})))
}

async fn sync_one_github(state: &AppState, kb_name: &str, mut source: Value) -> Value {
    let source_id = source
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let result = match sync_github_payload(state, kb_name, &source).await {
        Ok(result) => result,
        Err(error) => {
            source["last_sync_status"] = Value::String("error".into());
            source["last_sync_error"] = Value::String(error.message.clone());
            let _ = state
                .database
                .put_resource(GITHUB_DOMAIN, &source_id, &source);
            json!({
                "source_id": source_id,
                "repo": source.get("repo").cloned().unwrap_or(Value::Null),
                "ok": false,
                "skipped": false,
                "files_added": 0,
                "files_updated": 0,
                "files_removed": 0,
                "error": error.message
            })
        }
    };
    result
}

async fn sync_github_payload(state: &AppState, kb_name: &str, source: &Value) -> ApiResult<Value> {
    let source_id = source
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::internal("GitHub 来源缺少 ID。"))?;
    let repo = source
        .get("repo")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad_request("GitHub 来源缺少仓库。"))?;
    let (owner, repo_name) = parse_repository(repo)?;
    let branch = source
        .get("branch")
        .and_then(Value::as_str)
        .unwrap_or("main");
    let tree_url = format!(
        "https://api.github.com/repos/{owner}/{repo_name}/git/trees/{}?recursive=1",
        encode_component(branch)
    );
    let response = state
        .client
        .get(tree_url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .map_err(|error| ApiError::provider(format!("无法读取 GitHub 仓库：{error}")))?;
    if !response.status().is_success() {
        return Err(ApiError::provider(format!(
            "GitHub API 返回 HTTP {}",
            response.status()
        )));
    }
    let tree = response
        .json::<Value>()
        .await
        .map_err(|error| ApiError::provider(format!("GitHub 仓库响应无效：{error}")))?;
    let tree_sha = tree
        .get("sha")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let path_prefix = source
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim_matches('/');
    let glob = source.get("glob").and_then(Value::as_str).unwrap_or("*");
    let entries = tree
        .get("tree")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|entry| entry.get("type").and_then(Value::as_str) == Some("blob"))
        .filter_map(|entry| {
            let path = entry.get("path").and_then(Value::as_str)?.to_string();
            if !path_prefix.is_empty()
                && path != path_prefix
                && !path.starts_with(&format!("{path_prefix}/"))
            {
                return None;
            }
            if !glob_matches(glob, &path) {
                return None;
            }
            Some((
                path,
                entry
                    .get("sha")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            ))
        })
        .collect::<Vec<_>>();

    let raw_root = kb_raw_root(state, kb_name)?;
    let destination = raw_root.join("github").join(source_id);
    let previous = state
        .database
        .get_resource(GITHUB_MANIFEST_DOMAIN, source_id)?
        .and_then(|value| value.get("files").cloned())
        .unwrap_or_else(|| json!({}));
    let previous_map = string_map(&previous);
    let mut manifest = BTreeMap::new();
    let mut added = 0usize;
    let mut updated = 0usize;
    for (path, sha) in entries {
        let relative = path.trim_start_matches('/').to_string();
        let target = destination.join(&relative);
        if !target.starts_with(&destination) {
            continue;
        }
        let changed = previous_map.get(&relative) != Some(&sha);
        if changed {
            let url = format!(
                "https://raw.githubusercontent.com/{owner}/{repo_name}/{}/{path}",
                encode_component(&tree_sha),
            );
            let response =
                state.client.get(url).send().await.map_err(|error| {
                    ApiError::provider(format!("下载 GitHub 文件失败：{error}"))
                })?;
            if !response.status().is_success() {
                return Err(ApiError::provider(format!(
                    "下载 {path} 时 GitHub 返回 HTTP {}",
                    response.status()
                )));
            }
            let bytes = response
                .bytes()
                .await
                .map_err(|error| ApiError::provider(format!("读取 GitHub 文件失败：{error}")))?;
            if bytes.len() as u64 > MAX_SOURCE_FILE_BYTES {
                continue;
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    ApiError::internal(format!("无法创建 GitHub 副本目录：{error}"))
                })?;
            }
            std::fs::write(&target, &bytes)
                .map_err(|error| ApiError::internal(format!("写入 GitHub 文件失败：{error}")))?;
            if previous_map.contains_key(&relative) {
                updated += 1;
            } else {
                added += 1;
            }
        }
        manifest.insert(relative, sha);
    }
    let mut removed = 0usize;
    for relative in previous_map.keys() {
        if !manifest.contains_key(relative) {
            let target = destination.join(relative);
            if target.starts_with(&destination) && target.is_file() {
                let _ = std::fs::remove_file(target);
                removed += 1;
            }
        }
    }
    state.database.put_resource(
        GITHUB_MANIFEST_DOMAIN,
        source_id,
        &json!({"source_id": source_id, "files": manifest}),
    )?;
    let mut updated_source = source.clone();
    updated_source["last_synced_sha"] = Value::String(tree_sha);
    updated_source["last_synced_at"] = Value::String(now_iso());
    updated_source["last_sync_status"] = Value::String("success".into());
    updated_source["last_sync_error"] = Value::Null;
    updated_source["files_synced"] = Value::from(manifest.len());
    state
        .database
        .put_resource(GITHUB_DOMAIN, source_id, &updated_source)?;
    Ok(json!({
        "source_id": source_id,
        "repo": repo,
        "ok": true,
        "skipped": false,
        "files_added": added,
        "files_updated": updated,
        "files_removed": removed,
        "error": Value::Null
    }))
}

async fn sync_one_web(state: &AppState, kb_name: &str, source: &mut Value) -> Value {
    let source_id = source
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let result = match crawl_web_source(state, kb_name, source).await {
        Ok((pages, added, updated, removed, unchanged)) => {
            source["page_count"] = Value::from(pages);
            source["last_synced_at"] = Value::String(now_iso());
            source["last_sync_status"] = Value::String("success".into());
            source["last_sync_error"] = Value::Null;
            json!({
                "source_id": source_id,
                "url": source.get("url").cloned().unwrap_or(Value::Null),
                "ok": true,
                "page_count": pages,
                "pages_added": added,
                "pages_updated": updated,
                "pages_removed": removed,
                "pages_unchanged": unchanged,
                "error": Value::Null
            })
        }
        Err(error) => {
            source["last_synced_at"] = Value::String(now_iso());
            source["last_sync_status"] = Value::String("error".into());
            source["last_sync_error"] = Value::String(error.message.clone());
            json!({
                "source_id": source_id,
                "url": source.get("url").cloned().unwrap_or(Value::Null),
                "ok": false,
                "page_count": source.get("page_count").cloned().unwrap_or_else(|| json!(0)),
                "pages_added": 0,
                "pages_updated": 0,
                "pages_removed": 0,
                "pages_unchanged": 0,
                "error": error.message
            })
        }
    };
    result
}

async fn crawl_web_source(
    state: &AppState,
    kb_name: &str,
    source: &Value,
) -> ApiResult<(usize, usize, usize, usize, usize)> {
    let source_id = source
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::internal("网页来源缺少 ID。"))?;
    let seed = normalize_http_url(
        source.get("url").and_then(Value::as_str).unwrap_or(""),
        "网页来源地址",
    )?;
    let root = Url::parse(&seed).map_err(|error| ApiError::bad_request(error.to_string()))?;
    let origin = root.origin();
    let max_depth = source.get("max_depth").and_then(Value::as_u64).unwrap_or(3) as usize;
    let max_pages = (source
        .get("max_pages")
        .and_then(Value::as_u64)
        .unwrap_or(MAX_WEB_PAGES as u64) as usize)
        .clamp(1, MAX_WEB_PAGES);
    let previous = state
        .database
        .get_resource(WEB_MANIFEST_DOMAIN, source_id)?
        .and_then(|value| value.get("files").cloned())
        .unwrap_or_else(|| json!({}));
    let previous_map = string_map(&previous);
    let raw_root = kb_raw_root(state, kb_name)?;
    let destination = raw_root.join("web").join(source_id);
    let link_regex = Regex::new(r#"(?i)href\s*=\s*["']([^"'#]+)"#)
        .map_err(|error| ApiError::internal(error.to_string()))?;
    let title_regex = Regex::new(r"(?is)<title[^>]*>(.*?)</title>")
        .map_err(|error| ApiError::internal(error.to_string()))?;
    let mut queue = VecDeque::from([(root.clone(), 0usize)]);
    let mut visited = BTreeSet::new();
    let mut manifest = BTreeMap::new();
    let mut added = 0usize;
    let mut updated = 0usize;
    let mut unchanged = 0usize;
    while let Some((url, depth)) = queue.pop_front() {
        if visited.len() >= max_pages || !visited.insert(url.to_string()) {
            continue;
        }
        let response = state
            .client
            .get(url.clone())
            .header(
                "Accept",
                "text/html,application/xhtml+xml,text/plain;q=0.9,*/*;q=0.1",
            )
            .send()
            .await
            .map_err(|error| ApiError::provider(format!("抓取网页失败：{error}")))?;
        if !response.status().is_success() {
            continue;
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let is_html = content_type.contains("html") || content_type.contains("text/");
        if !is_html {
            continue;
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|error| ApiError::provider(format!("读取网页失败：{error}")))?;
        if bytes.len() > MAX_WEB_BYTES {
            continue;
        }
        let digest = format!("{:x}", Sha256::digest(&bytes));
        let extension = if content_type.contains("html") {
            "html"
        } else {
            "txt"
        };
        let relative = format!(
            "{}/{}.{}",
            short_hash(url.as_str()),
            short_hash(url.path()),
            extension
        );
        let target = destination.join(&relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| ApiError::internal(format!("无法创建网页副本目录：{error}")))?;
        }
        let old = previous_map.get(&relative);
        if old != Some(&digest) {
            std::fs::write(&target, &bytes)
                .map_err(|error| ApiError::internal(format!("无法保存网页副本：{error}")))?;
            if old.is_some() {
                updated += 1;
            } else {
                added += 1;
            }
        } else {
            unchanged += 1;
        }
        manifest.insert(
            relative,
            json!({
                "sha256": digest,
                "url": url.as_str(),
                "title": title_regex
                    .captures(&String::from_utf8_lossy(&bytes))
                    .and_then(|capture| capture.get(1).map(|value| value.as_str().trim().to_string()))
                    .unwrap_or_default()
            }),
        );
        if depth < max_depth {
            let text = String::from_utf8_lossy(&bytes);
            for capture in link_regex.captures_iter(&text) {
                let Some(href) = capture.get(1).map(|value| value.as_str()) else {
                    continue;
                };
                if href.starts_with("mailto:") || href.starts_with("javascript:") {
                    continue;
                }
                let Ok(link) = url.join(href) else {
                    continue;
                };
                if link.origin() == origin
                    && matches!(link.scheme(), "http" | "https")
                    && !link.as_str().contains('#')
                {
                    queue.push_back((link, depth + 1));
                }
            }
        }
    }
    let mut removed = 0usize;
    for relative in previous_map.keys() {
        if !manifest.contains_key(relative) {
            let target = destination.join(relative);
            if target.starts_with(&destination) && target.is_file() {
                let _ = std::fs::remove_file(target);
                removed += 1;
            }
        }
    }
    state.database.put_resource(
        WEB_MANIFEST_DOMAIN,
        source_id,
        &json!({"source_id": source_id, "files": manifest}),
    )?;
    Ok((visited.len(), added, updated, removed, unchanged))
}

fn ensure_web_job(state: &AppState, kb_name: &str, source: &Value) -> ApiResult<()> {
    let source_id = source
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::internal("网页来源缺少 ID。"))?;
    let interval = source
        .get("sync_interval_hours")
        .and_then(Value::as_u64)
        .unwrap_or(24)
        .clamp(1, 168);
    let existing = state.database.get_resource(WEB_JOB_DOMAIN, source_id)?;
    let job = existing.unwrap_or_else(|| {
        json!({
            "owner_id": "local",
            "kb_name": kb_name,
            "source_id": source_id,
            "state": "scheduled",
            "next_run_at": crate::storage::now() + interval as f64 * 3600.0,
            "last_run_at": Value::Null,
            "attempt": 0,
            "error": Value::Null,
            "cancel_requested": false
        })
    });
    state
        .database
        .put_resource(WEB_JOB_DOMAIN, source_id, &job)?;
    Ok(())
}

fn ensure_kb(state: &AppState, name: &str) -> ApiResult<Value> {
    state
        .database
        .get_resource("knowledge_base", name)?
        .ok_or_else(|| ApiError::not_found("知识库不存在。"))
}

fn ensure_kb_absent(state: &AppState, name: &str) -> ApiResult<()> {
    if state
        .database
        .get_resource("knowledge_base", name)?
        .is_some()
    {
        return Err(ApiError::conflict(format!("知识库“{name}”已经存在。")));
    }
    Ok(())
}

fn find_kb(state: &AppState, name: &str) -> ApiResult<Value> {
    let name = normalized_kb_name(name)?;
    ensure_kb(state, &name)
}

fn create_kb_record(state: &AppState, name: &str, mut fields: Value) -> ApiResult<Value> {
    let slug = kb_storage_slug(name);
    let root = state.paths.data.join("knowledge").join(slug);
    std::fs::create_dir_all(root.join("raw"))
        .map_err(|error| ApiError::internal(format!("无法创建知识库目录：{error}")))?;
    fields["name"] = Value::String(name.to_string());
    fields["id"] = Value::String(name.to_string());
    fields["path"] = Value::String(root.display().to_string());
    fields["source"] = Value::String("user".into());
    fields["assigned"] = Value::Bool(true);
    fields["created_at"] = fields
        .get("created_at")
        .cloned()
        .unwrap_or_else(|| Value::String(now_iso()));
    fields["updated_at"] = Value::String(now_iso());
    Ok(state
        .database
        .put_resource("knowledge_base", name, &fields)?)
}

fn list_for_kb(state: &AppState, domain: &str, kb_name: &str) -> ApiResult<Vec<Value>> {
    Ok(state
        .database
        .list_resources(domain)?
        .into_iter()
        .filter(|item| item.get("kb_name").and_then(Value::as_str) == Some(kb_name))
        .collect())
}

fn existing_directory(value: &str, label: &str) -> ApiResult<PathBuf> {
    let raw = value.trim();
    if raw.is_empty() {
        return Err(ApiError::bad_request(format!("{label}路径不能为空。")));
    }
    let path = PathBuf::from(raw);
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map_err(|error| ApiError::internal(format!("无法解析当前目录：{error}")))?
            .join(path)
    };
    let path = path
        .canonicalize()
        .map_err(|error| ApiError::bad_request(format!("{label}不可访问：{error}")))?;
    if !path.is_dir() {
        return Err(ApiError::bad_request(format!("{label}不是文件夹。")));
    }
    Ok(path)
}

fn probe_local_folder(value: &str) -> Result<(PathBuf, usize), ApiError> {
    let path = existing_directory(value, "文件夹")?;
    let count = WalkDir::new(&path)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .count();
    Ok((path, count))
}

fn copy_directory(
    source: &FilePath,
    destination: &FilePath,
) -> ApiResult<BTreeMap<String, String>> {
    std::fs::create_dir_all(destination)
        .map_err(|error| ApiError::internal(format!("无法创建复制目录：{error}")))?;
    let mut manifest = BTreeMap::new();
    for entry in WalkDir::new(source)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(source)
            .map_err(|_| ApiError::internal("源目录路径异常。"))?
            .to_string_lossy()
            .replace('\\', "/");
        let target = destination.join(&relative);
        if !target.starts_with(destination) {
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| ApiError::internal(format!("无法创建复制子目录：{error}")))?;
        }
        let metadata = entry
            .metadata()
            .map_err(|error| ApiError::internal(format!("无法读取源文件：{error}")))?;
        if metadata.len() > MAX_SOURCE_FILE_BYTES {
            continue;
        }
        let bytes = std::fs::read(entry.path())
            .map_err(|error| ApiError::internal(format!("无法读取源文件：{error}")))?;
        let digest = format!("{:x}", Sha256::digest(&bytes));
        if std::fs::read(&target).ok().as_deref() != Some(bytes.as_slice()) {
            std::fs::write(&target, &bytes)
                .map_err(|error| ApiError::internal(format!("无法写入复制文件：{error}")))?;
        }
        manifest.insert(relative, digest);
    }
    Ok(manifest)
}

async fn probe_lightrag(state: &AppState, body: &ProbeServerPayload) -> Value {
    let base = match normalize_http_url(&body.server_url, "LightRAG 服务地址") {
        Ok(value) => value,
        Err(error) => {
            return json!({
                "base_url": body.server_url,
                "ok": false,
                "reachable": false,
                "auth_required": false,
                "auth_ok": false,
                "error": error.message
            });
        }
    };
    let api_key = resolve_api_key(
        state,
        &body.api_key,
        body.use_saved_api_key,
        "lightrag-server",
    );
    let response = state.client.get(format!("{base}/auth-status")).send().await;
    let Ok(response) = response else {
        return json!({
            "base_url": base,
            "ok": false,
            "reachable": false,
            "auth_required": false,
            "auth_ok": false,
            "error": "无法连接 LightRAG 服务。"
        });
    };
    if !response.status().is_success() {
        return json!({
            "base_url": base,
            "ok": false,
            "reachable": false,
            "auth_required": false,
            "auth_ok": false,
            "error": format!("LightRAG 服务返回 HTTP {}", response.status())
        });
    }
    let payload = response.json::<Value>().await.unwrap_or_else(|_| json!({}));
    if !payload.get("auth_configured").is_some() && payload.get("core_version").is_none() {
        return json!({
            "base_url": base,
            "ok": false,
            "reachable": true,
            "auth_required": false,
            "auth_ok": false,
            "error": "服务可达，但不像是 LightRAG 服务。"
        });
    }
    let auth_required = payload
        .get("auth_configured")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let auth_ok = !auth_required || !api_key.is_empty();
    json!({
        "base_url": base,
        "ok": auth_ok,
        "reachable": true,
        "auth_required": auth_required,
        "auth_ok": auth_ok,
        "core_version": payload.get("core_version").cloned().unwrap_or(Value::Null),
        "api_version": payload.get("api_version").cloned().unwrap_or(Value::Null),
        "error": if auth_ok { Value::Null } else { Value::String("服务需要 API Key。".into()) }
    })
}

async fn probe_weknora_server(state: &AppState, body: &ProbeServerPayload) -> Value {
    let base = match normalize_http_url(&body.server_url, "WeKnora 服务地址") {
        Ok(value) => value,
        Err(error) => {
            return json!({
                "base_url": body.server_url,
                "knowledge_base_id": body.knowledge_base_id,
                "ok": false,
                "reachable": false,
                "credentials_ok": false,
                "knowledge_base_found": false,
                "error": error.message
            });
        }
    };
    let response = state
        .client
        .get(format!("{base}/api/v1/knowledge-bases"))
        .bearer_auth(body.api_key.trim())
        .send()
        .await;
    let Ok(response) = response else {
        return json!({
            "base_url": base,
            "knowledge_base_id": body.knowledge_base_id,
            "ok": false,
            "reachable": false,
            "credentials_ok": false,
            "knowledge_base_found": false,
            "error": "无法连接 WeKnora 服务。"
        });
    };
    if !response.status().is_success() {
        return json!({
            "base_url": base,
            "knowledge_base_id": body.knowledge_base_id,
            "ok": false,
            "reachable": true,
            "credentials_ok": false,
            "knowledge_base_found": false,
            "error": format!("WeKnora 返回 HTTP {}", response.status())
        });
    }
    let payload = response.json::<Value>().await.unwrap_or_else(|_| json!({}));
    let entries = payload
        .as_array()
        .cloned()
        .or_else(|| payload.get("data").and_then(Value::as_array).cloned())
        .or_else(|| payload.get("items").and_then(Value::as_array).cloned())
        .unwrap_or_default();
    let matched = entries.iter().find(|item| {
        item.get("id").and_then(Value::as_str) == Some(body.knowledge_base_id.as_str())
    });
    json!({
        "base_url": base,
        "knowledge_base_id": body.knowledge_base_id,
        "ok": matched.is_some(),
        "reachable": true,
        "credentials_ok": true,
        "knowledge_base_found": matched.is_some(),
        "knowledge_base_name": matched.and_then(|item| item.get("name")).cloned().unwrap_or(Value::Null),
        "error": if matched.is_some() { Value::Null } else { Value::String("未找到指定的 WeKnora 知识库。".into()) }
    })
}

async fn probe_ima_knowledge_base(state: &AppState, body: &ImaProbePayload) -> Value {
    let credentials = match resolve_ima_credentials(state, &body.client_id, &body.api_key) {
        Ok(value) => value,
        Err(error) => {
            return json!({
                "knowledge_base_id": body.knowledge_base_id,
                "ok": false,
                "credentials_ok": false,
                "knowledge_base_name": Value::Null,
                "description": Value::Null,
                "error": error.message
            })
        }
    };
    let result = ima_request(
        state,
        "get_knowledge_base",
        &credentials,
        json!({"ids": [body.knowledge_base_id]}),
    )
    .await;
    let Ok(payload) = result else {
        return json!({
            "knowledge_base_id": body.knowledge_base_id,
            "ok": false,
            "credentials_ok": false,
            "knowledge_base_name": Value::Null,
            "description": Value::Null,
            "error": "Could not reach Tencent IMA."
        });
    };
    let info = payload
        .get("infos")
        .and_then(|value| value.get(&body.knowledge_base_id))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let found = !info.as_object().is_none_or(Map::is_empty);
    json!({
        "knowledge_base_id": body.knowledge_base_id,
        "ok": found,
        "credentials_ok": true,
        "knowledge_base_name": info.get("name").cloned().unwrap_or(Value::Null),
        "description": info.get("description").cloned().unwrap_or(Value::Null),
        "error": if found { Value::Null } else { Value::String("未找到指定的 IMA 知识库。".into()) }
    })
}

async fn ima_request(
    state: &AppState,
    method: &str,
    credentials: &(String, String),
    body: Value,
) -> ApiResult<Value> {
    let response = state
        .client
        .post(format!("https://ima.qq.com/openapi/wiki/v1/{method}"))
        .header("ima-openapi-clientid", &credentials.0)
        .header("ima-openapi-apikey", &credentials.1)
        .json(&body)
        .send()
        .await
        .map_err(|error| ApiError::provider(format!("IMA 请求失败：{error}")))?;
    let status = response.status();
    let payload = response
        .json::<Value>()
        .await
        .map_err(|error| ApiError::provider(format!("IMA 响应无效：{error}")))?;
    if !status.is_success() {
        return Err(ApiError::provider(format!("IMA 返回 HTTP {status}")));
    }
    let code = payload
        .get("retcode")
        .or_else(|| payload.get("code"))
        .and_then(Value::as_i64)
        .unwrap_or(-1);
    if code != 0 {
        let message = payload
            .get("errmsg")
            .or_else(|| payload.get("msg"))
            .and_then(Value::as_str)
            .unwrap_or("IMA 返回业务错误");
        return Err(ApiError::provider(message.to_string()));
    }
    Ok(payload.get("data").cloned().unwrap_or_else(|| json!({})))
}

fn resolve_ima_credentials(
    state: &AppState,
    client_id: &str,
    api_key: &str,
) -> ApiResult<(String, String)> {
    let supplied_id = client_id.trim();
    let supplied_key = api_key.trim();
    match (supplied_id.is_empty(), supplied_key.is_empty()) {
        (false, false) => {
            return Ok((supplied_id.to_string(), supplied_key.to_string()));
        }
        (true, false) | (false, true) => {
            return Err(ApiError::bad_request(
                "IMA Client ID 与 API Key 必须同时提供，或同时留空使用账户配置。",
            ));
        }
        (true, true) => {}
    }
    let config = state
        .database
        .get_setting("rag_provider_config:ima")?
        .unwrap_or_else(|| json!({}));
    let stored_id = config
        .get("client_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let stored_key = config
        .get("api_key")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if stored_id.is_empty() || stored_key.is_empty() {
        return Err(ApiError::bad_request("请先配置 IMA Client ID 与 API Key。"));
    }
    Ok((stored_id, stored_key))
}

fn resolve_api_key(state: &AppState, supplied: &str, use_saved: bool, provider: &str) -> String {
    if !supplied.trim().is_empty() || !use_saved {
        return supplied.trim().to_string();
    }
    state
        .database
        .get_setting(&format!("rag_provider_config:{provider}"))
        .ok()
        .flatten()
        .and_then(|value| {
            value
                .get("api_key")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default()
}

fn fetch_kiwix_title<'a>(
    state: &'a AppState,
    server_url: &'a str,
    zim_name: &'a str,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ApiResult<String>> + Send + 'a>> {
    Box::pin(async move {
        let base = normalize_http_url(server_url, "Kiwix 服务地址")?;
        let zim = validate_zim_name(zim_name)?;
        let response = state
            .client
            .get(format!("{base}/raw/{zim}/meta/Title"))
            .send()
            .await
            .map_err(|error| ApiError::provider(format!("无法连接 kiwix-serve：{error}")))?;
        if !response.status().is_success() {
            return Err(ApiError::provider(format!(
                "kiwix-serve 返回 HTTP {}",
                response.status()
            )));
        }
        let title = response
            .text()
            .await
            .map_err(|error| ApiError::provider(format!("无法读取 Kiwix 标题：{error}")))?
            .trim()
            .chars()
            .take(300)
            .collect::<String>();
        if title.is_empty() {
            return Err(ApiError::provider("ZIM 没有可读取的标题。"));
        }
        Ok(title)
    })
}

fn parse_kiwix_catalog(text: &str, base: &str) -> Vec<Value> {
    let entry_regex = Regex::new(r"(?is)<entry\b[^>]*>(.*?)</entry>").ok();
    let title_regex = Regex::new(r"(?is)<title[^>]*>(.*?)</title>").ok();
    let link_regex = Regex::new(r#"(?is)<link\b[^>]*href\s*=\s*["']([^"']+)["']"#).ok();
    let name_regex = Regex::new(r"(?is)<name[^>]*>(.*?)</name>").ok();
    let Some(entry_regex) = entry_regex else {
        return Vec::new();
    };
    let mut seen = BTreeSet::new();
    let mut archives = Vec::new();
    for capture in entry_regex.captures_iter(text) {
        let Some(entry) = capture.get(1).map(|value| value.as_str()) else {
            continue;
        };
        let title = title_regex
            .as_ref()
            .and_then(|regex| regex.captures(entry))
            .and_then(|capture| capture.get(1).map(|value| clean_xml_text(value.as_str())))
            .unwrap_or_default();
        let mut zim = String::new();
        if let Some(link) = link_regex
            .as_ref()
            .and_then(|regex| regex.captures(entry))
            .and_then(|capture| capture.get(1).map(|value| value.as_str().to_string()))
        {
            let path = link
                .split("?")
                .next()
                .unwrap_or("")
                .split("/content/")
                .nth(1)
                .unwrap_or("");
            zim = path.split('/').next().unwrap_or("").to_string();
        }
        if zim.is_empty() {
            zim = name_regex
                .as_ref()
                .and_then(|regex| regex.captures(entry))
                .and_then(|capture| capture.get(1).map(|value| clean_xml_text(value.as_str())))
                .unwrap_or_default();
        }
        let _ = base;
        if zim.is_empty() || !seen.insert(zim.clone()) {
            continue;
        }
        archives.push(json!({
            "zim_name": zim,
            "title": if title.is_empty() { zim } else { title }
        }));
        if archives.len() >= 50 {
            break;
        }
    }
    archives
}

fn parse_kiwix_items(text: &str, zim: &str) -> Vec<Value> {
    let item_regex = Regex::new(r"(?is)<item\b[^>]*>(.*?)</item>").ok();
    let title_regex = Regex::new(r"(?is)<title[^>]*>(.*?)</title>").ok();
    let link_regex = Regex::new(r"(?is)<link[^>]*>(.*?)</link>").ok();
    let description_regex = Regex::new(r"(?is)<description[^>]*>(.*?)</description>").ok();
    let Some(item_regex) = item_regex else {
        return Vec::new();
    };
    let mut result = Vec::new();
    for capture in item_regex.captures_iter(text) {
        let Some(item) = capture.get(1).map(|value| value.as_str()) else {
            continue;
        };
        let title = title_regex
            .as_ref()
            .and_then(|regex| regex.captures(item))
            .and_then(|capture| capture.get(1).map(|value| clean_xml_text(value.as_str())))
            .unwrap_or_default();
        let link = link_regex
            .as_ref()
            .and_then(|regex| regex.captures(item))
            .and_then(|capture| capture.get(1).map(|value| clean_xml_text(value.as_str())))
            .unwrap_or_default();
        let marker = format!("/content/{zim}/");
        let article_path = link.split_once(&marker).map(|(_, value)| value.to_string());
        let Some(article_path) = article_path else {
            continue;
        };
        let excerpt = description_regex
            .as_ref()
            .and_then(|regex| regex.captures(item))
            .and_then(|capture| capture.get(1).map(|value| clean_xml_text(value.as_str())))
            .unwrap_or_default();
        result.push(json!({
            "title": if title.is_empty() { article_path.clone() } else { title },
            "article_path": article_path,
            "excerpt": excerpt.chars().take(500).collect::<String>()
        }));
        if result.len() >= 10 {
            break;
        }
    }
    result
}

fn clean_xml_text(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(1000)
        .collect()
}

fn normalize_http_url(value: &str, label: &str) -> ApiResult<String> {
    let value = value.trim();
    let parsed = Url::parse(value)
        .map_err(|error| ApiError::bad_request(format!("{label}无效：{error}")))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(ApiError::bad_request(format!(
            "{label}必须是 HTTP(S) 地址。"
        )));
    }
    if parsed.username() != "" || parsed.password().is_some() {
        return Err(ApiError::bad_request(format!("{label}不能包含凭据。")));
    }
    Ok(parsed.to_string().trim_end_matches('/').to_string())
}

fn validate_zim_name(value: &str) -> ApiResult<String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 128
        || !value
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_alphanumeric())
        || !value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
        })
    {
        return Err(ApiError::bad_request("ZIM 名称无效。"));
    }
    Ok(value.to_string())
}

fn parse_repository(value: &str) -> ApiResult<(String, String)> {
    let value = value.trim().trim_end_matches(".git");
    let value = value
        .strip_prefix("https://github.com/")
        .or_else(|| value.strip_prefix("http://github.com/"))
        .or_else(|| value.strip_prefix("git@github.com:"))
        .unwrap_or(value)
        .trim_matches('/');
    let mut parts = value.split('/');
    let owner = parts.next().unwrap_or("").trim();
    let repo = parts.next().unwrap_or("").trim();
    if owner.is_empty()
        || repo.is_empty()
        || !owner
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        || !repo.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
    {
        return Err(ApiError::bad_request("GitHub 仓库格式应为 owner/repo。"));
    }
    Ok((owner.to_string(), repo.to_string()))
}

fn encode_component(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

fn glob_matches(pattern: &str, path: &str) -> bool {
    let pattern = pattern.trim();
    if pattern.is_empty() || pattern == "*" || pattern == "**" {
        return true;
    }
    if !pattern.contains('/') {
        let file_name = path.rsplit('/').next().unwrap_or(path);
        return wildcard_matches(pattern, file_name);
    }
    wildcard_matches(pattern, path)
}

fn wildcard_matches(pattern: &str, value: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let mut dp = vec![false; value.len() + 1];
    dp[0] = true;
    for &token in pattern {
        let mut next = vec![false; value.len() + 1];
        if token == b'*' {
            next[0] = dp[0];
            for index in 1..=value.len() {
                next[index] = dp[index] || next[index - 1];
            }
        } else {
            for index in 1..=value.len() {
                next[index] = dp[index - 1] && (token == b'?' || token == value[index - 1]);
            }
        }
        dp = next;
    }
    dp[value.len()]
}

fn string_map(value: &Value) -> BTreeMap<String, String> {
    value
        .as_object()
        .map(|object| {
            object
                .iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|value| (key.clone(), value.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn short_hash(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))[..16].to_string()
}

fn kb_storage_slug(name: &str) -> String {
    let cleaned = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric()
                || matches!(character, '-' | '_' | '.')
                || !character.is_ascii()
            {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    format!("{}-{}", cleaned.trim_matches('_'), &short_hash(name))
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_glob_supports_markdown_files() {
        assert!(glob_matches("*.md", "docs/guide.md"));
        assert!(!glob_matches("*.md", "docs/guide.txt"));
        assert!(glob_matches("docs/**", "docs/guide.md"));
    }

    #[test]
    fn repository_names_are_normalized() {
        assert_eq!(
            parse_repository("https://github.com/acme/notes.git").unwrap(),
            ("acme".to_string(), "notes".to_string())
        );
    }

    #[test]
    fn zim_names_reject_path_escape() {
        assert!(validate_zim_name("wikipedia_en_all").is_ok());
        assert!(validate_zim_name("../secret").is_err());
    }
}
