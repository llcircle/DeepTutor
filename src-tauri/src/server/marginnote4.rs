use super::error::{ApiError, ApiResult};
use super::{AppState, ScopedState};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::{SecondsFormat, Utc};
use rand::rngs::OsRng;
use rand::RngCore;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

const MAX_SYNC_BATCH: usize = 2_000;
const MAX_DEVICE_NAME: usize = 128;
const MAX_DEVICE_KIND: usize = 32;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS mn4_objects (
    object_id     TEXT NOT NULL,
    device_id     TEXT NOT NULL,
    object_type   TEXT NOT NULL,
    title         TEXT NOT NULL DEFAULT '',
    content       TEXT NOT NULL DEFAULT '',
    excerpt       TEXT,
    document_id   TEXT,
    document_title TEXT,
    page          INTEGER,
    tags          TEXT NOT NULL DEFAULT '[]',
    links         TEXT NOT NULL DEFAULT '[]',
    color         TEXT,
    created_at    TEXT NOT NULL DEFAULT '',
    updated_at    TEXT NOT NULL DEFAULT '',
    synced_at     TEXT NOT NULL DEFAULT '',
    raw           TEXT NOT NULL DEFAULT '{}',
    PRIMARY KEY (object_id, device_id)
);
CREATE INDEX IF NOT EXISTS idx_mn4_type ON mn4_objects (object_type);
CREATE INDEX IF NOT EXISTS idx_mn4_doc ON mn4_objects (document_id);
CREATE INDEX IF NOT EXISTS idx_mn4_search ON mn4_objects (title);
CREATE TABLE IF NOT EXISTS mn4_devices (
    device_id    TEXT PRIMARY KEY,
    device_name  TEXT NOT NULL DEFAULT '',
    device_kind  TEXT NOT NULL DEFAULT 'macos',
    token_hash   TEXT NOT NULL,
    paired_at    TEXT NOT NULL DEFAULT '',
    last_seen    TEXT NOT NULL DEFAULT '',
    active       INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE IF NOT EXISTS mn4_cursors (
    device_id TEXT PRIMARY KEY,
    cursor    TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS mn4_tombstones (
    object_id  TEXT NOT NULL,
    device_id  TEXT NOT NULL,
    deleted_at TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (object_id, device_id)
);
"#;

#[derive(Debug, Default, Deserialize)]
struct PairRequest {
    #[serde(default)]
    device_name: String,
    #[serde(default = "default_device_kind")]
    device_kind: String,
}

#[derive(Debug, Serialize)]
struct PairResponse {
    device_id: String,
    token: String,
    device_name: String,
    device_kind: String,
}

#[derive(Debug, Deserialize)]
struct SyncObjectInput {
    object_id: String,
    object_type: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    excerpt: Option<String>,
    #[serde(default)]
    document_id: Option<String>,
    #[serde(default)]
    document_title: Option<String>,
    #[serde(default)]
    page: Option<i64>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    links: Vec<String>,
    #[serde(default)]
    color: Option<String>,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    updated_at: String,
    #[serde(default)]
    raw: Value,
}

#[derive(Debug, Deserialize)]
struct SyncRequest {
    #[serde(default)]
    cursor: String,
    #[serde(default)]
    objects: Vec<SyncObjectInput>,
    #[serde(default)]
    deleted_ids: Vec<String>,
}

#[derive(Debug, Serialize)]
struct SyncResponse {
    stored: usize,
    updated: usize,
    deleted: usize,
    new_cursor: String,
}

#[derive(Debug, Serialize)]
struct DeviceInfo {
    device_id: String,
    device_name: String,
    device_kind: String,
    paired_at: String,
    last_seen: String,
    active: bool,
}

fn default_device_kind() -> String {
    "macos".to_string()
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/marginnote4/pair", post(pair_device))
        .route("/marginnote4/devices", get(list_devices))
        .route("/marginnote4/devices/{device_id}", delete(revoke_device))
        .route("/marginnote4/status", get(status))
        .route("/marginnote4/sync", post(sync_objects))
        .route("/marginnote4/heartbeat", post(heartbeat))
}

async fn pair_device(
    ScopedState(state): ScopedState,
    headers: HeaderMap,
    Json(body): Json<PairRequest>,
) -> ApiResult<Json<PairResponse>> {
    if body.device_name.chars().count() > MAX_DEVICE_NAME
        || body.device_kind.chars().count() > MAX_DEVICE_KIND
    {
        return Err(ApiError::bad_request("设备名称或设备类型过长。"));
    }
    let path = session_store_path(&state, &requested_kb(&headers));
    let device_name = body.device_name.trim().to_string();
    let device_kind = if body.device_kind.trim().is_empty() {
        default_device_kind()
    } else {
        body.device_kind.trim().to_ascii_lowercase()
    };
    let result = tokio::task::spawn_blocking(move || {
        let mut store = MarginNoteStore::create(path)?;
        store.pair_device(&device_name, &device_kind)
    })
    .await
    .map_err(|error| ApiError::internal(format!("MN4 配对任务失败：{error}")))??;
    Ok(Json(PairResponse {
        device_id: result.0,
        token: result.1,
        device_name: result.2,
        device_kind: result.3,
    }))
}

async fn list_devices(
    ScopedState(state): ScopedState,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<DeviceInfo>>> {
    let path = session_store_path(&state, &requested_kb(&headers));
    let devices = tokio::task::spawn_blocking(move || {
        let mut store = MarginNoteStore::create(path)?;
        store.list_devices()
    })
    .await
    .map_err(|error| ApiError::internal(format!("MN4 设备读取任务失败：{error}")))??;
    Ok(Json(devices))
}

async fn revoke_device(
    ScopedState(state): ScopedState,
    Path(device_id): Path<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    if device_id.trim().is_empty() {
        return Err(ApiError::bad_request("设备 ID 不能为空。"));
    }
    let path = session_store_path(&state, &requested_kb(&headers));
    let device_id_for_task = device_id.clone();
    let revoked =
        tokio::task::spawn_blocking(move || match MarginNoteStore::open_existing(path)? {
            Some(store) => store.revoke_device(&device_id_for_task),
            None => Ok(false),
        })
        .await
        .map_err(|error| ApiError::internal(format!("MN4 设备撤销任务失败：{error}")))??;
    if !revoked {
        return Err(ApiError::not_found(format!("设备 {device_id} 不存在。")));
    }
    Ok(Json(json!({"status": "revoked", "device_id": device_id})))
}

async fn status(ScopedState(state): ScopedState, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let path = session_store_path(&state, &requested_kb(&headers));
    let (devices, objects) = tokio::task::spawn_blocking(move || {
        let mut store = MarginNoteStore::create(path)?;
        Ok::<_, ApiError>((store.list_devices()?.len(), store.count(None)?))
    })
    .await
    .map_err(|error| ApiError::internal(format!("MN4 状态读取任务失败：{error}")))??;
    Ok(Json(json!({
        "status": "ok",
        "devices": devices,
        "objects": objects
    })))
}

async fn sync_objects(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SyncRequest>,
) -> ApiResult<Json<SyncResponse>> {
    if body.objects.len() > MAX_SYNC_BATCH || body.deleted_ids.len() > MAX_SYNC_BATCH {
        return Err(ApiError::bad_request(format!(
            "单次 MN4 同步最多接受 {MAX_SYNC_BATCH} 个对象或删除标记。"
        )));
    }
    let (device_id, store) =
        authenticate_device(&state, &headers, request_authorization(&headers)?)?;
    let result = tokio::task::spawn_blocking(move || {
        let mut store = store;
        let result = store.ingest(&device_id, body)?;
        store.touch_device(&device_id)?;
        Ok::<_, ApiError>(result)
    })
    .await
    .map_err(|error| ApiError::internal(format!("MN4 同步任务失败：{error}")))??;
    Ok(Json(result))
}

async fn heartbeat(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let (device_id, store) =
        authenticate_device(&state, &headers, request_authorization(&headers)?)?;
    let count_device = device_id.clone();
    let response_device = device_id.clone();
    let object_count = tokio::task::spawn_blocking(move || {
        let mut store = store;
        store.touch_device(&device_id)?;
        store.count(Some(&count_device))
    })
    .await
    .map_err(|error| ApiError::internal(format!("MN4 心跳任务失败：{error}")))??;
    Ok(Json(json!({
        "status": "ok",
        "device_id": response_device,
        "object_count": object_count
    })))
}

fn request_authorization(headers: &HeaderMap) -> ApiResult<String> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| value.starts_with("MarginNote "))
        .map(ToOwned::to_owned)
        .ok_or_else(|| ApiError::unauthorized("缺少或错误的 MN4 设备认证头。"))
}

fn authenticate_device(
    state: &AppState,
    headers: &HeaderMap,
    authorization: String,
) -> ApiResult<(String, MarginNoteStore)> {
    let raw = authorization
        .strip_prefix("MarginNote ")
        .ok_or_else(|| ApiError::unauthorized("MN4 设备认证格式无效。"))?;
    let (device_id, token) = raw
        .split_once(':')
        .ok_or_else(|| ApiError::unauthorized("MN4 设备认证格式无效。"))?;
    if device_id.trim().is_empty() || token.is_empty() {
        return Err(ApiError::unauthorized("MN4 设备认证格式无效。"));
    }
    let kb = requested_kb(headers);
    let candidates = device_store_candidates(state, &kb)?;
    for path in candidates {
        if let Some(store) = MarginNoteStore::open_existing(path)? {
            if store.verify_token(device_id.trim(), token)? {
                return Ok((device_id.trim().to_string(), store));
            }
        }
    }
    Err(ApiError {
        status: StatusCode::FORBIDDEN,
        code: "forbidden",
        message: "MN4 设备凭据无效。".to_string(),
        retryable: false,
    })
}

fn requested_kb(headers: &HeaderMap) -> String {
    headers
        .get("x-mn4-kb")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("default")
        .chars()
        .take(120)
        .collect()
}

fn session_store_path(state: &AppState, kb_name: &str) -> PathBuf {
    if let Ok(Some(metadata)) = state.database.get_resource("knowledge_base", kb_name) {
        if let Some(path) = metadata.get("db_path").and_then(Value::as_str) {
            let pinned = PathBuf::from(path);
            if !path.trim().is_empty() && pinned.starts_with(&state.paths.root) {
                return pinned;
            }
        }
    }
    state
        .paths
        .data
        .join("marginnote4")
        .join(format!("{}.db", safe_db_component(kb_name)))
}

fn device_store_candidates(state: &AppState, kb_name: &str) -> ApiResult<Vec<PathBuf>> {
    let filename = format!("{}.db", safe_db_component(kb_name));
    let mut candidates = Vec::new();
    let admin = state.paths.data.join("marginnote4").join(&filename);
    if admin.is_file() {
        candidates.push(admin);
    }
    let users_root = state.paths.data.join("users");
    if users_root.is_dir() {
        let entries = std::fs::read_dir(&users_root)
            .map_err(|error| ApiError::internal(format!("无法枚举用户数据目录：{error}")))?;
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let candidate = entry.path().join("marginnote4").join(&filename);
            if candidate.is_file() {
                candidates.push(candidate);
            }
        }
    }
    candidates.sort();
    candidates.dedup();
    Ok(candidates)
}

fn safe_db_component(value: &str) -> String {
    let value = value.trim();
    let mut output = value
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if output.is_empty() {
        output.push_str("default");
    }
    output.truncate(100);
    output
}

struct MarginNoteStore {
    path: PathBuf,
}

impl MarginNoteStore {
    fn create(path: PathBuf) -> ApiResult<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| ApiError::internal(format!("无法创建 MN4 数据目录：{error}")))?;
        }
        let store = Self { path };
        store.initialize()?;
        Ok(store)
    }

    fn open_existing(path: PathBuf) -> ApiResult<Option<Self>> {
        Ok(path.is_file().then_some(Self { path }))
    }

    fn initialize(&self) -> ApiResult<()> {
        let connection = self.connection()?;
        connection
            .execute_batch(SCHEMA)
            .map_err(|error| ApiError::internal(format!("无法初始化 MN4 数据库：{error}")))
    }

    fn connection(&self) -> ApiResult<Connection> {
        let connection = Connection::open(&self.path)
            .map_err(|error| ApiError::internal(format!("无法打开 MN4 数据库：{error}")))?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(|error| ApiError::internal(format!("无法启用 MN4 WAL：{error}")))?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .map_err(|error| ApiError::internal(format!("无法配置 MN4 数据库：{error}")))?;
        Ok(connection)
    }

    fn pair_device(
        &mut self,
        device_name: &str,
        device_kind: &str,
    ) -> ApiResult<(String, String, String, String)> {
        let device_id = random_token(12);
        let token = random_token(32);
        let token_hash = hash_token(&token);
        let now = now_iso();
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction()
            .map_err(|error| ApiError::internal(format!("无法开始 MN4 配对事务：{error}")))?;
        transaction
            .execute(
                "INSERT INTO mn4_devices
                 (device_id, device_name, device_kind, token_hash, paired_at, last_seen, active)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5, 1)",
                params![device_id, device_name, device_kind, token_hash, now],
            )
            .map_err(|error| ApiError::internal(format!("无法保存 MN4 设备：{error}")))?;
        transaction
            .execute(
                "INSERT OR REPLACE INTO mn4_cursors (device_id, cursor) VALUES (?1, '')",
                [&device_id],
            )
            .map_err(|error| ApiError::internal(format!("无法创建 MN4 同步游标：{error}")))?;
        transaction
            .commit()
            .map_err(|error| ApiError::internal(format!("无法提交 MN4 配对事务：{error}")))?;
        Ok((
            device_id,
            token,
            device_name.to_string(),
            device_kind.to_string(),
        ))
    }

    fn verify_token(&self, device_id: &str, token: &str) -> ApiResult<bool> {
        let connection = self.connection()?;
        let stored: Option<(String, i64)> = connection
            .query_row(
                "SELECT token_hash, active FROM mn4_devices WHERE device_id = ?1",
                [device_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|error| ApiError::internal(format!("无法校验 MN4 设备：{error}")))?;
        Ok(stored.is_some_and(|(hash, active)| {
            active != 0 && constant_time_eq(&hash, &hash_token(token))
        }))
    }

    fn touch_device(&mut self, device_id: &str) -> ApiResult<()> {
        let connection = self.connection()?;
        connection
            .execute(
                "UPDATE mn4_devices SET last_seen = ?1 WHERE device_id = ?2",
                params![now_iso(), device_id],
            )
            .map_err(|error| ApiError::internal(format!("无法更新 MN4 设备心跳：{error}")))?;
        Ok(())
    }

    fn list_devices(&mut self) -> ApiResult<Vec<DeviceInfo>> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT device_id, device_name, device_kind, paired_at, last_seen, active
                 FROM mn4_devices ORDER BY paired_at",
            )
            .map_err(|error| ApiError::internal(format!("无法读取 MN4 设备：{error}")))?;
        let rows = statement
            .query_map([], |row| {
                Ok(DeviceInfo {
                    device_id: row.get(0)?,
                    device_name: row.get(1)?,
                    device_kind: row.get(2)?,
                    paired_at: row.get(3)?,
                    last_seen: row.get(4)?,
                    active: row.get::<_, i64>(5)? != 0,
                })
            })
            .map_err(|error| ApiError::internal(format!("无法读取 MN4 设备：{error}")))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| ApiError::internal(format!("无法解析 MN4 设备：{error}")))
    }

    fn revoke_device(&self, device_id: &str) -> ApiResult<bool> {
        let connection = self.connection()?;
        let changed = connection
            .execute(
                "UPDATE mn4_devices SET active = 0 WHERE device_id = ?1 AND active = 1",
                [device_id],
            )
            .map_err(|error| ApiError::internal(format!("无法撤销 MN4 设备：{error}")))?;
        Ok(changed > 0)
    }

    fn ingest(&mut self, device_id: &str, body: SyncRequest) -> ApiResult<SyncResponse> {
        let mut stored = 0usize;
        let mut updated = 0usize;
        let mut deleted = 0usize;
        let now = now_iso();
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction()
            .map_err(|error| ApiError::internal(format!("无法开始 MN4 同步事务：{error}")))?;
        for object in body.objects {
            if object.object_id.trim().is_empty()
                || !matches!(
                    object.object_type.as_str(),
                    "note" | "excerpt" | "card" | "mindmap_node" | "document" | "comment"
                )
            {
                continue;
            }
            let existed: Option<String> = transaction
                .query_row(
                    "SELECT synced_at FROM mn4_objects WHERE object_id = ?1 AND device_id = ?2",
                    params![object.object_id, device_id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| ApiError::internal(format!("无法查询 MN4 对象：{error}")))?;
            let tags = serde_json::to_string(&object.tags)
                .map_err(|error| ApiError::internal(format!("无法编码 MN4 标签：{error}")))?;
            let links = serde_json::to_string(&object.links)
                .map_err(|error| ApiError::internal(format!("无法编码 MN4 链接：{error}")))?;
            let raw_value = if object.raw.is_null() {
                json!({})
            } else {
                object.raw
            };
            let raw = serde_json::to_string(&raw_value)
                .map_err(|error| ApiError::internal(format!("无法编码 MN4 原始数据：{error}")))?;
            transaction
                .execute(
                    "INSERT INTO mn4_objects
                     (object_id, device_id, object_type, title, content, excerpt,
                      document_id, document_title, page, tags, links, color,
                      created_at, updated_at, synced_at, raw)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)
                     ON CONFLICT(object_id, device_id) DO UPDATE SET
                       object_type=excluded.object_type,
                       title=excluded.title, content=excluded.content,
                       excerpt=excluded.excerpt, document_id=excluded.document_id,
                       document_title=excluded.document_title, page=excluded.page,
                       tags=excluded.tags, links=excluded.links, color=excluded.color,
                       created_at=excluded.created_at, updated_at=excluded.updated_at,
                       synced_at=excluded.synced_at, raw=excluded.raw",
                    params![
                        object.object_id,
                        device_id,
                        object.object_type,
                        object.title,
                        object.content,
                        object.excerpt,
                        object.document_id,
                        object.document_title,
                        object.page,
                        tags,
                        links,
                        object.color,
                        object.created_at,
                        object.updated_at,
                        now,
                        raw
                    ],
                )
                .map_err(|error| ApiError::internal(format!("无法写入 MN4 对象：{error}")))?;
            if existed.is_some() {
                updated += 1;
            } else {
                stored += 1;
            }
        }
        for object_id in body.deleted_ids {
            if object_id.trim().is_empty() {
                continue;
            }
            transaction
                .execute(
                    "INSERT OR REPLACE INTO mn4_tombstones
                     (object_id, device_id, deleted_at) VALUES (?1, ?2, ?3)",
                    params![object_id, device_id, now],
                )
                .map_err(|error| ApiError::internal(format!("无法记录 MN4 删除：{error}")))?;
            transaction
                .execute(
                    "DELETE FROM mn4_objects WHERE object_id = ?1 AND device_id = ?2",
                    params![object_id, device_id],
                )
                .map_err(|error| ApiError::internal(format!("无法删除 MN4 对象：{error}")))?;
            deleted += 1;
        }
        let new_cursor = now_iso();
        transaction
            .execute(
                "INSERT OR REPLACE INTO mn4_cursors (device_id, cursor) VALUES (?1, ?2)",
                params![device_id, new_cursor],
            )
            .map_err(|error| ApiError::internal(format!("无法保存 MN4 同步游标：{error}")))?;
        transaction
            .commit()
            .map_err(|error| ApiError::internal(format!("无法提交 MN4 同步事务：{error}")))?;
        Ok(SyncResponse {
            stored,
            updated,
            deleted,
            new_cursor,
        })
    }

    fn count(&mut self, device_id: Option<&str>) -> ApiResult<usize> {
        let connection = self.connection()?;
        let count: i64 = if let Some(device_id) = device_id {
            connection.query_row(
                "SELECT COUNT(*) FROM mn4_objects WHERE device_id = ?1",
                [device_id],
                |row| row.get(0),
            )
        } else {
            connection.query_row("SELECT COUNT(*) FROM mn4_objects", [], |row| row.get(0))
        }
        .map_err(|error| ApiError::internal(format!("无法统计 MN4 对象：{error}")))?;
        Ok(count.max(0) as usize)
    }
}

fn random_token(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    OsRng.fill_bytes(&mut buffer);
    URL_SAFE_NO_PAD.encode(buffer)
}

fn hash_token(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.as_bytes()
        .iter()
        .zip(right.as_bytes())
        .fold(0u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 数据库名不会形成路径逃逸() {
        assert_eq!(safe_db_component("../../secret"), "______secret");
        assert_eq!(safe_db_component("math-notes"), "math-notes");
    }

    #[test]
    fn 设备令牌比较不会忽略内容差异() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
    }
}
