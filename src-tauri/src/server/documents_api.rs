use super::error::{ApiError, ApiResult};
use super::llm;
use super::ScopedState;
use super::{documents, engine, AppState};
use axum::body::Bytes;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::header;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::Stream;
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{json, Value};
use std::convert::Infallible;
use std::io::{Cursor, Write};
use std::path::{Path as FilePath, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

const DOCUMENT_DOMAIN: &str = "document";
const OPERATION_DOMAIN: &str = "document_operation";
const THREAD_DOMAIN: &str = "document_thread";
const SUGGESTION_DOMAIN: &str = "document_suggestion";
const REVIEW_DOMAIN: &str = "document_review";
const MAX_DOCUMENT_CHARS: usize = 600_000;
const MAX_SELECTION_CHARS: usize = 120_000;
const MAX_ASSET_BYTES: usize = 25 * 1024 * 1024;

static DOCUMENT_WRITE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[derive(Debug, Deserialize)]
struct ThreadQuery {
    #[allow(dead_code)]
    dt_workspace: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SearchQuery {
    q: Option<String>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/documents", get(list_documents).post(create_document))
        .route("/documents/actions/edit", post(edit_text))
        .route("/documents/actions/edit-react", post(edit_text_react))
        .route(
            "/documents/actions/edit-react/stream",
            post(edit_text_react_stream),
        )
        .route("/documents/actions/automark", post(auto_mark))
        .route("/documents/history", get(history))
        .route("/documents/history/{operation_id}", get(operation))
        .route("/documents/tool-calls/{operation_id}", get(tool_call))
        .route("/documents/import/docx", post(import_docx))
        .route("/documents/export/docx", post(export_docx))
        .route("/documents/review/due", get(due_reviews))
        .route(
            "/documents/{doc_id}",
            get(get_document)
                .put(update_document)
                .delete(delete_document),
        )
        .route(
            "/documents/{doc_id}/suggestions",
            get(list_suggestions).post(create_suggestion),
        )
        .route(
            "/documents/{doc_id}/suggestions/{suggestion_id}",
            post(review_suggestion),
        )
        .route("/documents/{doc_id}/backlinks", get(backlinks))
        .route("/documents/{doc_id}/assets", post(upload_asset))
        .route("/documents/{doc_id}/assets/{name}", get(read_asset))
        .route("/documents/{doc_id}/clips", post(append_clip))
        .route("/documents/{doc_id}/review", post(review_card))
        .route(
            "/documents/{doc_id}/threads",
            get(list_threads).post(anchor_thread),
        )
        .route(
            "/documents/{doc_id}/threads/{session_id}",
            axum::routing::delete(delete_thread),
        )
        .route("/documents/preview-research", post(preview_research))
}

async fn list_documents(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    let mut documents = state
        .database
        .list_resources(DOCUMENT_DOMAIN)?
        .into_iter()
        .map(document_summary)
        .collect::<Vec<_>>();
    documents.sort_by(|left, right| {
        number_field(right, "updated_at")
            .partial_cmp(&number_field(left, "updated_at"))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(Json(json!({"documents": documents})))
}

async fn create_document(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let _guard = DOCUMENT_WRITE_LOCK.lock();
    let id = Uuid::new_v4().simple().to_string();
    let now = crate::storage::now();
    let title = body
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .chars()
        .take(160)
        .collect::<String>();
    let content = body
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    ensure_document_size(&content)?;
    let blocks = body
        .get("blocks")
        .filter(|value| value.is_array())
        .cloned()
        .unwrap_or(Value::Null);
    let document = json!({
        "id": id,
        "doc_id": id,
        "title": title,
        "content": content,
        "blocks": blocks,
        "revision": 1,
        "parent_id": body.get("parent_id").cloned().unwrap_or(Value::Null),
        "icon": body.get("icon").and_then(Value::as_str).unwrap_or(""),
        "cover": body.get("cover").and_then(Value::as_str).unwrap_or(""),
        "pinned": body.get("pinned").and_then(Value::as_bool).unwrap_or(false),
        "created_at": now,
        "updated_at": now
    });
    let saved = state
        .database
        .put_resource(DOCUMENT_DOMAIN, &id, &document)?;
    Ok(Json(saved))
}

async fn get_document(
    ScopedState(state): ScopedState,
    Path(doc_id): Path<String>,
) -> ApiResult<Json<Value>> {
    Ok(Json(load_document(&state, &doc_id)?))
}

async fn update_document(
    ScopedState(state): ScopedState,
    Path(doc_id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let _guard = DOCUMENT_WRITE_LOCK.lock();
    let current = load_document(&state, &doc_id)?;
    if let Some(expected) = body.get("expected_revision").and_then(Value::as_i64) {
        let actual = current.get("revision").and_then(Value::as_i64).unwrap_or(0);
        if expected != actual {
            return Err(ApiError::conflict(format!(
                "笔记已被其他操作更新：期望版本 {expected}，当前版本 {actual}"
            )));
        }
    }
    let mut next = current;
    for key in [
        "title",
        "content",
        "blocks",
        "parent_id",
        "icon",
        "cover",
        "pinned",
    ] {
        if let Some(value) = body.get(key) {
            next[key] = value.clone();
        }
    }
    if let Some(content) = next.get("content").and_then(Value::as_str) {
        ensure_document_size(content)?;
    }
    let revision = next
        .get("revision")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .saturating_add(1);
    next["revision"] = Value::from(revision);
    next["updated_at"] = Value::from(crate::storage::now());
    Ok(Json(state.database.put_resource(
        DOCUMENT_DOMAIN,
        &doc_id,
        &next,
    )?))
}

async fn delete_document(
    ScopedState(state): ScopedState,
    Path(doc_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let _guard = DOCUMENT_WRITE_LOCK.lock();
    load_document(&state, &doc_id)?;
    state.database.delete_resource(DOCUMENT_DOMAIN, &doc_id)?;
    state.database.delete_resource(SUGGESTION_DOMAIN, &doc_id)?;
    state.database.delete_resource(THREAD_DOMAIN, &doc_id)?;
    let root = document_root(&state, &doc_id);
    if root.exists() {
        std::fs::remove_dir_all(&root)
            .map_err(|error| ApiError::internal(format!("无法删除笔记附件目录：{error}")))?;
    }
    Ok(Json(json!({"deleted": true})))
}

async fn upload_asset(
    ScopedState(state): ScopedState,
    Path(doc_id): Path<String>,
    mut multipart: Multipart,
) -> ApiResult<Json<Value>> {
    load_document(&state, &doc_id)?;
    let mut data = None;
    let mut mime_type = String::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|error| ApiError::bad_request(format!("无法读取附件表单：{error}")))?
    {
        if field.name() == Some("file") {
            mime_type = field.content_type().unwrap_or("").to_string();
            data =
                Some(field.bytes().await.map_err(|error| {
                    ApiError::bad_request(format!("无法读取附件内容：{error}"))
                })?);
        }
    }
    let data = data.ok_or_else(|| ApiError::bad_request("附件表单缺少 file"))?;
    if data.is_empty() || data.len() > MAX_ASSET_BYTES {
        return Err(ApiError::bad_request("附件必须在 1 字节到 25 MB 之间"));
    }
    let suffix = match mime_type.as_str() {
        "image/png" => ".png",
        "image/jpeg" => ".jpg",
        "image/webp" => ".webp",
        "image/gif" => ".gif",
        "audio/mpeg" => ".mp3",
        "audio/wav" | "audio/x-wav" => ".wav",
        "video/mp4" => ".mp4",
        "application/pdf" => ".pdf",
        _ => return Err(ApiError::bad_request("不支持该附件类型")),
    };
    let name = format!("{}{suffix}", Uuid::new_v4().simple());
    let directory = document_root(&state, &doc_id).join("assets");
    std::fs::create_dir_all(&directory)
        .map_err(|error| ApiError::internal(format!("无法创建附件目录：{error}")))?;
    std::fs::write(directory.join(&name), &data)
        .map_err(|error| ApiError::internal(format!("无法保存附件：{error}")))?;
    Ok(Json(json!({
        "url": format!("/api/documents/{doc_id}/assets/{name}")
    })))
}

async fn read_asset(
    ScopedState(state): ScopedState,
    Path((doc_id, name)): Path<(String, String)>,
    _query: Query<ThreadQuery>,
) -> ApiResult<Response> {
    load_document(&state, &doc_id)?;
    let valid = name.len() == 36
        && name
            .chars()
            .take(32)
            .all(|character| character.is_ascii_hexdigit())
        && matches!(
            name.get(32..),
            Some(".png" | ".jpg" | ".webp" | ".gif" | ".mp3" | ".wav" | ".mp4" | ".pdf")
        );
    if !valid {
        return Err(ApiError::not_found("附件不存在"));
    }
    let path = document_root(&state, &doc_id).join("assets").join(name);
    if !path.is_file() {
        return Err(ApiError::not_found("附件不存在"));
    }
    let data = tokio::fs::read(&path)
        .await
        .map_err(|error| ApiError::internal(format!("无法读取附件：{error}")))?;
    let mime = mime_guess::from_path(&path).first_or_octet_stream();
    Ok((
        [
            (header::CONTENT_TYPE, mime.as_ref()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (
                header::CACHE_CONTROL,
                "private, max-age=31536000, immutable",
            ),
        ],
        data,
    )
        .into_response())
}

async fn append_clip(
    ScopedState(state): ScopedState,
    Path(doc_id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let _guard = DOCUMENT_WRITE_LOCK.lock();
    let mut document = load_document(&state, &doc_id)?;
    let title = body
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .chars()
        .take(200)
        .collect::<String>();
    let markdown = body
        .get("markdown")
        .and_then(Value::as_str)
        .unwrap_or("")
        .chars()
        .take(200_000)
        .collect::<String>();
    let source_url = body.get("source_url").and_then(Value::as_str).unwrap_or("");
    if !source_url.is_empty()
        && !(source_url.starts_with("https://") || source_url.starts_with("http://"))
    {
        return Err(ApiError::bad_request("来源链接只支持 HTTP 或 HTTPS"));
    }
    let clip = json!({
        "id": Uuid::new_v4().simple().to_string(),
        "type": "clip",
        "props": {
            "title": title,
            "markdown": markdown,
            "sourceUrl": source_url,
            "sourceId": body.get("source_id").cloned().unwrap_or(Value::String(String::new())),
            "favoriteType": body.get("favorite_type").cloned().unwrap_or(Value::String(String::new())),
            "summary": body.get("summary").cloned().unwrap_or(Value::String(String::new())),
            "metadata": body.get("favorite_metadata").cloned().unwrap_or_else(|| json!({})),
            "expanded": false
        },
        "content": null,
        "children": []
    });
    let mut blocks = document
        .get("blocks")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    blocks.push(clip);
    document["blocks"] = Value::Array(blocks.clone());
    document["content"] = Value::String(blocks_to_markdown(&blocks));
    document["revision"] = Value::from(
        document
            .get("revision")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            .saturating_add(1),
    );
    document["updated_at"] = Value::from(crate::storage::now());
    Ok(Json(state.database.put_resource(
        DOCUMENT_DOMAIN,
        &doc_id,
        &document,
    )?))
}

async fn list_suggestions(
    ScopedState(state): ScopedState,
    Path(doc_id): Path<String>,
) -> ApiResult<Json<Value>> {
    load_document(&state, &doc_id)?;
    let suggestions = state
        .database
        .get_resource(SUGGESTION_DOMAIN, &doc_id)?
        .and_then(|value| value.get("items").and_then(Value::as_array).cloned())
        .unwrap_or_default();
    Ok(Json(json!({"suggestions": suggestions})))
}

async fn create_suggestion(
    ScopedState(state): ScopedState,
    Path(doc_id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let _guard = DOCUMENT_WRITE_LOCK.lock();
    load_document(&state, &doc_id)?;
    let id = Uuid::new_v4().simple().to_string();
    let suggestion = json!({
        "id": id,
        "operation": body.get("operation").and_then(Value::as_str).unwrap_or("replace"),
        "block_id": body.get("block_id").and_then(Value::as_str).unwrap_or(""),
        "markdown": body.get("markdown").and_then(Value::as_str).unwrap_or(""),
        "explanation": body.get("explanation").and_then(Value::as_str).unwrap_or(""),
        "original": body.get("original").cloned().unwrap_or(Value::Null),
        "anchors": body.get("anchors").cloned().unwrap_or(Value::Null),
        "preserve_children": body.get("preserve_children").and_then(Value::as_bool).unwrap_or(false),
        "blocks": body.get("blocks").cloned().unwrap_or(Value::Null),
        "status": "pending"
    });
    let mut record = state
        .database
        .get_resource(SUGGESTION_DOMAIN, &doc_id)?
        .unwrap_or_else(|| json!({"items": []}));
    record["items"]
        .as_array_mut()
        .expect("建议集合必须是数组")
        .push(suggestion.clone());
    state
        .database
        .put_resource(SUGGESTION_DOMAIN, &doc_id, &record)?;
    Ok(Json(suggestion))
}

async fn review_suggestion(
    ScopedState(state): ScopedState,
    Path((doc_id, suggestion_id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let _guard = DOCUMENT_WRITE_LOCK.lock();
    let mut document = load_document(&state, &doc_id)?;
    let mut record = state
        .database
        .get_resource(SUGGESTION_DOMAIN, &doc_id)?
        .ok_or_else(|| ApiError::not_found("建议不存在"))?;
    let suggestions = record
        .get_mut("items")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| ApiError::internal("建议集合损坏"))?;
    let suggestion = suggestions
        .iter_mut()
        .find(|item| item.get("id").and_then(Value::as_str) == Some(suggestion_id.as_str()))
        .ok_or_else(|| ApiError::not_found("建议不存在"))?;
    let accept = body.get("accept").and_then(Value::as_bool).unwrap_or(false);
    if accept {
        let replacement = body
            .get("blocks")
            .cloned()
            .or_else(|| suggestion.get("blocks").cloned())
            .filter(|value| value.is_array());
        if let Some(replacement) = replacement {
            document["blocks"] = replacement.clone();
            document["content"] = Value::String(blocks_to_markdown(
                replacement.as_array().expect("已过滤数组"),
            ));
        } else {
            apply_suggestion(&mut document, suggestion)?;
        }
        suggestion["status"] = Value::String("accepted".into());
        document["revision"] = Value::from(
            document
                .get("revision")
                .and_then(Value::as_i64)
                .unwrap_or(0)
                .saturating_add(1),
        );
        document["updated_at"] = Value::from(crate::storage::now());
        state
            .database
            .put_resource(DOCUMENT_DOMAIN, &doc_id, &document)?;
    } else {
        suggestion["status"] = Value::String("dismissed".into());
    }
    state
        .database
        .put_resource(SUGGESTION_DOMAIN, &doc_id, &record)?;
    Ok(Json(document))
}

fn apply_suggestion(document: &mut Value, suggestion: &Value) -> ApiResult<()> {
    let operation = suggestion
        .get("operation")
        .and_then(Value::as_str)
        .unwrap_or("replace");
    let block_id = suggestion
        .get("block_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let markdown = suggestion
        .get("markdown")
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut blocks = document
        .get("blocks")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let index = blocks
        .iter()
        .position(|block| block.get("id").and_then(Value::as_str) == Some(block_id));
    match operation {
        "insert" => {
            let block = json!({
                "id": Uuid::new_v4().simple().to_string(),
                "type": "paragraph",
                "props": {},
                "content": markdown,
                "children": []
            });
            let at = index.unwrap_or(blocks.len());
            blocks.insert(at, block);
        }
        "delete" => {
            let index = index.ok_or_else(|| ApiError::bad_request("删除建议找不到目标块"))?;
            blocks.remove(index);
        }
        "replace" => {
            let index = index.ok_or_else(|| ApiError::bad_request("替换建议找不到目标块"))?;
            blocks[index]["content"] = Value::String(markdown.to_string());
        }
        "comment" => {}
        _ => return Err(ApiError::bad_request("未知的笔记建议操作")),
    }
    document["blocks"] = Value::Array(blocks.clone());
    document["content"] = Value::String(blocks_to_markdown(&blocks));
    Ok(())
}

async fn backlinks(
    ScopedState(state): ScopedState,
    Path(doc_id): Path<String>,
) -> ApiResult<Json<Value>> {
    load_document(&state, &doc_id)?;
    let needle = format!("/co-writer/{doc_id}");
    let pages = state
        .database
        .list_resources(DOCUMENT_DOMAIN)?
        .into_iter()
        .filter(|document| document.get("id").and_then(Value::as_str) != Some(doc_id.as_str()))
        .filter(|document| {
            document
                .get("content")
                .and_then(Value::as_str)
                .is_some_and(|content| content.contains(&needle))
        })
        .map(document_summary)
        .collect::<Vec<_>>();
    Ok(Json(json!({"pages": pages})))
}

async fn list_threads(
    ScopedState(state): ScopedState,
    Path(doc_id): Path<String>,
) -> ApiResult<Json<Value>> {
    load_document(&state, &doc_id)?;
    let threads = state
        .database
        .get_resource(THREAD_DOMAIN, &doc_id)?
        .and_then(|value| value.get("items").and_then(Value::as_array).cloned())
        .unwrap_or_default();
    Ok(Json(json!({"threads": threads})))
}

async fn anchor_thread(
    ScopedState(state): ScopedState,
    Path(doc_id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let _guard = DOCUMENT_WRITE_LOCK.lock();
    load_document(&state, &doc_id)?;
    let session_id = body
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiError::bad_request("缺少 session_id"))?
        .to_string();
    let now = crate::storage::now();
    let thread = json!({
        "session_id": session_id,
        "block_id": body.get("block_id").and_then(Value::as_str).unwrap_or(""),
        "quote": body.get("quote").and_then(Value::as_str).unwrap_or(""),
        "origin": body.get("origin").and_then(Value::as_str).unwrap_or("ask"),
        "created_at": now,
        "title": body.get("title").and_then(Value::as_str).unwrap_or(""),
        "status": body.get("status").and_then(Value::as_str).unwrap_or("idle"),
        "updated_at": now,
        "message_count": 0,
        "last_message": ""
    });
    let mut record = state
        .database
        .get_resource(THREAD_DOMAIN, &doc_id)?
        .unwrap_or_else(|| json!({"items": []}));
    let items = record
        .get_mut("items")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| ApiError::internal("侧边对话集合损坏"))?;
    items
        .retain(|item| item.get("session_id").and_then(Value::as_str) != Some(session_id.as_str()));
    items.push(thread.clone());
    state
        .database
        .put_resource(THREAD_DOMAIN, &doc_id, &record)?;
    Ok(Json(thread))
}

async fn delete_thread(
    ScopedState(state): ScopedState,
    Path((doc_id, session_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let _guard = DOCUMENT_WRITE_LOCK.lock();
    let mut record = state
        .database
        .get_resource(THREAD_DOMAIN, &doc_id)?
        .ok_or_else(|| ApiError::not_found("侧边对话不存在"))?;
    let items = record
        .get_mut("items")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| ApiError::internal("侧边对话集合损坏"))?;
    let before = items.len();
    items
        .retain(|item| item.get("session_id").and_then(Value::as_str) != Some(session_id.as_str()));
    let deleted = items.len() != before;
    state
        .database
        .put_resource(THREAD_DOMAIN, &doc_id, &record)?;
    Ok(Json(json!({"deleted": deleted})))
}

async fn due_reviews(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    let now = crate::storage::now();
    let mut cards = Vec::new();
    for document in state.database.list_resources(DOCUMENT_DOMAIN)? {
        let doc_id = document
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if let Some(record) = state.database.get_resource(REVIEW_DOMAIN, doc_id)? {
            for card in record
                .get("cards")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if number_field(card, "due_at") <= now {
                    let mut card = card.clone();
                    card["doc_id"] = Value::String(doc_id.to_string());
                    cards.push(card);
                }
            }
        }
    }
    Ok(Json(json!({"cards": cards})))
}

async fn review_card(
    ScopedState(state): ScopedState,
    Path(doc_id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    load_document(&state, &doc_id)?;
    let card_id = body
        .get("card_id")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad_request("缺少 card_id"))?;
    let remembered = body
        .get("remembered")
        .and_then(Value::as_bool)
        .ok_or_else(|| ApiError::bad_request("缺少 remembered"))?;
    let now = crate::storage::now();
    let interval = if remembered {
        3.0 * 86_400.0
    } else {
        10.0 * 60.0
    };
    let mut record = state
        .database
        .get_resource(REVIEW_DOMAIN, &doc_id)?
        .unwrap_or_else(|| json!({"cards": []}));
    let cards = record
        .get_mut("cards")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| ApiError::internal("复习卡片集合损坏"))?;
    let card = cards
        .iter_mut()
        .find(|card| card.get("id").and_then(Value::as_str) == Some(card_id))
        .ok_or_else(|| ApiError::not_found("复习卡片不存在"))?;
    card["due_at"] = Value::from(now + interval);
    card["last_reviewed_at"] = Value::from(now);
    card["interval_days"] = Value::from(if remembered { 3.0 } else { 1.0 / 144.0 });
    let result = card.clone();
    state
        .database
        .put_resource(REVIEW_DOMAIN, &doc_id, &record)?;
    Ok(Json(result))
}

async fn history(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    let history = state.database.list_resources(OPERATION_DOMAIN)?;
    Ok(Json(json!({
        "history": history,
        "total": history.len()
    })))
}

async fn operation(
    ScopedState(state): ScopedState,
    Path(operation_id): Path<String>,
) -> ApiResult<Json<Value>> {
    state
        .database
        .get_resource(OPERATION_DOMAIN, &operation_id)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("编辑记录不存在"))
}

async fn tool_call(
    ScopedState(state): ScopedState,
    Path(operation_id): Path<String>,
) -> ApiResult<Json<Value>> {
    state
        .database
        .get_resource("document_tool_call", &operation_id)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("工具调用记录不存在"))
}

async fn import_docx(
    ScopedState(state): ScopedState,
    mut multipart: Multipart,
) -> ApiResult<Json<Value>> {
    let mut filename = String::new();
    let mut bytes: Option<Bytes> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|error| ApiError::bad_request(format!("无法读取 DOCX 表单：{error}")))?
    {
        if field.name() == Some("file") {
            filename = field.file_name().unwrap_or("document.docx").to_string();
            bytes = Some(
                field
                    .bytes()
                    .await
                    .map_err(|error| ApiError::bad_request(format!("无法读取 DOCX：{error}")))?,
            );
        }
    }
    if !filename.to_ascii_lowercase().ends_with(".docx") {
        return Err(ApiError::bad_request("只支持导入 .docx 文件"));
    }
    let bytes = bytes.ok_or_else(|| ApiError::bad_request("表单缺少 file"))?;
    if bytes.is_empty() || bytes.len() > 20 * 1024 * 1024 {
        return Err(ApiError::bad_request("DOCX 必须在 1 字节到 20 MB 之间"));
    }
    let extracted = super::parsing::extract(&state, &filename, &bytes)
        .await
        .map_err(ApiError::bad_request)?;
    let title = FilePath::new(&filename)
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("导入的笔记")
        .chars()
        .take(120)
        .collect::<String>();
    create_document(
        ScopedState(state),
        Json(json!({
            "title": title,
            "content": extracted.text,
            "blocks": null
        })),
    )
    .await
}

async fn export_docx(Json(body): Json<Value>) -> ApiResult<Response> {
    let title = body
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("co-writer")
        .trim()
        .to_string();
    let content = body.get("content").and_then(Value::as_str).unwrap_or("");
    ensure_document_size(content)?;
    let data = markdown_to_docx(content, &title)
        .map_err(|error| ApiError::internal(format!("无法生成 DOCX：{error}")))?;
    let filename = safe_download_filename(&title);
    let disposition = content_disposition(&filename);
    Ok((
        [
            (
                header::CONTENT_TYPE,
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            ),
            (header::CONTENT_DISPOSITION, disposition.as_str()),
        ],
        data,
    )
        .into_response())
}

#[derive(Debug, Deserialize)]
struct EditRequest {
    text: String,
    instruction: String,
    action: Option<String>,
    llm_selection: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct ReactEditRequest {
    selected_text: String,
    #[serde(default)]
    instruction: String,
    mode: Option<String>,
    #[serde(default)]
    tools: Vec<String>,
    kb_name: Option<String>,
    llm_selection: Option<Value>,
}

async fn edit_text(
    ScopedState(state): ScopedState,
    Json(body): Json<EditRequest>,
) -> ApiResult<Json<Value>> {
    ensure_document_size(&body.text)?;
    if body.instruction.chars().count() > 10_000 {
        return Err(ApiError::bad_request("编辑要求过长"));
    }
    let action = body.action.as_deref().unwrap_or("rewrite");
    let prompt = format!(
        "编辑模式：{}\n用户要求：{}\n\n待编辑内容：\n{}",
        action, body.instruction, body.text
    );
    let result = run_edit_model(&state, &prompt, body.llm_selection.as_ref()).await?;
    let operation_id = record_operation(
        &state,
        "edit",
        json!({
            "input": {"text": body.text, "instruction": body.instruction, "action": action},
            "output": {"edited_text": result}
        }),
    )?;
    Ok(Json(json!({
        "edited_text": result,
        "operation_id": operation_id
    })))
}

async fn edit_text_react(
    ScopedState(state): ScopedState,
    Json(body): Json<ReactEditRequest>,
) -> ApiResult<Json<Value>> {
    let (prompt, tools) = react_prompt(&body)?;
    let result = run_edit_model(&state, &prompt, body.llm_selection.as_ref()).await?;
    let operation_id = record_operation(
        &state,
        "react_edit",
        json!({
            "mode": body.mode,
            "tools": tools,
            "kb_name": body.kb_name,
            "input": {"selected_text": body.selected_text, "instruction": body.instruction},
            "output": {"edited_text": result}
        }),
    )?;
    Ok(Json(json!({
        "edited_text": result,
        "operation_id": operation_id,
        "tools_used": tools
    })))
}

async fn edit_text_react_stream(
    ScopedState(state): ScopedState,
    Json(body): Json<ReactEditRequest>,
) -> ApiResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    let (prompt, tools) = react_prompt(&body)?;
    let state_for_stream = state.clone();
    let selection = body.llm_selection.clone();
    let input = json!({
        "mode": body.mode,
        "tools": tools,
        "kb_name": body.kb_name,
        "input": {"selected_text": body.selected_text, "instruction": body.instruction}
    });
    let stream = async_stream::stream! {
        yield Ok::<Event, Infallible>(
            Event::default().event("stream").data(
                json!({"type": "stage", "stage": "responding"}).to_string()
            )
        );
        match run_edit_model(&state_for_stream, &prompt, selection.as_ref()).await {
            Ok(text) => {
                if let Ok(operation_id) = record_operation(
                    &state_for_stream,
                    "react_edit",
                    json!({"input": input, "output": {"edited_text": text}}),
                ) {
                    let data = json!({
                        "edited_text": text,
                        "operation_id": operation_id,
                        "tools_used": tools
                    });
                    yield Ok::<Event, Infallible>(
                        Event::default().event("result").data(data.to_string())
                    );
                }
            }
            Err(error) => {
                yield Ok::<Event, Infallible>(
                    Event::default()
                        .event("error")
                        .data(json!({"detail": error.message}).to_string())
                );
            }
        }
    };
    Ok(Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("heartbeat"),
    ))
}

async fn auto_mark(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let text = body
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad_request("缺少 text"))?;
    ensure_document_size(text)?;
    let prompt = format!(
        "请为下面的 Markdown 添加清晰、克制的小标题和重点标记，只输出修改后的 Markdown：\n\n{text}"
    );
    let result = run_edit_model(&state, &prompt, body.get("llm_selection")).await?;
    let operation_id = record_operation(
        &state,
        "automark",
        json!({"input": {"text": text}, "output": {"marked_text": result}}),
    )?;
    Ok(Json(json!({
        "marked_text": result,
        "operation_id": operation_id
    })))
}

async fn preview_research(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let query = body
        .get("query")
        .or_else(|| body.get("instruction"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut results = Vec::new();
    for base in state.database.list_resources("knowledge_base")? {
        if let Some(name) = base.get("name").and_then(Value::as_str) {
            if name.to_lowercase().contains(&query.to_lowercase()) {
                results.push(json!({"type": "knowledge_base", "name": name}));
            }
        }
    }
    for document in state.database.list_resources(DOCUMENT_DOMAIN)? {
        let content = document
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("");
        if !query.is_empty() && content.to_lowercase().contains(&query.to_lowercase()) {
            results.push(json!({
                "type": "document",
                "id": document.get("id"),
                "title": document.get("title")
            }));
        }
        if results.len() >= 12 {
            break;
        }
    }
    Ok(Json(json!({"sources": results})))
}

fn react_prompt(body: &ReactEditRequest) -> ApiResult<(String, Vec<String>)> {
    if body.selected_text.chars().count() > MAX_SELECTION_CHARS {
        return Err(ApiError::bad_request("选中文本过长"));
    }
    if body.selected_text.trim().is_empty() {
        return Err(ApiError::bad_request("请先选中一段文本"));
    }
    if body.instruction.chars().count() > 10_000 {
        return Err(ApiError::bad_request("编辑要求过长"));
    }
    let mode = body.mode.as_deref().unwrap_or("rewrite");
    let instruction = match (mode, body.instruction.trim()) {
        ("rewrite", "") => "润色文本，保持原意和结构",
        ("shorten", "") => "压缩文本，保留关键信息",
        ("expand", "") => "扩展文本，补充必要细节",
        ("none", "") => return Err(ApiError::bad_request("请输入编辑要求")),
        (_, value) => value,
    };
    let tools = body
        .tools
        .iter()
        .filter(|tool| matches!(tool.as_str(), "rag" | "web"))
        .cloned()
        .collect::<Vec<_>>();
    let prompt = format!(
        "你是 Markdown 编辑助手。只输出编辑后的片段，不要解释或代码围栏。\n模式：{mode}\n要求：{instruction}\n\n{}",
        body.selected_text
    );
    Ok((prompt, tools))
}

async fn run_edit_model(
    state: &AppState,
    prompt: &str,
    selection: Option<&Value>,
) -> ApiResult<String> {
    let request = json!({"llm_selection": selection.cloned().unwrap_or(Value::Null)});
    let settings = engine::resolve_llm_settings(state, &request)
        .map_err(|error| ApiError::bad_request(error))?;
    let messages = vec![
        json!({"role": "system", "content": "你是严谨的 Markdown 编辑助手。"}),
        json!({"role": "user", "content": prompt}),
    ];
    let cancel = CancellationToken::new();
    let response = llm::stream_completion(
        &state.client,
        &settings,
        &messages,
        &[],
        None,
        &cancel,
        |_| {},
    )
    .await
    .map_err(|error| ApiError::provider(format!("模型编辑失败：{error}")))?;
    let content = response
        .content
        .trim()
        .trim_start_matches("```markdown")
        .trim_start_matches("```md")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim()
        .to_string();
    if content.is_empty() {
        return Err(ApiError::provider("模型没有返回编辑结果"));
    }
    Ok(content)
}

fn record_operation(state: &AppState, action: &str, payload: Value) -> ApiResult<String> {
    let id = format!(
        "{}_{}",
        chrono::Utc::now().format("%Y%m%d_%H%M%S"),
        &Uuid::new_v4().simple().to_string()[..6]
    );
    let mut record = payload;
    record["id"] = Value::String(id.clone());
    record["action"] = Value::String(action.to_string());
    record["timestamp"] = Value::String(chrono::Utc::now().to_rfc3339());
    state
        .database
        .put_resource(OPERATION_DOMAIN, &id, &record)?;
    Ok(id)
}

fn load_document(state: &AppState, doc_id: &str) -> ApiResult<Value> {
    validate_doc_id(doc_id)?;
    state
        .database
        .get_resource(DOCUMENT_DOMAIN, doc_id)?
        .ok_or_else(|| ApiError::not_found("笔记不存在"))
}

fn validate_doc_id(doc_id: &str) -> ApiResult<()> {
    let valid = !doc_id.is_empty()
        && doc_id.len() <= 64
        && doc_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'));
    if valid {
        Ok(())
    } else {
        Err(ApiError::not_found("笔记不存在"))
    }
}

fn document_root(state: &AppState, doc_id: &str) -> PathBuf {
    state.paths.data.join("documents").join(doc_id)
}

fn document_summary(mut document: Value) -> Value {
    let content = document
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .chars()
        .take(280)
        .collect::<String>();
    document.as_object_mut().map(|object| {
        object.remove("blocks");
        object.insert("preview".into(), Value::String(content));
        object
    });
    document
}

fn number_field(value: &Value, key: &str) -> f64 {
    value.get(key).and_then(Value::as_f64).unwrap_or_default()
}

fn ensure_document_size(content: &str) -> ApiResult<()> {
    if content.chars().count() > MAX_DOCUMENT_CHARS {
        Err(ApiError::bad_request("文档内容超过 600,000 字符限制"))
    } else {
        Ok(())
    }
}

fn blocks_to_markdown(blocks: &[Value]) -> String {
    blocks
        .iter()
        .filter_map(|block| {
            if let Some(content) = block.get("content").and_then(Value::as_str) {
                Some(content.to_string())
            } else {
                block
                    .pointer("/props/markdown")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            }
        })
        .filter(|content| !content.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn safe_download_filename(title: &str) -> String {
    let base = title
        .chars()
        .map(|character| {
            if matches!(
                character,
                '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
            ) {
                '-'
            } else {
                character
            }
        })
        .take(80)
        .collect::<String>();
    let trimmed = base.trim_matches([' ', '.']);
    let stem = if trimmed.is_empty() {
        "co-writer"
    } else {
        trimmed
    };
    if stem.to_ascii_lowercase().ends_with(".docx") {
        stem.to_string()
    } else {
        format!("{stem}.docx")
    }
}

fn content_disposition(filename: &str) -> String {
    let ascii = filename
        .chars()
        .filter(|character| character.is_ascii() && *character != '"' && *character != '\\')
        .collect::<String>();
    let fallback = if ascii.trim().is_empty() {
        "co-writer.docx".to_string()
    } else {
        ascii
    };
    format!(
        "attachment; filename=\"{fallback}\"; filename*=UTF-8''{}",
        url::form_urlencoded::byte_serialize(filename.as_bytes()).collect::<String>()
    )
}

fn markdown_to_docx(markdown: &str, title: &str) -> Result<Vec<u8>, String> {
    let mut paragraphs = Vec::new();
    if !title.trim().is_empty() {
        paragraphs.push(("Title".to_string(), title.trim().to_string()));
    }
    for line in markdown.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            paragraphs.push(("Normal".to_string(), String::new()));
        } else if let Some(value) = trimmed.strip_prefix("### ") {
            paragraphs.push(("Heading3".to_string(), value.to_string()));
        } else if let Some(value) = trimmed.strip_prefix("## ") {
            paragraphs.push(("Heading2".to_string(), value.to_string()));
        } else if let Some(value) = trimmed.strip_prefix("# ") {
            paragraphs.push(("Heading1".to_string(), value.to_string()));
        } else if let Some(value) = trimmed.strip_prefix("- ") {
            paragraphs.push(("ListParagraph".to_string(), format!("• {value}")));
        } else {
            paragraphs.push(("Normal".to_string(), trimmed.to_string()));
        }
    }
    let body = paragraphs
        .into_iter()
        .map(|(style, text)| {
            let escaped = xml_escape(&text);
            format!(
                "<w:p><w:pPr><w:pStyle w:val=\"{style}\"/></w:pPr><w:r><w:t xml:space=\"preserve\">{escaped}</w:t></w:r></w:p>"
            )
        })
        .collect::<String>();
    let document_xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
         <w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
         <w:body>{body}<w:sectPr><w:pgSz w:w=\"11906\" w:h=\"16838\"/>\
         <w:pgMar w:top=\"1440\" w:right=\"1440\" w:bottom=\"1440\" w:left=\"1440\"/></w:sectPr>\
         </w:body></w:document>"
    );
    let content_types = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
        <Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
        <Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
        <Default Extension=\"xml\" ContentType=\"application/xml\"/>\
        <Override PartName=\"/word/document.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/>\
        </Types>";
    let relationships = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
        <Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
        <Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"word/document.xml\"/>\
        </Relationships>";
    let mut buffer = Cursor::new(Vec::new());
    {
        let mut zip = ZipWriter::new(&mut buffer);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (name, content) in [
            ("[Content_Types].xml", content_types),
            ("_rels/.rels", relationships),
            ("word/document.xml", document_xml.as_str()),
        ] {
            zip.start_file(name, options)
                .map_err(|error| error.to_string())?;
            zip.write_all(content.as_bytes())
                .map_err(|error| error.to_string())?;
        }
        zip.finish().map_err(|error| error.to_string())?;
    }
    Ok(buffer.into_inner())
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 块内容会转换为_markdown() {
        let blocks = vec![
            json!({"id": "a", "content": "# 标题"}),
            json!({"id": "b", "props": {"markdown": "正文"}}),
        ];
        assert_eq!(blocks_to_markdown(&blocks), "# 标题\n\n正文");
    }

    #[test]
    fn docx_导出包含必需_ooxml_部件() {
        let bytes = markdown_to_docx("# 标题\n\n正文", "测试").unwrap();
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        assert!(archive.by_name("[Content_Types].xml").is_ok());
        assert!(archive.by_name("word/document.xml").is_ok());
    }

    #[test]
    fn 文件名清理不会保留目录分隔符() {
        assert_eq!(safe_download_filename("a/b:c"), "a-b-c.docx");
    }
}
