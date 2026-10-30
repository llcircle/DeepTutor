use super::ScopedState;
use super::{ApiError, ApiResult, AppState};
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use parking_lot::Mutex;
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::sync::LazyLock;
use uuid::Uuid;

const NOTEBOOK_DOMAIN: &str = "notebook";
static NOTEBOOK_WRITE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static THINKING_TAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<(?:think|thinking|analysis|scratchpad)>.*?</(?:think|thinking|analysis|scratchpad)>")
        .expect("思考标签正则必须有效")
});

#[derive(Debug, Deserialize)]
struct LocationQuery {
    notebook_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AddRecordRequest {
    #[serde(default)]
    notebook_ids: Vec<String>,
    #[serde(rename = "record_type", alias = "type")]
    record_type: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    user_query: String,
    #[serde(default)]
    output: String,
    #[serde(default = "empty_object")]
    metadata: Value,
    #[serde(default)]
    kb_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MoveRecordRequest {
    target_notebook_id: String,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/notebooks", get(list_notebooks).post(create_notebook))
        .route("/notebooks/health", get(health))
        .route("/notebooks/statistics", get(statistics))
        .route("/notebooks/actions/add-record", post(add_record))
        .route(
            "/notebooks/actions/add-record-with-summary",
            post(add_record_with_summary),
        )
        .route(
            "/notebooks/records/{record_id}/location",
            get(locate_record),
        )
        .route(
            "/notebooks/{notebook_id}",
            get(get_notebook)
                .put(update_notebook)
                .delete(delete_notebook),
        )
        .route("/notebooks/{notebook_id}/export", get(export_notebook))
        .route(
            "/notebooks/{notebook_id}/records/{record_id}",
            put(update_record).delete(remove_record),
        )
        .route(
            "/notebooks/{notebook_id}/records/{record_id}/actions/copy",
            post(copy_record),
        )
        .route(
            "/notebooks/{notebook_id}/records/{record_id}/actions/move",
            post(move_record),
        )
        .route(
            "/notebooks/{notebook_id}/records/{record_id}/actions/{action}",
            post(relocate_record),
        )
}

async fn health() -> Json<Value> {
    Json(json!({"status": "healthy", "service": "notebook", "runtime": "rust"}))
}

async fn list_notebooks(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    let mut notebooks = state
        .database
        .list_resources(NOTEBOOK_DOMAIN)?
        .into_iter()
        .map(summary_value)
        .collect::<Vec<_>>();
    notebooks.sort_by(|left, right| {
        number_field(right, "updated_at")
            .partial_cmp(&number_field(left, "updated_at"))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(Json(json!({
        "notebooks": notebooks,
        "total": notebooks.len()
    })))
}

/// 供工具门控判断当前工作区是否存在任何笔记本。
pub fn has_notebooks(state: &AppState) -> Result<bool, String> {
    Ok(!state.database.list_resources(NOTEBOOK_DOMAIN)?.is_empty())
}

async fn create_notebook(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let name = required_text(&body, "name", "笔记本名称不能为空")?;
    let now = chrono::Utc::now().timestamp_micros() as f64 / 1_000_000.0;
    let id = short_id();
    let notebook = json!({
        "id": id,
        "notebook_id": id,
        "name": name,
        "description": text_or_default(&body, "description"),
        "color": body.get("color").and_then(Value::as_str).unwrap_or("#3B82F6"),
        "icon": body.get("icon").and_then(Value::as_str).unwrap_or("book"),
        "created_at": now,
        "updated_at": now,
        "records": []
    });
    let saved = state
        .database
        .put_resource(NOTEBOOK_DOMAIN, &id, &notebook)?;
    Ok(Json(
        json!({"success": true, "notebook": summary_value(saved)}),
    ))
}

async fn get_notebook(
    ScopedState(state): ScopedState,
    Path(notebook_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let mut notebook =
        load_notebook(&state, &notebook_id)?.ok_or_else(|| ApiError::not_found("笔记本不存在"))?;
    if !notebook.get("records").is_some_and(Value::is_array) {
        notebook["records"] = Value::Array(Vec::new());
    }
    Ok(Json(notebook))
}

async fn update_notebook(
    ScopedState(state): ScopedState,
    Path(notebook_id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let _guard = NOTEBOOK_WRITE_LOCK.lock();
    let mut notebook =
        load_notebook(&state, &notebook_id)?.ok_or_else(|| ApiError::not_found("笔记本不存在"))?;
    for key in ["name", "description", "color", "icon"] {
        if let Some(value) = body.get(key) {
            notebook[key] = value.clone();
        }
    }
    notebook["updated_at"] = Value::from(now());
    let saved = state
        .database
        .put_resource(NOTEBOOK_DOMAIN, &notebook_id, &notebook)?;
    Ok(Json(
        json!({"success": true, "notebook": summary_value(saved)}),
    ))
}

async fn delete_notebook(
    ScopedState(state): ScopedState,
    Path(notebook_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let _guard = NOTEBOOK_WRITE_LOCK.lock();
    if !state
        .database
        .delete_resource(NOTEBOOK_DOMAIN, &notebook_id)?
    {
        return Err(ApiError::not_found("笔记本不存在"));
    }
    Ok(Json(json!({"success": true, "message": "笔记本已删除"})))
}

async fn add_record(
    ScopedState(state): ScopedState,
    Json(request): Json<AddRecordRequest>,
) -> ApiResult<Json<Value>> {
    let summary = record_summary(&request);
    let result = insert_record(&state, request, summary)?;
    Ok(Json(json!({
        "success": true,
        "summary": result.0,
        "record": result.1,
        "added_to_notebooks": result.2
    })))
}

async fn add_record_with_summary(
    ScopedState(state): ScopedState,
    Json(request): Json<AddRecordRequest>,
) -> ApiResult<Response> {
    let summary = record_summary(&request);
    let result = match insert_record(&state, request, summary.clone()) {
        Ok(result) => result,
        Err(error) => {
            let payload = json!({"type": "error", "detail": error.message});
            return Ok((
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/event-stream; charset=utf-8")],
                format!("data: {}\n\n", payload),
            )
                .into_response());
        }
    };
    let chunk = json!({"type": "summary_chunk", "content": result.0});
    let final_event = json!({
        "type": "result",
        "success": true,
        "summary": result.0,
        "record": result.1,
        "added_to_notebooks": result.2
    });
    let body = format!("data: {chunk}\n\ndata: {final_event}\n\n");
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/event-stream; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
            (header::HeaderName::from_static("x-accel-buffering"), "no"),
        ],
        body,
    )
        .into_response())
}

fn insert_record(
    state: &AppState,
    request: AddRecordRequest,
    summary: String,
) -> Result<(String, Value, Vec<String>), ApiError> {
    if request.record_type.trim().is_empty() {
        return Err(ApiError::bad_request("记录类型不能为空"));
    }
    if request.title.trim().is_empty() {
        return Err(ApiError::bad_request("记录标题不能为空"));
    }
    let timestamp = now();
    let record = json!({
        "id": short_id(),
        "type": request.record_type,
        "title": request.title.trim(),
        "summary": summary,
        "user_query": request.user_query,
        "output": request.output,
        "metadata": if request.metadata.is_object() { request.metadata } else { json!({}) },
        "created_at": timestamp,
        "kb_name": request.kb_name
    });

    let _guard = NOTEBOOK_WRITE_LOCK.lock();
    let mut accepted = Vec::new();
    for notebook_id in request.notebook_ids {
        let Some(mut notebook) = load_notebook(state, &notebook_id)? else {
            continue;
        };
        if !notebook.get("records").is_some_and(Value::is_array) {
            notebook["records"] = Value::Array(Vec::new());
        }
        let records = notebook["records"]
            .as_array_mut()
            .expect("记录数组已初始化");
        records.push(record.clone());
        notebook["updated_at"] = Value::from(timestamp);
        if state
            .database
            .put_resource(NOTEBOOK_DOMAIN, &notebook_id, &notebook)
            .is_ok()
        {
            accepted.push(notebook_id);
        }
    }
    Ok((record_summary_text(&record), record, accepted))
}

async fn update_record(
    ScopedState(state): ScopedState,
    Path((notebook_id, record_id)): Path<(String, String)>,
    Json(changes): Json<Value>,
) -> ApiResult<Json<Value>> {
    if changes.get("output").is_some() || changes.get("user_query").is_some() {
        return Err(ApiError::bad_request(
            "收藏记录保留原始内容；需要编辑正文时请先把它添加到笔记。",
        ));
    }
    let allowed = ["title", "summary", "metadata", "kb_name"];
    if !allowed.iter().any(|key| changes.get(*key).is_some()) {
        return Err(ApiError::bad_request("没有可更新的字段"));
    }

    let _guard = NOTEBOOK_WRITE_LOCK.lock();
    let mut notebook =
        load_notebook(&state, &notebook_id)?.ok_or_else(|| ApiError::not_found("笔记本不存在"))?;
    let records = notebook
        .get_mut("records")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| ApiError::internal("笔记本记录结构损坏"))?;
    let record = records
        .iter_mut()
        .find(|item| item.get("id").and_then(Value::as_str) == Some(record_id.as_str()))
        .ok_or_else(|| ApiError::not_found("记录不存在"))?;
    for key in allowed {
        if let Some(value) = changes.get(key) {
            record[key] = value.clone();
        }
    }
    if let Some(summary) = record.get("summary").and_then(Value::as_str) {
        record["summary"] = Value::String(clean_summary(summary));
    }
    let updated = record.clone();
    notebook["updated_at"] = Value::from(now());
    state
        .database
        .put_resource(NOTEBOOK_DOMAIN, &notebook_id, &notebook)?;
    Ok(Json(json!({"success": true, "record": updated})))
}

async fn remove_record(
    ScopedState(state): ScopedState,
    Path((notebook_id, record_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let _guard = NOTEBOOK_WRITE_LOCK.lock();
    let mut notebook =
        load_notebook(&state, &notebook_id)?.ok_or_else(|| ApiError::not_found("笔记本不存在"))?;
    let records = notebook
        .get_mut("records")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| ApiError::internal("笔记本记录结构损坏"))?;
    let before = records.len();
    records.retain(|item| item.get("id").and_then(Value::as_str) != Some(record_id.as_str()));
    if records.len() == before {
        return Err(ApiError::not_found("记录不存在"));
    }
    notebook["updated_at"] = Value::from(now());
    state
        .database
        .put_resource(NOTEBOOK_DOMAIN, &notebook_id, &notebook)?;
    Ok(Json(json!({"success": true, "message": "记录已移除"})))
}

async fn relocate_record(
    ScopedState(state): ScopedState,
    Path((notebook_id, record_id, action)): Path<(String, String, String)>,
    Json(request): Json<MoveRecordRequest>,
) -> ApiResult<Json<Value>> {
    if notebook_id == request.target_notebook_id {
        return Err(ApiError::conflict("源笔记本和目标笔记本不能相同"));
    }
    let _guard = NOTEBOOK_WRITE_LOCK.lock();
    let mut source = load_notebook(&state, &notebook_id)?
        .ok_or_else(|| ApiError::not_found("源笔记本不存在"))?;
    let mut target = load_notebook(&state, &request.target_notebook_id)?
        .ok_or_else(|| ApiError::not_found("目标笔记本不存在"))?;
    let source_records = source
        .get_mut("records")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| ApiError::internal("源笔记本记录结构损坏"))?;
    let index = source_records
        .iter()
        .position(|item| item.get("id").and_then(Value::as_str) == Some(record_id.as_str()))
        .ok_or_else(|| ApiError::not_found("记录不存在"))?;
    let mut record = source_records[index].clone();
    if !target.get("records").is_some_and(Value::is_array) {
        target["records"] = Value::Array(Vec::new());
    }
    let target_records = target["records"]
        .as_array_mut()
        .expect("目标记录数组已初始化");

    match action.as_str() {
        "copy" => {
            record["id"] = Value::String(short_id());
            if let Some(metadata) = record.get_mut("metadata").and_then(Value::as_object_mut) {
                metadata.insert(
                    "copied_from".into(),
                    json!({"notebook_id": notebook_id, "record_id": record_id}),
                );
            }
            target_records.push(record.clone());
        }
        "move" => {
            if let Some(existing) = target_records
                .iter()
                .find(|item| item.get("id").and_then(Value::as_str) == Some(record_id.as_str()))
            {
                if existing != &record {
                    return Err(ApiError::conflict(
                        "目标笔记本包含同一 ID 但内容不同的记录，已拒绝覆盖。",
                    ));
                }
            } else {
                target_records.push(record.clone());
            }
            source
                .get_mut("records")
                .and_then(Value::as_array_mut)
                .expect("源记录数组已验证")
                .remove(index);
            source["updated_at"] = Value::from(now());
        }
        _ => return Err(ApiError::bad_request("操作必须是 copy 或 move")),
    }
    target["updated_at"] = Value::from(now());

    // 目标先写入，避免移动过程中断导致唯一副本丢失。
    state
        .database
        .put_resource(NOTEBOOK_DOMAIN, &request.target_notebook_id, &target)?;
    if action == "move" {
        state
            .database
            .put_resource(NOTEBOOK_DOMAIN, &notebook_id, &source)?;
    }
    Ok(Json(json!({"success": true, "record": record})))
}

async fn copy_record(
    state: ScopedState,
    Path((notebook_id, record_id)): Path<(String, String)>,
    Json(request): Json<MoveRecordRequest>,
) -> ApiResult<Json<Value>> {
    relocate_record(
        state,
        Path((notebook_id, record_id, "copy".to_string())),
        Json(request),
    )
    .await
}

async fn move_record(
    state: ScopedState,
    Path((notebook_id, record_id)): Path<(String, String)>,
    Json(request): Json<MoveRecordRequest>,
) -> ApiResult<Json<Value>> {
    relocate_record(
        state,
        Path((notebook_id, record_id, "move".to_string())),
        Json(request),
    )
    .await
}

async fn locate_record(
    ScopedState(state): ScopedState,
    Path(record_id): Path<String>,
    Query(query): Query<LocationQuery>,
) -> ApiResult<Json<Value>> {
    let preferred = query.notebook_id.unwrap_or_default();
    let mut ids = Vec::new();
    if !preferred.is_empty() {
        ids.push(preferred.clone());
    }
    ids.extend(
        state
            .database
            .list_resources(NOTEBOOK_DOMAIN)?
            .into_iter()
            .filter_map(|item| item.get("id").and_then(Value::as_str).map(String::from))
            .filter(|id| id != &preferred),
    );
    for notebook_id in ids {
        let Some(notebook) = load_notebook(&state, &notebook_id)? else {
            continue;
        };
        if notebook
            .get("records")
            .and_then(Value::as_array)
            .is_some_and(|records| {
                records
                    .iter()
                    .any(|item| item.get("id").and_then(Value::as_str) == Some(record_id.as_str()))
            })
        {
            return Ok(Json(
                json!({"notebook_id": notebook_id, "record_id": record_id}),
            ));
        }
    }
    Err(ApiError::not_found("收藏记录不存在"))
}

async fn export_notebook(
    ScopedState(state): ScopedState,
    Path(notebook_id): Path<String>,
) -> ApiResult<Response> {
    let notebook =
        load_notebook(&state, &notebook_id)?.ok_or_else(|| ApiError::not_found("笔记本不存在"))?;
    let markdown = export_markdown(&notebook);
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/markdown; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{notebook_id}.md\"").as_str(),
            ),
        ],
        markdown,
    )
        .into_response())
}

async fn statistics(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    let notebooks = state.database.list_resources(NOTEBOOK_DOMAIN)?;
    let mut total_records = 0usize;
    let mut records_by_type = Map::from_iter(
        [
            "solve",
            "question",
            "research",
            "chat",
            "co_writer",
            "tutorbot",
            "reading",
            "video_learning",
        ]
        .into_iter()
        .map(|kind| (kind.to_string(), Value::from(0))),
    );
    for notebook in &notebooks {
        for record in notebook
            .get("records")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            total_records += 1;
            if let Some(kind) = record.get("type").and_then(Value::as_str) {
                let count = records_by_type
                    .get(kind)
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                records_by_type.insert(kind.to_string(), Value::from(count + 1));
            }
        }
    }
    let total_notebooks = notebooks.len();
    let mut recent = notebooks.into_iter().map(summary_value).collect::<Vec<_>>();
    recent.sort_by(|left, right| {
        number_field(right, "updated_at")
            .partial_cmp(&number_field(left, "updated_at"))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    recent.truncate(5);
    Ok(Json(json!({
        "total_notebooks": total_notebooks,
        "total_records": total_records,
        "records_by_type": records_by_type,
        "recent_notebooks": recent
    })))
}

fn load_notebook(state: &AppState, notebook_id: &str) -> Result<Option<Value>, String> {
    state.database.get_resource(NOTEBOOK_DOMAIN, notebook_id)
}

fn summary_value(mut notebook: Value) -> Value {
    let record_count = notebook
        .get("records")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    if let Some(object) = notebook.as_object_mut() {
        object.remove("records");
        object.insert("record_count".into(), Value::from(record_count));
        if !object.contains_key("notebook_id") {
            if let Some(id) = object.get("id").cloned() {
                object.insert("notebook_id".into(), id);
            }
        }
    }
    notebook
}

fn export_markdown(notebook: &Value) -> String {
    let name = notebook
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("未命名笔记本");
    let mut lines = vec![format!("# {name}")];
    if let Some(description) = notebook
        .get("description")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        lines.extend(["".into(), format!("> {description}")]);
    }
    for record in notebook
        .get("records")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        lines.extend(["".into(), "---".into(), "".into()]);
        lines.push(format!(
            "## {}",
            record
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("(无标题)")
        ));
        let kind = record.get("type").and_then(Value::as_str).unwrap_or("");
        let stamp = record
            .get("created_at")
            .and_then(Value::as_f64)
            .map(format_timestamp)
            .unwrap_or_default();
        let metadata = [kind, stamp.as_str()]
            .into_iter()
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>();
        if !metadata.is_empty() {
            lines.extend(["".into(), format!("*{}*", metadata.join(" · "))]);
        }
        if let Some(summary) = record
            .get("summary")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            lines.extend(["".into(), format!("> {summary}")]);
        }
        if let Some(output) = record
            .get("output")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            lines.extend(["".into(), output.to_string()]);
        }
    }
    format!("{}\n", lines.join("\n"))
}

fn record_summary(request: &AddRecordRequest) -> String {
    let provided = clean_summary(&request.summary);
    if !provided.is_empty() {
        return provided;
    }
    let plain = request
        .output
        .replace(['#', '*', '`', '>', '|'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    plain.chars().take(280).collect()
}

fn record_summary_text(record: &Value) -> String {
    record
        .get("summary")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn clean_summary(value: &str) -> String {
    THINKING_TAG.replace_all(value, "").trim().to_string()
}

fn required_text(body: &Value, key: &str, message: &str) -> Result<String, ApiError> {
    body.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(String::from)
        .ok_or_else(|| ApiError::bad_request(message))
}

fn text_or_default(body: &Value, key: &str) -> String {
    body.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn number_field(value: &Value, key: &str) -> f64 {
    value.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

fn empty_object() -> Value {
    json!({})
}

fn now() -> f64 {
    chrono::Utc::now().timestamp_micros() as f64 / 1_000_000.0
}

fn short_id() -> String {
    Uuid::new_v4().simple().to_string()[..8].to_string()
}

fn format_timestamp(value: f64) -> String {
    chrono::DateTime::from_timestamp(value as i64, 0)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 导出会保留标题摘要和正文() {
        let notebook = json!({
            "name": "数学",
            "description": "复习资料",
            "records": [{
                "title": "函数",
                "type": "chat",
                "summary": "一句话摘要",
                "output": "正文内容",
                "created_at": 0
            }]
        });
        let markdown = export_markdown(&notebook);
        assert!(markdown.contains("# 数学"));
        assert!(markdown.contains("> 复习资料"));
        assert!(markdown.contains("## 函数"));
        assert!(markdown.contains("正文内容"));
    }

    #[test]
    fn 摘要会移除思考过程() {
        assert_eq!(
            clean_summary("<thinking>内部</thinking>公开摘要"),
            "公开摘要"
        );
    }
}
