use super::error::{ApiError, ApiResult};
use super::files;
use super::AppState;
use super::ScopedState;
use axum::body::Bytes;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Component, Path as FilePath, PathBuf};
use std::sync::LazyLock;
use uuid::Uuid;

const LIBRARY_DOMAIN: &str = "library_file";
const WORKSPACE_ITEM_DOMAIN: &str = "workspace_item";

static LIBRARY_WRITE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[derive(Debug, Default, Deserialize)]
struct LibraryQuery {
    limit: Option<usize>,
    offset: Option<usize>,
    q: Option<String>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/files/library", get(list_library).post(add_library_file))
        .route("/files/library/", get(list_library).post(add_library_file))
        .route("/files/library/search", get(search_library))
        .route(
            "/files/library/{file_id}",
            get(get_library_file).delete(delete_library_file),
        )
        .route(
            "/files/library/{file_id}/download",
            get(download_library_file),
        )
        .route(
            "/files/library/{file_id}/restore",
            post(restore_library_file),
        )
        .route(
            "/files/workspace-items/{workspace_id}/{workspace_item_id}",
            get(workspace_item),
        )
}

async fn list_library(
    ScopedState(state): ScopedState,
    Query(query): Query<LibraryQuery>,
) -> ApiResult<Json<Value>> {
    if let Some(term) = query.q.as_deref().filter(|term| !term.trim().is_empty()) {
        return Ok(Json(Value::Array(search_entries(&state, term.trim())?)));
    }
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let offset = query.offset.unwrap_or(0);
    let entries = state
        .database
        .list_resources(LIBRARY_DOMAIN)?
        .into_iter()
        .filter(|entry| {
            !entry
                .get("is_deleted")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>();
    Ok(Json(Value::Array(entries)))
}

async fn add_library_file(
    ScopedState(state): ScopedState,
    mut multipart: Multipart,
) -> ApiResult<Json<Value>> {
    let mut filename = String::new();
    let mut declared_mime = String::new();
    let mut data: Option<Bytes> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|error| ApiError::bad_request(format!("无法读取文件库上传表单：{error}")))?
    {
        match field.name().unwrap_or_default() {
            "filename" => {
                filename = field
                    .text()
                    .await
                    .map_err(|error| ApiError::bad_request(format!("无法读取文件名：{error}")))?;
            }
            "mime_type" => {
                declared_mime = field.text().await.map_err(|error| {
                    ApiError::bad_request(format!("无法读取 MIME 类型：{error}"))
                })?;
            }
            "file" => {
                if filename.is_empty() {
                    filename = field.file_name().unwrap_or("file.bin").to_string();
                }
                data = Some(field.bytes().await.map_err(|error| {
                    ApiError::bad_request(format!("无法读取文件内容：{error}"))
                })?);
            }
            _ => {}
        }
    }
    let data = data.ok_or_else(|| ApiError::bad_request("上传表单缺少 file 字段"))?;
    if data.is_empty() {
        return Err(ApiError::bad_request("不能上传空文件"));
    }
    let filename = safe_filename(&filename);
    let mime_type = if declared_mime.trim().is_empty() {
        mime_guess::from_path(&filename)
            .first_or_octet_stream()
            .essence_str()
            .to_string()
    } else {
        declared_mime.trim().to_string()
    };
    let sha256 = hex::encode(Sha256::digest(&data));

    let _guard = LIBRARY_WRITE_LOCK.lock();
    if let Some(existing) = state
        .database
        .list_resources(LIBRARY_DOMAIN)?
        .into_iter()
        .find(|entry| {
            entry.get("sha256").and_then(Value::as_str) == Some(sha256.as_str())
                && !entry
                    .get("is_deleted")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
        })
    {
        return Ok(Json(existing));
    }

    let id = Uuid::new_v4().to_string();
    let extension = FilePath::new(&filename)
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| {
            let filtered = value
                .chars()
                .filter(|character| character.is_ascii_alphanumeric())
                .take(16)
                .collect::<String>();
            if filtered.is_empty() {
                String::new()
            } else {
                format!(".{filtered}")
            }
        })
        .unwrap_or_default();
    let relative = format!("{}{}", id, extension);
    let root = state.paths.uploads.join("library");
    std::fs::create_dir_all(&root)
        .map_err(|error| ApiError::internal(format!("无法创建文件库目录：{error}")))?;
    let target = root.join(&relative);
    std::fs::write(&target, &data)
        .map_err(|error| ApiError::internal(format!("无法保存文件库文件：{error}")))?;

    let now = crate::storage::now();
    let entry = json!({
        "id": id,
        "sha256": sha256,
        "filename": filename,
        "mime_type": mime_type,
        "size_bytes": data.len(),
        "library_path": relative,
        "created_at": now,
        "updated_at": now,
        "is_deleted": false,
        "deleted_at": null,
        "url": format!("/files/library/{id}/download")
    });
    if let Err(error) = state.database.put_resource(LIBRARY_DOMAIN, &id, &entry) {
        let _ = std::fs::remove_file(target);
        return Err(ApiError::from(error));
    }
    Ok(Json(entry))
}

async fn search_library(
    ScopedState(state): ScopedState,
    Query(query): Query<LibraryQuery>,
) -> ApiResult<Json<Value>> {
    let term = query
        .q
        .as_deref()
        .map(str::trim)
        .filter(|term| !term.is_empty())
        .ok_or_else(|| ApiError::bad_request("搜索词不能为空"))?;
    Ok(Json(Value::Array(search_entries(&state, term)?)))
}

async fn get_library_file(
    ScopedState(state): ScopedState,
    Path(file_id): Path<String>,
) -> ApiResult<Json<Value>> {
    state
        .database
        .get_resource(LIBRARY_DOMAIN, &file_id)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("文件库条目不存在"))
}

async fn download_library_file(
    ScopedState(state): ScopedState,
    Path(file_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let entry = state
        .database
        .get_resource(LIBRARY_DOMAIN, &file_id)?
        .ok_or_else(|| ApiError::not_found("文件库条目不存在"))?;
    if entry
        .get("is_deleted")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Err(ApiError::not_found("文件库条目已删除"));
    }
    let relative = entry
        .get("library_path")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::internal("文件库条目缺少存储路径"))?;
    let path = resolve_under(&state.paths.uploads.join("library"), relative)
        .filter(|path| path.is_file())
        .ok_or_else(|| ApiError::not_found("文件库文件不在磁盘上"))?;
    Ok(files::serve_path(path, &headers).await)
}

async fn delete_library_file(
    ScopedState(state): ScopedState,
    Path(file_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let _guard = LIBRARY_WRITE_LOCK.lock();
    let mut entry = state
        .database
        .get_resource(LIBRARY_DOMAIN, &file_id)?
        .ok_or_else(|| ApiError::not_found("文件库条目不存在"))?;
    entry["is_deleted"] = Value::Bool(true);
    entry["deleted_at"] = Value::from(crate::storage::now());
    entry["updated_at"] = Value::from(crate::storage::now());
    state
        .database
        .put_resource(LIBRARY_DOMAIN, &file_id, &entry)?;
    Ok(Json(json!({"id": file_id, "deleted": true})))
}

async fn restore_library_file(
    ScopedState(state): ScopedState,
    Path(file_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let _guard = LIBRARY_WRITE_LOCK.lock();
    let mut entry = state
        .database
        .get_resource(LIBRARY_DOMAIN, &file_id)?
        .ok_or_else(|| ApiError::not_found("文件库条目不存在"))?;
    entry["is_deleted"] = Value::Bool(false);
    entry["deleted_at"] = Value::Null;
    entry["updated_at"] = Value::from(crate::storage::now());
    state
        .database
        .put_resource(LIBRARY_DOMAIN, &file_id, &entry)?;
    Ok(Json(json!({"id": file_id, "restored": true})))
}

async fn workspace_item(
    ScopedState(state): ScopedState,
    Path((workspace_id, workspace_item_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let workspace_id = safe_segment(&workspace_id)?;
    let workspace_item_id = safe_segment(&workspace_item_id)?;
    let resource_id = format!("{workspace_id}:{workspace_item_id}");
    let entry = state
        .database
        .get_resource(WORKSPACE_ITEM_DOMAIN, &resource_id)?;
    let relative = entry
        .as_ref()
        .and_then(|entry| entry.get("relative_path"))
        .and_then(Value::as_str);
    let root = state.paths.uploads.join("workspace-items");
    let mut candidates = Vec::new();
    if let Some(relative) = relative {
        if let Some(path) = resolve_under(&root, relative) {
            candidates.push(path);
        }
    }
    candidates.push(root.join(&workspace_id).join(&workspace_item_id));
    let path = candidates
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| ApiError::not_found("工作区条目不存在"))?;
    Ok(files::serve_path(path, &headers).await)
}

fn search_entries(state: &AppState, term: &str) -> ApiResult<Vec<Value>> {
    let needle = term.to_lowercase();
    Ok(state
        .database
        .list_resources(LIBRARY_DOMAIN)?
        .into_iter()
        .filter(|entry| {
            !entry
                .get("is_deleted")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .filter(|entry| {
            entry
                .get("filename")
                .and_then(Value::as_str)
                .is_some_and(|filename| filename.to_lowercase().contains(&needle))
        })
        .collect())
}

fn safe_filename(value: &str) -> String {
    let name = value
        .replace('\\', "/")
        .rsplit('/')
        .next()
        .unwrap_or("file.bin")
        .trim()
        .to_string();
    if name.is_empty() || name == "." || name == ".." {
        "file.bin".to_string()
    } else {
        name.chars().take(240).collect()
    }
}

fn safe_segment(value: &str) -> ApiResult<String> {
    let valid = !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains('/')
        && !value.contains('\\')
        && !value.contains(':');
    if valid {
        Ok(value.to_string())
    } else {
        Err(ApiError::bad_request("路径片段不合法"))
    }
}

fn resolve_under(root: &FilePath, relative: &str) -> Option<PathBuf> {
    let normalized = relative.trim_start_matches(['/', '\\']).replace('\\', "/");
    let relative = FilePath::new(&normalized);
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return None;
    }
    let root = root.canonicalize().ok()?;
    let candidate = root.join(relative).canonicalize().ok()?;
    candidate.starts_with(root).then_some(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 上传文件名会移除目录片段() {
        assert_eq!(safe_filename(r"..\..\报告.pdf"), "报告.pdf");
        assert_eq!(safe_filename(""), "file.bin");
    }

    #[test]
    fn 搜索路径拒绝目录逃逸() {
        assert!(safe_segment("..").is_err());
        assert!(safe_segment("a/b").is_err());
        assert_eq!(safe_segment("file_1").unwrap(), "file_1");
    }
}
