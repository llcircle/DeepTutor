use super::documents;
use super::error::{ApiError, ApiResult};
use super::files;
use super::AppState;
use super::ScopedState;
use axum::body::Bytes;
use axum::extract::{Multipart, Path as AxumPath, Query, State};
use axum::http::{header, HeaderMap};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;
use walkdir::WalkDir;

const CHUNK_SIZE: usize = 1_200;
const CHUNK_OVERLAP: usize = 180;
const INDEX_FORMAT_VERSION: u32 = 1;

#[derive(Default)]
struct UploadForm {
    name: String,
    rag_provider: String,
    storage_workspace_id: String,
    pageindex_mode: String,
    search_mode: String,
    embedding_model: String,
    dest_subdir: String,
    rel_paths: Vec<String>,
    files: Vec<IncomingFile>,
}

struct IncomingFile {
    filename: String,
    bytes: Bytes,
}

#[derive(Debug, Deserialize)]
struct FolderPayload {
    path: String,
}

#[derive(Debug, Deserialize)]
struct MovePayload {
    source: String,
    dest_folder: String,
}

#[derive(Debug, Deserialize)]
struct NamePayload {
    name: String,
}

#[derive(Debug, Default, Deserialize)]
struct LibraryQuery {
    resource_library: Option<bool>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/knowledge-bases", get(list).post(create))
        .route("/knowledge-bases/list", get(list))
        .route("/knowledge-bases/health", get(health))
        .route(
            "/knowledge-bases/supported-file-types",
            get(supported_types),
        )
        .route("/knowledge-bases/delete", post(delete_by_name))
        .route("/knowledge-bases/default", get(default_kb))
        .route("/knowledge-bases/default/{kb_name}", put(set_default))
        .route("/knowledge-bases/rag-providers", get(rag_providers))
        .route("/knowledge-bases/embedding-usage", get(embedding_usage))
        .route(
            "/knowledge-bases/rag-pipelines/{provider}/config",
            get(get_provider_config).put(put_provider_config),
        )
        .route(
            "/knowledge-bases/rag-pipelines/{provider}/preflight",
            get(provider_preflight),
        )
        .route("/knowledge-bases/{kb_name}", get(get_kb).delete(delete_kb))
        .route("/knowledge-bases/{kb_name}/upload", post(upload))
        .route("/knowledge-bases/{kb_name}/files", get(list_files))
        .route("/knowledge-bases/{kb_name}/files/move", post(move_file))
        .route(
            "/knowledge-bases/{kb_name}/files/{*filename}",
            get(serve_file).delete(delete_file),
        )
        .route(
            "/knowledge-bases/{kb_name}/file-preview-text/{*filename}",
            get(file_preview_text),
        )
        .route("/knowledge-bases/{kb_name}/folders", post(create_folder))
        .route(
            "/knowledge-bases/{kb_name}/config",
            get(get_config).put(put_config),
        )
        .route("/knowledge-bases/{kb_name}/reindex", post(reindex))
        .route("/knowledge-bases/{kb_name}/retry", post(retry))
        .route(
            "/knowledge-bases/{kb_name}/indexing-readiness",
            get(indexing_readiness),
        )
        .route("/knowledge-bases/{kb_name}/indexing-run", get(indexing_run))
        .route(
            "/knowledge-bases/{kb_name}/indexing-run/{task_id}/cancel",
            post(cancel_indexing_run),
        )
        .route("/knowledge-bases/{kb_name}/progress", get(progress))
        .route(
            "/knowledge-bases/{kb_name}/reindex-config",
            get(reindex_config),
        )
        .route("/knowledge-bases/tasks/{task_id}/stream", get(task_stream))
}

async fn list(
    ScopedState(state): ScopedState,
    Query(query): Query<LibraryQuery>,
) -> ApiResult<Json<Value>> {
    let mut bases = state.database.list_resources("knowledge_base")?;
    if query.resource_library == Some(true) {
        bases.retain(|base| {
            base.get("source").and_then(Value::as_str) != Some("admin")
                || base.get("assigned").and_then(Value::as_bool) != Some(false)
        });
    }
    Ok(Json(Value::Array(bases)))
}

async fn create(ScopedState(state): ScopedState, multipart: Multipart) -> ApiResult<Json<Value>> {
    let form = collect_form(multipart, true).await?;
    if form.name.trim().is_empty() {
        return Err(ApiError::bad_request("缺少知识库名称。"));
    }
    create_or_upload(state, form, true).await
}

async fn upload(
    ScopedState(state): ScopedState,
    AxumPath(kb_name): AxumPath<String>,
    multipart: Multipart,
) -> ApiResult<Json<Value>> {
    let mut form = collect_form(multipart, false).await?;
    form.name = kb_name;
    create_or_upload(state, form, false).await
}

async fn collect_form(mut multipart: Multipart, require_name: bool) -> ApiResult<UploadForm> {
    let mut form = UploadForm::default();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|error| ApiError::bad_request(format!("无法读取上传表单：{error}")))?
    {
        let name = field.name().unwrap_or("").to_string();
        if name == "files" {
            let filename = field
                .file_name()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or("attachment.bin")
                .to_string();
            let bytes = field
                .bytes()
                .await
                .map_err(|error| ApiError::bad_request(format!("无法读取上传文件：{error}")))?;
            form.files.push(IncomingFile { filename, bytes });
            continue;
        }
        let value = field
            .text()
            .await
            .map_err(|error| ApiError::bad_request(format!("无法读取表单字段 {name}：{error}")))?;
        match name.as_str() {
            "name" => form.name = value,
            "rag_provider" => form.rag_provider = value,
            "storage_workspace_id" => form.storage_workspace_id = value,
            "pageindex_mode" => form.pageindex_mode = value,
            "search_mode" => form.search_mode = value,
            "embedding_model" => form.embedding_model = value,
            "dest_subdir" => form.dest_subdir = value,
            "rel_paths" => form.rel_paths.push(value),
            _ => {}
        }
    }
    if require_name && form.name.trim().is_empty() {
        return Err(ApiError::bad_request("缺少知识库名称。"));
    }
    Ok(form)
}

async fn create_or_upload(
    state: AppState,
    form: UploadForm,
    create: bool,
) -> ApiResult<Json<Value>> {
    let name = normalized_kb_name(&form.name)?;
    let existing = state.database.get_resource("knowledge_base", &name)?;
    if create && existing.is_some() {
        return Err(ApiError::conflict(format!("知识库“{name}”已经存在。")));
    }
    if !create && existing.is_none() {
        return Err(ApiError::not_found("知识库不存在。"));
    }
    let slug = kb_slug(&name);
    let root = state.paths.data.join("knowledge").join(&slug);
    let raw_root = root.join("raw");
    std::fs::create_dir_all(&raw_root)
        .map_err(|error| ApiError::internal(format!("无法创建知识库目录：{error}")))?;

    let provider = if form.rag_provider.trim().is_empty() {
        existing
            .as_ref()
            .and_then(|value| value.get("rag_provider"))
            .and_then(Value::as_str)
            .unwrap_or("native")
    } else {
        form.rag_provider.trim()
    };
    let destination_root = if form.dest_subdir.trim().is_empty() {
        raw_root.clone()
    } else {
        let relative = safe_relative_path(&form.dest_subdir, true)?;
        raw_root.join(relative)
    };
    std::fs::create_dir_all(&destination_root)
        .map_err(|error| ApiError::internal(format!("无法创建知识库子目录：{error}")))?;

    let mut saved = 0usize;
    let mut total_bytes = 0u64;
    for (index, file) in form.files.iter().enumerate() {
        if file.bytes.is_empty() {
            continue;
        }
        let relative = upload_relative_path(file, form.rel_paths.get(index), &form)?;
        let some = save_raw_file(&raw_root, &relative, &file.bytes)?;
        total_bytes = total_bytes.saturating_add(some);
        saved += 1;
    }

    let now = crate::storage::now();
    let mut payload = existing.clone().unwrap_or_else(|| {
        json!({
            "name": name,
            "created_at": now,
            "source": "user",
            "assigned": true,
            "read_only": false,
            "is_default": false
        })
    });
    payload["name"] = Value::String(name.clone());
    payload["rag_provider"] = Value::String(provider.to_string());
    payload["status"] = if form.files.is_empty() {
        existing
            .as_ref()
            .and_then(|value| value.get("status"))
            .cloned()
            .unwrap_or_else(|| Value::String("ready".to_string()))
    } else {
        Value::String("indexing".to_string())
    };
    payload["path"] = Value::String(root.display().to_string());
    payload["storage_workspace_id"] = Value::String(form.storage_workspace_id.clone());
    payload["pageindex_mode"] = Value::String(form.pageindex_mode.clone());
    payload["search_mode"] = Value::String(form.search_mode.clone());
    payload["embedding_model"] = Value::String(form.embedding_model.clone());
    payload["file_count"] = Value::from(
        existing
            .as_ref()
            .and_then(|value| value.get("file_count"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
            + saved as u64,
    );
    payload["total_bytes"] = Value::from(total_bytes);
    payload["updated_at"] = Value::from(now);
    state
        .database
        .put_resource("knowledge_base", &name, &payload)?;

    let task_id = format!("kb-task-{}", Uuid::new_v4());
    let message = if saved == 0 {
        "知识库已创建，等待上传文件。".to_string()
    } else {
        format!("已保存 {saved} 个文件，原生索引任务已在后台启动。")
    };
    if saved == 0 {
        state.database.put_resource(
            "knowledge_task",
            &task_id,
            &json!({
                "task_id": task_id,
                "knowledge_base": name,
                "state": "completed",
                "message": message,
                "created_at": now,
                "finished_at": crate::storage::now()
            }),
        )?;
    } else {
        start_index_task(&state, &name, &task_id)?;
    }
    Ok(Json(json!({
        "id": task_id,
        "task_id": task_id,
        "message": message,
        "noop": saved == 0,
        "status": if saved == 0 { "completed" } else { "running" }
    })))
}

struct IndexSummary {
    indexed_files: usize,
    chunks: usize,
    failed_files: usize,
    cancelled: bool,
}

pub(super) fn start_index_task(state: &AppState, kb_name: &str, task_id: &str) -> ApiResult<()> {
    let started_at = crate::storage::now();
    let initial_run = json!({
        "task_id": task_id,
        "state": "queued",
        "phase": "queued",
        "started_at": started_at,
        "last_activity_at": started_at,
        "last_progress_at": started_at,
        "phase_current": 0,
        "phase_total": 0,
        "cancel_requested": false,
        "recovery": "none",
        "documents": {},
        "usable_version": Value::Null,
        "error": Value::Null
    });
    state
        .database
        .put_resource("kb_index_run", kb_name, &initial_run)?;
    state.database.put_resource(
        "knowledge_task",
        task_id,
        &json!({
            "task_id": task_id,
            "knowledge_base": kb_name,
            "state": "running",
            "message": "索引任务已加入后台队列。",
            "created_at": started_at
        }),
    )?;

    let state = state.clone();
    let kb_name = kb_name.to_string();
    let task_id = task_id.to_string();
    tokio::spawn(async move {
        let result = index_knowledge_base(&state, &kb_name, &task_id).await;
        let (status, message) = match result {
            Ok(summary) if summary.cancelled => (
                "cancelled",
                format!(
                    "索引已取消，保留旧索引；本次处理 {} 个片段。",
                    summary.chunks
                ),
            ),
            Ok(summary) => (
                if summary.failed_files > 0 {
                    "completed_with_warnings"
                } else {
                    "completed"
                },
                format!(
                    "索引完成：{} 个文档，{} 个片段，{} 个失败。",
                    summary.indexed_files, summary.chunks, summary.failed_files
                ),
            ),
            Err(error) => ("failed", error),
        };
        if let Err(error) = state.database.put_resource(
            "knowledge_task",
            &task_id,
            &json!({
                "task_id": task_id,
                "knowledge_base": kb_name,
                "state": status,
                "message": message,
                "finished_at": crate::storage::now()
            }),
        ) {
            tracing::error!(%error, task_id, "无法写入知识库索引任务结果");
        }
    });
    Ok(())
}

async fn index_knowledge_base(
    state: &AppState,
    kb_name: &str,
    task_id: &str,
) -> Result<IndexSummary, String> {
    let kb = state
        .database
        .get_resource("knowledge_base", kb_name)?
        .ok_or_else(|| "知识库不存在。".to_string())?;
    let root = PathBuf::from(
        kb.get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| "知识库缺少存储路径。".to_string())?,
    );
    let raw_root = root.join("raw");
    let mut run = state
        .database
        .get_resource("kb_index_run", kb_name)?
        .unwrap_or_else(|| json!({}));
    run["task_id"] = Value::String(task_id.to_string());
    run["state"] = Value::String("running".to_string());
    run["phase"] = Value::String("parsing".to_string());
    run["last_activity_at"] = Value::from(crate::storage::now());
    run["last_progress_at"] = Value::from(crate::storage::now());
    run["cancel_requested"] = Value::Bool(false);
    state.database.put_resource("kb_index_run", kb_name, &run)?;

    let mut files = WalkDir::new(&raw_root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .collect::<Vec<_>>();
    files.sort();
    run["phase_total"] = Value::from(files.len());
    let mut documents_status = serde_json::Map::new();
    let mut indexed_files = 0usize;
    let mut chunk_count = 0usize;
    let mut failed_files = 0usize;

    for (file_index, path) in files.iter().enumerate() {
        let cancellation_requested = state
            .database
            .get_resource("kb_index_run", kb_name)?
            .and_then(|value| value.get("cancel_requested").and_then(Value::as_bool))
            .unwrap_or(false);
        if cancellation_requested {
            for staged in state.database.list_resources("kb_chunk_staging")? {
                if staged.get("task_id").and_then(Value::as_str) == Some(task_id) {
                    if let Some(id) = staged.get("id").and_then(Value::as_str) {
                        state.database.delete_resource("kb_chunk_staging", id)?;
                    }
                }
            }
            let finished_at = crate::storage::now();
            run["state"] = Value::String("cancelled".to_string());
            run["phase"] = Value::String("ready".to_string());
            run["finished_at"] = Value::from(finished_at);
            run["last_activity_at"] = Value::from(finished_at);
            run["last_progress_at"] = Value::from(finished_at);
            run["activity"] = Value::String("索引已取消，旧索引保持不变。".to_string());
            state.database.put_resource("kb_index_run", kb_name, &run)?;
            let mut kb = kb;
            kb["status"] = Value::String("ready".to_string());
            kb["progress"] = json!({
                "state": "cancelled",
                "processed": file_index,
                "total": files.len()
            });
            state
                .database
                .put_resource("knowledge_base", kb_name, &kb)?;
            return Ok(IndexSummary {
                indexed_files: 0,
                chunks: chunk_count,
                failed_files,
                cancelled: true,
            });
        }
        let relative = path
            .strip_prefix(&raw_root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        run["phase_current"] = Value::from(file_index + 1);
        run["current_document"] = Value::String(relative.clone());
        run["activity"] = Value::String("正在解析并切块".to_string());
        run["last_activity_at"] = Value::from(crate::storage::now());
        state.database.put_resource("kb_index_run", kb_name, &run)?;

        let extracted = super::parsing::extract_path(&state, path).await;
        match extracted {
            Ok(document) => {
                let parse_reused = document
                    .metadata
                    .get("parse_reused")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let parser = document
                    .metadata
                    .get("parse_engine")
                    .and_then(Value::as_str)
                    .unwrap_or("native")
                    .to_string();
                let document_metadata = document.metadata.clone();
                let chunks = documents::chunk_text(&document.text, CHUNK_SIZE, CHUNK_OVERLAP);
                for (chunk_index, chunk) in chunks.iter().enumerate() {
                    let id = format!(
                        "{}:{:x}:{chunk_index}",
                        kb_name,
                        Sha256::digest(relative.as_bytes())
                    );
                    state.database.put_resource(
                        "kb_chunk_staging",
                        &id,
                        &json!({
                            "kb_name": kb_name,
                            "source": relative,
                            "locator": {
                                "unit": "chunk",
                                "index": chunk_index,
                                "total": chunks.len()
                            },
                            "content": chunk,
                            "metadata": document_metadata.clone(),
                            "index_format_version": INDEX_FORMAT_VERSION,
                            "task_id": task_id
                        }),
                    )?;
                }
                documents_status.insert(
                    relative,
                    json!({
                        "status": "indexed",
                        "parse_reused": parse_reused,
                        "chunks": chunks.len(),
                        "parser": parser
                    }),
                );
                indexed_files += 1;
                chunk_count += chunks.len();
            }
            Err(error) => {
                failed_files += 1;
                documents_status.insert(
                    relative,
                    json!({"status": "failed", "reason": error, "parse_reused": false}),
                );
            }
        }
        run["documents"] = Value::Object(documents_status.clone());
        run["last_progress_at"] = Value::from(crate::storage::now());
        state.database.put_resource("kb_index_run", kb_name, &run)?;
    }

    let cancellation_requested = state
        .database
        .get_resource("kb_index_run", kb_name)?
        .and_then(|value| value.get("cancel_requested").and_then(Value::as_bool))
        .unwrap_or(false);
    if cancellation_requested {
        for staged in state.database.list_resources("kb_chunk_staging")? {
            if staged.get("task_id").and_then(Value::as_str) == Some(task_id) {
                if let Some(id) = staged.get("id").and_then(Value::as_str) {
                    state.database.delete_resource("kb_chunk_staging", id)?;
                }
            }
        }
        let finished_at = crate::storage::now();
        run["state"] = Value::String("cancelled".to_string());
        run["phase"] = Value::String("ready".to_string());
        run["finished_at"] = Value::from(finished_at);
        run["last_activity_at"] = Value::from(finished_at);
        run["last_progress_at"] = Value::from(finished_at);
        run["activity"] = Value::String("索引已取消，旧索引保持不变。".to_string());
        state.database.put_resource("kb_index_run", kb_name, &run)?;
        return Ok(IndexSummary {
            indexed_files: 0,
            chunks: chunk_count,
            failed_files,
            cancelled: true,
        });
    }

    if indexed_files > 0 {
        let mut staged_chunks = state
            .database
            .list_resources("kb_chunk_staging")?
            .into_iter()
            .filter(|value| value.get("task_id").and_then(Value::as_str) == Some(task_id))
            .collect::<Vec<_>>();
        staged_chunks.sort_by(|left, right| {
            left.get("source")
                .and_then(Value::as_str)
                .cmp(&right.get("source").and_then(Value::as_str))
                .then_with(|| {
                    left.pointer("/locator/index")
                        .and_then(Value::as_u64)
                        .cmp(&right.pointer("/locator/index").and_then(Value::as_u64))
                })
        });
        for old in state.database.list_resources("kb_chunk")? {
            if old.get("kb_name").and_then(Value::as_str) == Some(kb_name) {
                if let Some(id) = old.get("id").and_then(Value::as_str) {
                    state.database.delete_resource("kb_chunk", id)?;
                }
            }
        }
        for staged in staged_chunks {
            let Some(id) = staged.get("id").and_then(Value::as_str).map(str::to_string) else {
                continue;
            };
            state.database.put_resource("kb_chunk", &id, &staged)?;
            state.database.delete_resource("kb_chunk_staging", &id)?;
        }
    }

    let finished_at = crate::storage::now();
    let state_label = if failed_files > 0 && indexed_files == 0 {
        "failed"
    } else if failed_files > 0 {
        "completed_with_warnings"
    } else {
        "completed"
    };
    run["state"] = Value::String(state_label.to_string());
    run["phase"] = Value::String("ready".to_string());
    run["finished_at"] = Value::from(finished_at);
    run["last_activity_at"] = Value::from(finished_at);
    run["last_progress_at"] = Value::from(finished_at);
    run["documents"] = Value::Object(documents_status);
    run["usable_version"] = Value::from(INDEX_FORMAT_VERSION);
    run["error"] = if failed_files > 0 {
        Value::String(format!("{failed_files} 个文件无法提取文本，已保留原文件。"))
    } else {
        Value::Null
    };
    state.database.put_resource("kb_index_run", kb_name, &run)?;

    let mut kb = kb;
    kb["status"] = Value::String(state_label.to_string());
    kb["file_count"] = Value::from(indexed_files);
    kb["statistics"] = json!({
        "documents": indexed_files,
        "chunks": chunk_count,
        "failed_documents": failed_files
    });
    kb["progress"] = json!({
        "state": state_label,
        "processed": files.len(),
        "total": files.len()
    });
    kb["index_format_version"] = Value::from(INDEX_FORMAT_VERSION);
    kb["last_indexed_at"] = Value::from(finished_at);
    state
        .database
        .put_resource("knowledge_base", kb_name, &kb)?;
    Ok(IndexSummary {
        indexed_files,
        chunks: chunk_count,
        failed_files,
        cancelled: false,
    })
}

async fn health() -> Json<Value> {
    Json(json!({
        "status": "healthy",
        "engine": "native_rust",
        "formats": ["pdf", "docx", "pptx", "xlsx", "epub", "html", "txt", "md", "csv", "json"]
    }))
}

async fn supported_types() -> Json<Value> {
    let extensions = [
        ".pdf",
        ".docx",
        ".pptx",
        ".xlsx",
        ".epub",
        ".txt",
        ".md",
        ".markdown",
        ".html",
        ".htm",
        ".csv",
        ".tsv",
        ".json",
        ".jsonc",
        ".yaml",
        ".yml",
        ".toml",
        ".xml",
        ".svg",
        ".rst",
        ".tex",
        ".bib",
        ".py",
        ".rs",
        ".ts",
        ".tsx",
        ".js",
        ".jsx",
        ".java",
        ".go",
        ".c",
        ".cpp",
        ".h",
        ".hpp",
        ".sh",
        ".ps1",
        ".sql",
    ];
    Json(json!({
        "extensions": extensions.to_vec(),
        "accept": extensions.join(","),
        "max_file_size_bytes": 200 * 1024 * 1024,
        "allow_any_extension": true,
        "types": extensions.iter().map(|value| value.trim_start_matches('.')).collect::<Vec<_>>(),
        "engine": "rust"
    }))
}

async fn default_kb(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(
        state
            .database
            .get_setting("default_kb")?
            .unwrap_or(Value::Null),
    ))
}

async fn set_default(
    ScopedState(state): ScopedState,
    AxumPath(kb_name): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    let name = normalized_kb_name(&kb_name)?;
    if state
        .database
        .get_resource("knowledge_base", &name)?
        .is_none()
    {
        return Err(ApiError::not_found("知识库不存在。"));
    }
    let mut bases = state.database.list_resources("knowledge_base")?;
    for base in &mut bases {
        let current = base
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let id = base
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or(&current)
            .to_string();
        base["is_default"] = Value::Bool(id == name || current == name);
        state.database.put_resource("knowledge_base", &id, base)?;
    }
    state
        .database
        .set_setting("default_kb", &Value::String(name.clone()))?;
    Ok(Json(Value::String(name)))
}

async fn get_kb(
    ScopedState(state): ScopedState,
    AxumPath(kb_name): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    let name = normalized_kb_name(&kb_name)?;
    state
        .database
        .get_resource("knowledge_base", &name)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("知识库不存在。"))
}

async fn delete_kb(
    ScopedState(state): ScopedState,
    AxumPath(kb_name): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    delete_knowledge_base(&state, &kb_name)?;
    Ok(Json(json!({"deleted": true, "name": kb_name})))
}

async fn delete_by_name(
    ScopedState(state): ScopedState,
    Json(body): Json<NamePayload>,
) -> ApiResult<Json<Value>> {
    delete_knowledge_base(&state, &body.name)?;
    Ok(Json(json!({"deleted": true, "name": body.name})))
}

fn delete_knowledge_base(state: &AppState, name: &str) -> ApiResult<()> {
    let name = normalized_kb_name(name)?;
    let kb = state
        .database
        .get_resource("knowledge_base", &name)?
        .ok_or_else(|| ApiError::not_found("知识库不存在。"))?;
    for chunk in state.database.list_resources("kb_chunk")? {
        if chunk.get("kb_name").and_then(Value::as_str) == Some(name.as_str()) {
            if let Some(id) = chunk.get("id").and_then(Value::as_str) {
                state.database.delete_resource("kb_chunk", id)?;
            }
        }
    }
    state.database.delete_resource("kb_index_run", &name)?;
    state.database.delete_resource("knowledge_base", &name)?;
    if let Some(path) = kb.get("path").and_then(Value::as_str) {
        let root = Path::new(path);
        if root.starts_with(&state.paths.data) && root.exists() {
            std::fs::remove_dir_all(root)
                .map_err(|error| ApiError::internal(format!("无法删除知识库目录：{error}")))?;
        }
    }
    Ok(())
}

async fn list_files(
    ScopedState(state): ScopedState,
    AxumPath(kb_name): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    let name = normalized_kb_name(&kb_name)?;
    let raw_root = kb_raw_root(&state, &name)?;
    let mut files = Vec::new();
    if raw_root.is_dir() {
        let mut entries = WalkDir::new(&raw_root)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.path().cmp(right.path()));
        for entry in entries {
            if entry.depth() == 0 {
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(&raw_root)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .replace('\\', "/");
            if entry.file_type().is_dir() {
                files.push(json!({"name": format!("{relative}/"), "type": "folder"}));
            } else if entry.file_type().is_file() {
                let metadata = entry.metadata().ok();
                files.push(json!({
                    "name": relative,
                    "type": "file",
                    "size": metadata.as_ref().map(|value| value.len()).unwrap_or(0),
                    "modified": metadata
                        .and_then(|value| value.modified().ok())
                        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|value| value.as_secs_f64())
                        .unwrap_or(0.0),
                    "mime_type": mime_guess::from_path(entry.path()).first_or_octet_stream().to_string()
                }));
            }
        }
    }
    Ok(Json(json!({"files": files})))
}

async fn serve_file(
    ScopedState(state): ScopedState,
    AxumPath((kb_name, filename)): AxumPath<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let path = raw_file_path(&state, &kb_name, &filename)?;
    Ok(files::serve_path(path, &headers).await)
}

async fn file_preview_text(
    ScopedState(state): ScopedState,
    AxumPath((kb_name, filename)): AxumPath<(String, String)>,
) -> ApiResult<Response> {
    let path = raw_file_path(&state, &kb_name, &filename)?;
    let extracted = super::parsing::extract_path(&state, &path)
        .await
        .map_err(ApiError::bad_request)?;
    Ok((
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "private, max-age=300"),
        ],
        extracted.text,
    )
        .into_response())
}

async fn delete_file(
    ScopedState(state): ScopedState,
    AxumPath((kb_name, filename)): AxumPath<(String, String)>,
) -> ApiResult<Json<Value>> {
    let path = raw_file_path(&state, &kb_name, &filename)?;
    let name = normalized_kb_name(&kb_name)?;
    let was_indexed = state
        .database
        .list_resources("kb_chunk")?
        .iter()
        .any(|chunk| {
            chunk.get("kb_name").and_then(Value::as_str) == Some(name.as_str())
                && chunk.get("source").and_then(Value::as_str) == Some(filename.as_str())
        });
    std::fs::remove_file(&path)
        .map_err(|error| ApiError::internal(format!("无法删除知识库文件：{error}")))?;
    for chunk in state.database.list_resources("kb_chunk")? {
        if chunk.get("kb_name").and_then(Value::as_str) == Some(name.as_str())
            && chunk.get("source").and_then(Value::as_str) == Some(filename.as_str())
        {
            if let Some(id) = chunk.get("id").and_then(Value::as_str) {
                state.database.delete_resource("kb_chunk", id)?;
            }
        }
    }
    Ok(Json(json!({"deleted": true, "was_indexed": was_indexed})))
}

async fn create_folder(
    ScopedState(state): ScopedState,
    AxumPath(kb_name): AxumPath<String>,
    Json(body): Json<FolderPayload>,
) -> ApiResult<Json<Value>> {
    let root = kb_raw_root(&state, &kb_name)?;
    let relative = safe_relative_path(&body.path, true)?;
    std::fs::create_dir_all(root.join(&relative))
        .map_err(|error| ApiError::internal(format!("无法创建知识库文件夹：{error}")))?;
    Ok(Json(json!({"created": true, "path": body.path})))
}

async fn move_file(
    ScopedState(state): ScopedState,
    AxumPath(kb_name): AxumPath<String>,
    Json(body): Json<MovePayload>,
) -> ApiResult<Json<Value>> {
    let root = kb_raw_root(&state, &kb_name)?;
    let source_relative = safe_relative_path(&body.source, false)?;
    let source = root.join(&source_relative);
    if !source.is_file() {
        return Err(ApiError::not_found("源文件不存在。"));
    }
    let folder = safe_relative_path(&body.dest_folder, true)?;
    let target_dir = root.join(folder);
    std::fs::create_dir_all(&target_dir)
        .map_err(|error| ApiError::internal(format!("无法创建目标文件夹：{error}")))?;
    let filename = source
        .file_name()
        .ok_or_else(|| ApiError::bad_request("源文件缺少文件名。"))?;
    let target = target_dir.join(filename);
    if target.exists() {
        return Err(ApiError::conflict("目标文件夹已有同名文件。"));
    }
    std::fs::rename(&source, &target)
        .map_err(|error| ApiError::internal(format!("无法移动知识库文件：{error}")))?;
    Ok(Json(json!({
        "moved": true,
        "source": body.source,
        "dest_folder": body.dest_folder
    })))
}

async fn get_config(
    ScopedState(state): ScopedState,
    AxumPath(kb_name): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    let name = normalized_kb_name(&kb_name)?;
    let kb = state
        .database
        .get_resource("knowledge_base", &name)?
        .ok_or_else(|| ApiError::not_found("知识库不存在。"))?;
    Ok(Json(json!({
        "name": name,
        "rag_provider": kb.get("rag_provider").cloned().unwrap_or_else(|| Value::String("native".into())),
        "search_mode": kb.get("search_mode").cloned().unwrap_or(Value::Null),
        "pageindex_mode": kb.get("pageindex_mode").cloned().unwrap_or(Value::Null),
        "embedding_model": kb.get("embedding_model").cloned().unwrap_or(Value::Null),
        "chunk_size": CHUNK_SIZE,
        "chunk_overlap": CHUNK_OVERLAP
    })))
}

async fn put_config(
    ScopedState(state): ScopedState,
    AxumPath(kb_name): AxumPath<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let name = normalized_kb_name(&kb_name)?;
    let mut kb = state
        .database
        .get_resource("knowledge_base", &name)?
        .ok_or_else(|| ApiError::not_found("知识库不存在。"))?;
    if let Value::Object(patch) = body {
        for (key, value) in patch {
            if matches!(
                key.as_str(),
                "rag_provider"
                    | "search_mode"
                    | "pageindex_mode"
                    | "embedding_model"
                    | "description"
            ) {
                kb[&key] = value;
            }
        }
    }
    state.database.put_resource("knowledge_base", &name, &kb)?;
    get_config(ScopedState(state), AxumPath(name)).await
}

async fn reindex(
    ScopedState(state): ScopedState,
    AxumPath(kb_name): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    start_reindex(state, kb_name).await
}

async fn retry(
    ScopedState(state): ScopedState,
    AxumPath(kb_name): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    start_reindex(state, kb_name).await
}

async fn start_reindex(state: AppState, kb_name: String) -> ApiResult<Json<Value>> {
    let name = normalized_kb_name(&kb_name)?;
    if state
        .database
        .get_resource("knowledge_base", &name)?
        .is_none()
    {
        return Err(ApiError::not_found("知识库不存在。"));
    }
    let task_id = format!("kb-task-{}", Uuid::new_v4());
    start_index_task(&state, &name, &task_id)?;
    let message = "索引任务已在后台启动，旧索引在新索引完成前保持可用。";
    Ok(Json(json!({
        "id": task_id,
        "task_id": task_id,
        "message": message,
        "noop": false,
        "status": "running"
    })))
}

async fn indexing_readiness(
    ScopedState(state): ScopedState,
    AxumPath(kb_name): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    let raw_root = kb_raw_root(&state, &kb_name)?;
    let files = count_files(&raw_root);
    Ok(Json(json!({
        "ready": files > 0,
        "engine": "native_rust",
        "file_count": files,
        "checks": [
            {"key": "storage", "label": "便携存储目录", "ok": true, "detail": raw_root.display().to_string(), "optional": false},
            {"key": "documents", "label": "可解析文档", "ok": files > 0, "detail": format!("{files} 个文件"), "optional": false}
        ]
    })))
}

async fn indexing_run(
    ScopedState(state): ScopedState,
    AxumPath(kb_name): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    let name = normalized_kb_name(&kb_name)?;
    Ok(Json(json!({
        "run": state.database.get_resource("kb_index_run", &name)?
    })))
}

async fn cancel_indexing_run(
    ScopedState(state): ScopedState,
    AxumPath((kb_name, task_id)): AxumPath<(String, String)>,
) -> ApiResult<Json<Value>> {
    let name = normalized_kb_name(&kb_name)?;
    let mut run = state
        .database
        .get_resource("kb_index_run", &name)?
        .ok_or_else(|| ApiError::not_found("当前没有索引任务。"))?;
    if run.get("task_id").and_then(Value::as_str) != Some(task_id.as_str()) {
        return Err(ApiError::conflict("索引任务 ID 不匹配。"));
    }
    run["cancel_requested"] = Value::Bool(true);
    run["activity"] = Value::String("将在当前安全边界停止".to_string());
    state.database.put_resource("kb_index_run", &name, &run)?;
    Ok(Json(json!({"cancelled": true, "task_id": task_id})))
}

async fn progress(
    ScopedState(state): ScopedState,
    AxumPath(kb_name): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    let name = normalized_kb_name(&kb_name)?;
    let kb = state
        .database
        .get_resource("knowledge_base", &name)?
        .ok_or_else(|| ApiError::not_found("知识库不存在。"))?;
    Ok(Json(kb.get("progress").cloned().unwrap_or_else(
        || json!({"status": "idle", "processed": 0, "total": 0}),
    )))
}

async fn reindex_config(
    ScopedState(state): ScopedState,
    AxumPath(kb_name): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    let name = normalized_kb_name(&kb_name)?;
    let kb = state
        .database
        .get_resource("knowledge_base", &name)?
        .ok_or_else(|| ApiError::not_found("知识库不存在。"))?;
    let fingerprint = format!(
        "{:x}",
        Sha256::digest(
            format!(
                "{}:{}:{}:{}",
                name,
                kb.get("rag_provider")
                    .and_then(Value::as_str)
                    .unwrap_or("native"),
                CHUNK_SIZE,
                CHUNK_OVERLAP
            )
            .as_bytes()
        )
    );
    Ok(Json(json!({
        "fingerprint": fingerprint,
        "indexing_policy": {
            "chunk_size": CHUNK_SIZE,
            "chunk_overlap": CHUNK_OVERLAP
        },
        "embedding": {
            "model": kb.get("embedding_model").and_then(Value::as_str).unwrap_or("native-lexical"),
            "dimension": 0
        },
        "embedding_selection": Value::Null
    })))
}

async fn rag_providers(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    let modes = state
        .database
        .get_setting("rag_provider_modes")?
        .unwrap_or_else(|| json!({}));
    let providers = [
        (
            "native",
            "Rust 原生检索",
            "便携、离线优先的全文检索与知识库索引。",
        ),
        (
            "llamaindex",
            "LlamaIndex",
            "使用已配置的嵌入模型与混合检索。",
        ),
        ("lightrag", "LightRAG", "图增强知识检索。"),
        ("pageindex", "PageIndex", "结构化页面索引。"),
    ];
    Ok(Json(json!({
        "providers": providers.into_iter().map(|(id, name, description)| json!({
            "id": id,
            "name": name,
            "description": description,
            "configured": id == "native" || id != "pageindex",
            "requires_api_key": id == "pageindex",
            "modes": if id == "native" { json!(["hybrid", "keyword"]) } else { json!(["hybrid"]) },
            "default_mode": modes.get(id).and_then(Value::as_str).unwrap_or("hybrid"),
            "linkable": id == "lightrag",
            "setup_required": false
        })).collect::<Vec<_>>()
    })))
}

async fn embedding_usage(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(json!({
        "knowledge_bases": state
            .database
            .list_resources("knowledge_base")?
            .into_iter()
            .filter_map(|kb| {
                let selection = kb.get("embedding_model")?.as_str()?;
                if selection.trim().is_empty() {
                    return None;
                }
                Some(json!({
                    "profile_id": "",
                    "model_id": selection,
                    "name": kb.get("name").cloned().unwrap_or(Value::Null),
                    "workspace_name": kb.get("storage_workspace_id").cloned().unwrap_or(Value::Null)
                }))
            })
            .collect::<Vec<_>>()
    })))
}

async fn get_provider_config(
    ScopedState(state): ScopedState,
    AxumPath(provider): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    Ok(Json(
        state
            .database
            .get_setting(&format!("rag_provider_config:{provider}"))?
            .unwrap_or_else(|| default_provider_config(&provider)),
    ))
}

async fn put_provider_config(
    ScopedState(state): ScopedState,
    AxumPath(provider): AxumPath<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let value = state
        .database
        .merge_setting(&format!("rag_provider_config:{provider}"), &body)?;
    Ok(Json(value))
}

async fn provider_preflight(
    ScopedState(state): ScopedState,
    AxumPath(provider): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    let config = state
        .database
        .get_setting(&format!("rag_provider_config:{provider}"))?
        .unwrap_or_else(|| default_provider_config(&provider));
    let key_ok = provider == "native"
        || provider == "llamaindex"
        || config.get("api_key_set").and_then(Value::as_bool) == Some(true)
        || config
            .get("server_url")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty());
    Ok(Json(json!({
        "ok": key_ok,
        "checks": [{
            "key": "configuration",
            "label": "检索引擎配置",
            "ok": key_ok,
            "detail": if key_ok { "配置可用" } else { "需要补充 API Key 或服务地址" },
            "optional": false
        }]
    })))
}

async fn task_stream(
    ScopedState(state): ScopedState,
    AxumPath(task_id): AxumPath<String>,
) -> ApiResult<Response> {
    let task = state
        .database
        .get_resource("knowledge_task", &task_id)?
        .ok_or_else(|| ApiError::not_found("知识库任务不存在。"))?;
    let payload = json!({
        "type": "done",
        "task_id": task_id,
        "status": task.get("state").cloned().unwrap_or_else(|| Value::String("completed".into())),
        "message": task.get("message").cloned().unwrap_or(Value::Null)
    });
    Ok((
        [
            (header::CONTENT_TYPE, "text/event-stream; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        format!("event: done\ndata: {}\n\n", payload),
    )
        .into_response())
}

fn default_provider_config(provider: &str) -> Value {
    match provider {
        "llamaindex" => json!({
            "version": 1,
            "retrieval_profile": "hybrid",
            "top_k": 8,
            "vector_top_k_multiplier": 3,
            "bm25_top_k_multiplier": 3,
            "reranker_model": "",
            "rerank_top_k": 24,
            "vector_index_type": "flat",
            "hnsw_m": 32,
            "hnsw_ef_construction": 200,
            "hnsw_ef_search": 80,
            "chunk_size": CHUNK_SIZE,
            "chunk_overlap": CHUNK_OVERLAP,
            "image_description_concurrency": 2,
            "image_description_timeout_seconds": 60
        }),
        "graphrag" => json!({
            "version": 1,
            "response_type": "multiple paragraphs",
            "community_level": 2,
            "dynamic_community_selection": false
        }),
        "lightrag-server" => json!({
            "server_url": "",
            "api_key_set": false,
            "configured": false
        }),
        "pageindex" => json!({"api_key_set": false, "configured": false}),
        "ima" => json!({"client_id": "", "api_key_set": false, "configured": false}),
        _ => json!({
            "version": 1,
            "top_k": 8,
            "response_type": "multiple paragraphs",
            "max_concurrent_files": 2,
            "llm_model_max_async": 2,
            "entity_extract_max_gleaning": 1,
            "llm_timeout": 120,
            "llm_profile_id": "",
            "llm_model_id": ""
        }),
    }
}

fn upload_relative_path(
    file: &IncomingFile,
    rel_path: Option<&String>,
    form: &UploadForm,
) -> ApiResult<String> {
    let raw = rel_path
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .unwrap_or_else(|| file.filename.clone());
    let raw = if form.dest_subdir.trim().is_empty() {
        raw
    } else {
        format!(
            "{}/{}",
            form.dest_subdir.trim().trim_matches(['/', '\\']),
            raw.trim_start_matches(['/', '\\'])
        )
    };
    safe_relative_path(&raw, false)
}

fn save_raw_file(root: &Path, relative: &str, bytes: &[u8]) -> ApiResult<u64> {
    let path = root.join(relative);
    if !path.starts_with(root) {
        return Err(ApiError::bad_request("上传路径逃逸出知识库目录。"));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| ApiError::internal(format!("无法创建上传目录：{error}")))?;
    }
    std::fs::write(&path, bytes)
        .map_err(|error| ApiError::internal(format!("无法保存上传文件：{error}")))?;
    Ok(bytes.len() as u64)
}

fn safe_relative_path(value: &str, allow_empty: bool) -> ApiResult<String> {
    let normalized = value.trim().replace('\\', "/");
    if normalized.is_empty() {
        return if allow_empty {
            Ok(String::new())
        } else {
            Err(ApiError::bad_request("文件路径不能为空。"))
        };
    }
    if normalized.starts_with('/') || normalized.contains(':') || normalized.contains('\0') {
        return Err(ApiError::bad_request("文件路径必须是相对路径。"));
    }
    let mut parts = Vec::new();
    for component in Path::new(&normalized).components() {
        match component {
            Component::Normal(part) => {
                let part = part.to_string_lossy();
                if part.is_empty() {
                    continue;
                }
                parts.push(part.into_owned());
            }
            Component::CurDir => {}
            _ => return Err(ApiError::bad_request("文件路径包含非法片段。")),
        }
    }
    Ok(parts.join("/"))
}

pub(super) fn normalized_kb_name(name: &str) -> ApiResult<String> {
    let name = name.trim();
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|', '\0'])
    {
        return Err(ApiError::bad_request("知识库名称包含无效字符。"));
    }
    Ok(name.chars().take(120).collect())
}

fn kb_slug(name: &str) -> String {
    let sanitized = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric()
                || character == '-'
                || character == '_'
                || !character.is_ascii()
            {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    let hash = format!("{:x}", Sha256::digest(name.as_bytes()));
    format!("{}-{}", sanitized.trim_matches('_'), &hash[..10])
}

pub(super) fn kb_raw_root(state: &AppState, kb_name: &str) -> ApiResult<PathBuf> {
    let name = normalized_kb_name(kb_name)?;
    let kb = state
        .database
        .get_resource("knowledge_base", &name)?
        .ok_or_else(|| ApiError::not_found("知识库不存在。"))?;
    let root = kb
        .get("path")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| state.paths.data.join("knowledge").join(kb_slug(&name)));
    if !root.starts_with(&state.paths.data) {
        return Err(ApiError::internal("知识库路径不在便携数据目录中。"));
    }
    Ok(root.join("raw"))
}

pub(super) fn raw_file_path(state: &AppState, kb_name: &str, filename: &str) -> ApiResult<PathBuf> {
    let root = kb_raw_root(state, kb_name)?;
    let relative = safe_relative_path(filename, false)?;
    let requested = root.join(&relative);
    let canonical_root = root
        .canonicalize()
        .map_err(|error| ApiError::internal(format!("无法解析知识库目录：{error}")))?;
    let canonical = requested
        .canonicalize()
        .map_err(|_| ApiError::not_found("知识库文件不存在。"))?;
    if !canonical.starts_with(&canonical_root) || !canonical.is_file() {
        return Err(ApiError::forbidden("拒绝访问知识库目录之外的文件。"));
    }
    Ok(canonical)
}

fn count_files(root: &Path) -> usize {
    WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .count()
}

#[allow(dead_code)]
fn file_hashes(root: &Path) -> BTreeMap<String, String> {
    WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter_map(|entry| {
            let bytes = std::fs::read(entry.path()).ok()?;
            let relative = entry
                .path()
                .strip_prefix(root)
                .ok()?
                .to_string_lossy()
                .replace('\\', "/");
            Some((relative, format!("{:x}", Sha256::digest(bytes))))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 上传路径拒绝目录逃逸() {
        assert!(safe_relative_path("../secret.txt", false).is_err());
        assert!(safe_relative_path("C:/secret.txt", false).is_err());
        assert!(safe_relative_path("/secret.txt", false).is_err());
        assert_eq!(
            safe_relative_path("docs/lesson 1.md", false).unwrap(),
            "docs/lesson 1.md"
        );
    }

    #[test]
    fn 知识库目录名稳定且隔离() {
        let first = kb_slug("数学/基础");
        let second = kb_slug("数学 基础");
        assert_ne!(first, second);
        assert_eq!(first, kb_slug("数学/基础"));
        assert!(!first.contains('/'));
    }
}
