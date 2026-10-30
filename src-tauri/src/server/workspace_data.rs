use super::auth::Principal;
use super::error::{ApiError, ApiResult};
use super::{AppState, ScopedState};
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Extension, Json, Router};
use chrono::Utc;
use rusqlite::params;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path as FilePath, PathBuf};
use tokio_util::io::ReaderStream;
use uuid::Uuid;
use walkdir::WalkDir;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

const REGISTRATION_DOMAIN: &str = "workspace";
const OPERATIONS_KEY: &str = "workspace_data_operations";
const MANAGED_ROOT_KEY: &str = "workspace_managed_root";
const SYSTEM_WORKSPACE_ID: &str = "ws_system";
const GENERAL_WORKSPACE_ID: &str = "ws_general";
const MAX_INSTRUCTIONS: usize = 50_000;

const FEATURES: &[(&str, &str, &str)] = &[
    ("chat", "Conversations", "chat"),
    ("book", "Books", "books"),
    ("learning", "Mastery Path", "learning"),
    ("reading", "Immersive Reading", "reading"),
    ("timed_media", "Immersive Watching", "timed_media"),
    ("notebook", "Notebooks", "notebooks"),
    ("co-writer", "Writing", "co-writer"),
    ("courses", "Courses", "courses"),
    ("learning_journal", "Learning journal", "learning_journal"),
    ("files", "File library", "files"),
    ("knowledge_bases", "Knowledge bases", "knowledge"),
    ("parse_cache", "Document cache", "parse-cache"),
    ("outputs", "Generated outputs", "outputs"),
    (
        "presentations",
        "Presented files",
        "workspace_presentations",
    ),
    ("attachments", "Conversation attachments", "uploads"),
    (
        "historical_chat",
        "Historical conversation database",
        "historical/chat",
    ),
    (
        "historical_agent",
        "Historical agent outputs",
        "historical/agent",
    ),
    (
        "historical_output",
        "Historical output folder",
        "historical/output",
    ),
    (
        "historical_archive",
        "Historical archives",
        "historical/archive",
    ),
];

#[derive(Debug, Deserialize)]
struct DiscoverQuery {
    source_workspace_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct MigrationPayload {
    #[serde(default)]
    source_workspace_id: String,
    #[serde(default)]
    target_workspace_id: String,
    features: Vec<String>,
    #[serde(default)]
    include_historical: bool,
}

#[derive(Debug, Deserialize)]
struct KnowledgeMovePayload {
    source_id: String,
    #[serde(default)]
    target_workspace_id: String,
}

#[derive(Debug, Deserialize)]
struct WorkspacePatchPayload {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    archived: Option<bool>,
    #[serde(default)]
    instructions: Option<String>,
    #[serde(default)]
    resources: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct WorkspaceMigrationPayload {
    path: String,
}

#[derive(Debug, Deserialize)]
struct WorkspaceCreatePayload {
    name: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    instructions: Option<String>,
    #[serde(default)]
    resources: Option<Value>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/settings/workspace/registrations",
            get(list_registrations).post(create_registration),
        )
        .route(
            "/settings/workspace/registrations/{workspace_id}",
            patch(update_registration),
        )
        .route(
            "/settings/workspace/registrations/system-snapshot",
            get(system_snapshot).post(system_snapshot),
        )
        .route(
            "/settings/workspace/registrations/migrate-root",
            post(migrate_workspace_root),
        )
        .route(
            "/settings/workspace/registrations/{workspace_id}/migrate",
            post(migrate_registration),
        )
        .route("/settings/workspace/data/discover", get(discover_data))
        .route("/settings/workspace/data/preview", post(preview_data))
        .route("/settings/workspace/data/migrate", post(migrate_data))
        .route("/settings/workspace/data/export", post(export_data))
        .route("/settings/workspace/data/operations", get(list_operations))
        .route(
            "/settings/workspace/data/operations/{operation_id}/recover",
            post(recover_operation),
        )
        .route(
            "/settings/workspace/data/exports/{operation_id}",
            get(download_export),
        )
        .route(
            "/settings/workspace/knowledge-bases/preview-move",
            post(preview_knowledge_move),
        )
        .route(
            "/settings/workspace/knowledge-bases/move",
            post(move_knowledge_base),
        )
}

async fn list_registrations(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    ensure_builtin_workspaces(&state)?;
    Ok(Json(describe_catalog(&state)?))
}

pub(super) async fn create_registration(
    ScopedState(state): ScopedState,
    Json(payload): Json<WorkspaceCreatePayload>,
) -> ApiResult<Json<Value>> {
    let name = payload.name.trim();
    if name.is_empty() || name.chars().count() > 100 {
        return Err(ApiError::bad_request("工作区名称必须为 1 到 100 个字符"));
    }
    let instructions = normalize_instructions(payload.instructions.as_deref())?;
    let resources = normalize_workspace_resources(payload.resources)?;
    let workspace_id = format!("ws_{}", Uuid::new_v4().simple());
    let default_path = managed_root(&state)?.join(&workspace_id);
    let root = payload
        .path
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(|value| validate_new_workspace_path(&state, PathBuf::from(value)))
        .transpose()?
        .unwrap_or(default_path);
    if root.exists() {
        let mut entries = std::fs::read_dir(&root)
            .map_err(|error| ApiError::bad_request(format!("无法读取工作区目录：{error}")))?;
        if entries.next().is_some() {
            return Err(ApiError::conflict(
                "目标工作区目录必须为空，避免覆盖已有数据",
            ));
        }
    }
    std::fs::create_dir_all(&root)
        .map_err(|error| ApiError::internal(format!("无法创建工作区目录：{error}")))?;
    let record = normalize_registration(json!({
        "workspace_id": workspace_id,
        "display_name": name,
        "path": root.display().to_string(),
        "kind": "workspace",
        "follows_root": false,
        "archived": false,
        "created_at": Utc::now().to_rfc3339(),
        "instructions": instructions,
        "resources": resources
    }))?;
    state
        .database
        .put_resource(REGISTRATION_DOMAIN, &workspace_id, &record)?;
    let workspace = registration_with_status(&record)?;
    Ok(Json(json!({"workspace": workspace})))
}

async fn update_registration(
    ScopedState(state): ScopedState,
    Path(workspace_id): Path<String>,
    Json(payload): Json<WorkspacePatchPayload>,
) -> ApiResult<Json<Value>> {
    let mut record = registration_record(&state, &workspace_id)?;
    if let Some(name) = payload.name {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 100 {
            return Err(ApiError::bad_request("工作区名称必须为 1 到 100 个字符"));
        }
        record["display_name"] = Value::String(name.to_string());
    }
    if payload.path.is_some() {
        return Err(ApiError::bad_request(
            "修改工作区存储位置请使用迁移接口，以便生成可恢复副本",
        ));
    }
    if let Some(archived) = payload.archived {
        if record.get("kind").and_then(Value::as_str) == Some("system") && archived {
            return Err(ApiError::bad_request("系统工作区不能归档"));
        }
        record["archived"] = Value::Bool(archived);
    }
    if let Some(instructions) = payload.instructions {
        record["instructions"] = Value::String(normalize_instructions(Some(&instructions))?);
    }
    if let Some(resources) = payload.resources {
        record["resources"] = normalize_workspace_resources(Some(resources))?;
    }
    state
        .database
        .put_resource(REGISTRATION_DOMAIN, &workspace_id, &record)?;
    let saved = registration_record(&state, &workspace_id)?;
    let workspace = registration_with_status(&saved)?;
    Ok(Json(json!({"workspace": workspace})))
}

async fn discover_data(
    ScopedState(state): ScopedState,
    Query(query): Query<DiscoverQuery>,
) -> ApiResult<Json<Value>> {
    let workspace_id = query.source_workspace_id.unwrap_or_default();
    let result = discover(&state, &workspace_id)?;
    Ok(Json(result))
}

async fn preview_data(
    ScopedState(state): ScopedState,
    Json(payload): Json<MigrationPayload>,
) -> ApiResult<Json<Value>> {
    Ok(Json(preview(&state, &payload, false)?))
}

async fn migrate_data(
    ScopedState(state): ScopedState,
    Json(payload): Json<MigrationPayload>,
) -> ApiResult<Json<Value>> {
    if !state.active_turns.lock().is_empty() || has_active_turns(&state)? {
        return Err(ApiError::conflict(
            "请等待所有正在运行的对话结束后再迁移工作区数据",
        ));
    }
    let plan = preview(&state, &payload, false)?;
    let blockers = plan
        .get("blockers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !blockers.is_empty() {
        return Err(ApiError::conflict(
            blockers
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("；"),
        ));
    }
    let operation = migrate(&state, &payload, plan)?;
    Ok(Json(operation))
}

async fn export_data(
    ScopedState(state): ScopedState,
    Json(payload): Json<MigrationPayload>,
) -> ApiResult<Json<Value>> {
    if !state.active_turns.lock().is_empty() || has_active_turns(&state)? {
        return Err(ApiError::conflict(
            "请等待所有正在运行的对话结束后再导出工作区数据",
        ));
    }
    Ok(Json(export(&state, &payload)?))
}

async fn list_operations(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"operations": load_operations(&state)?})))
}

async fn recover_operation(
    ScopedState(state): ScopedState,
    Path(operation_id): Path<String>,
) -> ApiResult<Json<Value>> {
    validate_operation_id(&operation_id)?;
    let mut operations = load_operations(&state)?;
    let index = operations
        .iter()
        .position(|row| row.get("id").and_then(Value::as_str) == Some(operation_id.as_str()))
        .ok_or_else(|| ApiError::not_found("迁移操作不存在"))?;
    let mut operation = operations[index].clone();
    let status = operation
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("");
    if matches!(status, "completed" | "exported" | "recovered") {
        return Ok(Json(operation));
    }
    if let Some(paths) = operation.get("installed_paths").and_then(Value::as_array) {
        for path in paths.iter().filter_map(Value::as_str) {
            let candidate = PathBuf::from(path);
            ensure_inside(&candidate, &state.paths.root)?;
            if candidate.is_dir() {
                std::fs::remove_dir_all(&candidate)
                    .map_err(|error| ApiError::internal(format!("恢复时无法删除目录：{error}")))?;
            } else if candidate.is_file() {
                std::fs::remove_file(&candidate)
                    .map_err(|error| ApiError::internal(format!("恢复时无法删除文件：{error}")))?;
            }
        }
    }
    operation["status"] = Value::String("recovered".to_string());
    operation["recovered_at"] = Value::String(Utc::now().to_rfc3339());
    operations[index] = operation.clone();
    save_operations(&state, &operations)?;
    Ok(Json(operation))
}

async fn download_export(
    ScopedState(state): ScopedState,
    Path(operation_id): Path<String>,
) -> ApiResult<Response> {
    validate_operation_id(&operation_id)?;
    let path = state
        .paths
        .data
        .join("workspace-exports")
        .join(&operation_id)
        .join("export.zip");
    ensure_inside(&path, &state.paths.root)?;
    let file = tokio::fs::File::open(&path)
        .await
        .map_err(|_| ApiError::not_found("导出归档不存在"))?;
    let metadata = file
        .metadata()
        .await
        .map_err(|error| ApiError::internal(format!("无法读取导出归档：{error}")))?;
    let body = Body::from_stream(ReaderStream::new(file));
    let mut response = Response::new(body);
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/zip"),
    );
    response.headers_mut().insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&metadata.len().to_string())
            .map_err(|error| ApiError::internal(error.to_string()))?,
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!(
            "attachment; filename=\"deeptutor-data-{}.zip\"",
            &operation_id[..8.min(operation_id.len())]
        ))
        .map_err(|error| ApiError::internal(error.to_string()))?,
    );
    Ok(response)
}

async fn preview_knowledge_move(
    ScopedState(state): ScopedState,
    Json(payload): Json<KnowledgeMovePayload>,
) -> ApiResult<Json<Value>> {
    Ok(Json(knowledge_move_plan(
        &state,
        &payload.source_id,
        &payload.target_workspace_id,
    )?))
}

async fn move_knowledge_base(
    ScopedState(state): ScopedState,
    Json(payload): Json<KnowledgeMovePayload>,
) -> ApiResult<Json<Value>> {
    if !state.active_turns.lock().is_empty() || has_active_turns(&state)? {
        return Err(ApiError::conflict("请等待正在运行的对话结束后再移动知识库"));
    }
    let mut plan = knowledge_move_plan(&state, &payload.source_id, &payload.target_workspace_id)?;
    if plan
        .get("blockers")
        .and_then(Value::as_array)
        .is_some_and(|items| !items.is_empty())
    {
        return Err(ApiError::conflict(
            plan["blockers"]
                .as_array()
                .unwrap_or(&Vec::new())
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("；"),
        ));
    }
    let name = plan
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::internal("知识库移动计划缺少名称"))?
        .to_string();
    let source = PathBuf::from(
        plan.get("source_path")
            .and_then(Value::as_str)
            .unwrap_or(""),
    );
    let target = PathBuf::from(
        plan.get("target_path")
            .and_then(Value::as_str)
            .unwrap_or(""),
    );
    copy_tree_verified(&source, &target)?;
    let mut kb = state
        .database
        .get_resource("knowledge_base", &name)?
        .ok_or_else(|| ApiError::not_found("知识库不存在"))?;
    kb["path"] = Value::String(target.display().to_string());
    kb["storage_workspace_id"] = Value::String(payload.target_workspace_id.clone());
    state.database.put_resource("knowledge_base", &name, &kb)?;
    let old_ids = json!([
        payload.source_id,
        name,
        format!(
            "{}:kb:{}",
            plan.get("source_workspace_id")
                .and_then(Value::as_str)
                .unwrap_or(""),
            name
        )
    ]);
    rewrite_workspace_resource_assignments(&state, &old_ids, &plan["target_id"])?;
    if source.exists() {
        std::fs::remove_dir_all(&source)
            .map_err(|error| ApiError::internal(format!("无法清理源知识库目录：{error}")))?;
    }
    plan["status"] = Value::String("completed".to_string());
    plan["cleanup_path"] = Value::String(source.display().to_string());
    Ok(Json(plan))
}

async fn system_snapshot(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(write_system_snapshot(&state)?))
}

async fn migrate_registration(
    ScopedState(state): ScopedState,
    Path(workspace_id): Path<String>,
    Json(payload): Json<WorkspaceMigrationPayload>,
) -> ApiResult<Json<Value>> {
    ensure_no_active_turns(&state)?;
    let record = registration_record(&state, &workspace_id)?;
    migrate_workspace_directory(&state, &workspace_id, &record, &payload.path)?;
    Ok(Json(describe_catalog(&state)?))
}

async fn migrate_workspace_root(
    ScopedState(state): ScopedState,
    Json(payload): Json<WorkspaceMigrationPayload>,
) -> ApiResult<Json<Value>> {
    ensure_no_active_turns(&state)?;
    let target = validate_new_workspace_path(&state, PathBuf::from(&payload.path))?;
    let current = managed_root(&state)?;
    if current == target {
        return Ok(Json(describe_catalog(&state)?));
    }
    if target.starts_with(&current) || current.starts_with(&target) {
        return Err(ApiError::bad_request(
            "新的工作区根目录不能与当前根目录互相包含",
        ));
    }
    if target.exists() {
        let mut entries = std::fs::read_dir(&target)
            .map_err(|error| ApiError::bad_request(format!("无法读取目标根目录：{error}")))?;
        if entries.next().is_some() {
            return Err(ApiError::conflict("新的工作区根目录必须为空"));
        }
    }
    std::fs::create_dir_all(&target)
        .map_err(|error| ApiError::bad_request(format!("无法创建目标根目录：{error}")))?;
    let staging = target.join(format!(".workspace-root-import-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&staging)
        .map_err(|error| ApiError::internal(format!("无法创建工作区暂存目录：{error}")))?;
    let records = ensure_builtin_workspaces(&state)?
        .into_iter()
        .filter(|record| {
            record.get("follows_root").and_then(Value::as_bool) == Some(true)
                && record.get("kind").and_then(Value::as_str) != Some("session")
        })
        .collect::<Vec<_>>();
    let mut moved = Vec::new();
    for record in &records {
        let id = record
            .get("workspace_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        let old = PathBuf::from(record.get("path").and_then(Value::as_str).unwrap_or(""));
        if !old.starts_with(&current) || old == current {
            continue;
        }
        let relative = old
            .strip_prefix(&current)
            .map_err(|error| ApiError::internal(error.to_string()))?
            .to_path_buf();
        let stage = staging.join(&relative);
        copy_tree_verified(&old, &stage)?;
        moved.push((id.to_string(), old, target.join(&relative)));
    }
    for (id, old, new) in &moved {
        if new.exists() {
            return Err(ApiError::conflict(format!(
                "目标工作区目录已经存在：{}",
                new.display()
            )));
        }
        if let Some(parent) = new.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| ApiError::internal(format!("无法创建工作区父目录：{error}")))?;
        }
        let relative = new
            .strip_prefix(&target)
            .map_err(|error| ApiError::internal(error.to_string()))?;
        let stage = staging.join(relative);
        std::fs::rename(&stage, new)
            .map_err(|error| ApiError::internal(format!("无法发布工作区目录：{error}")))?;
        let mut updated = registration_record(&state, id)?;
        updated["path"] = Value::String(new.display().to_string());
        updated["follows_root"] = Value::Bool(true);
        state
            .database
            .put_resource(REGISTRATION_DOMAIN, id, &updated)?;
        if old.exists() && old != new {
            std::fs::remove_dir_all(old).map_err(|error| {
                ApiError::internal(format!("工作区已迁移，但无法清理旧目录：{error}"))
            })?;
        }
    }
    state.database.set_setting(
        MANAGED_ROOT_KEY,
        &Value::String(target.display().to_string()),
    )?;
    if staging.exists() {
        let _ = std::fs::remove_dir(&staging);
    }
    Ok(Json(describe_catalog(&state)?))
}

fn discover(state: &AppState, workspace_id: &str) -> ApiResult<Value> {
    let (base, is_default) = workspace_base(state, workspace_id)?;
    let sessions = session_count(state, workspace_id)?;
    let mut rows = Vec::new();
    for (feature, label, _) in FEATURES {
        let path = feature_path(&base, feature, is_default);
        let (files, bytes) = file_metrics(&path)?;
        let mut row = json!({
            "feature": feature,
            "label": label,
            "path": path.display().to_string(),
            "files": files,
            "bytes": bytes,
            "error": ""
        });
        if *feature == "chat" {
            row["sessions"] = Value::from(sessions);
        }
        rows.push(row);
    }
    let historical = discover_historical(&base, is_default);
    Ok(json!({
        "workspace_id": workspace_id,
        "features": rows,
        "historical": historical,
        "session_backend": "sqlite"
    }))
}

fn preview(
    state: &AppState,
    payload: &MigrationPayload,
    allow_same_target: bool,
) -> ApiResult<Value> {
    if payload.features.is_empty() {
        return Err(ApiError::bad_request("请至少选择一个数据功能"));
    }
    let mut requested = BTreeSet::new();
    for feature in &payload.features {
        validate_feature(feature)?;
        requested.insert(feature.clone());
    }
    let mut selected = requested.clone();
    add_dependencies(&mut selected, &requested);
    let (source_base, source_default) = workspace_base(state, &payload.source_workspace_id)?;
    let (target_base, target_default) = workspace_base(state, &payload.target_workspace_id)?;
    let mut rows = Vec::new();
    let mut blockers = Vec::new();
    let mut files = 0usize;
    let mut bytes = 0u64;
    for feature in &selected {
        let source = feature_path(&source_base, feature, source_default);
        let (count, size) = file_metrics(&source)?;
        if feature == "chat" {
            let sessions = session_count(state, &payload.source_workspace_id)?;
            files += sessions;
            rows.push(json!({
                "feature": feature,
                "path": source.display().to_string(),
                "files": sessions,
                "sessions": sessions,
                "bytes": size,
                "error": ""
            }));
        } else {
            files += count;
            bytes = bytes.saturating_add(size);
            rows.push(json!({
                "feature": feature,
                "path": source.display().to_string(),
                "files": count,
                "bytes": size,
                "error": ""
            }));
        }
        if !allow_same_target {
            let target = feature_path(&target_base, feature, target_default);
            let (target_files, _) = file_metrics(&target)?;
            if target_files > 0 {
                blockers.push(format!(
                    "目标工作区的“{}”已有数据，请选择空工作区",
                    feature_label(feature)
                ));
            }
        }
    }
    if !allow_same_target && payload.source_workspace_id == payload.target_workspace_id {
        blockers.push("来源工作区和目标工作区不能相同".to_string());
    }
    if has_active_turns(state)? {
        blockers.push("请等待正在运行的对话结束后再迁移数据".to_string());
    }
    if target_registration(state, &payload.target_workspace_id)
        .ok()
        .flatten()
        .and_then(|record| record.get("archived").and_then(Value::as_bool))
        == Some(true)
    {
        blockers.push("目标工作区已归档，请先恢复".to_string());
    }
    let dependencies = selected.difference(&requested).cloned().collect::<Vec<_>>();
    Ok(json!({
        "source_workspace_id": payload.source_workspace_id,
        "target_workspace_id": payload.target_workspace_id,
        "requested_features": payload.features,
        "features": selected,
        "dependencies": dependencies,
        "sessions": session_count(state, &payload.source_workspace_id)?,
        "files": files,
        "bytes": bytes,
        "blockers": blockers,
        "warnings": [],
        "rows": rows
    }))
}

fn validate_feature(feature: &str) -> ApiResult<()> {
    if FEATURES.iter().any(|(key, _, _)| *key == feature) {
        Ok(())
    } else {
        Err(ApiError::bad_request(format!(
            "不支持的数据功能：{feature}"
        )))
    }
}

fn feature_label(feature: &str) -> &str {
    FEATURES
        .iter()
        .find(|(key, _, _)| *key == feature)
        .map(|(_, label, _)| *label)
        .unwrap_or(feature)
}

fn add_dependencies(selected: &mut BTreeSet<String>, requested: &BTreeSet<String>) {
    let add = |selected: &mut BTreeSet<String>, names: &[&str]| {
        for name in names {
            selected.insert((*name).to_string());
        }
    };
    if requested.contains("chat") || requested.contains("attachments") {
        add(
            selected,
            &["chat", "attachments", "outputs", "presentations"],
        );
    }
    if requested.contains("learning") {
        add(
            selected,
            &[
                "learning",
                "knowledge_bases",
                "notebook",
                "learning_journal",
            ],
        );
    }
    if requested.contains("reading") {
        add(
            selected,
            &["reading", "files", "knowledge_bases", "notebook"],
        );
    }
    if requested.contains("notebook") {
        add(selected, &["notebook", "chat"]);
    }
}

fn ensure_builtin_workspaces(state: &AppState) -> ApiResult<Vec<Value>> {
    let root = managed_root(state)?;
    let existing = state
        .database
        .list_resources(REGISTRATION_DOMAIN)?
        .into_iter()
        .map(|record| {
            normalize_registration(record).map(|value| {
                (
                    value
                        .get("workspace_id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    value,
                )
            })
        })
        .collect::<ApiResult<BTreeMap<_, _>>>()?;
    let mut records = existing;
    for (id, name, kind) in [
        (SYSTEM_WORKSPACE_ID, "系统工作区", "system"),
        (GENERAL_WORKSPACE_ID, "通用工作区", "general"),
    ] {
        if records.contains_key(id) {
            continue;
        }
        let path = root.join(id);
        std::fs::create_dir_all(&path)
            .map_err(|error| ApiError::internal(format!("无法创建内置工作区：{error}")))?;
        if kind == "system" {
            let legacy = state.paths.data.join("skills");
            let target = path.join("skills");
            if legacy.is_dir() && !target.exists() {
                copy_tree_verified(&legacy, &target)?;
            } else {
                std::fs::create_dir_all(&target)
                    .map_err(|error| ApiError::internal(format!("无法创建技能目录：{error}")))?;
            }
        }
        let record = normalize_registration(json!({
            "workspace_id": id,
            "display_name": name,
            "path": path.display().to_string(),
            "kind": kind,
            "follows_root": true,
            "archived": false,
            "created_at": Utc::now().to_rfc3339(),
            "instructions": "",
            "resources": {
                "skills": null,
                "mcp": null,
                "knowledge_bases": null
            }
        }))?;
        state
            .database
            .put_resource(REGISTRATION_DOMAIN, id, &record)?;
        records.insert(id.to_string(), record);
    }
    let mut rows = records.into_values().collect::<Vec<_>>();
    rows.sort_by_key(|row| match row.get("kind").and_then(Value::as_str) {
        Some("system") => 0,
        Some("general") => 1,
        _ => 2,
    });
    Ok(rows)
}

fn describe_catalog(state: &AppState) -> ApiResult<Value> {
    let workspaces = ensure_builtin_workspaces(state)?
        .into_iter()
        .map(|record| registration_with_status(&record))
        .collect::<ApiResult<Vec<_>>>()?;
    Ok(json!({
        "root": managed_root(state)?.display().to_string(),
        "workspaces": workspaces,
        "migration": Value::Null
    }))
}

fn normalize_instructions(value: Option<&str>) -> ApiResult<String> {
    let value = value.unwrap_or("").trim();
    if value.chars().count() > MAX_INSTRUCTIONS {
        return Err(ApiError::bad_request(format!(
            "工作区指令不能超过 {MAX_INSTRUCTIONS} 个字符"
        )));
    }
    Ok(value.to_string())
}

fn normalize_workspace_resources(value: Option<Value>) -> ApiResult<Value> {
    let Some(value) = value else {
        return Ok(json!({
            "skills": null,
            "mcp": null,
            "knowledge_bases": null
        }));
    };
    let object = value
        .as_object()
        .ok_or_else(|| ApiError::bad_request("工作区资源必须是 JSON 对象"))?;
    let normalize = |key: &str| -> ApiResult<Value> {
        match object.get(key) {
            None | Some(Value::Null) => Ok(Value::Null),
            Some(Value::Array(items)) => {
                let mut unique = BTreeSet::new();
                for item in items {
                    let item = item.as_str().ok_or_else(|| {
                        ApiError::bad_request(format!("工作区资源 {key} 必须由字符串 ID 组成"))
                    })?;
                    if item.trim().is_empty() || item.chars().count() > 512 {
                        return Err(ApiError::bad_request(format!(
                            "工作区资源 {key} 包含无效 ID"
                        )));
                    }
                    unique.insert(item.to_string());
                }
                Ok(Value::Array(
                    unique.into_iter().map(Value::String).collect(),
                ))
            }
            _ => Err(ApiError::bad_request(format!(
                "工作区资源 {key} 必须是数组或 null"
            ))),
        }
    };
    Ok(json!({
        "skills": normalize("skills")?,
        "mcp": normalize("mcp")?,
        "knowledge_bases": normalize("knowledge_bases")?
    }))
}

fn normalize_registration(mut value: Value) -> ApiResult<Value> {
    let id = value
        .get("workspace_id")
        .or_else(|| value.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if id.is_empty() || id.chars().count() > 160 {
        return Err(ApiError::internal("工作区登记缺少有效标识"));
    }
    let display_name = value
        .get("display_name")
        .and_then(Value::as_str)
        .unwrap_or("工作区")
        .trim()
        .chars()
        .take(100)
        .collect::<String>();
    let path = value
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if path.is_empty() {
        return Err(ApiError::internal(format!("工作区 {id} 缺少存储路径")));
    }
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("workspace")
        .to_string();
    if !matches!(kind.as_str(), "system" | "general" | "workspace") {
        return Err(ApiError::internal(format!(
            "工作区 {id} 的类型无效：{kind}"
        )));
    }
    let resources = normalize_workspace_resources(value.get("resources").cloned())?;
    value["workspace_id"] = Value::String(id);
    value["id"] = value["workspace_id"].clone();
    value["display_name"] = Value::String(display_name);
    value["path"] = Value::String(path);
    value["kind"] = Value::String(kind);
    value["follows_root"] = Value::Bool(
        value
            .get("follows_root")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    );
    value["archived"] = Value::Bool(
        value
            .get("archived")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    );
    if value.get("created_at").and_then(Value::as_str).is_none() {
        value["created_at"] = Value::String(Utc::now().to_rfc3339());
    }
    value["instructions"] = Value::String(normalize_instructions(
        value.get("instructions").and_then(Value::as_str),
    )?);
    value["resources"] = resources;
    Ok(value)
}

fn validate_registered_path(state: &AppState, path: &FilePath) -> (String, String) {
    match confined_path(&state.paths.root, path) {
        Ok(path) if path.is_dir() => ("ready".to_string(), String::new()),
        Ok(_) => ("invalid".to_string(), "工作区目录不存在".to_string()),
        Err(error) => ("invalid".to_string(), error.message),
    }
}

fn registration_with_status(record: &Value) -> ApiResult<Value> {
    let mut row = normalize_registration(record.clone())?;
    let path = row
        .get("path")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .ok_or_else(|| ApiError::internal("工作区登记缺少路径"))?;
    let (status, error) = validate_registered_path_for_record(&path);
    row["status"] = Value::String(status);
    row["error"] = Value::String(error);
    Ok(row)
}

fn validate_registered_path_for_record(path: &FilePath) -> (String, String) {
    if path.is_symlink() {
        return (
            "invalid".to_string(),
            "工作区路径不能是符号链接".to_string(),
        );
    }
    if path.is_dir() {
        ("ready".to_string(), String::new())
    } else {
        ("invalid".to_string(), "工作区目录不存在".to_string())
    }
}

fn registration_record(state: &AppState, workspace_id: &str) -> ApiResult<Value> {
    ensure_builtin_workspaces(state)?;
    let workspace_id = if workspace_id.trim().is_empty() {
        GENERAL_WORKSPACE_ID
    } else {
        workspace_id.trim()
    };
    state
        .database
        .get_resource(REGISTRATION_DOMAIN, workspace_id)?
        .map(normalize_registration)
        .transpose()?
        .ok_or_else(|| ApiError::not_found("工作区不存在"))
}

fn target_registration(state: &AppState, workspace_id: &str) -> ApiResult<Option<Value>> {
    ensure_builtin_workspaces(state)?;
    let workspace_id = if workspace_id.trim().is_empty() {
        GENERAL_WORKSPACE_ID
    } else {
        workspace_id.trim()
    };
    state
        .database
        .get_resource(REGISTRATION_DOMAIN, workspace_id)?
        .map(normalize_registration)
        .transpose()
}

fn managed_root(state: &AppState) -> ApiResult<PathBuf> {
    let configured = state
        .database
        .get_setting(MANAGED_ROOT_KEY)?
        .and_then(|value| value.as_str().map(PathBuf::from));
    let path = configured.unwrap_or_else(|| state.paths.data.join("workspaces"));
    let path = confined_path(&state.paths.root, &path)?;
    std::fs::create_dir_all(&path)
        .map_err(|error| ApiError::internal(format!("无法创建工作区根目录：{error}")))?;
    Ok(path)
}

fn validate_new_workspace_path(state: &AppState, path: PathBuf) -> ApiResult<PathBuf> {
    if path.as_os_str().is_empty() {
        return Err(ApiError::bad_request("工作区路径不能为空"));
    }
    let path = confined_path(&state.paths.root, &path)?;
    if path == state.paths.root {
        return Err(ApiError::bad_request("不能直接使用安装根目录作为工作区"));
    }
    if path.exists() && !path.is_dir() {
        return Err(ApiError::bad_request("工作区路径必须指向文件夹"));
    }
    if path.is_symlink() {
        return Err(ApiError::bad_request("工作区路径不能是符号链接"));
    }
    Ok(path)
}

fn workspace_base(state: &AppState, workspace_id: &str) -> ApiResult<(PathBuf, bool)> {
    let record = target_registration(state, workspace_id)?
        .ok_or_else(|| ApiError::not_found("工作区不存在"))?;
    let path = PathBuf::from(
        record
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::internal("工作区登记缺少路径"))?,
    );
    let path = confined_path(&state.paths.root, &path)?;
    std::fs::create_dir_all(&path)
        .map_err(|error| ApiError::internal(format!("无法创建工作区目录：{error}")))?;
    let is_default = workspace_id.trim().is_empty()
        || record.get("kind").and_then(Value::as_str) == Some("general");
    Ok((path, is_default))
}

pub(super) fn workspace_root_for_tools(
    state: &AppState,
    workspace_id: &str,
) -> Result<PathBuf, String> {
    workspace_base(state, workspace_id)
        .map(|(path, _)| path)
        .map_err(|error| error.message)
}

fn feature_path(base: &FilePath, feature: &str, _is_default: bool) -> PathBuf {
    match feature {
        "chat" => base.join("chat"),
        "attachments" => base.join("chat").join("attachments"),
        "files" => base.join("library"),
        "knowledge_bases" => base.join("knowledge"),
        "parse_cache" => base.join("parse-cache"),
        "presentations" => base.join("workspace_presentations"),
        "historical_chat" => base.join("historical").join("chat"),
        "historical_agent" => base.join("historical").join("agent"),
        "historical_output" => base.join("historical").join("output"),
        "historical_archive" => base.join("historical").join("archive"),
        other => base.join(other),
    }
}

fn file_metrics(path: &FilePath) -> ApiResult<(usize, u64)> {
    if !path.exists() {
        return Ok((0, 0));
    }
    if path.is_symlink() {
        return Err(ApiError::conflict(format!(
            "数据目录包含符号链接：{}",
            path.display()
        )));
    }
    if path.is_file() {
        let size = path
            .metadata()
            .map_err(|error| ApiError::internal(format!("无法读取文件信息：{error}")))?
            .len();
        return Ok((1, size));
    }
    let mut files = 0usize;
    let mut bytes = 0u64;
    for entry in WalkDir::new(path).follow_links(false) {
        let entry =
            entry.map_err(|error| ApiError::internal(format!("无法遍历数据目录：{error}")))?;
        let kind = entry.file_type();
        if kind.is_symlink() {
            return Err(ApiError::conflict(format!(
                "数据目录包含符号链接：{}",
                entry.path().display()
            )));
        }
        if kind.is_file() {
            files += 1;
            bytes = bytes.saturating_add(
                entry
                    .metadata()
                    .map_err(|error| ApiError::internal(format!("无法读取文件信息：{error}")))?
                    .len(),
            );
        }
    }
    Ok((files, bytes))
}

fn session_count(state: &AppState, workspace_id: &str) -> ApiResult<usize> {
    let is_default = workspace_id.trim().is_empty()
        || workspace_id == GENERAL_WORKSPACE_ID
        || target_registration(state, workspace_id)?
            .as_ref()
            .and_then(|record| record.get("kind").and_then(Value::as_str))
            == Some("general");
    Ok(state.database.with_connection(|connection| {
        let count: i64 = if is_default {
            connection
                .query_row(
                    "SELECT COUNT(*) FROM sessions WHERE deleted_at IS NULL AND (workspace_id = '' OR workspace_id = ?1)",
                    params![GENERAL_WORKSPACE_ID],
                    |row| row.get(0),
                )
                .map_err(|error| format!("无法统计默认工作区会话：{error}"))?
        } else {
            connection
                .query_row(
                    "SELECT COUNT(*) FROM sessions WHERE deleted_at IS NULL AND workspace_id = ?1",
                    params![workspace_id],
                    |row| row.get(0),
                )
                .map_err(|error| format!("无法统计工作区会话：{error}"))?
        };
        Ok(count.max(0) as usize)
    })?)
}

fn has_active_turns(state: &AppState) -> ApiResult<bool> {
    let count = state.database.with_connection(|connection| {
        connection
            .query_row(
                "SELECT COUNT(*) FROM turns WHERE status IN ('queued','running','waiting_input')",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|error| format!("无法检查运行中的对话：{error}"))
    })?;
    Ok(count > 0)
}

fn ensure_no_active_turns(state: &AppState) -> ApiResult<()> {
    if !state.active_turns.lock().is_empty() || has_active_turns(state)? {
        return Err(ApiError::conflict(
            "请等待所有正在运行的对话结束后再迁移工作区",
        ));
    }
    Ok(())
}

fn discover_historical(base: &FilePath, is_default: bool) -> Vec<Value> {
    let mut candidates = vec![base.join("historical")];
    if is_default {
        candidates.push(base.join("learning").join("archive"));
    }
    let mut seen = BTreeSet::new();
    candidates
        .into_iter()
        .filter(|path| path.exists() && path.is_dir() && !path.is_symlink())
        .filter_map(|path| {
            let value = path.display().to_string();
            seen.insert(value.clone()).then(|| {
                json!({
                    "path": value,
                    "kind": "historical",
                    "export_only": true
                })
            })
        })
        .collect()
}

fn validate_operation_id(operation_id: &str) -> ApiResult<()> {
    Uuid::parse_str(operation_id)
        .map(|_| ())
        .map_err(|_| ApiError::bad_request("迁移操作 ID 无效"))
}

fn load_operations(state: &AppState) -> ApiResult<Vec<Value>> {
    Ok(state
        .database
        .get_setting(OPERATIONS_KEY)?
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default())
}

fn save_operations(state: &AppState, operations: &[Value]) -> ApiResult<()> {
    state
        .database
        .set_setting(OPERATIONS_KEY, &Value::Array(operations.to_vec()))?;
    Ok(())
}

fn append_operation(state: &AppState, operation: &Value) -> ApiResult<()> {
    let mut operations = load_operations(state)?;
    operations.retain(|row| row.get("id") != operation.get("id"));
    operations.insert(0, operation.clone());
    operations.truncate(100);
    save_operations(state, &operations)
}

fn update_operation(state: &AppState, operation: &Value) -> ApiResult<()> {
    let mut operations = load_operations(state)?;
    if let Some(index) = operations
        .iter()
        .position(|row| row.get("id") == operation.get("id"))
    {
        operations[index] = operation.clone();
    } else {
        operations.insert(0, operation.clone());
    }
    operations.truncate(100);
    save_operations(state, &operations)
}

fn migrate(state: &AppState, payload: &MigrationPayload, plan: Value) -> ApiResult<Value> {
    let operation_id = Uuid::new_v4().to_string();
    let created_at = Utc::now().to_rfc3339();
    let features = plan
        .get("features")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|value| value.as_str().map(str::to_string))
        .collect::<Vec<_>>();
    let recovery = state
        .paths
        .data
        .join("workspace-recovery")
        .join(&operation_id);
    let mut operation = json!({
        "id": operation_id,
        "kind": "migration",
        "status": "preparing",
        "source_workspace_id": payload.source_workspace_id,
        "target_workspace_id": payload.target_workspace_id,
        "features": features,
        "files": plan.get("files").cloned().unwrap_or_else(|| json!(0)),
        "bytes": plan.get("bytes").cloned().unwrap_or_else(|| json!(0)),
        "created_at": created_at,
        "updated_at": Utc::now().to_rfc3339(),
        "recovery_path": recovery.display().to_string(),
        "installed_paths": [],
        "session_ids": [],
        "error": ""
    });
    append_operation(state, &operation)?;
    let result = perform_migration(state, payload, &features, &recovery, &mut operation);
    operation["updated_at"] = Value::String(Utc::now().to_rfc3339());
    match result {
        Ok(()) => {
            operation["status"] = Value::String("completed".to_string());
            update_operation(state, &operation)?;
            Ok(operation)
        }
        Err(error) => {
            operation["status"] = Value::String("failed".to_string());
            operation["error"] = Value::String(error.message.clone());
            update_operation(state, &operation)?;
            Err(error)
        }
    }
}

fn perform_migration(
    state: &AppState,
    payload: &MigrationPayload,
    features: &[String],
    recovery: &FilePath,
    operation: &mut Value,
) -> ApiResult<()> {
    ensure_no_active_turns(state)?;
    std::fs::create_dir_all(recovery)
        .map_err(|error| ApiError::internal(format!("无法创建恢复目录：{error}")))?;
    let (source_base, source_default) = workspace_base(state, &payload.source_workspace_id)?;
    let (target_base, target_default) = workspace_base(state, &payload.target_workspace_id)?;
    let skip_attachments = features.iter().any(|feature| feature == "chat");
    let mut installed = Vec::new();
    let staging = target_base.join(format!(".data-migration-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&staging)
        .map_err(|error| ApiError::internal(format!("无法创建迁移暂存目录：{error}")))?;
    operation["status"] = Value::String("copying".to_string());
    update_operation(state, operation)?;

    for feature in features {
        if skip_attachments && feature == "attachments" {
            continue;
        }
        let source = feature_path(&source_base, feature, source_default);
        if !source.exists() {
            continue;
        }
        let recovery_path = recovery.join(feature);
        copy_tree_verified(&source, &recovery_path)?;
        let stage = staging.join(feature);
        copy_tree_verified(&source, &stage)?;
        installed.push(feature_path(&target_base, feature, target_default));
    }

    operation["status"] = Value::String("transferring".to_string());
    update_operation(state, operation)?;
    for (feature, destination) in features
        .iter()
        .filter(|feature| !(skip_attachments && feature.as_str() == "attachments"))
        .map(|feature| (feature, feature_path(&target_base, feature, target_default)))
    {
        let stage = staging.join(feature);
        if !stage.exists() {
            continue;
        }
        merge_directory(&stage, &destination)?;
    }

    if features.iter().any(|feature| feature == "chat") {
        let session_ids = session_ids_for_workspace(state, &payload.source_workspace_id)?;
        let exported = export_sessions_value(state, &session_ids)?;
        write_json_file(&recovery.join("sessions.json"), &exported)?;
        operation["session_ids"] = json!(session_ids);
        operation["status"] = Value::String("committed".to_string());
        update_operation(state, operation)?;
        move_sessions_to_workspace(state, &session_ids, &payload.target_workspace_id)?;
    }
    operation["installed_paths"] = json!(installed
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>());
    if staging.exists() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    Ok(())
}

fn export(state: &AppState, payload: &MigrationPayload) -> ApiResult<Value> {
    if payload.features.is_empty() {
        return Err(ApiError::bad_request("请至少选择一个数据功能"));
    }
    for feature in &payload.features {
        validate_feature(feature)?;
    }
    let operation_id = Uuid::new_v4().to_string();
    let directory = state
        .paths
        .data
        .join("workspace-exports")
        .join(&operation_id);
    std::fs::create_dir_all(&directory)
        .map_err(|error| ApiError::internal(format!("无法创建导出目录：{error}")))?;
    let archive_path = directory.join("export.zip");
    let mut operation = json!({
        "id": operation_id,
        "kind": "export",
        "status": "preparing",
        "source_workspace_id": payload.source_workspace_id,
        "features": payload.features,
        "created_at": Utc::now().to_rfc3339(),
        "updated_at": Utc::now().to_rfc3339(),
        "download_url": format!("/api/settings/workspace/data/exports/{operation_id}"),
        "error": ""
    });
    append_operation(state, &operation)?;
    let result = build_export_archive(state, payload, &archive_path);
    operation["updated_at"] = Value::String(Utc::now().to_rfc3339());
    match result {
        Ok((files, bytes)) => {
            operation["status"] = Value::String("exported".to_string());
            operation["files"] = Value::from(files);
            operation["bytes"] = Value::from(bytes);
            update_operation(state, &operation)?;
            Ok(operation)
        }
        Err(error) => {
            operation["status"] = Value::String("failed".to_string());
            operation["error"] = Value::String(error.message.clone());
            update_operation(state, &operation)?;
            Err(error)
        }
    }
}

fn build_export_archive(
    state: &AppState,
    payload: &MigrationPayload,
    archive_path: &FilePath,
) -> ApiResult<(usize, u64)> {
    let (base, is_default) = workspace_base(state, &payload.source_workspace_id)?;
    let file = std::fs::File::create(archive_path)
        .map_err(|error| ApiError::internal(format!("无法创建导出归档：{error}")))?;
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o644);
    let mut rewritten = BTreeSet::new();
    let skip_attachments = payload.features.iter().any(|feature| feature == "chat");
    let mut files = 0usize;
    let mut bytes = 0u64;
    for feature in &payload.features {
        if skip_attachments && feature == "attachments" {
            continue;
        }
        let root = feature_path(&base, feature, is_default);
        for entry in WalkDir::new(&root).follow_links(false) {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) if !root.exists() => break,
                Err(error) => return Err(ApiError::internal(format!("无法遍历导出目录：{error}"))),
            };
            if entry.file_type().is_symlink() {
                return Err(ApiError::conflict(format!(
                    "导出目录包含符号链接：{}",
                    entry.path().display()
                )));
            }
            if !entry.file_type().is_file() {
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(&root)
                .map_err(|error| ApiError::internal(error.to_string()))?
                .to_string_lossy()
                .replace('\\', "/");
            let name = format!("{feature}/{relative}");
            if !rewritten.insert(name.clone()) {
                continue;
            }
            add_file_to_zip(&mut zip, options, &name, entry.path())?;
            files += 1;
            bytes = bytes.saturating_add(entry.metadata().map(|meta| meta.len()).unwrap_or(0));
        }
    }
    if payload.features.iter().any(|feature| feature == "chat") {
        let ids = session_ids_for_workspace(state, &payload.source_workspace_id)?;
        let sessions = export_sessions_value(state, &ids)?;
        let data = serde_json::to_vec_pretty(&sessions)
            .map_err(|error| ApiError::internal(format!("无法序列化会话：{error}")))?;
        zip.start_file("chat/conversations.json", options)
            .map_err(|error| ApiError::internal(format!("无法写入会话归档：{error}")))?;
        zip.write_all(&data)
            .map_err(|error| ApiError::internal(format!("无法写入会话归档：{error}")))?;
        files += 1;
        bytes = bytes.saturating_add(data.len() as u64);
    }
    if payload.include_historical {
        for historical in discover_historical(&base, is_default) {
            let Some(path) = historical.get("path").and_then(Value::as_str) else {
                continue;
            };
            add_tree_to_zip(&mut zip, options, FilePath::new(path), "historical-extra")?;
        }
    }
    let manifest = json!({
        "format": "deeptutor-portable-export-v1",
        "generated_at": Utc::now().to_rfc3339(),
        "source_workspace_id": payload.source_workspace_id,
        "features": payload.features,
        "include_historical": payload.include_historical
    });
    zip.start_file("manifest.json", options)
        .map_err(|error| ApiError::internal(format!("无法写入导出清单：{error}")))?;
    zip.write_all(
        serde_json::to_string_pretty(&manifest)
            .map_err(|error| ApiError::internal(error.to_string()))?
            .as_bytes(),
    )
    .map_err(|error| ApiError::internal(format!("无法写入导出清单：{error}")))?;
    zip.finish()
        .map_err(|error| ApiError::internal(format!("无法完成导出归档：{error}")))?;
    Ok((files, bytes))
}

fn add_tree_to_zip(
    zip: &mut ZipWriter<std::fs::File>,
    options: SimpleFileOptions,
    root: &FilePath,
    prefix: &str,
) -> ApiResult<()> {
    for entry in WalkDir::new(root).follow_links(false) {
        let entry =
            entry.map_err(|error| ApiError::internal(format!("无法遍历历史目录：{error}")))?;
        if entry.file_type().is_symlink() {
            return Err(ApiError::conflict("历史目录包含符号链接"));
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
        add_file_to_zip(zip, options, &format!("{prefix}/{relative}"), entry.path())?;
    }
    Ok(())
}

fn add_file_to_zip(
    zip: &mut ZipWriter<std::fs::File>,
    options: SimpleFileOptions,
    name: &str,
    path: &FilePath,
) -> ApiResult<()> {
    zip.start_file(name, options)
        .map_err(|error| ApiError::internal(format!("无法写入导出文件 {name}：{error}")))?;
    let mut file = std::fs::File::open(path)
        .map_err(|error| ApiError::internal(format!("无法读取导出文件：{error}")))?;
    std::io::copy(&mut file, zip)
        .map_err(|error| ApiError::internal(format!("无法写入导出文件 {name}：{error}")))?;
    Ok(())
}

fn session_ids_for_workspace(state: &AppState, workspace_id: &str) -> ApiResult<Vec<String>> {
    let is_default = workspace_id.trim().is_empty()
        || workspace_id == GENERAL_WORKSPACE_ID
        || target_registration(state, workspace_id)?
            .as_ref()
            .and_then(|record| record.get("kind").and_then(Value::as_str))
            == Some("general");
    Ok(state.database.with_connection(|connection| {
        let sql = if is_default {
            "SELECT id FROM sessions WHERE deleted_at IS NULL AND (workspace_id = '' OR workspace_id = ?1) ORDER BY created_at"
        } else {
            "SELECT id FROM sessions WHERE deleted_at IS NULL AND workspace_id = ?1 ORDER BY created_at"
        };
        let mut statement = connection
            .prepare(sql)
            .map_err(|error| format!("无法准备会话导出查询：{error}"))?;
        let rows = statement
            .query_map(params![if is_default { GENERAL_WORKSPACE_ID } else { workspace_id }], |row| {
                row.get::<_, String>(0)
            })
            .map_err(|error| format!("无法读取会话列表：{error}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("无法解析会话列表：{error}"))
    })?)
}

fn export_sessions_value(state: &AppState, session_ids: &[String]) -> ApiResult<Value> {
    Ok(state.database.with_connection(|connection| {
        let mut sessions = Vec::new();
        for session_id in session_ids {
            let session = connection
                .query_row(
                    r#"
                    SELECT id, title, created_at, updated_at, status,
                           preferences_json, workspace_id, archived
                    FROM sessions WHERE id = ?1
                    "#,
                    params![session_id],
                    |row| {
                        let preferences: String = row.get(5)?;
                        Ok(json!({
                            "id": row.get::<_, String>(0)?,
                            "title": row.get::<_, String>(1)?,
                            "created_at": row.get::<_, f64>(2)?,
                            "updated_at": row.get::<_, f64>(3)?,
                            "status": row.get::<_, String>(4)?,
                            "preferences": serde_json::from_str::<Value>(&preferences).unwrap_or_else(|_| json!({})),
                            "workspace_id": row.get::<_, String>(6)?,
                            "archived": row.get::<_, i64>(7)? != 0
                        }))
                    },
                )
                .map_err(|error| format!("无法读取会话 {session_id}：{error}"))?;
            let mut messages = Vec::new();
            let mut statement = connection
                .prepare(
                    r#"
                    SELECT id, role, content, capability, events_json,
                           attachments_json, metadata_json, created_at,
                           parent_message_id
                    FROM messages WHERE session_id = ?1 ORDER BY id
                    "#,
                )
                .map_err(|error| format!("无法准备消息导出：{error}"))?;
            let rows = statement
                .query_map(params![session_id], |row| {
                    let events: String = row.get(4)?;
                    let attachments: String = row.get(5)?;
                    let metadata: String = row.get(6)?;
                    Ok(json!({
                        "id": row.get::<_, i64>(0)?,
                        "role": row.get::<_, String>(1)?,
                        "content": row.get::<_, String>(2)?,
                        "capability": row.get::<_, Option<String>>(3)?,
                        "events": serde_json::from_str::<Value>(&events).unwrap_or_else(|_| json!([])),
                        "attachments": serde_json::from_str::<Value>(&attachments).unwrap_or_else(|_| json!([])),
                        "metadata": serde_json::from_str::<Value>(&metadata).unwrap_or_else(|_| json!({})),
                        "created_at": row.get::<_, f64>(7)?,
                        "parent_message_id": row.get::<_, Option<i64>>(8)?
                    }))
                })
                .map_err(|error| format!("无法读取消息导出：{error}"))?;
            for message in rows {
                messages.push(message.map_err(|error| format!("无法解析消息导出：{error}"))?);
            }
            let mut session = session;
            session["messages"] = Value::Array(messages);
            sessions.push(session);
        }
        Ok(json!({
            "format": "deeptutor-sessions-v1",
            "exported_at": Utc::now().to_rfc3339(),
            "sessions": sessions
        }))
    })?)
}

fn move_sessions_to_workspace(
    state: &AppState,
    session_ids: &[String],
    target_workspace_id: &str,
) -> ApiResult<()> {
    Ok(state.database.with_connection(|connection| {
        let transaction = connection
            .transaction()
            .map_err(|error| format!("无法开始会话迁移事务：{error}"))?;
        for session_id in session_ids {
            let preferences: Option<String> = transaction
                .query_row(
                    "SELECT preferences_json FROM sessions WHERE id = ?1",
                    params![session_id],
                    |row| row.get(0),
                )
                .ok();
            let mut preferences = preferences
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                .unwrap_or_else(|| json!({}));
            preferences["workspace_id"] = Value::String(target_workspace_id.to_string());
            transaction
                .execute(
                    "UPDATE sessions SET workspace_id = ?2, preferences_json = ?3, updated_at = ?4 WHERE id = ?1",
                    params![
                        session_id,
                        target_workspace_id,
                        preferences.to_string(),
                        crate::storage::now()
                    ],
                )
                .map_err(|error| format!("无法迁移会话 {session_id}：{error}"))?;
        }
        transaction
            .commit()
            .map_err(|error| format!("无法提交会话迁移：{error}"))
    })?)
}

fn knowledge_move_plan(
    state: &AppState,
    source_id: &str,
    target_workspace_id: &str,
) -> ApiResult<Value> {
    ensure_builtin_workspaces(state)?;
    let (source_workspace_id, name) = parse_knowledge_id(source_id)?;
    let mut knowledge_bases = state.database.list_resources("knowledge_base")?;
    let kb = knowledge_bases
        .iter_mut()
        .find(|row| {
            row.get("name").and_then(Value::as_str) == Some(name.as_str())
                || row.get("id").and_then(Value::as_str) == Some(name.as_str())
                || source_id.ends_with(&format!(":kb:{name}"))
        })
        .ok_or_else(|| ApiError::not_found("知识库不存在"))?
        .clone();
    let stored_source = kb
        .get("storage_workspace_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let source_workspace_id = if source_workspace_id.is_empty() {
        stored_source.to_string()
    } else {
        source_workspace_id
    };
    let (source_base, _) = workspace_base(state, &source_workspace_id)?;
    let (target_base, _) = workspace_base(state, target_workspace_id)?;
    let source_path = kb
        .get("path")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| feature_path(&source_base, "knowledge_bases", false).join(&name));
    let source_path = confined_path(&state.paths.root, &source_path)?;
    let target_path = confined_path(
        &state.paths.root,
        &feature_path(&target_base, "knowledge_bases", false).join(&name),
    )?;
    let mut blockers = Vec::new();
    if source_workspace_id == target_workspace_id {
        blockers.push("请选择不同的目标工作区".to_string());
    }
    if !source_path.is_dir() || source_path.is_symlink() {
        blockers.push("源知识库目录不存在或不是普通目录".to_string());
    }
    if target_path.exists() || target_path.is_symlink() {
        blockers.push("目标工作区已存在同名知识库".to_string());
    }
    let source_record = target_registration(state, &source_workspace_id)?
        .ok_or_else(|| ApiError::not_found("来源工作区不存在"))?;
    let target_record = target_registration(state, target_workspace_id)?
        .ok_or_else(|| ApiError::not_found("目标工作区不存在"))?;
    if source_record.get("archived").and_then(Value::as_bool) == Some(true)
        || target_record.get("archived").and_then(Value::as_bool) == Some(true)
    {
        blockers.push("请先恢复来源或目标工作区".to_string());
    }
    let old_ids = json!([
        source_id,
        name,
        format!("{source_workspace_id}:kb:{name}"),
        format!(":kb:{name}")
    ]);
    let old_set = old_ids
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<BTreeSet<_>>();
    let assignments = ensure_builtin_workspaces(state)?
        .into_iter()
        .filter(|record| {
            record
                .get("resources")
                .and_then(|resources| resources.get("knowledge_bases"))
                .and_then(Value::as_array)
                .is_some_and(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .any(|item| old_set.contains(item))
                })
        })
        .map(|record| {
            json!({
                "workspace_id": record.get("workspace_id").cloned().unwrap_or(Value::Null),
                "display_name": record.get("display_name").cloned().unwrap_or(Value::Null)
            })
        })
        .collect::<Vec<_>>();
    let (files, bytes) = if source_path.is_dir() && !source_path.is_symlink() {
        file_metrics(&source_path)?
    } else {
        (0, 0)
    };
    let target_id = format!("{target_workspace_id}:kb:{name}");
    Ok(json!({
        "source_id": source_id,
        "target_id": target_id,
        "source_workspace_id": source_workspace_id,
        "target_workspace_id": target_workspace_id,
        "name": name,
        "source_path": source_path.display().to_string(),
        "target_path": target_path.display().to_string(),
        "files": files,
        "bytes": bytes,
        "assignments": assignments,
        "blockers": blockers
    }))
}

fn parse_knowledge_id(source_id: &str) -> ApiResult<(String, String)> {
    let source_id = source_id.trim();
    if source_id.is_empty() || source_id.chars().count() > 512 {
        return Err(ApiError::bad_request("请选择有效的知识库"));
    }
    let (workspace, name) = if let Some((workspace, name)) = source_id.rsplit_once(":kb:") {
        (workspace.to_string(), name.to_string())
    } else {
        (String::new(), source_id.to_string())
    };
    let name = name.trim();
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|'])
    {
        return Err(ApiError::bad_request("知识库标识包含无效字符"));
    }
    Ok((workspace, name.to_string()))
}

fn rewrite_workspace_resource_assignments(
    state: &AppState,
    old_ids: &Value,
    target_id: &Value,
) -> ApiResult<()> {
    let old_ids = old_ids
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<BTreeSet<_>>();
    let target_id = target_id
        .as_str()
        .ok_or_else(|| ApiError::internal("知识库目标 ID 无效"))?;
    for mut record in ensure_builtin_workspaces(state)? {
        let Some(items) = record
            .get("resources")
            .and_then(|value| value.get("knowledge_bases"))
            .and_then(Value::as_array)
            .cloned()
        else {
            continue;
        };
        let mut changed = false;
        let mut seen = BTreeSet::new();
        let mut next = Vec::new();
        for item in items {
            let Some(value) = item.as_str() else {
                continue;
            };
            let value = if old_ids.contains(value) {
                changed = true;
                target_id
            } else {
                value
            };
            if seen.insert(value.to_string()) {
                next.push(Value::String(value.to_string()));
            }
        }
        if changed {
            record["resources"]["knowledge_bases"] = Value::Array(next);
            let id = record
                .get("workspace_id")
                .and_then(Value::as_str)
                .ok_or_else(|| ApiError::internal("工作区登记缺少 ID"))?;
            state
                .database
                .put_resource(REGISTRATION_DOMAIN, id, &record)?;
        }
    }
    Ok(())
}

fn write_system_snapshot(state: &AppState) -> ApiResult<Value> {
    let system = registration_record(state, SYSTEM_WORKSPACE_ID)?;
    let root = PathBuf::from(
        system
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::internal("系统工作区缺少路径"))?,
    );
    let root = confined_path(&state.paths.root, &root)?;
    std::fs::create_dir_all(&root)
        .map_err(|error| ApiError::internal(format!("无法创建系统工作区：{error}")))?;
    let catalog = state
        .database
        .get_setting("catalog")?
        .unwrap_or_else(|| json!({}));
    let mut models = Map::new();
    if let Some(services) = catalog.get("services").and_then(Value::as_object) {
        for (service, value) in services {
            let profiles = value
                .get("profiles")
                .and_then(Value::as_array)
                .map(|profiles| {
                    profiles
                        .iter()
                        .map(|profile| {
                            let models = profile
                                .get("models")
                                .and_then(Value::as_array)
                                .map(|items| {
                                    items
                                        .iter()
                                        .map(|model| {
                                            json!({
                                                "id": model.get("id").cloned().unwrap_or(Value::Null),
                                                "name": model.get("name").cloned().unwrap_or(Value::Null),
                                                "model": model.get("model").cloned().unwrap_or(Value::Null)
                                            })
                                        })
                                        .collect::<Vec<_>>()
                                })
                                .unwrap_or_default();
                            json!({
                                "id": profile.get("id").cloned().unwrap_or(Value::Null),
                                "name": profile.get("name").cloned().unwrap_or(Value::Null),
                                "binding": profile.get("binding").cloned().unwrap_or(Value::Null),
                                "endpoint_origin": endpoint_origin(profile.get("base_url")),
                                "models": models
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            models.insert(
                service.clone(),
                json!({
                    "active_profile_id": value.get("active_profile_id").cloned().unwrap_or(Value::Null),
                    "active_model_id": value.get("active_model_id").cloned().unwrap_or(Value::Null),
                    "profiles": profiles
                }),
            );
        }
    }
    let mcp_value = state
        .database
        .get_setting("mcp")?
        .unwrap_or_else(|| json!({}));
    let servers = mcp_value
        .get("servers")
        .and_then(Value::as_object)
        .map(|servers| {
            servers
                .iter()
                .map(|(name, server)| {
                    json!({
                        "name": name,
                        "enabled": server.get("enabled").cloned().unwrap_or_else(|| json!(true)),
                        "transport": server.get("transport").cloned().unwrap_or_else(|| json!("unknown")),
                        "endpoint_origin": endpoint_origin(
                            server.get("url").or_else(|| server.get("endpoint"))
                        )
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let snapshot = json!({
        "generated_at": Utc::now().to_rfc3339(),
        "models": models,
        "mcp": servers
    });
    write_json_file(&root.join("system-snapshot.json"), &snapshot)?;
    write_json_file(
        &state
            .paths
            .data
            .join("settings")
            .join("system-workspace-snapshot.json"),
        &snapshot,
    )?;
    Ok(snapshot)
}

fn endpoint_origin(value: Option<&Value>) -> Value {
    let Some(raw) = value.and_then(Value::as_str) else {
        return Value::String(String::new());
    };
    url::Url::parse(raw)
        .ok()
        .and_then(|url| {
            let host = url.host_str()?;
            Some(url::Url::parse(&format!("{}://{host}", url.scheme())).ok())
        })
        .flatten()
        .map(|url| Value::String(url.origin().ascii_serialization()))
        .unwrap_or_else(|| Value::String(String::new()))
}

fn write_json_file(path: &FilePath, value: &Value) -> ApiResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| ApiError::internal(format!("无法创建 JSON 目录：{error}")))?;
    }
    let data = serde_json::to_vec_pretty(value)
        .map_err(|error| ApiError::internal(format!("无法序列化 JSON：{error}")))?;
    let temporary = path.with_extension(format!("tmp-{}", Uuid::new_v4()));
    std::fs::write(&temporary, data)
        .map_err(|error| ApiError::internal(format!("无法写入 JSON 文件：{error}")))?;
    if path.exists() {
        std::fs::remove_file(path)
            .map_err(|error| ApiError::internal(format!("无法替换 JSON 文件：{error}")))?;
    }
    std::fs::rename(&temporary, path)
        .map_err(|error| ApiError::internal(format!("无法发布 JSON 文件：{error}")))
}

fn migrate_workspace_directory(
    state: &AppState,
    workspace_id: &str,
    record: &Value,
    destination: &str,
) -> ApiResult<()> {
    let target = validate_new_workspace_path(state, PathBuf::from(destination))?;
    let source = PathBuf::from(
        record
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::internal("工作区登记缺少路径"))?,
    );
    let source = confined_path(&state.paths.root, &source)?;
    if source == target {
        return Ok(());
    }
    if target.exists() {
        let mut entries = std::fs::read_dir(&target)
            .map_err(|error| ApiError::bad_request(format!("无法读取目标目录：{error}")))?;
        if entries.next().is_some() {
            return Err(ApiError::conflict("目标工作区目录必须为空"));
        }
    }
    let parent = target
        .parent()
        .ok_or_else(|| ApiError::bad_request("目标工作区路径缺少父目录"))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| ApiError::internal(format!("无法创建目标父目录：{error}")))?;
    let staging = parent.join(format!(".workspace-migrate-{}", Uuid::new_v4()));
    copy_tree_verified(&source, &staging)?;
    if target.exists() {
        std::fs::remove_dir(&target)
            .map_err(|error| ApiError::conflict(format!("目标目录不是空文件夹：{error}")))?;
    }
    std::fs::rename(&staging, &target)
        .map_err(|error| ApiError::internal(format!("无法发布迁移后的工作区：{error}")))?;
    let mut updated = record.clone();
    updated["path"] = Value::String(target.display().to_string());
    updated["follows_root"] = Value::Bool(false);
    if let Err(error) = state
        .database
        .put_resource(REGISTRATION_DOMAIN, workspace_id, &updated)
    {
        let _ = std::fs::remove_dir_all(&target);
        return Err(ApiError::internal(error));
    }
    if source.exists() && source != target {
        std::fs::remove_dir_all(&source)
            .map_err(|error| ApiError::internal(format!("无法清理旧工作区目录：{error}")))?;
    }
    Ok(())
}

fn copy_tree_verified(source: &FilePath, target: &FilePath) -> ApiResult<()> {
    if !source.exists() {
        return Ok(());
    }
    if source.is_symlink() || target.is_symlink() {
        return Err(ApiError::conflict("迁移目录不能包含符号链接"));
    }
    if target.exists() {
        let mut entries = std::fs::read_dir(target)
            .map_err(|error| ApiError::internal(format!("无法读取复制目标：{error}")))?;
        if entries.next().is_some() {
            return Err(ApiError::conflict(format!(
                "复制目标必须为空：{}",
                target.display()
            )));
        }
        std::fs::remove_dir(target)
            .map_err(|error| ApiError::internal(format!("无法准备复制目标：{error}")))?;
    }
    if source.is_file() {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| ApiError::internal(format!("无法创建复制目录：{error}")))?;
        }
        copy_file_verified(source, target)?;
        return Ok(());
    }
    std::fs::create_dir_all(target)
        .map_err(|error| ApiError::internal(format!("无法创建复制目录：{error}")))?;
    let result = (|| {
        for entry in WalkDir::new(source).follow_links(false) {
            let entry =
                entry.map_err(|error| ApiError::internal(format!("无法遍历复制来源：{error}")))?;
            let relative = entry
                .path()
                .strip_prefix(source)
                .map_err(|error| ApiError::internal(error.to_string()))?;
            if relative.as_os_str().is_empty() {
                continue;
            }
            let destination = target.join(relative);
            if entry.file_type().is_symlink() {
                return Err(ApiError::conflict(format!(
                    "迁移来源包含符号链接：{}",
                    entry.path().display()
                )));
            }
            if entry.file_type().is_dir() {
                std::fs::create_dir_all(&destination).map_err(|error| {
                    ApiError::internal(format!("无法创建目录 {}：{error}", destination.display()))
                })?;
            } else if entry.file_type().is_file() {
                copy_file_verified(entry.path(), &destination)?;
            } else {
                return Err(ApiError::conflict("工作区包含不支持的特殊文件"));
            }
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(target);
    }
    result
}

fn copy_file_verified(source: &FilePath, target: &FilePath) -> ApiResult<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| ApiError::internal(format!("无法创建复制目录：{error}")))?;
    }
    let before = hash_file(source)?;
    std::fs::copy(source, target)
        .map_err(|error| ApiError::internal(format!("无法复制文件：{error}")))?;
    let after_source = hash_file(source)?;
    let after_target = hash_file(target)?;
    if before != after_source || before != after_target {
        return Err(ApiError::conflict(format!(
            "复制期间源文件发生变化：{}",
            source.display()
        )));
    }
    Ok(())
}

fn hash_file(path: &FilePath) -> ApiResult<String> {
    let mut file = std::fs::File::open(path)
        .map_err(|error| ApiError::internal(format!("无法读取文件：{error}")))?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| ApiError::internal(format!("无法读取文件：{error}")))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(hex::encode(digest.finalize()))
}

fn merge_directory(source: &FilePath, target: &FilePath) -> ApiResult<()> {
    if !source.exists() {
        return Ok(());
    }
    if source.is_symlink() || target.is_symlink() {
        return Err(ApiError::conflict("合并目录不能包含符号链接"));
    }
    std::fs::create_dir_all(target)
        .map_err(|error| ApiError::internal(format!("无法创建目标目录：{error}")))?;
    for entry in WalkDir::new(source).follow_links(false) {
        let entry =
            entry.map_err(|error| ApiError::internal(format!("无法遍历待合并目录：{error}")))?;
        let relative = entry
            .path()
            .strip_prefix(source)
            .map_err(|error| ApiError::internal(error.to_string()))?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        let destination = target.join(relative);
        if entry.file_type().is_symlink() {
            return Err(ApiError::conflict("待合并目录包含符号链接"));
        }
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&destination)
                .map_err(|error| ApiError::internal(format!("无法创建目录：{error}")))?;
        } else if entry.file_type().is_file() {
            if destination.exists() {
                return Err(ApiError::conflict(format!(
                    "目标位置已存在同名文件：{}",
                    destination.display()
                )));
            }
            copy_file_verified(entry.path(), &destination)?;
        } else {
            return Err(ApiError::conflict("待迁移数据包含不支持的特殊文件"));
        }
    }
    Ok(())
}

fn ensure_inside(candidate: &FilePath, root: &FilePath) -> ApiResult<PathBuf> {
    confined_path(root, candidate)
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
        .any(|component| matches!(component, std::path::Component::ParentDir))
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
    let ancestor = std::fs::canonicalize(ancestor)
        .map_err(|error| ApiError::bad_request(format!("无法解析路径：{error}")))?;
    if !ancestor.starts_with(&root) {
        return Err(ApiError::forbidden(
            "为保持便携性，所有工作区数据必须位于安装目录内",
        ));
    }
    let suffix = absolute
        .strip_prefix(
            absolute
                .ancestors()
                .find(|path| path.exists())
                .ok_or_else(|| ApiError::bad_request("路径无有效父目录"))?,
        )
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    let resolved = ancestor.join(suffix);
    if resolved.exists() && resolved.is_symlink() {
        return Err(ApiError::bad_request("目标路径不能是符号链接"));
    }
    Ok(resolved)
}
