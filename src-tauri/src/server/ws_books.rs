use super::error::{ApiError, ApiResult};
use super::ScopedState;
use super::{engine, llm, AppState};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const BOOK_DOMAIN: &str = "book";
const INPUTS_PREFIX: &str = "book_inputs:";
const SPINE_PREFIX: &str = "book_spine:";
const PAGE_PREFIX: &str = "book_page:";
const PROGRESS_PREFIX: &str = "book_progress:";
const CAPTURE_PREFIX: &str = "book_capture:";
const EVENT_PREFIX: &str = "book_event:";
const JOB_PREFIX: &str = "book_job:";

static STATE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static ACTIVE_JOBS: LazyLock<Mutex<HashMap<String, CancellationToken>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Debug)]
struct BookActionError {
    status: u16,
    code: &'static str,
    message: String,
    current_revision: Option<u64>,
}

impl BookActionError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: 400,
            code: "bad_request",
            message: message.into(),
            current_revision: None,
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: 404,
            code: "not_found",
            message: message.into(),
            current_revision: None,
        }
    }

    fn conflict(message: impl Into<String>, current_revision: Option<u64>) -> Self {
        Self {
            status: 409,
            code: "revision_conflict",
            message: message.into(),
            current_revision,
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: 500,
            code: "internal_error",
            message: message.into(),
            current_revision: None,
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "type": "error",
            "content": self.message,
            "status": self.status,
            "code": self.code,
            "current_revision": self.current_revision
        })
    }
}

impl From<String> for BookActionError {
    fn from(value: String) -> Self {
        Self::internal(value)
    }
}

pub async fn websocket(ws: WebSocketUpgrade, ScopedState(state): ScopedState) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    let (mut sender, mut receiver) = socket.split();
    let mut live = state.book_events.subscribe();
    let mut subscribed_book: Option<String> = None;
    let mut active_book: Option<String> = None;
    let mut active_create = false;
    let mut cursor = 0u64;
    let (result_tx, mut result_rx) = mpsc::unbounded_channel::<(Option<String>, Value)>();

    loop {
        tokio::select! {
            incoming = receiver.next() => {
                let Some(Ok(message)) = incoming else {
                    break;
                };
                let Message::Text(text) = message else {
                    if matches!(message, Message::Close(_)) {
                        break;
                    }
                    continue;
                };
                let payload = match serde_json::from_str::<Value>(&text) {
                    Ok(payload) => payload,
                    Err(error) => {
                        if send_json(&mut sender, &BookActionError::bad_request(format!(
                            "书籍 WebSocket 消息不是合法 JSON：{error}"
                        )).to_json()).await.is_err() {
                            break;
                        }
                        continue;
                    }
                };
                let kind = payload.get("type").and_then(Value::as_str).unwrap_or("").trim();
                if kind.is_empty() {
                    let _ = send_json(
                        &mut sender,
                        &BookActionError::bad_request("书籍 WebSocket 消息缺少 type 字段").to_json(),
                    ).await;
                    continue;
                }
                if kind == "subscribe" {
                    let book_id = payload.get("book_id").and_then(Value::as_str).unwrap_or("").trim();
                    if book_id.is_empty() {
                        let _ = send_json(
                            &mut sender,
                            &BookActionError::bad_request("subscribe 请求缺少 book_id").to_json(),
                        ).await;
                        continue;
                    }
                    if load_book(&state, book_id).ok().flatten().is_none() {
                        let _ = send_json(
                            &mut sender,
                            &BookActionError::not_found(format!("书籍不存在：{book_id}")).to_json(),
                        ).await;
                        continue;
                    }
                    let requested = payload
                        .get("after_seq")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let latest = latest_seq(&state, book_id).unwrap_or(0);
                    let reset = requested > latest;
                    let effective = if reset { 0 } else { requested };
                    if send_json(&mut sender, &json!({
                        "type": "subscribed",
                        "book_id": book_id,
                        "latest_seq": latest,
                        "reset": reset
                    })).await.is_err() {
                        break;
                    }
                    let replay = list_events_after(&state, book_id, effective).unwrap_or_default();
                    for event in replay {
                        cursor = cursor.max(event.get("seq").and_then(Value::as_u64).unwrap_or(0));
                        if send_json(&mut sender, &event).await.is_err() {
                            return;
                        }
                    }
                    subscribed_book = Some(book_id.to_string());
                    continue;
                }
                if !matches!(
                    kind,
                    "create" | "confirm_proposal" | "compile_page" | "regenerate_block"
                ) {
                    let _ = send_json(
                        &mut sender,
                        &BookActionError::bad_request(format!("未知书籍操作：{kind}")).to_json(),
                    ).await;
                    continue;
                }
                if active_book.is_some() || active_create {
                    let _ = send_json(
                        &mut sender,
                        &BookActionError::conflict("上一条书籍操作尚未结束", None).to_json(),
                    ).await;
                    continue;
                }
                let action_book = payload
                    .get("book_id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .filter(|value| !value.is_empty());
                active_create = kind == "create";
                active_book = action_book.clone();
                let task_state = state.clone();
                let tx = result_tx.clone();
                tokio::spawn(async move {
                    let result = match run_action(&task_state, &payload).await {
                        Ok(value) => value,
                        Err(error) => error.to_json(),
                    };
                    let result_book = result.get("book_id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or(action_book);
                    let _ = tx.send((result_book, result));
                });
            }
            event = live.recv() => {
                let Ok(event) = event else {
                    continue;
                };
                let event_book = event
                    .pointer("/metadata/book_id")
                    .and_then(Value::as_str)
                    .or_else(|| event.get("book_id").and_then(Value::as_str));
                let subscribed = subscribed_book.as_deref().is_some_and(|book_id| {
                    event_book.is_none() || event_book == Some(book_id)
                });
                let active = if active_create {
                    true
                } else {
                    active_book.as_deref().is_some_and(|book_id| {
                        event_book.is_none() || event_book == Some(book_id)
                    })
                };
                if !subscribed && !active {
                    continue;
                }
                let seq = event.get("seq").and_then(Value::as_u64).unwrap_or(0);
                if subscribed && seq > 0 && seq <= cursor {
                    continue;
                }
                if subscribed && seq > cursor {
                    cursor = seq;
                }
                if send_json(&mut sender, &event).await.is_err() {
                    return;
                }
            }
            result = result_rx.recv() => {
                let Some((result_book, result)) = result else {
                    continue;
                };
                if active_book.is_none() && !active_create && result_book.is_some() {
                    active_book = result_book;
                }
                if send_json(&mut sender, &result).await.is_err() {
                    return;
                }
                active_create = false;
                active_book = None;
            }
        }
    }
}

async fn send_json<S>(sender: &mut S, value: &Value) -> Result<(), ()>
where
    S: SinkExt<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    sender
        .send(Message::Text(value.to_string().into()))
        .await
        .map_err(|_| ())
}

async fn run_action(state: &AppState, payload: &Value) -> Result<Value, BookActionError> {
    match payload.get("type").and_then(Value::as_str).unwrap_or("") {
        "create" => create_book(state, payload.clone()).await,
        "confirm_proposal" => confirm_proposal(state, payload.clone()).await,
        "compile_page" => {
            let book_id = required_str(payload, "book_id")?;
            let page_id = required_str(payload, "page_id")?;
            let expected = payload.get("expected_revision").and_then(Value::as_u64);
            let force = payload
                .get("force")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let revision = claim_revision(state, &book_id, expected, "compile_page")?;
            let page = compile_page_now(state, &book_id, &page_id, force, None).await?;
            Ok(json!({
                "type": "compile_page_result",
                "book_id": book_id,
                "page": page,
                "book_revision": revision
            }))
        }
        "regenerate_block" => {
            let book_id = required_str(payload, "book_id")?;
            let page_id = required_str(payload, "page_id")?;
            let block_id = required_str(payload, "block_id")?;
            let expected = payload.get("expected_revision").and_then(Value::as_u64);
            let revision = claim_revision(state, &book_id, expected, "regenerate_block")?;
            let block = regenerate_block_now(
                state,
                &book_id,
                &page_id,
                &block_id,
                payload.get("params_override").cloned(),
                None,
            )
            .await?;
            Ok(json!({
                "type": "regenerate_block_result",
                "book_id": book_id,
                "block": block,
                "book_revision": revision
            }))
        }
        other => Err(BookActionError::bad_request(format!(
            "未知书籍操作：{other}"
        ))),
    }
}

fn required_str(payload: &Value, key: &str) -> Result<String, BookActionError> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| BookActionError::bad_request(format!("缺少字段 {key}")))
}

fn now() -> f64 {
    crate::storage::now()
}

fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4().simple())
}

fn book_exists(state: &AppState, book_id: &str) -> Result<bool, String> {
    Ok(state.database.get_resource(BOOK_DOMAIN, book_id)?.is_some())
}

fn load_book(state: &AppState, book_id: &str) -> Result<Option<Value>, String> {
    state.database.get_resource(BOOK_DOMAIN, book_id)
}

fn save_book(state: &AppState, book: &mut Value) -> Result<(), String> {
    let book_id = book
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| "书籍缺少 id".to_string())?
        .to_string();
    book["updated_at"] = Value::from(now());
    state.database.put_resource(BOOK_DOMAIN, &book_id, book)?;
    Ok(())
}

fn inputs_domain(book_id: &str) -> String {
    format!("{INPUTS_PREFIX}{book_id}")
}

fn spine_domain(book_id: &str) -> String {
    format!("{SPINE_PREFIX}{book_id}")
}

fn page_domain(book_id: &str) -> String {
    format!("{PAGE_PREFIX}{book_id}")
}

fn progress_domain(book_id: &str) -> String {
    format!("{PROGRESS_PREFIX}{book_id}")
}

fn capture_domain(book_id: &str) -> String {
    format!("{CAPTURE_PREFIX}{book_id}")
}

fn event_domain(book_id: &str) -> String {
    format!("{EVENT_PREFIX}{book_id}")
}

fn load_spine(state: &AppState, book_id: &str) -> Result<Option<Value>, String> {
    state.database.get_resource(&spine_domain(book_id), "spine")
}

fn save_spine(state: &AppState, book_id: &str, spine: &mut Value) -> Result<(), String> {
    spine["book_id"] = Value::String(book_id.to_string());
    spine["updated_at"] = Value::from(now());
    state
        .database
        .put_resource(&spine_domain(book_id), "spine", spine)?;
    Ok(())
}

fn load_page(state: &AppState, book_id: &str, page_id: &str) -> Result<Option<Value>, String> {
    state.database.get_resource(&page_domain(book_id), page_id)
}

fn save_page(state: &AppState, book_id: &str, page: &mut Value) -> Result<(), String> {
    let page_id = page
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| "页面缺少 id".to_string())?
        .to_string();
    page["book_id"] = Value::String(book_id.to_string());
    page["updated_at"] = Value::from(now());
    state
        .database
        .put_resource(&page_domain(book_id), &page_id, page)?;
    Ok(())
}

fn list_pages(state: &AppState, book_id: &str) -> Result<Vec<Value>, String> {
    let mut pages = state.database.list_resources(&page_domain(book_id))?;
    pages.sort_by(|left, right| {
        let left_order = left.get("order").and_then(Value::as_i64).unwrap_or(0);
        let right_order = right.get("order").and_then(Value::as_i64).unwrap_or(0);
        let left_created = left
            .get("created_at")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let right_created = right
            .get("created_at")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        left_order
            .cmp(&right_order)
            .then_with(|| left_created.total_cmp(&right_created))
    });
    Ok(pages)
}

fn load_progress(state: &AppState, book_id: &str) -> Result<Value, String> {
    if let Some(progress) = state
        .database
        .get_resource(&progress_domain(book_id), "progress")?
    {
        return Ok(progress);
    }
    let progress = json!({
        "book_id": book_id,
        "current_page_id": "",
        "visited_page_ids": [],
        "bookmarked_page_ids": [],
        "quiz_attempts": [],
        "weak_chapters": [],
        "score": 0,
        "updated_at": now()
    });
    state
        .database
        .put_resource(&progress_domain(book_id), "progress", &progress)?;
    Ok(progress)
}

fn save_progress(state: &AppState, book_id: &str, progress: &mut Value) -> Result<(), String> {
    progress["book_id"] = Value::String(book_id.to_string());
    progress["updated_at"] = Value::from(now());
    state
        .database
        .put_resource(&progress_domain(book_id), "progress", progress)?;
    Ok(())
}

fn latest_seq(state: &AppState, book_id: &str) -> Result<u64, String> {
    Ok(state
        .database
        .list_resources(&event_domain(book_id))?
        .into_iter()
        .filter_map(|event| event.get("seq").and_then(Value::as_u64))
        .max()
        .unwrap_or(0))
}

fn list_events_after(
    state: &AppState,
    book_id: &str,
    after_seq: u64,
) -> Result<Vec<Value>, String> {
    let mut events = state
        .database
        .list_resources(&event_domain(book_id))?
        .into_iter()
        .filter(|event| event.get("seq").and_then(Value::as_u64).unwrap_or(0) > after_seq)
        .collect::<Vec<_>>();
    events.sort_by_key(|event| event.get("seq").and_then(Value::as_u64).unwrap_or(0));
    Ok(events)
}

fn append_event(
    state: &AppState,
    book_id: &str,
    kind: &str,
    stage: &str,
    page_id: &str,
    block_id: &str,
    extra: Value,
) -> Result<Value, String> {
    let _guard = STATE_LOCK.lock();
    let seq = latest_seq(state, book_id)? + 1;
    let mut metadata = extra
        .get("metadata")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    // 额外元数据不能覆盖事件路由所依赖的规范字段。
    metadata.insert("kind".to_string(), Value::String(kind.to_string()));
    metadata.insert("book_id".to_string(), Value::String(book_id.to_string()));
    if !page_id.is_empty() {
        metadata.insert("page_id".to_string(), Value::String(page_id.to_string()));
    }
    if !block_id.is_empty() {
        metadata.insert("block_id".to_string(), Value::String(block_id.to_string()));
    }
    let event_type = match kind {
        "stage_start" | "stage_end" => kind,
        "error" => "error",
        _ => "progress",
    };
    let event = json!({
        "id": seq,
        "seq": seq,
        "type": event_type,
        "source": "book_engine",
        "content": kind,
        "stage": stage,
        "book_id": book_id,
        "metadata": metadata,
        "created_at": now()
    });
    let id = format!("{seq:020}");
    state
        .database
        .put_resource(&event_domain(book_id), &id, &event)?;
    let _ = state.book_events.send(event.clone());
    Ok(event)
}

fn emit(
    state: &AppState,
    book_id: &str,
    kind: &str,
    stage: &str,
    page_id: &str,
    block_id: &str,
    metadata: Value,
) {
    let _ = append_event(
        state,
        book_id,
        kind,
        stage,
        page_id,
        block_id,
        json!({"metadata": metadata}),
    );
}

fn claim_revision(
    state: &AppState,
    book_id: &str,
    expected: Option<u64>,
    operation: &str,
) -> Result<u64, BookActionError> {
    let _guard = STATE_LOCK.lock();
    let mut book = load_book(state, book_id)
        .map_err(BookActionError::internal)?
        .ok_or_else(|| BookActionError::not_found(format!("书籍不存在：{book_id}")))?;
    let revision = book.get("revision").and_then(Value::as_u64).unwrap_or(1);
    if let Some(expected) = expected {
        if expected != revision {
            return Err(BookActionError::conflict(
                format!("书籍内容已更新，当前版本为 {revision}"),
                Some(revision),
            ));
        }
    }
    let next = revision.saturating_add(1);
    book["revision"] = Value::from(next);
    book["last_operation"] = Value::String(operation.to_string());
    save_book(state, &mut book).map_err(BookActionError::internal)?;
    Ok(next)
}

fn language_is_chinese(language: &str) -> bool {
    language.to_ascii_lowercase().starts_with("zh")
}

fn detect_language(requested: &str, intent: &str, fallback: &str) -> String {
    let requested = requested.trim();
    if !requested.is_empty() && requested != "en" {
        return requested.to_string();
    }
    if intent
        .chars()
        .any(|character| ('\u{4e00}'..='\u{9fff}').contains(&character))
    {
        return "zh".to_string();
    }
    if requested.is_empty() {
        fallback.to_string()
    } else {
        requested.to_string()
    }
}

async fn llm_text(
    state: &AppState,
    system_prompt: &str,
    user_prompt: &str,
    cancel: &CancellationToken,
) -> Result<String, String> {
    let settings = engine::resolve_llm_settings(state, &json!({}))?;
    let messages = vec![
        json!({"role": "system", "content": system_prompt}),
        json!({"role": "user", "content": user_prompt}),
    ];
    let response = llm::stream_completion(
        &state.client,
        &settings,
        &messages,
        &[],
        None,
        cancel,
        |_| {},
    )
    .await?;
    let text = response.content.trim().to_string();
    if text.is_empty() {
        return Err("模型没有返回可见文本".to_string());
    }
    Ok(text)
}

async fn llm_json(
    state: &AppState,
    system_prompt: &str,
    user_prompt: &str,
    cancel: &CancellationToken,
) -> Result<Value, String> {
    let strict_system = format!(
        "{system_prompt}\n\n只输出一个合法 JSON 对象，不要 Markdown 代码围栏，不要输出解释性前后缀。"
    );
    let mut last_error = String::from("模型没有返回可用 JSON");
    for attempt in 0..2 {
        let retry_suffix = if attempt == 0 {
            ""
        } else {
            "\n\n上一次响应无法解析。请重新输出严格合法、完整闭合的 JSON 对象。"
        };
        let raw = llm_text(
            state,
            &strict_system,
            &format!("{user_prompt}{retry_suffix}"),
            cancel,
        )
        .await?;
        match parse_json_object(&raw) {
            Ok(value) => return Ok(value),
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

fn parse_json_object(raw: &str) -> Result<Value, String> {
    let trimmed = raw.trim();
    let without_fence = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```JSON"))
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|value| value.strip_suffix("```"))
        .unwrap_or(trimmed)
        .trim();
    if let Ok(value) = serde_json::from_str::<Value>(without_fence) {
        if value.is_object() {
            return Ok(value);
        }
    }
    let start = without_fence
        .find('{')
        .ok_or_else(|| "模型响应中没有 JSON 对象".to_string())?;
    let end = without_fence
        .rfind('}')
        .ok_or_else(|| "模型响应中的 JSON 对象未闭合".to_string())?;
    if end <= start {
        return Err("模型响应中的 JSON 对象范围无效".to_string());
    }
    serde_json::from_str(&without_fence[start..=end])
        .map_err(|error| format!("无法解析模型返回的 JSON：{error}"))
}

fn source_context(
    state: &AppState,
    knowledge_bases: &[String],
    query: &str,
    limit: usize,
) -> String {
    if knowledge_bases.is_empty() {
        return String::new();
    }
    let terms = query
        .to_lowercase()
        .split_whitespace()
        .filter(|term| term.chars().count() > 1)
        .map(str::to_string)
        .collect::<HashSet<_>>();
    let mut scored = Vec::new();
    for chunk in state
        .database
        .list_resources("kb_chunk")
        .unwrap_or_default()
    {
        let kb_name = chunk
            .get("kb_name")
            .or_else(|| chunk.get("knowledge_base"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if !knowledge_bases.iter().any(|name| name == kb_name) {
            continue;
        }
        let text = chunk
            .get("text")
            .or_else(|| chunk.get("content"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if text.trim().is_empty() {
            continue;
        }
        let lower = text.to_lowercase();
        let score = terms
            .iter()
            .filter(|term| lower.contains(term.as_str()))
            .count();
        scored.push((score, text.to_string()));
    }
    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    let mut context = String::new();
    for (_, text) in scored.into_iter().take(limit) {
        let preview = text.chars().take(1200).collect::<String>();
        if !context.is_empty() {
            context.push_str("\n---\n");
        }
        context.push_str(&preview);
        if context.chars().count() >= 10_000 {
            break;
        }
    }
    context
}

async fn create_book(state: &AppState, payload: Value) -> Result<Value, BookActionError> {
    let intent = payload
        .get("user_intent")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if intent.is_empty() {
        return Err(BookActionError::bad_request("创建书籍需要 user_intent"));
    }
    let requested_language = payload
        .get("language")
        .and_then(Value::as_str)
        .unwrap_or("en");
    let fallback_language = payload
        .get("fallback_language")
        .and_then(Value::as_str)
        .unwrap_or("en");
    let language = detect_language(requested_language, &intent, fallback_language);
    let depth = payload
        .get("depth")
        .and_then(Value::as_str)
        .unwrap_or("standard")
        .to_string();
    let knowledge_bases = payload
        .get("knowledge_bases")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|value| value.as_str().map(str::to_string))
        .collect::<Vec<_>>();
    let book_id = new_id("bk");
    let cancel = register_job(&book_id);
    emit(
        state,
        &book_id,
        "stage_start",
        "ideation",
        "",
        "",
        json!({"stage": "ideation"}),
    );
    let source_summary = source_context(state, &knowledge_bases, &intent, 6);
    let proposal_prompt = if language_is_chinese(&language) {
        format!(
            "学习目标：{intent}\n\n可用资料：\n{}\n\n请设计一本结构完整、可由章节阅读和练习组成的学习书。",
            if source_summary.is_empty() {
                "（无额外资料）"
            } else {
                &source_summary
            }
        )
    } else {
        format!(
            "Learning goal: {intent}\n\nAvailable source material:\n{}\n\nDesign one coherent learning book.",
            if source_summary.is_empty() {
                "(none)"
            } else {
                &source_summary
            }
        )
    };
    let proposal = llm_json(
        state,
        if language_is_chinese(&language) {
            "你是 DeepTutor 的课程策划。输出字段：title、description、scope、target_level、estimated_chapters（2 到 8 的整数）、rationale。"
        } else {
            "You are DeepTutor's curriculum architect. Return title, description, scope, target_level, estimated_chapters (2-8), and rationale."
        },
        &proposal_prompt,
        &cancel,
    )
    .await
    .map(|value| normalize_proposal(value, &intent, &language))
    .unwrap_or_else(|_| fallback_proposal(&intent, &language));

    let timestamp = now();
    let title = proposal
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or(&intent)
        .to_string();
    let mut book = json!({
        "id": book_id,
        "revision": 1,
        "title": title,
        "description": proposal.get("description").cloned().unwrap_or(Value::String(String::new())),
        "status": "draft",
        "proposal": proposal,
        "knowledge_bases": knowledge_bases,
        "language": language,
        "depth": depth,
        "page_count": 0,
        "chapter_count": proposal.get("estimated_chapters").and_then(Value::as_u64).unwrap_or(4),
        "created_at": timestamp,
        "updated_at": timestamp,
        "metadata": {
            "lazy_compile": false,
            "source_quality": {
                "status": if source_summary.is_empty() { "warning" } else { "ready" },
                "requested_kbs": payload.get("knowledge_bases").cloned().unwrap_or_else(|| json!([])),
                "covered_kbs": payload.get("knowledge_bases").cloned().unwrap_or_else(|| json!([])),
                "missing_kbs": [],
                "coverage": {},
                "chunk_count": if source_summary.is_empty() { 0 } else { source_summary.lines().count() },
                "warnings": if source_summary.is_empty() { json!(["未检索到知识库材料，书籍将依据学习目标生成。"]) } else { json!([]) }
            }
        },
        "kb_fingerprints": {},
        "kb_document_fingerprints": {},
        "stale_page_ids": [],
        "stale_detected_at": 0
    });
    save_book(state, &mut book).map_err(BookActionError::internal)?;
    let inputs = payload.clone();
    state
        .database
        .put_resource(&inputs_domain(&book_id), "inputs", &inputs)
        .map_err(BookActionError::internal)?;
    load_progress(state, &book_id).map_err(BookActionError::internal)?;
    emit(
        state,
        &book_id,
        "proposal_ready",
        "ideation",
        "",
        "",
        json!({
            "title": book.get("title").cloned().unwrap_or(Value::Null),
            "proposal": book.get("proposal").cloned().unwrap_or(Value::Null)
        }),
    );
    emit(
        state,
        &book_id,
        "stage_end",
        "ideation",
        "",
        "",
        json!({"stage": "ideation"}),
    );
    unregister_job(&book_id);
    let book = load_book(state, &book_id)
        .map_err(BookActionError::internal)?
        .ok_or_else(|| BookActionError::internal("书籍创建后无法读取"))?;
    Ok(json!({
        "type": "create_result",
        "book_id": book_id,
        "book": book,
        "proposal": book.get("proposal").cloned().unwrap_or(Value::Null)
    }))
}

fn normalize_proposal(mut value: Value, intent: &str, language: &str) -> Value {
    let fallback = fallback_proposal(intent, language);
    for key in ["title", "description", "scope", "target_level", "rationale"] {
        if !value.get(key).is_some_and(Value::is_string) {
            value[key] = fallback
                .get(key)
                .cloned()
                .unwrap_or(Value::String(String::new()));
        }
    }
    let chapters = value
        .get("estimated_chapters")
        .and_then(Value::as_u64)
        .unwrap_or(4)
        .clamp(2, 8);
    value["estimated_chapters"] = Value::from(chapters);
    value
}

fn fallback_proposal(intent: &str, language: &str) -> Value {
    let title = intent
        .lines()
        .next()
        .unwrap_or(intent)
        .trim()
        .chars()
        .take(80)
        .collect::<String>();
    if language_is_chinese(language) {
        json!({
            "title": if title.is_empty() { "自定义学习书".to_string() } else { title },
            "description": format!("围绕“{intent}”组织的一本循序渐进学习书。"),
            "scope": "从核心概念到综合应用",
            "target_level": "mixed",
            "estimated_chapters": 5,
            "rationale": "先建立概念基础，再通过示例、练习和复习巩固迁移能力。"
        })
    } else {
        json!({
            "title": if title.is_empty() { "Custom Learning Book".to_string() } else { title },
            "description": format!("A progressive learning book built around: {intent}"),
            "scope": "From core concepts to applied synthesis",
            "target_level": "mixed",
            "estimated_chapters": 5,
            "rationale": "Build foundations first, then consolidate through examples, practice, and review."
        })
    }
}

async fn confirm_proposal(state: &AppState, payload: Value) -> Result<Value, BookActionError> {
    let book_id = required_str(&payload, "book_id")?;
    let expected = payload.get("expected_revision").and_then(Value::as_u64);
    let revision = claim_revision(state, &book_id, expected, "confirm_proposal")?;
    let mut book = load_book(state, &book_id)
        .map_err(BookActionError::internal)?
        .ok_or_else(|| BookActionError::not_found(format!("书籍不存在：{book_id}")))?;
    if let Some(edited) = payload.get("proposal").filter(|value| value.is_object()) {
        book["proposal"] = edited.clone();
        if let Some(title) = edited.get("title").and_then(Value::as_str) {
            if !title.trim().is_empty() {
                book["title"] = Value::String(title.to_string());
            }
        }
        if let Some(description) = edited.get("description").and_then(Value::as_str) {
            book["description"] = Value::String(description.to_string());
        }
        save_book(state, &mut book).map_err(BookActionError::internal)?;
    }
    let language = book
        .get("language")
        .and_then(Value::as_str)
        .unwrap_or("en")
        .to_string();
    let proposal = book.get("proposal").cloned().unwrap_or_else(|| {
        fallback_proposal(
            book.get("title").and_then(Value::as_str).unwrap_or(""),
            &language,
        )
    });
    let knowledge_bases = book
        .get("knowledge_bases")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|value| value.as_str().map(str::to_string))
        .collect::<Vec<_>>();
    let query = proposal.get("title").and_then(Value::as_str).unwrap_or("");
    let context = source_context(state, &knowledge_bases, query, 8);
    let cancel = register_job(&book_id);
    emit(
        state,
        &book_id,
        "stage_start",
        "exploration",
        "",
        "",
        json!({"stage": "exploration"}),
    );
    emit(
        state,
        &book_id,
        "exploration_ready",
        "exploration",
        "",
        "",
        json!({
            "queries": [query],
            "coverage": if context.is_empty() { json!({}) } else { json!({"kb": context.lines().count()}) },
            "candidate_concepts": [],
            "summary": context.chars().take(600).collect::<String>()
        }),
    );
    emit(
        state,
        &book_id,
        "stage_end",
        "exploration",
        "",
        "",
        json!({"stage": "exploration"}),
    );
    emit(
        state,
        &book_id,
        "stage_start",
        "synthesis",
        "",
        "",
        json!({"stage": "synthesis"}),
    );
    let prompt = if language_is_chinese(&language) {
        format!(
            "书籍提案：\n{}\n\n资料：\n{}\n\n请输出 chapters 数组和 concept_graph。每章字段：title、learning_objectives、content_type（theory/derivation/history/practice/concept）、source_anchors、prerequisites、summary。concept_graph 字段为 nodes 和 edges。章节数量遵循 estimated_chapters。",
            serde_json::to_string_pretty(&proposal).unwrap_or_default(),
            if context.is_empty() { "（无）" } else { &context }
        )
    } else {
        format!(
            "Book proposal:\n{}\n\nMaterial:\n{}\n\nReturn a chapters array and concept_graph. Each chapter has title, learning_objectives, content_type (theory/derivation/history/practice/concept), source_anchors, prerequisites, summary. concept_graph has nodes and edges.",
            serde_json::to_string_pretty(&proposal).unwrap_or_default(),
            if context.is_empty() { "(none)" } else { &context }
        )
    };
    let generated = llm_json(
        state,
        if language_is_chinese(&language) {
            "你是 DeepTutor 的章节架构师。输出结构清晰、先修关系合理的章节树。"
        } else {
            "You are DeepTutor's spine architect. Design a well-sequenced chapter tree."
        },
        &prompt,
        &cancel,
    )
    .await
    .unwrap_or_else(|_| fallback_spine_payload(&proposal, &language));
    let mut spine = normalize_spine(&book_id, generated, &proposal, &language);
    save_spine(state, &book_id, &mut spine).map_err(BookActionError::internal)?;
    book["status"] = Value::String("spine_ready".to_string());
    book["chapter_count"] = Value::from(
        spine
            .get("chapters")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0),
    );
    save_book(state, &mut book).map_err(BookActionError::internal)?;
    emit(
        state,
        &book_id,
        "spine_ready",
        "synthesis",
        "",
        "",
        json!({
            "chapter_count": book.get("chapter_count").cloned().unwrap_or(Value::from(0)),
            "concept_node_count": spine.pointer("/concept_graph/nodes").and_then(Value::as_array).map(Vec::len).unwrap_or(0),
            "concept_edge_count": spine.pointer("/concept_graph/edges").and_then(Value::as_array).map(Vec::len).unwrap_or(0),
            "spine": spine
        }),
    );
    emit(
        state,
        &book_id,
        "stage_end",
        "synthesis",
        "",
        "",
        json!({"stage": "synthesis"}),
    );
    unregister_job(&book_id);
    Ok(json!({
        "type": "confirm_proposal_result",
        "book_id": book_id,
        "book": book,
        "spine": spine,
        "book_revision": revision
    }))
}

fn fallback_spine_payload(proposal: &Value, language: &str) -> Value {
    let title = proposal
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("Learning Book");
    let count = proposal
        .get("estimated_chapters")
        .and_then(Value::as_u64)
        .unwrap_or(5)
        .clamp(2, 8) as usize;
    let (types, names) = if language_is_chinese(language) {
        (
            [
                "concept",
                "theory",
                "derivation",
                "practice",
                "history",
                "theory",
                "practice",
                "concept",
            ],
            [
                "基础概念",
                "核心原理",
                "推理与证明",
                "练习与反馈",
                "背景与演进",
                "综合联系",
                "迁移应用",
                "总结复习",
            ],
        )
    } else {
        (
            [
                "concept",
                "theory",
                "derivation",
                "practice",
                "history",
                "theory",
                "practice",
                "concept",
            ],
            [
                "Foundations",
                "Core Principles",
                "Reasoning and Proof",
                "Practice and Feedback",
                "Context and Evolution",
                "Synthesis",
                "Transfer",
                "Review",
            ],
        )
    };
    let chapters = (0..count)
        .map(|index| {
            json!({
                "id": new_id("ch"),
                "title": if index == 0 { format!("{title}: {}", names[index]) } else { names[index].to_string() },
                "learning_objectives": [
                    if language_is_chinese(language) {
                        format!("理解并解释{}的关键概念", names[index])
                    } else {
                        format!("Understand and explain the key ideas of {}", names[index])
                    }
                ],
                "content_type": types[index],
                "source_anchors": [],
                "prerequisites": if index == 0 { json!([]) } else { json!([index - 1]) },
                "summary": format!("{} — {title}", names[index]),
                "order": index
            })
        })
        .collect::<Vec<_>>();
    let nodes = chapters
        .iter()
        .enumerate()
        .map(|(index, chapter)| {
            json!({
                "id": format!("concept_{index}"),
                "label": chapter.get("title").and_then(Value::as_str).unwrap_or(""),
                "chapter_id": chapter.get("id").and_then(Value::as_str).unwrap_or(""),
                "description": chapter.get("summary").and_then(Value::as_str).unwrap_or(""),
                "weight": 1.0
            })
        })
        .collect::<Vec<_>>();
    let edges = (1..count)
        .map(|index| {
            json!({
                "src": format!("concept_{}", index - 1),
                "dst": format!("concept_{index}"),
                "relation": "depends_on",
                "rationale": "后续章节建立在前一章概念之上"
            })
        })
        .collect::<Vec<_>>();
    json!({
        "chapters": chapters,
        "concept_graph": {"nodes": nodes, "edges": edges},
        "exploration_summary": ""
    })
}

fn normalize_spine(book_id: &str, value: Value, proposal: &Value, language: &str) -> Value {
    let mut chapters = value
        .get("chapters")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if chapters.is_empty() {
        return fallback_spine_payload(proposal, language).as_object().map_or_else(
            || json!({"book_id": book_id, "chapters": [], "version": 1, "updated_at": now(), "concept_graph": {"nodes": [], "edges": []}}),
            |object| {
                let mut object = object.clone();
                object.insert("book_id".to_string(), Value::String(book_id.to_string()));
                object.insert("version".to_string(), Value::from(1));
                object.insert("updated_at".to_string(), Value::from(now()));
                Value::Object(object)
            },
        );
    }
    for (index, chapter) in chapters.iter_mut().enumerate() {
        if !chapter.get("id").is_some_and(Value::is_string) {
            chapter["id"] = Value::String(new_id("ch"));
        }
        chapter["order"] = Value::from(index);
        if !chapter.get("title").is_some_and(Value::is_string) {
            chapter["title"] = Value::String(format!("Chapter {}", index + 1));
        }
        if !chapter.get("content_type").is_some_and(Value::is_string) {
            chapter["content_type"] = Value::String("theory".to_string());
        }
        if !chapter
            .get("learning_objectives")
            .is_some_and(Value::is_array)
        {
            chapter["learning_objectives"] = json!([]);
        }
        if !chapter.get("source_anchors").is_some_and(Value::is_array) {
            chapter["source_anchors"] = json!([]);
        }
        if !chapter.get("prerequisites").is_some_and(Value::is_array) {
            chapter["prerequisites"] = json!([]);
        }
        if !chapter.get("page_ids").is_some_and(Value::is_array) {
            chapter["page_ids"] = json!([]);
        }
        if !chapter.get("summary").is_some_and(Value::is_string) {
            chapter["summary"] = Value::String(String::new());
        }
    }
    let graph = value
        .get("concept_graph")
        .cloned()
        .unwrap_or_else(|| json!({"nodes": [], "edges": []}));
    json!({
        "book_id": book_id,
        "chapters": chapters,
        "version": value.get("version").and_then(Value::as_u64).unwrap_or(1),
        "updated_at": now(),
        "concept_graph": {
            "nodes": graph.get("nodes").cloned().unwrap_or_else(|| json!([])),
            "edges": graph.get("edges").cloned().unwrap_or_else(|| json!([]))
        },
        "exploration_summary": value.get("exploration_summary").cloned().unwrap_or(Value::String(String::new()))
    })
}

fn register_job(book_id: &str) -> CancellationToken {
    let mut jobs = ACTIVE_JOBS.lock();
    if let Some(existing) = jobs.get(book_id) {
        return existing.clone();
    }
    let token = CancellationToken::new();
    jobs.insert(book_id.to_string(), token.clone());
    token
}

fn unregister_job(book_id: &str) {
    ACTIVE_JOBS.lock().remove(book_id);
}

pub fn is_book_working(book_id: &str) -> bool {
    ACTIVE_JOBS.lock().contains_key(book_id)
}

fn cancel_book_job(book_id: &str) {
    if let Some(token) = ACTIVE_JOBS.lock().remove(book_id) {
        token.cancel();
    }
}

fn block_template(content_type: &str) -> Vec<(&'static str, Value)> {
    match content_type {
        "derivation" => vec![
            ("section", json!({"role": "setup", "target_words": 1400})),
            (
                "animation",
                json!({"focus": "推导过程", "transition_in": "逐步观察推导过程"}),
            ),
            (
                "section",
                json!({"role": "formal_proof", "target_words": 1200}),
            ),
            (
                "code",
                json!({"language": "python", "intent": "verify", "transition_in": "用数值方法验证结果"}),
            ),
            (
                "callout",
                json!({"variant": "key_idea", "transition_in": "提炼结果的含义"}),
            ),
            (
                "section",
                json!({"role": "interpretation", "target_words": 1000}),
            ),
            (
                "quiz",
                json!({"num_questions": 2, "transition_in": "检查你的推导理解"}),
            ),
        ],
        "history" => vec![
            ("section", json!({"role": "context", "target_words": 1200})),
            ("timeline", json!({"transition_in": "关键里程碑"})),
            (
                "section",
                json!({"role": "narrative", "target_words": 1500}),
            ),
            (
                "figure",
                json!({"variant": "illustration", "transition_in": "用图示梳理时代脉络"}),
            ),
            (
                "callout",
                json!({"variant": "summary", "transition_in": "这段历史今天的意义"}),
            ),
            ("section", json!({"role": "analysis", "target_words": 1000})),
            (
                "quiz",
                json!({"num_questions": 2, "transition_in": "快速回顾"}),
            ),
        ],
        "practice" => vec![
            ("section", json!({"role": "brief", "target_words": 1000})),
            (
                "quiz",
                json!({"num_questions": 3, "difficulty": "easy", "transition_in": "热身练习"}),
            ),
            (
                "code",
                json!({"language": "python", "intent": "scaffold", "transition_in": "亲手试一试"}),
            ),
            (
                "section",
                json!({"role": "walkthrough", "target_words": 1200}),
            ),
            (
                "interactive",
                json!({"interaction": "guided exercise", "transition_in": "交互练习"}),
            ),
            (
                "quiz",
                json!({"num_questions": 3, "difficulty": "hard", "transition_in": "进一步挑战"}),
            ),
            (
                "callout",
                json!({"variant": "common_pitfall", "transition_in": "留意常见陷阱"}),
            ),
        ],
        "concept" => vec![
            (
                "section",
                json!({"role": "definition", "target_words": 1400}),
            ),
            (
                "figure",
                json!({"variant": "mindmap", "transition_in": "连接相关概念"}),
            ),
            ("section", json!({"role": "examples", "target_words": 1200})),
            (
                "flash_cards",
                json!({"count": 5, "transition_in": "用记忆卡巩固"}),
            ),
            (
                "callout",
                json!({"variant": "common_pitfall", "transition_in": "常见误区"}),
            ),
            (
                "figure",
                json!({"variant": "comparison", "transition_in": "并排比较"}),
            ),
            (
                "quiz",
                json!({"num_questions": 3, "transition_in": "自我检查"}),
            ),
        ],
        _ => vec![
            (
                "section",
                json!({"role": "introduction", "target_words": 1200}),
            ),
            (
                "figure",
                json!({"variant": "diagram", "transition_in": "可视化核心结构"}),
            ),
            (
                "section",
                json!({"role": "deep_dive", "target_words": 1600}),
            ),
            (
                "callout",
                json!({"variant": "key_idea", "transition_in": "需要记住的核心思想"}),
            ),
            (
                "code",
                json!({"language": "python", "intent": "example", "transition_in": "代码示例"}),
            ),
            ("section", json!({"role": "synthesis", "target_words": 800})),
            (
                "quiz",
                json!({"num_questions": 3, "transition_in": "检查理解"}),
            ),
            (
                "flash_cards",
                json!({"count": 5, "transition_in": "快速记忆"}),
            ),
        ],
    }
}

fn chapter_strings(chapter: &Value, key: &str) -> Vec<String> {
    chapter
        .get(key)
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn make_block(
    block_type: &str,
    mut params: Value,
    chapter: &Value,
    transition: Option<String>,
) -> Value {
    if let Some(params) = params.as_object_mut() {
        params.insert(
            "chapter_title".to_string(),
            chapter
                .get("title")
                .cloned()
                .unwrap_or(Value::String(String::new())),
        );
        params.insert(
            "chapter_summary".to_string(),
            chapter
                .get("summary")
                .cloned()
                .unwrap_or(Value::String(String::new())),
        );
        params.insert(
            "objectives".to_string(),
            chapter
                .get("learning_objectives")
                .cloned()
                .unwrap_or_else(|| json!([])),
        );
        params.insert(
            "anchors".to_string(),
            chapter
                .get("source_anchors")
                .cloned()
                .unwrap_or_else(|| json!([])),
        );
    }
    let mut metadata = Map::new();
    if let Some(transition) = transition.filter(|value| !value.trim().is_empty()) {
        metadata.insert("transition_in".to_string(), Value::String(transition));
    }
    let timestamp = now();
    json!({
        "id": new_id("blk"),
        "type": block_type,
        "status": "pending",
        "title": "",
        "params": params,
        "payload": {},
        "source_anchors": [],
        "metadata": metadata,
        "error": "",
        "created_at": timestamp,
        "updated_at": timestamp
    })
}

fn static_plan(book: &Value, chapter: &Value) -> Vec<Value> {
    let content_type = chapter
        .get("content_type")
        .and_then(Value::as_str)
        .unwrap_or("theory");
    let allowed = book
        .pointer("/metadata/block_types")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(|value| value.to_string())
                .collect::<HashSet<_>>()
        });
    let mut blocks = block_template(content_type)
        .into_iter()
        .filter(|(kind, _)| {
            *kind == "section"
                || allowed
                    .as_ref()
                    .map(|allowed| allowed.contains(*kind))
                    .unwrap_or(true)
        })
        .map(|(kind, mut params)| {
            let transition = params
                .as_object_mut()
                .and_then(|object| object.remove("transition_in"))
                .and_then(|value| value.as_str().map(str::to_string));
            make_block(kind, params, chapter, transition)
        })
        .collect::<Vec<_>>();
    if blocks.is_empty() {
        blocks.push(make_block(
            "section",
            json!({"role": "core", "target_words": 1700}),
            chapter,
            None,
        ));
    }
    blocks
}

async fn plan_blocks(
    state: &AppState,
    book: &Value,
    chapter: &Value,
    language: &str,
    cancel: &CancellationToken,
) -> Vec<Value> {
    let objectives = chapter_strings(chapter, "learning_objectives");
    let chapter_title = chapter.get("title").and_then(Value::as_str).unwrap_or("");
    let chapter_summary = chapter.get("summary").and_then(Value::as_str).unwrap_or("");
    let content_type = chapter
        .get("content_type")
        .and_then(Value::as_str)
        .unwrap_or("theory");
    let prompt = if language_is_chinese(language) {
        format!(
            "章节：{chapter_title}\n摘要：{chapter_summary}\n内容类型：{content_type}\n学习目标：\n- {}\n\n输出 blocks 数组。每个元素只有 type、focus、transition_in、params。可用 type：section、text、callout、quiz、figure、interactive、animation、code、timeline、flash_cards。至少包含一个 section，总数 4 到 9。",
            objectives.join("\n- ")
        )
    } else {
        format!(
            "Chapter: {chapter_title}\nSummary: {chapter_summary}\nContent type: {content_type}\nObjectives:\n- {}\n\nReturn a blocks array. Each item has type, focus, transition_in, params. Allowed types: section, text, callout, quiz, figure, interactive, animation, code, timeline, flash_cards. Include at least one section and 4-9 blocks total.",
            objectives.join("\n- ")
        )
    };
    if let Ok(value) = llm_json(
        state,
        if language_is_chinese(language) {
            "你是 DeepTutor 的章节内容规划器。为学习者安排由浅入深、具有视觉与练习支持的内容块顺序。"
        } else {
            "You are DeepTutor's section planner. Order a pedagogically sound sequence of content blocks."
        },
        &prompt,
        cancel,
    )
    .await
    {
        if let Some(items) = value.get("blocks").and_then(Value::as_array) {
            let allowed = book
                .pointer("/metadata/block_types")
                .and_then(Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect::<HashSet<_>>()
                });
            let mut blocks = Vec::new();
            for item in items.iter().take(10) {
                let kind = item
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_ascii_lowercase();
                if !is_plannable_block(&kind) {
                    continue;
                }
                if kind != "section"
                    && allowed
                        .as_ref()
                        .is_some_and(|allowed| !allowed.contains(&kind))
                {
                    continue;
                }
                let mut params = item
                    .get("params")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                if let Some(focus) = item.get("focus").and_then(Value::as_str) {
                    params.insert("focus".to_string(), Value::String(focus.to_string()));
                }
                blocks.push(make_block(
                    &kind,
                    Value::Object(params),
                    chapter,
                    item.get("transition_in")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                ));
            }
            if !blocks.is_empty() {
                if !blocks.iter().any(|block| block.get("type") == Some(&Value::String("section".into()))) {
                    blocks.insert(
                        0,
                        make_block(
                            "section",
                            json!({"role": "core", "target_words": 1700}),
                            chapter,
                            None,
                        ),
                    );
                }
                return blocks;
            }
        }
    }
    static_plan(book, chapter)
}

fn is_plannable_block(kind: &str) -> bool {
    matches!(
        kind,
        "section"
            | "text"
            | "callout"
            | "quiz"
            | "figure"
            | "interactive"
            | "animation"
            | "code"
            | "timeline"
            | "flash_cards"
            | "deep_dive"
    )
}

fn chapter_by_id<'a>(spine: &'a Value, chapter_id: &str) -> Option<&'a Value> {
    spine
        .get("chapters")
        .and_then(Value::as_array)?
        .iter()
        .find(|chapter| chapter.get("id").and_then(Value::as_str) == Some(chapter_id))
}

fn find_block_index(page: &Value, block_id: &str) -> Option<usize> {
    page.get("blocks")
        .and_then(Value::as_array)?
        .iter()
        .position(|block| block.get("id").and_then(Value::as_str) == Some(block_id))
}

fn overview_page(book: &Value, spine: &Value, overview_chapter: &Value) -> Value {
    let timestamp = now();
    let language = book.get("language").and_then(Value::as_str).unwrap_or("en");
    let chinese = language_is_chinese(language);
    let title = book.get("title").and_then(Value::as_str).unwrap_or("");
    let description = book
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("");
    let chapters = spine
        .get("chapters")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|chapter| chapter.get("content_type").and_then(Value::as_str) != Some("overview"))
        .map(|chapter| {
            json!({
                "id": chapter.get("id").cloned().unwrap_or(Value::Null),
                "title": chapter.get("title").cloned().unwrap_or(Value::Null),
                "summary": chapter.get("summary").cloned().unwrap_or(Value::Null),
                "objectives": chapter.get("learning_objectives").cloned().unwrap_or_else(|| json!([])),
                "order": chapter.get("order").cloned().unwrap_or(Value::from(0)),
                "content_type": chapter.get("content_type").cloned().unwrap_or(Value::Null),
                "page_id": chapter.get("page_ids").and_then(Value::as_array).and_then(|ids| ids.first()).cloned().unwrap_or(Value::String(String::new()))
            })
        })
        .collect::<Vec<_>>();
    let graph = spine
        .get("concept_graph")
        .cloned()
        .unwrap_or_else(|| json!({"nodes": [], "edges": []}));
    let node_to_chapter = graph
        .get("nodes")
        .and_then(Value::as_array)
        .map(|nodes| {
            nodes
                .iter()
                .filter_map(|node| {
                    Some((
                        node.get("id").and_then(Value::as_str)?.to_string(),
                        Value::String(
                            node.get("chapter_id")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                        ),
                    ))
                })
                .collect::<Map<String, Value>>()
        })
        .unwrap_or_default();
    let intro = if chinese {
        format!(
            "# {title}\n\n{description}\n\n本书包含 {} 个概念、{} 个章节。建议先阅读概览，再按章节顺序学习，并在每章完成练习。",
            graph.get("nodes").and_then(Value::as_array).map(Vec::len).unwrap_or(0),
            chapters.len()
        )
    } else {
        format!(
            "# {title}\n\n{description}\n\nThis book contains {} concepts across {} chapters. Start with the overview, then read in order and complete each chapter's practice.",
            graph.get("nodes").and_then(Value::as_array).map(Vec::len).unwrap_or(0),
            chapters.len()
        )
    };
    let text_block = json!({
        "id": new_id("blk"),
        "type": "text",
        "status": "ready",
        "title": if chinese { "阅读指南" } else { "Reading guide" },
        "params": {"role": "overview_intro"},
        "payload": {"format": "markdown", "content": intro, "body": intro},
        "source_anchors": [],
        "metadata": {"generated_by": "overview"},
        "error": "",
        "created_at": timestamp,
        "updated_at": timestamp
    });
    let graph_block = json!({
        "id": new_id("blk"),
        "type": "concept_graph",
        "status": "ready",
        "title": if chinese { "章节地图" } else { "Chapter map" },
        "params": {},
        "payload": {
            "graph": graph,
            "index": {
                "chapters": chapters,
                "node_to_chapter": node_to_chapter
            }
        },
        "source_anchors": [],
        "metadata": {"generated_by": "overview"},
        "error": "",
        "created_at": timestamp,
        "updated_at": timestamp
    });
    json!({
        "id": new_id("pg"),
        "book_id": book.get("id").cloned().unwrap_or(Value::Null),
        "chapter_id": overview_chapter.get("id").cloned().unwrap_or(Value::Null),
        "title": overview_chapter.get("title").cloned().unwrap_or_else(|| Value::String(if chinese { "全书概览".to_string() } else { "Book Overview".to_string() })),
        "learning_objectives": overview_chapter.get("learning_objectives").cloned().unwrap_or_else(|| json!([])),
        "content_type": "overview",
        "status": "ready",
        "order": 0,
        "blocks": [text_block, graph_block],
        "links": [],
        "parent_page_id": "",
        "error": "",
        "created_at": timestamp,
        "updated_at": timestamp
    })
}

async fn compile_page_now(
    state: &AppState,
    book_id: &str,
    page_id: &str,
    force: bool,
    supplied_cancel: Option<CancellationToken>,
) -> Result<Value, BookActionError> {
    let (cancel, owned_job) = match supplied_cancel {
        Some(token) => (token, false),
        None => (register_job(book_id), true),
    };
    let result = compile_page_inner(state, book_id, page_id, force, &cancel).await;
    if owned_job {
        unregister_job(book_id);
    }
    result
}

async fn compile_page_inner(
    state: &AppState,
    book_id: &str,
    page_id: &str,
    force: bool,
    cancel: &CancellationToken,
) -> Result<Value, BookActionError> {
    let book = load_book(state, book_id)
        .map_err(BookActionError::internal)?
        .ok_or_else(|| BookActionError::not_found(format!("书籍不存在：{book_id}")))?;
    if book.get("status").and_then(Value::as_str) == Some("paused") {
        return Err(BookActionError {
            status: 409,
            code: "book_paused",
            message: "书籍生成已暂停，请先显式恢复。".to_string(),
            current_revision: book.get("revision").and_then(Value::as_u64),
        });
    }
    let spine = load_spine(state, book_id)
        .map_err(BookActionError::internal)?
        .ok_or_else(|| BookActionError::bad_request("书籍还没有确认章节骨架"))?;
    let mut page = load_page(state, book_id, page_id)
        .map_err(BookActionError::internal)?
        .ok_or_else(|| BookActionError::not_found(format!("页面不存在：{page_id}")))?;
    if page.get("status").and_then(Value::as_str) == Some("ready") && !force {
        return Ok(page);
    }
    let chapter_id = page.get("chapter_id").and_then(Value::as_str).unwrap_or("");
    let chapter = chapter_by_id(&spine, chapter_id)
        .cloned()
        .ok_or_else(|| BookActionError::not_found(format!("章节不存在：{chapter_id}")))?;
    if page.get("content_type").and_then(Value::as_str) == Some("overview") {
        let language = book.get("language").and_then(Value::as_str).unwrap_or("en");
        let mut refreshed = overview_page(&book, &spine, &chapter);
        if let Some(existing_id) = page.get("id").and_then(Value::as_str) {
            refreshed["id"] = Value::String(existing_id.to_string());
        }
        refreshed["created_at"] = page
            .get("created_at")
            .cloned()
            .unwrap_or_else(|| Value::from(now()));
        save_page(state, book_id, &mut refreshed).map_err(BookActionError::internal)?;
        emit(
            state,
            book_id,
            "overview_ready",
            "overview",
            page_id,
            "",
            json!({"status": "ready"}),
        );
        finalize_book_if_complete(state, book_id).await?;
        return Ok(refreshed);
    }
    if force {
        reset_page_for_force_compile(&mut page);
    }
    emit(
        state,
        book_id,
        "page_compile_started",
        "compilation",
        page_id,
        "",
        json!({"title": page.get("title").cloned().unwrap_or(Value::Null)}),
    );
    if page
        .get("blocks")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty)
    {
        page["status"] = Value::String("planning".to_string());
        save_page(state, book_id, &mut page).map_err(BookActionError::internal)?;
        emit(
            state,
            book_id,
            "page_planning",
            "page_plan",
            page_id,
            "",
            json!({"title": page.get("title").cloned().unwrap_or(Value::Null)}),
        );
        let language = book.get("language").and_then(Value::as_str).unwrap_or("en");
        let blocks = plan_blocks(state, &book, &chapter, language, cancel).await;
        page["blocks"] = Value::Array(blocks);
        save_page(state, book_id, &mut page).map_err(BookActionError::internal)?;
        let block_types = page
            .get("blocks")
            .and_then(Value::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|block| block.get("type").and_then(Value::as_str))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        emit(
            state,
            book_id,
            "page_planned",
            "page_plan",
            page_id,
            "",
            json!({"block_types": block_types}),
        );
    }
    page["status"] = Value::String("generating".to_string());
    save_page(state, book_id, &mut page).map_err(BookActionError::internal)?;
    let language = book
        .get("language")
        .and_then(Value::as_str)
        .unwrap_or("en")
        .to_string();
    let block_count = page
        .get("blocks")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    for index in 0..block_count {
        if cancel.is_cancelled() {
            return Err(BookActionError {
                status: 409,
                code: "cancelled",
                message: "书籍生成已取消".to_string(),
                current_revision: book.get("revision").and_then(Value::as_u64),
            });
        }
        let block_snapshot = page
            .pointer(&format!("/blocks/{index}"))
            .cloned()
            .ok_or_else(|| BookActionError::internal("页面内容块索引无效"))?;
        if block_snapshot.get("status").and_then(Value::as_str) == Some("ready") {
            continue;
        }
        let block_id = block_snapshot
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let block_type = block_snapshot
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        emit(
            state,
            book_id,
            "block_started",
            "block",
            page_id,
            &block_id,
            json!({"block_type": block_type}),
        );
        page["blocks"][index]["status"] = Value::String("generating".to_string());
        save_page(state, book_id, &mut page).map_err(BookActionError::internal)?;
        let generated = generate_block(
            state,
            &book,
            &spine,
            &chapter,
            &page,
            &block_snapshot,
            &language,
            cancel,
        )
        .await;
        match generated {
            Ok(mut block) => {
                let previous = if index > 0 {
                    page.pointer(&format!("/blocks/{}", index - 1)).cloned()
                } else {
                    None
                };
                if block.get("status").and_then(Value::as_str) == Some("ready") {
                    attach_bridge_text(
                        state,
                        &book,
                        &chapter,
                        previous.as_ref(),
                        &mut block,
                        cancel,
                    )
                    .await;
                }
                page["blocks"][index] = block;
            }
            Err(error) => {
                let failure = classify_failure(&error);
                page["blocks"][index]["status"] = Value::String("error".to_string());
                page["blocks"][index]["error"] = Value::String(error.clone());
                page["blocks"][index]["metadata"]["failure"] = failure;
            }
        }
        page["blocks"][index]["updated_at"] = Value::from(now());
        save_page(state, book_id, &mut page).map_err(BookActionError::internal)?;
        let block = &page["blocks"][index];
        let ready = block.get("status").and_then(Value::as_str) == Some("ready");
        emit(
            state,
            book_id,
            if ready { "block_ready" } else { "block_error" },
            "block",
            page_id,
            &block_id,
            json!({
                "block_type": block.get("type").cloned().unwrap_or(Value::Null),
                "status": block.get("status").cloned().unwrap_or(Value::Null),
                "error": block.get("error").cloned().unwrap_or(Value::Null),
                "failure": block.pointer("/metadata/failure").cloned().unwrap_or(Value::Null),
                "payload_keys": block.get("payload").and_then(Value::as_object).map(|payload| payload.keys().cloned().collect::<Vec<_>>()).unwrap_or_default()
            }),
        );
    }
    finalize_page_status(&mut page);
    page["updated_at"] = Value::from(now());
    save_page(state, book_id, &mut page).map_err(BookActionError::internal)?;
    emit(
        state,
        book_id,
        "page_compiled",
        "compilation",
        page_id,
        "",
        json!({
            "status": page.get("status").cloned().unwrap_or(Value::Null),
            "blocks": page.get("blocks").and_then(Value::as_array).map(|blocks| blocks.iter().map(|block| json!({
                "id": block.get("id").cloned().unwrap_or(Value::Null),
                "type": block.get("type").cloned().unwrap_or(Value::Null),
                "status": block.get("status").cloned().unwrap_or(Value::Null)
            })).collect::<Vec<_>>()).unwrap_or_default()
        }),
    );
    finalize_book_if_complete(state, book_id).await?;
    Ok(page)
}

fn reset_page_for_force_compile(page: &mut Value) {
    if let Some(blocks) = page.get_mut("blocks").and_then(Value::as_array_mut) {
        for block in blocks {
            if block.get("type").and_then(Value::as_str) == Some("user_note") {
                continue;
            }
            if block
                .pointer("/metadata/edited_by_user")
                .and_then(Value::as_bool)
                == Some(true)
            {
                continue;
            }
            block["status"] = Value::String("pending".to_string());
            block["payload"] = json!({});
            block["source_anchors"] = json!([]);
            block["error"] = Value::String(String::new());
            if let Some(metadata) = block.get_mut("metadata").and_then(Value::as_object_mut) {
                metadata.remove("failure");
                metadata.remove("retry_attempts");
            }
        }
    }
    page["status"] = Value::String("pending".to_string());
    page["error"] = Value::String(String::new());
}

fn finalize_page_status(page: &mut Value) {
    let blocks = page
        .get("blocks")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if blocks.is_empty() {
        page["status"] = Value::String("error".to_string());
        page["error"] = Value::String("页面没有规划任何内容块".to_string());
        return;
    }
    let ready = blocks
        .iter()
        .filter(|block| block.get("status").and_then(Value::as_str) == Some("ready"))
        .count();
    let errors = blocks
        .iter()
        .filter(|block| block.get("status").and_then(Value::as_str) == Some("error"))
        .count();
    if ready == blocks.len() {
        page["status"] = Value::String("ready".to_string());
        page["error"] = Value::String(String::new());
    } else if ready == 0 {
        page["status"] = Value::String("error".to_string());
        page["error"] = Value::String(format!("全部 {errors} 个内容块生成失败"));
    } else {
        page["status"] = Value::String("partial".to_string());
        page["error"] = Value::String(format!("{errors}/{} 个内容块生成失败", blocks.len()));
    }
}

fn classify_failure(message: &str) -> Value {
    let lower = message.to_ascii_lowercase();
    let (kind, retryable) = if lower.contains("json") || lower.contains("解析") {
        ("json_parse", true)
    } else if lower.contains("429") || lower.contains("rate limit") || lower.contains("限流") {
        ("rate_limit", true)
    } else if lower.contains("timeout") || lower.contains("超时") {
        ("timeout", true)
    } else if lower.contains("没有返回") || lower.contains("empty") {
        ("empty_response", true)
    } else if lower.contains("api")
        || lower.contains("provider")
        || lower.contains("模型")
        || lower.contains("服务")
    {
        ("provider_error", true)
    } else {
        ("generator_error", true)
    };
    json!({
        "kind": kind,
        "message": message,
        "retryable": retryable,
        "source": "rust_book_engine"
    })
}

async fn generate_block(
    state: &AppState,
    book: &Value,
    spine: &Value,
    chapter: &Value,
    page: &Value,
    block: &Value,
    language: &str,
    cancel: &CancellationToken,
) -> Result<Value, String> {
    let block_type = block
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let started = now();
    let mut result = block.clone();
    let chapter_title = chapter.get("title").and_then(Value::as_str).unwrap_or("");
    let chapter_summary = chapter.get("summary").and_then(Value::as_str).unwrap_or("");
    let objectives = chapter_strings(chapter, "learning_objectives");
    let knowledge_bases = book
        .get("knowledge_bases")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let focus = block
        .pointer("/params/focus")
        .and_then(Value::as_str)
        .unwrap_or(chapter_title);
    let context = source_context(
        state,
        &knowledge_bases,
        &format!("{chapter_title} {focus}"),
        8,
    );
    let chinese = language_is_chinese(language);
    let none = if chinese { "（无）" } else { "(none)" };
    let base_context = if chinese {
        format!(
            "书籍：{}\n章节：{chapter_title}\n章节摘要：{}\n学习目标：\n- {}\n资料来源：\n{}",
            book.get("title").and_then(Value::as_str).unwrap_or(""),
            if chapter_summary.is_empty() {
                none
            } else {
                chapter_summary
            },
            if objectives.is_empty() {
                none.to_string()
            } else {
                objectives.join("\n- ")
            },
            if context.is_empty() { none } else { &context }
        )
    } else {
        format!(
            "Book: {}\nChapter: {chapter_title}\nChapter summary: {}\nObjectives:\n- {}\nSource material:\n{}",
            book.get("title").and_then(Value::as_str).unwrap_or(""),
            if chapter_summary.is_empty() {
                none
            } else {
                chapter_summary
            },
            if objectives.is_empty() {
                none.to_string()
            } else {
                objectives.join("\n- ")
            },
            if context.is_empty() { none } else { &context }
        )
    };
    let payload = match block_type.as_str() {
        "section" => {
            let role = block
                .pointer("/params/role")
                .and_then(Value::as_str)
                .unwrap_or("core");
            let target_words = block
                .pointer("/params/target_words")
                .and_then(Value::as_u64)
                .unwrap_or(1400);
            let system = if chinese {
                "你是 DeepTutor 的长章节作者。内容必须准确、循序推进，并充分利用给定资料；无法从资料确认的事实要明确区分。"
            } else {
                "You are DeepTutor's long-form section author. Be accurate, progressive, and grounded in the supplied material."
            };
            let prompt = if chinese {
                format!(
                    "{base_context}\n\n主题：{focus}\n章节作用：{role}\n目标篇幅：约 {target_words} 字。\n\n输出 fields：intro（导语）、subsections（2 到 5 个对象，每个含 heading、role、focus、body、target_words）、key_takeaway（核心要点）。body 使用 Markdown，可含公式与例子。"
                )
            } else {
                format!(
                    "{base_context}\n\nFocus: {focus}\nSection role: {role}\nTarget length: about {target_words} words.\n\nReturn intro, subsections (2-5 objects with heading, role, focus, body, target_words), and key_takeaway. Body must be Markdown and may include formulas and examples."
                )
            };
            let value = llm_json(state, system, &prompt, cancel).await?;
            let mut subsections = value
                .get("subsections")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .take(6)
                .filter_map(|mut sub| {
                    let body = sub.get("body").and_then(Value::as_str)?.trim().to_string();
                    if body.is_empty() {
                        return None;
                    }
                    let heading = sub
                        .get("heading")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if !heading.is_empty() && !body.starts_with('#') {
                        sub["body"] = Value::String(format!("### {heading}\n\n{body}"));
                    }
                    Some(sub)
                })
                .collect::<Vec<_>>();
            if subsections.is_empty() {
                let fallback = llm_text(
                    state,
                    system,
                    &format!("{base_context}\n\n请围绕“{focus}”写一节完整、连贯的教学正文，使用 Markdown 小标题组织。"),
                    cancel,
                )
                .await?;
                subsections = vec![json!({
                    "heading": focus,
                    "role": role,
                    "focus": focus,
                    "body": fallback,
                    "target_words": target_words
                })];
            }
            json!({
                "format": "section",
                "intro": value.get("intro").cloned().unwrap_or(Value::String(String::new())),
                "subsections": subsections,
                "key_takeaway": value.get("key_takeaway").cloned().unwrap_or(Value::String(String::new())),
                "focus": focus,
                "role": role
            })
        }
        "text" => {
            let body = llm_text(
                state,
                if chinese {
                    "你是 DeepTutor 的教学作者。只输出可直接阅读的 Markdown 正文。"
                } else {
                    "You are DeepTutor's instructional writer. Return only readable Markdown prose."
                },
                &format!("{base_context}\n\n请围绕“{focus}”写一段清晰、有例子、有结论的教学正文。"),
                cancel,
            )
            .await?;
            json!({"format": "markdown", "body": body, "role": "explanation"})
        }
        "callout" => {
            let variant = block
                .pointer("/params/variant")
                .and_then(Value::as_str)
                .unwrap_or("key_idea");
            let label = match (variant, chinese) {
                ("common_pitfall", true) => "常见误区",
                ("summary", true) => "小结",
                ("tip", true) => "小提示",
                ("common_pitfall", false) => "Watch Out",
                ("summary", false) => "Summary",
                ("tip", false) => "Tip",
                (_, true) => "核心要点",
                (_, false) => "Key Idea",
            };
            let body = llm_text(
                state,
                if chinese {
                    "你是 DeepTutor 的教学编辑。只输出简洁、准确、可独立理解的提示正文，使用 Markdown。"
                } else {
                    "You are DeepTutor's instructional editor. Return concise, accurate Markdown."
                },
                &format!("{base_context}\n\n请为“{focus}”生成一个 {label}。不要复述整章。"),
                cancel,
            )
            .await?;
            json!({"variant": variant, "label": label, "body": body})
        }
        "quiz" => {
            let count = block
                .pointer("/params/num_questions")
                .and_then(Value::as_u64)
                .unwrap_or(3)
                .clamp(1, 8);
            let difficulty = block
                .pointer("/params/difficulty")
                .and_then(Value::as_str)
                .unwrap_or("medium");
            let value = llm_json(
                state,
                if chinese {
                    "你是 DeepTutor 的出题专家。题目必须可由章节内容作答，选择题恰好提供 A-D 四个选项，并给出正确键与解释。"
                } else {
                    "You are DeepTutor's assessment author. Questions must be answerable from the chapter; choice questions need exactly A-D options, a correct key, and an explanation."
                },
                &format!(
                    "{base_context}\n\n围绕“{focus}”生成 {count} 道 {difficulty} 难度题。输出 questions 数组；每题含 question_id、question、question_type、options、correct_answer、explanation、difficulty。"
                ),
                cancel,
            )
            .await?;
            let questions = value
                .get("questions")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .take(count as usize)
                .filter_map(normalize_question)
                .collect::<Vec<_>>();
            if questions.is_empty() {
                return Err("出题模型没有返回有效题目".to_string());
            }
            json!({"questions": questions, "topic": focus})
        }
        "figure" => {
            let variant = block
                .pointer("/params/variant")
                .and_then(Value::as_str)
                .unwrap_or("diagram");
            let value = llm_json(
                state,
                if chinese {
                    "你是 DeepTutor 的信息图设计器。选择 svg、mermaid 或 chartjs，生成自包含、可直接渲染的代码。SVG 必须有 xmlns；Chart.js 输出可解析的配置 JSON；Mermaid 输出合法 DSL。"
                } else {
                    "You are DeepTutor's information designer. Choose svg, mermaid, or chartjs and return self-contained renderable code. SVG needs xmlns; Chart.js must be parsable config JSON; Mermaid must be valid DSL."
                },
                &format!(
                    "{base_context}\n\n为“{focus}”设计一个 {variant} 图。输出 render_type、description、chart_type、language、code。code 是字符串。不要依赖外部图片。"
                ),
                cancel,
            )
            .await?;
            normalize_visual_payload(value)?
        }
        "interactive" => {
            let value = llm_json(
                state,
                if chinese {
                    "你是 DeepTutor 的交互课件工程师。生成一个完整、自包含、无外部依赖的单文件 HTML，从 <!DOCTYPE html> 到 </html>，包含清晰的交互和移动端适配。"
                } else {
                    "You are DeepTutor's interactive lesson engineer. Return one complete self-contained HTML document from <!DOCTYPE html> through </html>, with clear interaction and responsive layout."
                },
                &format!(
                    "{base_context}\n\n为“{focus}”设计交互练习。输出 description、chart_type、html。HTML 不得引用 Node、Python、本地服务器或外部脚本文件。"
                ),
                cancel,
            )
            .await?;
            let html = value
                .get("html")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            validate_html_document(&html)?;
            json!({
                "render_type": "html",
                "code": {"language": "html", "content": html},
                "description": value.get("description").cloned().unwrap_or(Value::String(String::new())),
                "chart_type": value.get("chart_type").cloned().unwrap_or(Value::String("interactive".to_string()))
            })
        }
        "animation" => {
            let value = llm_json(
                state,
                if chinese {
                    "你是 DeepTutor 的数学动画设计师。生成一个完整、自包含的 SVG 动画，使用 CSS 或 SMIL；必须含 xmlns、可访问标题和完整闭合标签，不得依赖外部文件。"
                } else {
                    "You are DeepTutor's math animation designer. Return a complete self-contained SVG animation using CSS or SMIL, with xmlns, accessible title, closed tags, and no external assets."
                },
                &format!(
                    "{base_context}\n\n把“{focus}”制作成分步动画。输出 svg、summary、description、key_points。"
                ),
                cancel,
            )
            .await?;
            let svg = value
                .get("svg")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            validate_svg_document(&svg)?;
            let book_id = book.get("id").and_then(Value::as_str).unwrap_or("book");
            let block_id = block.get("id").and_then(Value::as_str).unwrap_or("block");
            let relative = format!("books/{book_id}/{block_id}.svg");
            let path = state.paths.outputs.join(&relative);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|error| format!("无法创建动画输出目录：{error}"))?;
            }
            std::fs::write(&path, svg.as_bytes())
                .map_err(|error| format!("无法保存动画 SVG：{error}"))?;
            let url = format!("/files/outputs/{relative}");
            json!({
                "render_type": "svg",
                "artifacts": [{
                    "type": "image",
                    "url": url,
                    "filename": format!("{block_id}.svg"),
                    "content_type": "image/svg+xml",
                    "label": value.get("summary").cloned().unwrap_or(Value::String(String::new()))
                }],
                "video_url": url,
                "filename": format!("{block_id}.svg"),
                "summary": value.get("summary").cloned().unwrap_or(Value::String(String::new())),
                "key_points": value.get("key_points").cloned().unwrap_or_else(|| json!([])),
                "description": value.get("description").cloned().unwrap_or(Value::String(String::new()))
            })
        }
        "code" => {
            let language_name = block
                .pointer("/params/language")
                .and_then(Value::as_str)
                .unwrap_or("python");
            let intent = block
                .pointer("/params/intent")
                .and_then(Value::as_str)
                .unwrap_or("example");
            let value = llm_json(
                state,
                if chinese {
                    "你是 DeepTutor 的编程讲师。代码必须完整、可运行、与本章一致，并解释关键步骤。"
                } else {
                    "You are DeepTutor's programming instructor. Return complete runnable code aligned with the chapter and explain the key steps."
                },
                &format!(
                    "{base_context}\n\n为“{focus}”编写一个 {language_name} 示例，用途：{intent}。输出 language、code、explanation。"
                ),
                cancel,
            )
            .await?;
            let code = value
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            if code.is_empty() {
                return Err("代码模型没有返回内容".to_string());
            }
            json!({
                "language": value.get("language").and_then(Value::as_str).unwrap_or(language_name),
                "code": code,
                "explanation": value.get("explanation").cloned().unwrap_or(Value::String(String::new())),
                "intent": intent
            })
        }
        "timeline" => {
            let value = llm_json(
                state,
                if chinese {
                    "你是 DeepTutor 的史实编辑。只输出有依据、按时间排序的事件；不确定的日期明确写“约”。"
                } else {
                    "You are DeepTutor's chronology editor. Return dated events in chronological order and mark uncertain dates with 'c.'."
                },
                &format!(
                    "{base_context}\n\n为“{focus}”输出 events 数组，每项含 date、title、description，最多 8 项。"
                ),
                cancel,
            )
            .await?;
            let events = value
                .get("events")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .take(8)
                .filter_map(|mut event| {
                    let title = event
                        .get("title")
                        .and_then(Value::as_str)?
                        .trim()
                        .to_string();
                    if title.is_empty() {
                        return None;
                    }
                    if !event.get("date").is_some_and(Value::is_string) {
                        event["date"] = Value::String(String::new());
                    }
                    if !event.get("description").is_some_and(Value::is_string) {
                        event["description"] = Value::String(String::new());
                    }
                    Some(event)
                })
                .collect::<Vec<_>>();
            if events.is_empty() {
                return Err("时间线模型没有返回有效事件".to_string());
            }
            json!({"events": events})
        }
        "flash_cards" => {
            let count = block
                .pointer("/params/count")
                .and_then(Value::as_u64)
                .unwrap_or(5)
                .clamp(3, 8);
            let value = llm_json(
                state,
                if chinese {
                    "你是 DeepTutor 的记忆卡设计器。每张卡只考查一个概念，问题与答案必须准确、简洁。"
                } else {
                    "You are DeepTutor's flashcard designer. Each card tests one concept with an accurate, concise prompt and answer."
                },
                &format!(
                    "{base_context}\n\n为“{focus}”生成 {count} 张记忆卡。输出 cards 数组，每项含 front、back、hint。"
                ),
                cancel,
            )
            .await?;
            let cards = value
                .get("cards")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .take(count as usize)
                .filter_map(|card| {
                    let front = card.get("front").and_then(Value::as_str)?.trim();
                    let back = card.get("back").and_then(Value::as_str)?.trim();
                    if front.is_empty() || back.is_empty() {
                        return None;
                    }
                    Some(json!({
                        "front": front,
                        "back": back,
                        "hint": card.get("hint").and_then(Value::as_str).unwrap_or("")
                    }))
                })
                .collect::<Vec<_>>();
            if cards.is_empty() {
                return Err("记忆卡模型没有返回有效卡片".to_string());
            }
            json!({"cards": cards})
        }
        "deep_dive" => {
            let value = llm_json(
                state,
                if chinese {
                    "你是 DeepTutor 的拓展阅读策划。提出与本章直接相关、值得单独成章的深入主题。"
                } else {
                    "You are DeepTutor's further-reading planner. Suggest tightly related topics worth a dedicated chapter."
                },
                &format!(
                    "{base_context}\n\n基于“{focus}”输出 suggestions 数组，每项含 topic、rationale，2 到 5 项。"
                ),
                cancel,
            )
            .await?;
            let suggestions = value
                .get("suggestions")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .take(5)
                .filter(|item| {
                    item.get("topic")
                        .and_then(Value::as_str)
                        .is_some_and(|topic| !topic.trim().is_empty())
                })
                .collect::<Vec<_>>();
            json!({"suggestions": suggestions})
        }
        "concept_graph" => {
            let graph = spine
                .get("concept_graph")
                .cloned()
                .unwrap_or_else(|| json!({"nodes": [], "edges": []}));
            let chapters = spine
                .get("chapters")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            json!({
                "graph": graph,
                "index": {
                    "chapters": chapters,
                    "node_to_chapter": {}
                }
            })
        }
        "user_note" => json!({"body": ""}),
        other => return Err(format!("暂不支持的内容块类型：{other}")),
    };
    result["payload"] = payload;
    result["status"] = Value::String("ready".to_string());
    result["error"] = Value::String(String::new());
    result["updated_at"] = Value::from(now());
    result["metadata"]["generation_ms"] = Value::from(((now() - started) * 1000.0) as u64);
    result["metadata"]["generated_by"] = Value::String("rust_book_engine".to_string());
    result["metadata"]
        .as_object_mut()
        .map(|metadata| metadata.remove("failure"));
    if page.get("id").is_some() {
        result["metadata"]["page_id"] = page.get("id").cloned().unwrap_or(Value::Null);
    }
    Ok(result)
}

fn normalize_question(mut question: Value) -> Option<Value> {
    let prompt = question
        .get("question")
        .or_else(|| question.get("prompt"))
        .and_then(Value::as_str)?
        .trim()
        .to_string();
    if prompt.is_empty() {
        return None;
    }
    if !question.get("question_id").is_some_and(Value::is_string) {
        question["question_id"] = Value::String(new_id("q"));
    }
    if !question.get("question_type").is_some_and(Value::is_string) {
        question["question_type"] = Value::String("short_answer".to_string());
    }
    if !question.get("options").is_some_and(Value::is_object) {
        question["options"] = json!({});
    }
    if !question.get("correct_answer").is_some_and(Value::is_string) {
        question["correct_answer"] = Value::String(String::new());
    }
    if !question.get("explanation").is_some_and(Value::is_string) {
        question["explanation"] = Value::String(String::new());
    }
    if !question.get("difficulty").is_some_and(Value::is_string) {
        question["difficulty"] = Value::String("medium".to_string());
    }
    question["question"] = Value::String(prompt);
    Some(question)
}

fn normalize_visual_payload(value: Value) -> Result<Value, String> {
    let render_type = value
        .get("render_type")
        .and_then(Value::as_str)
        .unwrap_or("svg")
        .trim()
        .to_ascii_lowercase();
    if !matches!(render_type.as_str(), "svg" | "mermaid" | "chartjs") {
        return Err(format!("不支持的静态图类型：{render_type}"));
    }
    let code = value
        .get("code")
        .and_then(Value::as_str)
        .or_else(|| value.pointer("/code/content").and_then(Value::as_str))
        .unwrap_or("")
        .trim()
        .to_string();
    if code.is_empty() {
        return Err("图示模型没有返回代码".to_string());
    }
    match render_type.as_str() {
        "svg" => validate_svg_document(&code)?,
        "mermaid" => {
            if !code.lines().any(|line| {
                let line = line.trim_start();
                line.starts_with("graph ")
                    || line.starts_with("flowchart ")
                    || line.starts_with("mindmap")
                    || line.starts_with("timeline")
                    || line.starts_with("sequenceDiagram")
                    || line.starts_with("classDiagram")
            }) {
                return Err("Mermaid 代码缺少有效图表声明".to_string());
            }
        }
        "chartjs" => {
            let parsed = serde_json::from_str::<Value>(&code)
                .map_err(|error| format!("Chart.js 配置不是合法 JSON：{error}"))?;
            if !parsed.is_object() {
                return Err("Chart.js 配置必须是 JSON 对象".to_string());
            }
        }
        _ => {}
    }
    let language =
        value
            .get("language")
            .and_then(Value::as_str)
            .unwrap_or(match render_type.as_str() {
                "svg" => "svg",
                "mermaid" => "mermaid",
                _ => "javascript",
            });
    Ok(json!({
        "render_type": render_type,
        "code": {"language": language, "content": code},
        "description": value.get("description").cloned().unwrap_or(Value::String(String::new())),
        "chart_type": value.get("chart_type").cloned().unwrap_or(Value::String(String::new()))
    }))
}

fn validate_svg_document(svg: &str) -> Result<(), String> {
    let lower = svg.to_ascii_lowercase();
    if !lower.contains("<svg") || !lower.contains("</svg>") {
        return Err("SVG 文档缺少 <svg> 根元素或未闭合".to_string());
    }
    if !lower.contains("xmlns=") {
        return Err("SVG 文档缺少 xmlns 属性".to_string());
    }
    if lower.contains("<script") && lower.contains("src=") {
        return Err("SVG 动画不得加载外部脚本".to_string());
    }
    Ok(())
}

fn validate_html_document(html: &str) -> Result<(), String> {
    let lower = html.to_ascii_lowercase();
    if !lower.starts_with("<!doctype html>") && !lower.starts_with("<html") {
        return Err("交互内容不是完整 HTML 文档".to_string());
    }
    if !lower.contains("</html>") {
        return Err("交互 HTML 文档未闭合".to_string());
    }
    if lower.contains("node_modules") || lower.contains("python ") || lower.contains("localhost:") {
        return Err("交互内容包含不允许的运行时依赖".to_string());
    }
    Ok(())
}

async fn attach_bridge_text(
    state: &AppState,
    book: &Value,
    chapter: &Value,
    previous: Option<&Value>,
    block: &mut Value,
    cancel: &CancellationToken,
) {
    let Some(previous) = previous else {
        return;
    };
    let hint = block
        .pointer("/metadata/transition_in")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if hint.is_empty() {
        return;
    }
    let language = book.get("language").and_then(Value::as_str).unwrap_or("en");
    let previous_kind = previous
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("content");
    let prompt = format!(
        "章节：{}\n上一内容块类型：{previous_kind}\n下一内容块提示：{hint}\n\n只写一到两句自然的过渡文字，使用 Markdown，不要标题。",
        chapter.get("title").and_then(Value::as_str).unwrap_or("")
    );
    if let Ok(text) = llm_text(
        state,
        if language_is_chinese(language) {
            "你是 DeepTutor 的教学编辑。只输出连贯的过渡段落。"
        } else {
            "You are DeepTutor's instructional editor. Return only a short transitional passage."
        },
        &prompt,
        cancel,
    )
    .await
    {
        let text = text.trim();
        if !text.is_empty() {
            block["payload"]["bridge_text"] = Value::String(text.to_string());
        }
    }
}

async fn regenerate_block_now(
    state: &AppState,
    book_id: &str,
    page_id: &str,
    block_id: &str,
    params_override: Option<Value>,
    supplied_cancel: Option<CancellationToken>,
) -> Result<Value, BookActionError> {
    let (cancel, owned_job) = match supplied_cancel {
        Some(token) => (token, false),
        None => (register_job(book_id), true),
    };
    let result =
        regenerate_block_inner(state, book_id, page_id, block_id, params_override, &cancel).await;
    if owned_job {
        unregister_job(book_id);
    }
    result
}

async fn regenerate_block_inner(
    state: &AppState,
    book_id: &str,
    page_id: &str,
    block_id: &str,
    params_override: Option<Value>,
    cancel: &CancellationToken,
) -> Result<Value, BookActionError> {
    let book = load_book(state, book_id)
        .map_err(BookActionError::internal)?
        .ok_or_else(|| BookActionError::not_found(format!("书籍不存在：{book_id}")))?;
    if book.get("status").and_then(Value::as_str) == Some("paused") {
        return Err(BookActionError {
            status: 409,
            code: "book_paused",
            message: "书籍生成已暂停，请先显式恢复。".to_string(),
            current_revision: book.get("revision").and_then(Value::as_u64),
        });
    }
    let spine = load_spine(state, book_id)
        .map_err(BookActionError::internal)?
        .ok_or_else(|| BookActionError::bad_request("书籍还没有章节骨架"))?;
    let mut page = load_page(state, book_id, page_id)
        .map_err(BookActionError::internal)?
        .ok_or_else(|| BookActionError::not_found(format!("页面不存在：{page_id}")))?;
    let index = find_block_index(&page, block_id)
        .ok_or_else(|| BookActionError::not_found(format!("内容块不存在：{block_id}")))?;
    let chapter_id = page.get("chapter_id").and_then(Value::as_str).unwrap_or("");
    let chapter = chapter_by_id(&spine, chapter_id)
        .cloned()
        .ok_or_else(|| BookActionError::not_found(format!("章节不存在：{chapter_id}")))?;
    let mut block = page
        .pointer(&format!("/blocks/{index}"))
        .cloned()
        .ok_or_else(|| BookActionError::internal("页面内容块索引无效"))?;
    if let Some(overrides) = params_override.and_then(|value| value.as_object().cloned()) {
        if !block.get("params").is_some_and(Value::is_object) {
            block["params"] = json!({});
        }
        for (key, value) in overrides {
            block["params"][key] = value;
        }
    }
    let block_type = block
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    emit(
        state,
        book_id,
        "block_started",
        "block",
        page_id,
        block_id,
        json!({"block_type": block_type}),
    );
    block["status"] = Value::String("generating".to_string());
    page["blocks"][index] = block.clone();
    save_page(state, book_id, &mut page).map_err(BookActionError::internal)?;
    let language = book.get("language").and_then(Value::as_str).unwrap_or("en");
    let mut generated = generate_block(
        state, &book, &spine, &chapter, &page, &block, language, cancel,
    )
    .await
    .map_err(|error| BookActionError::internal(error))?;
    if let Some(metadata) = generated.get_mut("metadata").and_then(Value::as_object_mut) {
        metadata.remove("edited_by_user");
    }
    let previous = if index > 0 {
        page.pointer(&format!("/blocks/{}", index - 1)).cloned()
    } else {
        None
    };
    attach_bridge_text(
        state,
        &book,
        &chapter,
        previous.as_ref(),
        &mut generated,
        cancel,
    )
    .await;
    page["blocks"][index] = generated.clone();
    finalize_page_status(&mut page);
    save_page(state, book_id, &mut page).map_err(BookActionError::internal)?;
    emit(
        state,
        book_id,
        "block_ready",
        "block",
        page_id,
        block_id,
        json!({
            "block_type": generated.get("type").cloned().unwrap_or(Value::Null),
            "status": "ready",
            "payload_keys": generated.get("payload").and_then(Value::as_object).map(|payload| payload.keys().cloned().collect::<Vec<_>>()).unwrap_or_default()
        }),
    );
    finalize_book_if_complete(state, book_id).await?;
    Ok(generated)
}

async fn finalize_book_if_complete(state: &AppState, book_id: &str) -> Result<(), BookActionError> {
    let Some(mut book) = load_book(state, book_id).map_err(BookActionError::internal)? else {
        return Ok(());
    };
    let pages = list_pages(state, book_id).map_err(BookActionError::internal)?;
    if pages.is_empty() {
        return Ok(());
    }
    let unfinished = pages.iter().any(|page| {
        matches!(
            page.get("status").and_then(Value::as_str),
            Some("pending" | "planning" | "generating")
        )
    });
    book["page_count"] = Value::from(pages.len());
    if !unfinished && book.get("status").and_then(Value::as_str) != Some("ready") {
        book["status"] = Value::String("ready".to_string());
        if let Some(metadata) = book.get_mut("metadata").and_then(Value::as_object_mut) {
            metadata.remove("pause_reason");
            metadata.remove("pause_kind");
        }
        save_book(state, &mut book).map_err(BookActionError::internal)?;
        emit(
            state,
            book_id,
            "book_ready",
            "compilation",
            "",
            "",
            json!({"page_count": pages.len()}),
        );
    } else if book.get("page_count").and_then(Value::as_u64) != Some(pages.len() as u64) {
        save_book(state, &mut book).map_err(BookActionError::internal)?;
    }
    Ok(())
}

pub async fn confirm_spine(state: &AppState, body: Value) -> Result<Value, BookActionError> {
    let book_id = required_str(&body, "book_id")?;
    let expected = body.get("expected_revision").and_then(Value::as_u64);
    let revision = claim_revision(state, &book_id, expected, "confirm_spine")?;
    let mut book = load_book(state, &book_id)
        .map_err(BookActionError::internal)?
        .ok_or_else(|| BookActionError::not_found(format!("书籍不存在：{book_id}")))?;
    let language = book
        .get("language")
        .and_then(Value::as_str)
        .unwrap_or("en")
        .to_string();
    let proposal = book.get("proposal").cloned().unwrap_or_else(|| {
        fallback_proposal(
            book.get("title").and_then(Value::as_str).unwrap_or(""),
            &language,
        )
    });
    let mut spine =
        if let Some(edited) = body.get("spine").filter(|value| value.is_object()).cloned() {
            normalize_spine(&book_id, edited, &proposal, &language)
        } else {
            load_spine(state, &book_id)
                .map_err(BookActionError::internal)?
                .ok_or_else(|| BookActionError::bad_request("书籍还没有章节骨架"))?
        };
    let auto_compile = body
        .get("auto_compile")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let requested_block_types = body.get("block_types").cloned().unwrap_or(Value::Null);
    if !requested_block_types.is_null() {
        book["metadata"]["block_types"] = requested_block_types;
    }
    ensure_overview_chapter(&mut spine, &book, &language);
    let mut chapters = spine
        .get("chapters")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let existing_pages = list_pages(state, &book_id).map_err(BookActionError::internal)?;
    let mut pages = Vec::new();
    for (index, chapter) in chapters.iter_mut().enumerate() {
        let chapter_id = chapter
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let existing = existing_pages.iter().find(|page| {
            page.get("chapter_id").and_then(Value::as_str) == Some(chapter_id.as_str())
        });
        let timestamp = now();
        let mut page = if let Some(existing) = existing {
            let mut page = existing.clone();
            page["order"] = Value::from(index);
            page["title"] = chapter
                .get("title")
                .cloned()
                .unwrap_or(Value::String(String::new()));
            page["content_type"] = chapter
                .get("content_type")
                .cloned()
                .unwrap_or_else(|| Value::String("theory".to_string()));
            page["learning_objectives"] = chapter
                .get("learning_objectives")
                .cloned()
                .unwrap_or_else(|| json!([]));
            page
        } else {
            json!({
                "id": new_id("pg"),
                "book_id": book_id,
                "chapter_id": chapter_id,
                "title": chapter.get("title").cloned().unwrap_or(Value::String(String::new())),
                "learning_objectives": chapter.get("learning_objectives").cloned().unwrap_or_else(|| json!([])),
                "content_type": chapter.get("content_type").cloned().unwrap_or_else(|| Value::String("theory".to_string())),
                "status": "pending",
                "order": index,
                "blocks": [],
                "links": [],
                "parent_page_id": "",
                "error": "",
                "created_at": timestamp,
                "updated_at": timestamp
            })
        };
        chapter["page_ids"] = json!([page.get("id").cloned().unwrap_or(Value::Null)]);
        chapter["order"] = Value::from(index);
        save_page(state, &book_id, &mut page).map_err(BookActionError::internal)?;
        pages.push(page);
    }
    spine["chapters"] = Value::Array(chapters);
    save_spine(state, &book_id, &mut spine).map_err(BookActionError::internal)?;
    if let Some(overview_chapter) = spine
        .get("chapters")
        .and_then(Value::as_array)
        .and_then(|chapters| chapters.first())
        .cloned()
    {
        if overview_chapter.get("content_type").and_then(Value::as_str) == Some("overview") {
            let mut overview = pages
                .iter()
                .find(|page| page.get("chapter_id") == overview_chapter.get("id"))
                .cloned()
                .unwrap_or_else(|| {
                    let timestamp = now();
                    json!({
                        "id": new_id("pg"),
                        "book_id": book_id,
                        "chapter_id": overview_chapter.get("id").cloned().unwrap_or(Value::Null),
                        "created_at": timestamp,
                        "order": 0,
                        "links": [],
                        "parent_page_id": "",
                        "blocks": []
                    })
                });
            let fresh = overview_page(&book, &spine, &overview_chapter);
            overview["title"] = fresh.get("title").cloned().unwrap_or(Value::Null);
            overview["learning_objectives"] = fresh
                .get("learning_objectives")
                .cloned()
                .unwrap_or_else(|| json!([]));
            overview["content_type"] = Value::String("overview".to_string());
            overview["status"] = Value::String("ready".to_string());
            overview["blocks"] = fresh.get("blocks").cloned().unwrap_or_else(|| json!([]));
            overview["error"] = Value::String(String::new());
            save_page(state, &book_id, &mut overview).map_err(BookActionError::internal)?;
            if let Some(page) = pages
                .iter_mut()
                .find(|page| page.get("chapter_id") == overview_chapter.get("id"))
            {
                *page = overview.clone();
            } else {
                pages.insert(0, overview.clone());
            }
            if let Some(chapter) = spine
                .get_mut("chapters")
                .and_then(Value::as_array_mut)
                .and_then(|chapters| chapters.first_mut())
            {
                chapter["page_ids"] = json!([overview.get("id").cloned().unwrap_or(Value::Null)]);
            }
            save_spine(state, &book_id, &mut spine).map_err(BookActionError::internal)?;
            emit(
                state,
                book_id.as_str(),
                "overview_ready",
                "overview",
                overview.get("id").and_then(Value::as_str).unwrap_or(""),
                "",
                json!({"status": "ready"}),
            );
        }
    }
    pages.sort_by_key(|page| page.get("order").and_then(Value::as_i64).unwrap_or(0));
    book["page_count"] = Value::from(pages.len());
    book["status"] = Value::String("compiling".to_string());
    book["metadata"]["lazy_compile"] = Value::Bool(!auto_compile);
    book["metadata"]["compile_started_at"] = Value::from(now());
    if let Some(metadata) = book.get_mut("metadata").and_then(Value::as_object_mut) {
        metadata.remove("pause_reason");
        metadata.remove("pause_kind");
    }
    save_book(state, &mut book).map_err(BookActionError::internal)?;
    if auto_compile {
        spawn_page_queue(state.clone(), book_id.clone(), pages.clone());
    } else {
        finalize_book_if_complete(state, &book_id).await?;
    }
    Ok(json!({
        "type": "confirm_spine_result",
        "book_id": book_id,
        "pages": pages,
        "book_revision": revision
    }))
}

fn ensure_overview_chapter(spine: &mut Value, book: &Value, language: &str) {
    let chinese = language_is_chinese(language);
    if !spine.get("chapters").is_some_and(Value::is_array) {
        spine["chapters"] = json!([]);
    }
    let chapters = spine["chapters"].as_array_mut().expect("chapters");
    let existing = chapters.iter().position(|chapter| {
        chapter.get("auto_overview").and_then(Value::as_bool) == Some(true)
            || chapter.get("content_type").and_then(Value::as_str) == Some("overview")
    });
    if let Some(index) = existing {
        let chapter = chapters.remove(index);
        chapters.insert(0, chapter);
    } else {
        let title = book
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or(if chinese {
                "学习书"
            } else {
                "Learning Book"
            });
        chapters.insert(
            0,
            json!({
                "id": new_id("ch"),
                "title": if chinese { format!("{title}：全书概览") } else { format!("{title}: Overview") },
                "learning_objectives": if chinese {
                    json!(["了解全书结构", "建立章节之间的联系", "制定阅读顺序"])
                } else {
                    json!(["Understand the book structure", "Connect chapters", "Plan a reading path"])
                },
                "content_type": "overview",
                "source_anchors": [],
                "prerequisites": [],
                "page_ids": [],
                "summary": if chinese { "自动生成的全书导览与章节地图。" } else { "Generated orientation and chapter map." },
                "order": 0,
                "auto_overview": true
            }),
        );
    }
    for (index, chapter) in chapters.iter_mut().enumerate() {
        chapter["order"] = Value::from(index);
    }
}

fn spawn_page_queue(state: AppState, book_id: String, pages: Vec<Value>) {
    let token = register_job(&book_id);
    tokio::spawn(async move {
        for page in pages {
            if token.is_cancelled() {
                break;
            }
            if page.get("status").and_then(Value::as_str) == Some("ready") {
                continue;
            }
            let Some(page_id) = page.get("id").and_then(Value::as_str) else {
                continue;
            };
            let _ = compile_page_now(&state, &book_id, page_id, false, Some(token.clone())).await;
        }
        let _ = finalize_book_if_complete(&state, &book_id).await;
        unregister_job(&book_id);
    });
}

#[derive(Debug, serde::Deserialize)]
struct IncludeBlocksQuery {
    include_blocks: Option<bool>,
}

#[derive(Debug, serde::Deserialize)]
struct EstimateQuery {
    depth: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct CaptureQuery {
    status: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct RefreshFingerprintQuery {
    force: Option<bool>,
    expected_revision: Option<u64>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/books/health", get(api_health))
        .route("/books/block-types", get(api_block_types))
        .route("/books/estimate-basis", get(api_estimate_basis))
        .route("/estimate-basis", get(api_estimate_basis))
        .route("/books", get(api_list_books).post(api_create_book))
        .route(
            "/books/{book_id}",
            get(api_get_book).delete(api_delete_book),
        )
        .route("/books/{book_id}/spine", get(api_get_spine))
        .route("/books/{book_id}/pages/{page_id}", get(api_get_page))
        .route(
            "/books/{book_id}/learning-captures",
            get(api_list_captures).post(api_create_capture),
        )
        .route(
            "/books/{book_id}/learning-captures/{capture_id}",
            patch(api_update_capture),
        )
        .route("/books/confirm-proposal", post(api_confirm_proposal))
        .route("/books/confirm-spine", post(api_confirm_spine))
        .route("/books/compile-page", post(api_compile_page))
        .route("/books/regenerate-block", post(api_regenerate_block))
        .route("/books/insert-block", post(api_insert_block))
        .route("/books/update-block", post(api_update_block))
        .route("/books/delete-block", post(api_delete_block))
        .route("/books/move-block", post(api_move_block))
        .route("/books/change-block-type", post(api_change_block_type))
        .route("/books/deep-dive", post(api_deep_dive))
        .route("/books/supplement", post(api_supplement))
        .route("/books/quiz-attempt", post(api_quiz_attempt))
        .route("/books/progress/visit", post(api_mark_visited))
        .route("/books/progress/bookmark", post(api_toggle_bookmark))
        .route("/books/page-chat-session", post(api_page_chat_session))
        .route("/books/resume", post(api_resume))
        .route("/books/pause", post(api_pause))
        .route("/books/rebuild", post(api_rebuild))
        .route("/books/{book_id}/health", get(api_book_health))
        .route(
            "/books/{book_id}/refresh-fingerprints",
            post(api_refresh_fingerprints),
        )
        .route("/books/{book_id}/export", get(api_export_book))
}

fn api_error(error: BookActionError) -> ApiError {
    ApiError {
        status: StatusCode::from_u16(error.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        code: error.code,
        message: if let Some(revision) = error.current_revision {
            format!("{}（current_revision={revision}）", error.message)
        } else {
            error.message
        },
        retryable: error.status >= 500,
    }
}

fn reading_summary(state: &AppState, book: &Value) -> Value {
    let book_id = book.get("id").and_then(Value::as_str).unwrap_or("");
    let progress = load_progress(state, book_id).unwrap_or_else(|_| json!({}));
    let total = book.get("page_count").and_then(Value::as_u64).unwrap_or(0) as usize;
    let visited = progress
        .get("visited_page_ids")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .collect::<HashSet<_>>()
                .len()
        })
        .unwrap_or(0);
    let percent = if total == 0 {
        0
    } else {
        ((visited.min(total) as f64 / total as f64) * 100.0).round() as u64
    };
    json!({
        "current_page_id": progress.get("current_page_id").cloned().unwrap_or(Value::String(String::new())),
        "visited_pages": visited,
        "total_pages": total,
        "percent": percent
    })
}

fn generation_overview(state: &AppState, book: &Value) -> Value {
    let status = book
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("draft");
    let book_id = book.get("id").and_then(Value::as_str).unwrap_or("");
    let working = is_book_working(book_id);
    let interrupted = status == "compiling" && !working;
    json!({
        "status": status,
        "can_resume": matches!(status, "compiling" | "paused" | "error"),
        "pause_reason": book.pointer("/metadata/pause_reason").cloned().unwrap_or(Value::String(String::new())),
        "source_quality": book.pointer("/metadata/source_quality").cloned().unwrap_or(Value::Null),
        "working": working,
        "interrupted": interrupted,
        "started_at": book.pointer("/metadata/compile_started_at").and_then(Value::as_f64).unwrap_or(0.0)
    })
}

fn generation_summary(state: &AppState, book: &Value, pages: &[Value]) -> Value {
    let mut overview = generation_overview(state, book);
    let mut page_counts = Map::new();
    let mut failed_blocks = 0u64;
    let mut failed_pages = 0u64;
    let mut retryable_pages = 0u64;
    let mut queued_pages = 0u64;
    let mut failure_categories: Map<String, Value> = Map::new();
    for page in pages {
        let status = page
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("pending");
        *page_counts
            .entry(status.to_string())
            .or_insert_with(|| Value::from(0)) =
            Value::from(page_counts.get(status).and_then(Value::as_u64).unwrap_or(0) + 1);
        if matches!(status, "pending" | "planning" | "generating") {
            queued_pages += 1;
        }
        if matches!(status, "error" | "partial") {
            retryable_pages += 1;
        }
        if status == "error" {
            failed_pages += 1;
        }
        for block in page
            .get("blocks")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if block.get("status").and_then(Value::as_str) == Some("error") {
                failed_blocks += 1;
                let kind = block
                    .pointer("/metadata/failure/kind")
                    .and_then(Value::as_str)
                    .unwrap_or("generator_error");
                *failure_categories
                    .entry(kind.to_string())
                    .or_insert_with(|| Value::from(0)) = Value::from(
                    failure_categories
                        .get(kind)
                        .and_then(Value::as_u64)
                        .unwrap_or(0)
                        + 1,
                );
            }
        }
    }
    page_counts.insert("total".to_string(), Value::from(pages.len()));
    overview["book_id"] = book.get("id").cloned().unwrap_or(Value::Null);
    overview["pages"] = Value::Object(page_counts);
    overview["failed_blocks"] = Value::from(failed_blocks);
    overview["retryable_pages"] = Value::from(retryable_pages);
    overview["queued_pages"] = Value::from(queued_pages);
    overview["failed_pages"] = Value::from(failed_pages);
    overview["failure_categories"] = Value::Object(failure_categories);
    overview
}

fn book_payload(state: &AppState, mut book: Value) -> Value {
    book["permission"] = Value::String("edit".to_string());
    book["source"] = Value::String("own".to_string());
    book["can_edit"] = Value::Bool(true);
    book["can_delete"] = Value::Bool(true);
    book["reading"] = reading_summary(state, &book);
    book["generation"] = generation_overview(state, &book);
    book
}

fn api_books_list(state: &AppState) -> Result<Value, String> {
    let mut books = state.database.list_resources(BOOK_DOMAIN)?;
    books.sort_by(|left, right| {
        let left_updated = left
            .get("updated_at")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let right_updated = right
            .get("updated_at")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        right_updated.total_cmp(&left_updated)
    });
    let books = books
        .into_iter()
        .map(|book| book_payload(state, book))
        .collect::<Vec<_>>();
    Ok(json!({"books": books, "can_create": true}))
}

async fn api_list_books(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(api_books_list(&state)?))
}

async fn api_health() -> Json<Value> {
    Json(json!({"status": "healthy", "service": "book", "runtime": "rust"}))
}

async fn api_block_types() -> Json<Value> {
    let values = [
        "animation",
        "callout",
        "code",
        "concept_graph",
        "deep_dive",
        "figure",
        "flash_cards",
        "interactive",
        "quiz",
        "section",
        "text",
        "timeline",
        "user_note",
    ];
    Json(json!({
        "block_types": values.into_iter().map(|value| json!({
            "value": value,
            "planner_default": matches!(value, "section" | "callout" | "quiz" | "figure" | "interactive" | "animation" | "code" | "timeline" | "flash_cards")
        })).collect::<Vec<_>>()
    }))
}

async fn api_estimate_basis(Query(query): Query<EstimateQuery>) -> Json<Value> {
    let depth = query.depth.unwrap_or_else(|| "standard".to_string());
    let scale = match depth.as_str() {
        "brief" => 0.5,
        "deep" => 1.6,
        _ => 1.0,
    };
    let basis_rows: [(&str, i64, f64, f64); 6] = [
        ("theory", 8, 3600.0, 225.0),
        ("derivation", 7, 3600.0, 210.0),
        ("history", 7, 3700.0, 195.0),
        ("practice", 7, 2200.0, 180.0),
        ("concept", 7, 2600.0, 195.0),
        ("overview", 3, 0.0, 0.0),
    ];
    let basis = basis_rows
        .into_iter()
        .map(|(name, blocks, words, seconds)| {
            (
                name.to_string(),
                json!({
                    "blocks": blocks,
                    "words": (words * scale).round(),
                    "seconds": seconds
                }),
            )
        })
        .collect::<Map<String, Value>>();
    Json(json!({"depth": depth, "basis": basis}))
}

async fn api_create_book(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let result = create_book(&state, body).await.map_err(api_error)?;
    Ok(Json(json!({
        "book": result.get("book").cloned().unwrap_or(Value::Null),
        "proposal": result.get("proposal").cloned().unwrap_or(Value::Null)
    })))
}

async fn api_get_book(
    ScopedState(state): ScopedState,
    AxumPath(book_id): AxumPath<String>,
    Query(query): Query<IncludeBlocksQuery>,
) -> ApiResult<Json<Value>> {
    let book = load_book(&state, &book_id)?.ok_or_else(|| ApiError::not_found("书籍不存在"))?;
    let spine = load_spine(&state, &book_id)?;
    let mut pages = list_pages(&state, &book_id)?;
    let generation = generation_summary(&state, &book, &pages);
    let include_blocks = query.include_blocks.unwrap_or(true);
    if !include_blocks {
        for page in &mut pages {
            let count = page
                .get("blocks")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            page["blocks"] = json!([]);
            page["block_count"] = Value::from(count);
        }
    }
    let progress = load_progress(&state, &book_id)?;
    Ok(Json(json!({
        "book": book_payload(&state, book),
        "spine": spine,
        "pages": pages,
        "progress": progress,
        "generation": generation
    })))
}

async fn api_delete_book(
    ScopedState(state): ScopedState,
    AxumPath(book_id): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    if !book_exists(&state, &book_id)? {
        return Err(ApiError::not_found("书籍不存在"));
    }
    cancel_book_job(&book_id);
    for page in list_pages(&state, &book_id).unwrap_or_default() {
        if let Some(page_id) = page.get("id").and_then(Value::as_str) {
            let _ = state
                .database
                .delete_resource(&page_domain(&book_id), page_id);
        }
    }
    for event in state
        .database
        .list_resources(&event_domain(&book_id))
        .unwrap_or_default()
    {
        if let Some(event_id) = event.get("id").and_then(Value::as_str) {
            let _ = state
                .database
                .delete_resource(&event_domain(&book_id), event_id);
        }
    }
    state
        .database
        .delete_resource(&spine_domain(&book_id), "spine")?;
    state
        .database
        .delete_resource(&progress_domain(&book_id), "progress")?;
    state
        .database
        .delete_resource(&inputs_domain(&book_id), "inputs")?;
    for capture in state
        .database
        .list_resources(&capture_domain(&book_id))
        .unwrap_or_default()
    {
        if let Some(capture_id) = capture.get("id").and_then(Value::as_str) {
            let _ = state
                .database
                .delete_resource(&capture_domain(&book_id), capture_id);
        }
    }
    state.database.delete_resource(BOOK_DOMAIN, &book_id)?;
    Ok(Json(json!({"deleted": true, "book_id": book_id})))
}

async fn api_get_spine(
    ScopedState(state): ScopedState,
    AxumPath(book_id): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    let spine =
        load_spine(&state, &book_id)?.ok_or_else(|| ApiError::not_found("书籍章节骨架不存在"))?;
    Ok(Json(json!({"spine": spine})))
}

async fn api_get_page(
    ScopedState(state): ScopedState,
    AxumPath((book_id, page_id)): AxumPath<(String, String)>,
) -> ApiResult<Json<Value>> {
    let page =
        load_page(&state, &book_id, &page_id)?.ok_or_else(|| ApiError::not_found("书页不存在"))?;
    Ok(Json(json!({"page": page})))
}

async fn api_confirm_proposal(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let result = confirm_proposal(&state, body).await.map_err(api_error)?;
    Ok(Json(json!({
        "book": result.get("book").cloned().unwrap_or(Value::Null),
        "spine": result.get("spine").cloned().unwrap_or(Value::Null),
        "book_revision": result.get("book_revision").cloned().unwrap_or(Value::Null)
    })))
}

async fn api_confirm_spine(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let result = confirm_spine(&state, body).await.map_err(api_error)?;
    Ok(Json(json!({
        "pages": result.get("pages").cloned().unwrap_or_else(|| json!([])),
        "book_revision": result.get("book_revision").cloned().unwrap_or(Value::Null)
    })))
}

async fn api_compile_page(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let result = run_action(&state, &json!({"type": "compile_page", "book_id": body.get("book_id"), "page_id": body.get("page_id"), "force": body.get("force").cloned().unwrap_or(Value::Bool(false)), "expected_revision": body.get("expected_revision").cloned().unwrap_or(Value::Null)}))
        .await
        .map_err(api_error)?;
    Ok(Json(json!({
        "page": result.get("page").cloned().unwrap_or(Value::Null),
        "book_revision": result.get("book_revision").cloned().unwrap_or(Value::Null)
    })))
}

async fn api_regenerate_block(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let result = run_action(&state, &json!({"type": "regenerate_block", "book_id": body.get("book_id"), "page_id": body.get("page_id"), "block_id": body.get("block_id"), "params_override": body.get("params_override").cloned().unwrap_or(Value::Null), "expected_revision": body.get("expected_revision").cloned().unwrap_or(Value::Null)}))
        .await
        .map_err(api_error)?;
    Ok(Json(json!({
        "block": result.get("block").cloned().unwrap_or(Value::Null),
        "book_revision": result.get("book_revision").cloned().unwrap_or(Value::Null)
    })))
}

async fn api_insert_block(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let book_id = required_str(&body, "book_id").map_err(api_error)?;
    let page_id = required_str(&body, "page_id").map_err(api_error)?;
    let block_type = required_str(&body, "block_type").map_err(api_error)?;
    if !matches!(
        block_type.as_str(),
        "section"
            | "text"
            | "callout"
            | "quiz"
            | "figure"
            | "interactive"
            | "animation"
            | "code"
            | "timeline"
            | "flash_cards"
            | "deep_dive"
            | "user_note"
    ) {
        return Err(ApiError::bad_request(format!(
            "未知内容块类型：{block_type}"
        )));
    }
    let expected = body.get("expected_revision").and_then(Value::as_u64);
    let revision = claim_revision(&state, &book_id, expected, "insert_block").map_err(api_error)?;
    let book = load_book(&state, &book_id)?.ok_or_else(|| ApiError::not_found("书籍不存在"))?;
    let spine =
        load_spine(&state, &book_id)?.ok_or_else(|| ApiError::bad_request("书籍还没有章节骨架"))?;
    let mut page =
        load_page(&state, &book_id, &page_id)?.ok_or_else(|| ApiError::not_found("书页不存在"))?;
    let chapter = chapter_by_id(
        &spine,
        page.get("chapter_id").and_then(Value::as_str).unwrap_or(""),
    )
    .cloned()
    .ok_or_else(|| ApiError::not_found("章节不存在"))?;
    let params = body.get("params").cloned().unwrap_or_else(|| json!({}));
    let mut block = make_block(&block_type, params, &chapter, None);
    block["status"] = Value::String(if block_type == "user_note" {
        "ready".to_string()
    } else {
        "pending".to_string()
    });
    if let Some(position) = body.get("position").and_then(Value::as_u64) {
        let blocks = page
            .get_mut("blocks")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| ApiError::internal("页面 blocks 字段损坏"))?;
        let position = (position as usize).min(blocks.len());
        blocks.insert(position, block.clone());
    } else {
        if !page.get("blocks").is_some_and(Value::is_array) {
            page["blocks"] = json!([]);
        }
        page["blocks"]
            .as_array_mut()
            .expect("blocks")
            .push(block.clone());
    }
    save_page(&state, &book_id, &mut page)?;
    let compile_now = body
        .get("compile_now")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    if compile_now && block_type != "user_note" {
        block = regenerate_block_now(
            &state,
            &book_id,
            &page_id,
            block.get("id").and_then(Value::as_str).unwrap_or(""),
            None,
            None,
        )
        .await
        .map_err(api_error)?;
    }
    Ok(Json(json!({"block": block, "book_revision": revision})))
}

async fn api_update_block(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let book_id = required_str(&body, "book_id").map_err(api_error)?;
    let page_id = required_str(&body, "page_id").map_err(api_error)?;
    let block_id = required_str(&body, "block_id").map_err(api_error)?;
    let expected = body.get("expected_revision").and_then(Value::as_u64);
    let revision = claim_revision(&state, &book_id, expected, "update_block").map_err(api_error)?;
    let mut page =
        load_page(&state, &book_id, &page_id)?.ok_or_else(|| ApiError::not_found("书页不存在"))?;
    let index =
        find_block_index(&page, &block_id).ok_or_else(|| ApiError::not_found("内容块不存在"))?;
    let block_type = page
        .pointer(&format!("/blocks/{index}/type"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if !matches!(block_type.as_str(), "text" | "callout" | "user_note") {
        return Err(ApiError::bad_request(
            "只有 text、callout 和 user_note 内容块支持正文编辑",
        ));
    }
    if let Some(title) = body.get("title").and_then(Value::as_str) {
        page["blocks"][index]["title"] = Value::String(title.to_string());
    }
    if let Some(body_text) = body.get("body").and_then(Value::as_str) {
        let key = if block_type == "text"
            && page["blocks"][index].pointer("/payload/content").is_some()
        {
            "content"
        } else {
            "body"
        };
        page["blocks"][index]["payload"][key] = Value::String(body_text.to_string());
        page["blocks"][index]["metadata"]["edited_by_user"] = Value::Bool(true);
        page["blocks"][index]["status"] = Value::String("ready".to_string());
        page["blocks"][index]["error"] = Value::String(String::new());
    }
    page["blocks"][index]["updated_at"] = Value::from(now());
    let block = page["blocks"][index].clone();
    save_page(&state, &book_id, &mut page)?;
    Ok(Json(json!({"block": block, "book_revision": revision})))
}

async fn api_delete_block(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let book_id = required_str(&body, "book_id").map_err(api_error)?;
    let page_id = required_str(&body, "page_id").map_err(api_error)?;
    let block_id = required_str(&body, "block_id").map_err(api_error)?;
    let expected = body.get("expected_revision").and_then(Value::as_u64);
    let revision = claim_revision(&state, &book_id, expected, "delete_block").map_err(api_error)?;
    let mut page =
        load_page(&state, &book_id, &page_id)?.ok_or_else(|| ApiError::not_found("书页不存在"))?;
    let index =
        find_block_index(&page, &block_id).ok_or_else(|| ApiError::not_found("内容块不存在"))?;
    page["blocks"].as_array_mut().expect("blocks").remove(index);
    finalize_page_status(&mut page);
    save_page(&state, &book_id, &mut page)?;
    Ok(Json(json!({"ok": true, "book_revision": revision})))
}

async fn api_move_block(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let book_id = required_str(&body, "book_id").map_err(api_error)?;
    let page_id = required_str(&body, "page_id").map_err(api_error)?;
    let block_id = required_str(&body, "block_id").map_err(api_error)?;
    let new_position = body
        .get("new_position")
        .and_then(Value::as_u64)
        .ok_or_else(|| ApiError::bad_request("缺少 new_position"))? as usize;
    let expected = body.get("expected_revision").and_then(Value::as_u64);
    let revision = claim_revision(&state, &book_id, expected, "move_block").map_err(api_error)?;
    let mut page =
        load_page(&state, &book_id, &page_id)?.ok_or_else(|| ApiError::not_found("书页不存在"))?;
    let index =
        find_block_index(&page, &block_id).ok_or_else(|| ApiError::not_found("内容块不存在"))?;
    let blocks = page
        .get_mut("blocks")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| ApiError::internal("页面 blocks 字段损坏"))?;
    let block = blocks.remove(index);
    blocks.insert(new_position.min(blocks.len()), block);
    save_page(&state, &book_id, &mut page)?;
    Ok(Json(json!({"ok": true, "book_revision": revision})))
}

async fn api_change_block_type(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let book_id = required_str(&body, "book_id").map_err(api_error)?;
    let page_id = required_str(&body, "page_id").map_err(api_error)?;
    let block_id = required_str(&body, "block_id").map_err(api_error)?;
    let new_type = required_str(&body, "new_type").map_err(api_error)?;
    if !is_plannable_block(&new_type) && !matches!(new_type.as_str(), "concept_graph" | "user_note")
    {
        return Err(ApiError::bad_request(format!("未知内容块类型：{new_type}")));
    }
    let expected = body.get("expected_revision").and_then(Value::as_u64);
    let revision =
        claim_revision(&state, &book_id, expected, "change_block_type").map_err(api_error)?;
    let mut page =
        load_page(&state, &book_id, &page_id)?.ok_or_else(|| ApiError::not_found("书页不存在"))?;
    let index =
        find_block_index(&page, &block_id).ok_or_else(|| ApiError::not_found("内容块不存在"))?;
    page["blocks"][index]["type"] = Value::String(new_type.clone());
    page["blocks"][index]["status"] = Value::String("pending".to_string());
    page["blocks"][index]["payload"] = json!({});
    page["blocks"][index]["error"] = Value::String(String::new());
    if let Some(overrides) = body
        .get("params_override")
        .and_then(Value::as_object)
        .cloned()
    {
        for (key, value) in overrides {
            page["blocks"][index]["params"][key] = value;
        }
    }
    save_page(&state, &book_id, &mut page)?;
    let block = if new_type == "user_note" {
        page["blocks"][index]["status"] = Value::String("ready".to_string());
        save_page(&state, &book_id, &mut page)?;
        page["blocks"][index].clone()
    } else {
        regenerate_block_now(&state, &book_id, &page_id, &block_id, None, None)
            .await
            .map_err(api_error)?
    };
    Ok(Json(json!({"block": block, "book_revision": revision})))
}

async fn api_deep_dive(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let book_id = required_str(&body, "book_id").map_err(api_error)?;
    let parent_page_id = required_str(&body, "parent_page_id").map_err(api_error)?;
    let topic = required_str(&body, "topic").map_err(api_error)?;
    let block_id = body
        .get("block_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let content_type = body
        .get("content_type")
        .and_then(Value::as_str)
        .unwrap_or("concept")
        .to_string();
    let expected = body.get("expected_revision").and_then(Value::as_u64);
    let revision = claim_revision(&state, &book_id, expected, "deep_dive").map_err(api_error)?;
    let mut book = load_book(&state, &book_id)?.ok_or_else(|| ApiError::not_found("书籍不存在"))?;
    let mut spine =
        load_spine(&state, &book_id)?.ok_or_else(|| ApiError::bad_request("书籍还没有章节骨架"))?;
    let parent_page = load_page(&state, &book_id, &parent_page_id)?
        .ok_or_else(|| ApiError::not_found("父页面不存在"))?;
    let timestamp = now();
    let chapter_id = new_id("ch");
    let page_id = new_id("pg");
    let chapter = json!({
        "id": chapter_id,
        "title": topic,
        "learning_objectives": [
            format!("理解“{topic}”的核心概念"),
            format!("把“{topic}”与当前章节建立联系")
        ],
        "content_type": content_type,
        "source_anchors": [],
        "prerequisites": [parent_page.get("chapter_id").cloned().unwrap_or(Value::Null)],
        "page_ids": [page_id],
        "summary": format!("围绕“{topic}”展开的深入章节"),
        "order": spine.get("chapters").and_then(Value::as_array).map(Vec::len).unwrap_or(0),
        "deep_dive": true
    });
    if !spine.get("chapters").is_some_and(Value::is_array) {
        spine["chapters"] = json!([]);
    }
    spine["chapters"]
        .as_array_mut()
        .expect("chapters")
        .push(chapter.clone());
    save_spine(&state, &book_id, &mut spine)?;
    let mut page = json!({
        "id": page_id,
        "book_id": book_id,
        "chapter_id": chapter_id,
        "title": topic,
        "learning_objectives": chapter.get("learning_objectives").cloned().unwrap_or_else(|| json!([])),
        "content_type": content_type,
        "status": "pending",
        "order": chapter.get("order").cloned().unwrap_or(Value::from(0)),
        "blocks": [],
        "links": [{
            "target_page_id": parent_page_id,
            "relation": "references",
            "label": "回到原章节"
        }],
        "parent_page_id": parent_page_id,
        "error": "",
        "created_at": timestamp,
        "updated_at": timestamp
    });
    save_page(&state, &book_id, &mut page)?;
    if !block_id.is_empty() {
        let mut parent = parent_page;
        if let Some(index) = find_block_index(&parent, &block_id) {
            parent["blocks"][index]["metadata"]["deep_dive_pages"][&topic] =
                Value::String(page_id.clone());
            save_page(&state, &book_id, &mut parent)?;
        }
    }
    book["page_count"] = Value::from(list_pages(&state, &book_id).unwrap_or_default().len());
    book["status"] = Value::String("compiling".to_string());
    save_book(&state, &mut book)?;
    let page = compile_page_now(&state, &book_id, &page_id, false, None)
        .await
        .map_err(api_error)?;
    Ok(Json(json!({"page": page, "book_revision": revision})))
}

async fn api_supplement(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let book_id = required_str(&body, "book_id").map_err(api_error)?;
    let page_id = required_str(&body, "page_id").map_err(api_error)?;
    let topic = required_str(&body, "topic").map_err(api_error)?;
    let expected = body.get("expected_revision").and_then(Value::as_u64);
    let revision = claim_revision(&state, &book_id, expected, "supplement").map_err(api_error)?;
    let book = load_book(&state, &book_id)?.ok_or_else(|| ApiError::not_found("书籍不存在"))?;
    let spine =
        load_spine(&state, &book_id)?.ok_or_else(|| ApiError::bad_request("书籍还没有章节骨架"))?;
    let mut page =
        load_page(&state, &book_id, &page_id)?.ok_or_else(|| ApiError::not_found("书页不存在"))?;
    let chapter = chapter_by_id(
        &spine,
        page.get("chapter_id").and_then(Value::as_str).unwrap_or(""),
    )
    .cloned()
    .ok_or_else(|| ApiError::not_found("章节不存在"))?;
    let block = make_block(
        "section",
        json!({
            "role": "remedial",
            "focus": topic,
            "target_words": 700,
            "chapter_title": chapter.get("title").cloned().unwrap_or(Value::Null),
            "chapter_summary": chapter.get("summary").cloned().unwrap_or(Value::Null),
            "objectives": chapter.get("learning_objectives").cloned().unwrap_or_else(|| json!([]))
        }),
        &chapter,
        Some(
            if language_is_chinese(book.get("language").and_then(Value::as_str).unwrap_or("en")) {
                "补充一个更易理解的讲解与例子"
            } else {
                "A simpler explanation and worked example"
            }
            .to_string(),
        ),
    );
    let block_id = block
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    page["blocks"]
        .as_array_mut()
        .ok_or_else(|| ApiError::internal("页面 blocks 字段损坏"))?
        .push(block);
    save_page(&state, &book_id, &mut page)?;
    let block = regenerate_block_now(&state, &book_id, &page_id, &block_id, None, None)
        .await
        .map_err(api_error)?;
    Ok(Json(json!({"block": block, "book_revision": revision})))
}

async fn api_mark_visited(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let book_id = required_str(&body, "book_id").map_err(api_error)?;
    let page_id = required_str(&body, "page_id").map_err(api_error)?;
    let mut progress = load_progress(&state, &book_id)?;
    progress["current_page_id"] = Value::String(page_id.clone());
    if !progress
        .get("visited_page_ids")
        .is_some_and(Value::is_array)
    {
        progress["visited_page_ids"] = json!([]);
    }
    let visited = progress["visited_page_ids"]
        .as_array_mut()
        .expect("visited");
    if !visited
        .iter()
        .any(|value| value.as_str() == Some(page_id.as_str()))
    {
        visited.push(Value::String(page_id));
    }
    save_progress(&state, &book_id, &mut progress)?;
    Ok(Json(json!({"progress": progress})))
}

async fn api_toggle_bookmark(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let book_id = required_str(&body, "book_id").map_err(api_error)?;
    let page_id = required_str(&body, "page_id").map_err(api_error)?;
    let mut progress = load_progress(&state, &book_id)?;
    if !progress
        .get("bookmarked_page_ids")
        .is_some_and(Value::is_array)
    {
        progress["bookmarked_page_ids"] = json!([]);
    }
    let bookmarks = progress["bookmarked_page_ids"]
        .as_array_mut()
        .expect("bookmarks");
    if let Some(index) = bookmarks
        .iter()
        .position(|value| value.as_str() == Some(page_id.as_str()))
    {
        bookmarks.remove(index);
    } else {
        bookmarks.push(Value::String(page_id));
    }
    save_progress(&state, &book_id, &mut progress)?;
    Ok(Json(json!({"progress": progress})))
}

async fn api_quiz_attempt(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let book_id = required_str(&body, "book_id").map_err(api_error)?;
    let page_id = required_str(&body, "page_id").map_err(api_error)?;
    let block_id = required_str(&body, "block_id").map_err(api_error)?;
    let question_id = body
        .get("question_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let user_answer = body
        .get("user_answer")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let is_correct = body.get("is_correct").and_then(Value::as_bool);
    let submission_id = body
        .get("submission_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut progress = load_progress(&state, &book_id)?;
    if !progress.get("quiz_attempts").is_some_and(Value::is_array) {
        progress["quiz_attempts"] = json!([]);
    }
    let attempts = progress["quiz_attempts"].as_array_mut().expect("attempts");
    if !submission_id.is_empty() {
        if let Some(existing) = attempts.iter().find(|attempt| {
            attempt.get("submission_id").and_then(Value::as_str) == Some(submission_id.as_str())
        }) {
            let same = existing.get("page_id").and_then(Value::as_str) == Some(page_id.as_str())
                && existing.get("block_id").and_then(Value::as_str) == Some(block_id.as_str())
                && existing.get("question_id").and_then(Value::as_str)
                    == Some(question_id.as_str())
                && existing.get("user_answer").and_then(Value::as_str)
                    == Some(user_answer.as_str())
                && existing.get("is_correct").and_then(Value::as_bool) == is_correct;
            if !same {
                return Err(ApiError::conflict(
                    "同一个 submission_id 被用于不同的测验答案",
                ));
            }
            return Ok(Json(json!({"progress": progress})));
        }
    }
    attempts.push(json!({
        "block_id": block_id,
        "page_id": page_id,
        "question_id": question_id,
        "user_answer": user_answer,
        "is_correct": is_correct,
        "submission_id": submission_id,
        "timestamp": now()
    }));
    recompute_progress(&state, &book_id, &mut progress)?;
    save_progress(&state, &book_id, &mut progress)?;
    Ok(Json(json!({"progress": progress})))
}

fn recompute_progress(state: &AppState, book_id: &str, progress: &mut Value) -> Result<(), String> {
    let pages = list_pages(state, book_id)?;
    let page_to_chapter = pages
        .iter()
        .filter_map(|page| {
            Some((
                page.get("id").and_then(Value::as_str)?.to_string(),
                page.get("chapter_id")?.as_str()?.to_string(),
            ))
        })
        .collect::<HashMap<_, _>>();
    let attempts = progress
        .get("quiz_attempts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut latest: HashMap<(String, String), Value> = HashMap::new();
    for attempt in attempts {
        let block_id = attempt
            .get("block_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let question_id = attempt
            .get("question_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let timestamp = attempt
            .get("timestamp")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let key = (block_id, question_id);
        let replace = latest
            .get(&key)
            .and_then(|existing| existing.get("timestamp"))
            .and_then(Value::as_f64)
            .is_none_or(|existing| timestamp >= existing);
        if replace {
            latest.insert(key, attempt);
        }
    }
    let score = latest
        .values()
        .filter(|attempt| attempt.get("is_correct").and_then(Value::as_bool) == Some(true))
        .count();
    let mut weak = latest
        .values()
        .filter(|attempt| attempt.get("is_correct").and_then(Value::as_bool) == Some(false))
        .filter_map(|attempt| {
            attempt
                .get("page_id")
                .and_then(Value::as_str)
                .and_then(|page_id| page_to_chapter.get(page_id))
                .cloned()
        })
        .collect::<Vec<_>>();
    weak.sort();
    weak.dedup();
    progress["score"] = Value::from(score);
    progress["weak_chapters"] = Value::Array(weak.into_iter().map(Value::String).collect());
    Ok(())
}

async fn api_page_chat_session(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let book_id = required_str(&body, "book_id").map_err(api_error)?;
    let page_id = required_str(&body, "page_id").map_err(api_error)?;
    let session_id = required_str(&body, "session_id").map_err(api_error)?;
    if load_page(&state, &book_id, &page_id)?.is_none() {
        return Err(ApiError::not_found("书页不存在"));
    }
    let mut book = load_book(&state, &book_id)?.ok_or_else(|| ApiError::not_found("书籍不存在"))?;
    book["metadata"]["page_chat_sessions"][&page_id] = Value::String(session_id);
    save_book(&state, &mut book)?;
    Ok(Json(json!({"book": book_payload(&state, book)})))
}

async fn api_pause(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let book_id = required_str(&body, "book_id").map_err(api_error)?;
    let expected = body.get("expected_revision").and_then(Value::as_u64);
    claim_revision(&state, &book_id, expected, "pause").map_err(api_error)?;
    cancel_book_job(&book_id);
    let mut book = load_book(&state, &book_id)?.ok_or_else(|| ApiError::not_found("书籍不存在"))?;
    if load_spine(&state, &book_id)?.is_none() {
        return Err(ApiError::bad_request("书籍还没有章节骨架"));
    }
    book["status"] = Value::String("paused".to_string());
    book["metadata"]["pause_reason"] = Value::String("用户暂停了生成。".to_string());
    book["metadata"]["pause_kind"] = Value::String("user".to_string());
    save_book(&state, &mut book)?;
    let mut pages = list_pages(&state, &book_id)?;
    for page in &mut pages {
        if matches!(
            page.get("status").and_then(Value::as_str),
            Some("planning" | "generating")
        ) {
            page["status"] = Value::String("pending".to_string());
            page["error"] = Value::String(String::new());
            save_page(&state, &book_id, page)?;
        }
    }
    emit(
        &state,
        &book_id,
        "compilation_paused",
        "compilation",
        "",
        "",
        json!({"reason": "用户暂停了生成。", "manual": true}),
    );
    Ok(Json(json!({"pages": pages})))
}

async fn api_resume(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let book_id = required_str(&body, "book_id").map_err(api_error)?;
    let expected = body.get("expected_revision").and_then(Value::as_u64);
    claim_revision(&state, &book_id, expected, "resume").map_err(api_error)?;
    let mut book = load_book(&state, &book_id)?.ok_or_else(|| ApiError::not_found("书籍不存在"))?;
    if load_spine(&state, &book_id)?.is_none() {
        return Err(ApiError::bad_request("书籍还没有章节骨架"));
    }
    let mut pages = list_pages(&state, &book_id)?;
    let mut unfinished = Vec::new();
    for page in &mut pages {
        if matches!(
            page.get("status").and_then(Value::as_str),
            Some("planning" | "generating")
        ) {
            page["status"] = Value::String("pending".to_string());
            save_page(&state, &book_id, page)?;
        }
        if matches!(
            page.get("status").and_then(Value::as_str),
            Some("pending" | "planning" | "generating" | "error" | "partial")
        ) {
            unfinished.push(page.clone());
        }
    }
    if !unfinished.is_empty() {
        book["status"] = Value::String("compiling".to_string());
        book["metadata"]["compile_started_at"] = Value::from(now());
        if let Some(metadata) = book.get_mut("metadata").and_then(Value::as_object_mut) {
            metadata.remove("pause_reason");
            metadata.remove("pause_kind");
        }
        save_book(&state, &mut book)?;
        spawn_page_queue(state.clone(), book_id.clone(), unfinished);
    } else {
        finalize_book_if_complete(&state, &book_id)
            .await
            .map_err(api_error)?;
    }
    Ok(Json(json!({"pages": list_pages(&state, &book_id)?})))
}

async fn api_rebuild(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let book_id = required_str(&body, "book_id").map_err(api_error)?;
    let expected = body.get("expected_revision").and_then(Value::as_u64);
    claim_revision(&state, &book_id, expected, "rebuild").map_err(api_error)?;
    cancel_book_job(&book_id);
    let mut book = load_book(&state, &book_id)?.ok_or_else(|| ApiError::not_found("书籍不存在"))?;
    let mut spine =
        load_spine(&state, &book_id)?.ok_or_else(|| ApiError::bad_request("书籍还没有章节骨架"))?;
    for page in list_pages(&state, &book_id)? {
        if let Some(page_id) = page.get("id").and_then(Value::as_str) {
            state
                .database
                .delete_resource(&page_domain(&book_id), page_id)?;
        }
    }
    if let Some(chapters) = spine.get_mut("chapters").and_then(Value::as_array_mut) {
        for chapter in chapters {
            chapter["page_ids"] = json!([]);
        }
    }
    save_spine(&state, &book_id, &mut spine)?;
    book["status"] = Value::String("spine_ready".to_string());
    book["page_count"] = Value::from(0);
    save_book(&state, &mut book)?;
    let auto_compile = body
        .get("auto_compile")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let result = confirm_spine(
        &state,
        json!({
            "book_id": book_id,
            "spine": spine,
            "auto_compile": auto_compile
        }),
    )
    .await
    .map_err(api_error)?;
    Ok(Json(json!({
        "pages": result.get("pages").cloned().unwrap_or_else(|| json!([])),
        "book_revision": result.get("book_revision").cloned().unwrap_or(Value::Null)
    })))
}

#[derive(Debug, serde::Deserialize)]
struct CapturePath {
    book_id: String,
    capture_id: String,
}

async fn api_list_captures(
    ScopedState(state): ScopedState,
    AxumPath(book_id): AxumPath<String>,
    Query(query): Query<CaptureQuery>,
) -> ApiResult<Json<Value>> {
    let captures = state
        .database
        .list_resources(&capture_domain(&book_id))?
        .into_iter()
        .filter(|capture| {
            query
                .status
                .as_deref()
                .is_none_or(|status| capture.get("status").and_then(Value::as_str) == Some(status))
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({"captures": captures})))
}

async fn api_create_capture(
    ScopedState(state): ScopedState,
    AxumPath(book_id): AxumPath<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let book = load_book(&state, &book_id)?.ok_or_else(|| ApiError::not_found("书籍不存在"))?;
    let page_id = required_str(&body, "page_id").map_err(api_error)?;
    let block_id = required_str(&body, "block_id").map_err(api_error)?;
    let source_text = body
        .get("source_text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if source_text.is_empty() {
        return Err(ApiError::bad_request("source_text 不能为空"));
    }
    let locator = body
        .get("source_locator")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let capture_id = new_id("lc");
    let mut hasher = Sha256::new();
    hasher.update(book_id.as_bytes());
    hasher.update(page_id.as_bytes());
    hasher.update(block_id.as_bytes());
    hasher.update(locator.as_bytes());
    hasher.update(source_text.as_bytes());
    let content_hash = hex::encode(hasher.finalize());
    let timestamp = now();
    let capture = json!({
        "id": capture_id,
        "book_id": book_id,
        "page_id": page_id,
        "block_id": block_id,
        "capture_type": body.get("capture_type").and_then(Value::as_str).unwrap_or("selection"),
        "source_text": source_text,
        "context_before": body.get("context_before").and_then(Value::as_str).unwrap_or(""),
        "context_after": body.get("context_after").and_then(Value::as_str).unwrap_or(""),
        "source_locator": locator,
        "book_title": body.get("book_title").and_then(Value::as_str).unwrap_or_else(|| book.get("title").and_then(Value::as_str).unwrap_or("")),
        "chapter_title": body.get("chapter_title").and_then(Value::as_str).unwrap_or(""),
        "user_note": body.get("user_note").and_then(Value::as_str).unwrap_or(""),
        "content_hash": content_hash,
        "status": body.get("status").and_then(Value::as_str).unwrap_or("captured"),
        "version": 1,
        "rejected_reason": "",
        "created_at": timestamp,
        "updated_at": timestamp
    });
    state.database.put_resource(
        &capture_domain(&book_id),
        capture.get("id").and_then(Value::as_str).unwrap_or(""),
        &capture,
    )?;
    Ok(Json(json!({"capture": capture})))
}

async fn api_update_capture(
    ScopedState(state): ScopedState,
    AxumPath(CapturePath {
        book_id,
        capture_id,
    }): AxumPath<CapturePath>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let mut capture = state
        .database
        .get_resource(&capture_domain(&book_id), &capture_id)?
        .ok_or_else(|| ApiError::not_found("学习摘录不存在"))?;
    for key in ["status", "user_note", "rejected_reason"] {
        if let Some(value) = body.get(key).and_then(Value::as_str) {
            capture[key] = Value::String(value.to_string());
        }
    }
    capture["version"] =
        Value::from(capture.get("version").and_then(Value::as_u64).unwrap_or(1) + 1);
    capture["updated_at"] = Value::from(now());
    state
        .database
        .put_resource(&capture_domain(&book_id), &capture_id, &capture)?;
    Ok(Json(json!({"capture": capture})))
}

fn kb_fingerprint(state: &AppState, kb_name: &str) -> String {
    let base = state
        .database
        .get_resource("knowledge_base", kb_name)
        .ok()
        .flatten()
        .unwrap_or(Value::Null);
    let chunks = state
        .database
        .list_resources("kb_chunk")
        .unwrap_or_default()
        .into_iter()
        .filter(|chunk| {
            chunk
                .get("kb_name")
                .or_else(|| chunk.get("knowledge_base"))
                .and_then(Value::as_str)
                == Some(kb_name)
        })
        .collect::<Vec<_>>();
    let mut hasher = Sha256::new();
    hasher.update(base.to_string().as_bytes());
    for chunk in chunks {
        hasher.update(chunk.to_string().as_bytes());
    }
    hex::encode(hasher.finalize())
}

fn current_kb_fingerprints(state: &AppState, book: &Value) -> Map<String, Value> {
    book.get("knowledge_bases")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(|name| (name.to_string(), Value::String(kb_fingerprint(state, name))))
                .collect::<Map<String, Value>>()
        })
        .unwrap_or_default()
}

fn kb_drift(state: &AppState, book: &Value) -> Value {
    let book_id = book.get("id").and_then(Value::as_str).unwrap_or("");
    let current = current_kb_fingerprints(state, book);
    let saved = book
        .get("kb_fingerprints")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut new_kbs = Vec::new();
    let mut removed_kbs = Vec::new();
    let mut changed_kbs = Vec::new();
    for (name, fingerprint) in &current {
        match saved.get(name) {
            None => new_kbs.push(name.clone()),
            Some(saved) if saved != fingerprint => changed_kbs.push(name.clone()),
            _ => {}
        }
    }
    for name in saved.keys() {
        if !current.contains_key(name) {
            removed_kbs.push(name.clone());
        }
    }
    let has_drift = !new_kbs.is_empty() || !removed_kbs.is_empty() || !changed_kbs.is_empty();
    json!({
        "book_id": book_id,
        "has_drift": has_drift,
        "new_kbs": new_kbs,
        "removed_kbs": removed_kbs,
        "changed_kbs": changed_kbs,
        "stale_page_ids": book.get("stale_page_ids").cloned().unwrap_or_else(|| json!([]))
    })
}

fn log_health(state: &AppState, book_id: &str) -> Value {
    let events = state
        .database
        .list_resources(&event_domain(book_id))
        .unwrap_or_default();
    let errors = events
        .iter()
        .filter(|event| {
            event.get("type").and_then(Value::as_str) == Some("error")
                || event.get("content").and_then(Value::as_str) == Some("block_error")
        })
        .count();
    let block_failures = events
        .iter()
        .filter(|event| event.get("content").and_then(Value::as_str) == Some("block_error"))
        .count();
    let last_compile = events
        .iter()
        .find(|event| event.get("content").and_then(Value::as_str) == Some("page_compiled"))
        .and_then(|event| event.get("created_at"))
        .cloned()
        .unwrap_or(Value::Null);
    json!({
        "book_id": book_id,
        "total_entries": events.len(),
        "error_entries": errors,
        "block_failures": block_failures,
        "last_compile_at": last_compile,
        "last_error_at": events.iter().find(|event| {
            event.get("content").and_then(Value::as_str) == Some("block_error")
        }).and_then(|event| event.get("created_at")).cloned().unwrap_or(Value::Null),
        "repeated_failures": []
    })
}

async fn api_book_health(
    ScopedState(state): ScopedState,
    AxumPath(book_id): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    let book = load_book(&state, &book_id)?.ok_or_else(|| ApiError::not_found("书籍不存在"))?;
    let pages = list_pages(&state, &book_id)?;
    Ok(Json(json!({
        "kb_drift": kb_drift(&state, &book),
        "log_health": log_health(&state, &book_id),
        "generation": generation_summary(&state, &book, &pages)
    })))
}

async fn api_refresh_fingerprints(
    ScopedState(state): ScopedState,
    AxumPath(book_id): AxumPath<String>,
    Query(query): Query<RefreshFingerprintQuery>,
) -> ApiResult<Json<Value>> {
    let _guard = STATE_LOCK.lock();
    let mut book = load_book(&state, &book_id)?.ok_or_else(|| ApiError::not_found("书籍不存在"))?;
    let revision = book.get("revision").and_then(Value::as_u64).unwrap_or(1);
    if let Some(expected) = query.expected_revision {
        if expected != revision {
            return Err(ApiError::conflict(format!(
                "书籍内容已更新，当前版本为 {revision}"
            )));
        }
    }
    let stale = book
        .get("stale_page_ids")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    if stale > 0 && !query.force.unwrap_or(false) {
        return Err(ApiError::conflict(
            "仍有页面等待重新编译；如确认忽略，请使用 force=true",
        ));
    }
    let fingerprints = current_kb_fingerprints(&state, &book);
    let next_revision = revision.saturating_add(1);
    book["revision"] = Value::from(next_revision);
    book["last_operation"] = Value::String("refresh_fingerprints".to_string());
    book["kb_fingerprints"] = Value::Object(fingerprints.clone());
    book["stale_page_ids"] = json!([]);
    book["stale_detected_at"] = Value::from(0);
    save_book(&state, &mut book)?;
    Ok(Json(json!({
        "book_id": book_id,
        "kb_fingerprints": fingerprints,
        "stale_page_ids": [],
        "book_revision": next_revision
    })))
}

async fn api_export_book(
    ScopedState(state): ScopedState,
    AxumPath(book_id): AxumPath<String>,
) -> ApiResult<Response> {
    let book = load_book(&state, &book_id)?.ok_or_else(|| ApiError::not_found("书籍不存在"))?;
    let pages = list_pages(&state, &book_id)?;
    let markdown = render_book_markdown(&book, &pages);
    let filename = sanitize_download_filename(
        book.get("title")
            .and_then(Value::as_str)
            .unwrap_or("deeptutor-book"),
    );
    Ok((
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                "text/markdown; charset=utf-8".to_string(),
            ),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}.md\""),
            ),
        ],
        markdown,
    )
        .into_response())
}

fn sanitize_download_filename(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if matches!(
                character,
                '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0'
            ) || character.is_control()
            {
                '_'
            } else {
                character
            }
        })
        .take(80)
        .collect::<String>()
        .trim_matches('.')
        .to_string()
        .pipe_non_empty("deeptutor-book")
}

trait NonEmptyString {
    fn pipe_non_empty(self, fallback: &str) -> String;
}

impl NonEmptyString for String {
    fn pipe_non_empty(self, fallback: &str) -> String {
        if self.trim().is_empty() {
            fallback.to_string()
        } else {
            self
        }
    }
}

fn render_book_markdown(book: &Value, pages: &[Value]) -> String {
    let mut output = String::new();
    output.push_str(&format!(
        "# {}\n\n{}\n\n",
        book.get("title")
            .and_then(Value::as_str)
            .unwrap_or("DeepTutor Book"),
        book.get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
    ));
    for page in pages {
        output.push_str(&format!(
            "## {}\n\n",
            page.get("title")
                .and_then(Value::as_str)
                .unwrap_or("Chapter")
        ));
        for block in page
            .get("blocks")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let block_type = block.get("type").and_then(Value::as_str).unwrap_or("");
            match block_type {
                "section" => {
                    if let Some(intro) = block.pointer("/payload/intro").and_then(Value::as_str) {
                        output.push_str(intro);
                        output.push_str("\n\n");
                    }
                    for subsection in block
                        .pointer("/payload/subsections")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        if let Some(body) = subsection.get("body").and_then(Value::as_str) {
                            output.push_str(body);
                            output.push_str("\n\n");
                        }
                    }
                    if let Some(takeaway) = block
                        .pointer("/payload/key_takeaway")
                        .and_then(Value::as_str)
                    {
                        output.push_str(&format!("> **Key takeaway:** {takeaway}\n\n"));
                    }
                }
                "text" => {
                    if let Some(body) = block
                        .pointer("/payload/body")
                        .and_then(Value::as_str)
                        .or_else(|| block.pointer("/payload/content").and_then(Value::as_str))
                    {
                        output.push_str(body);
                        output.push_str("\n\n");
                    }
                }
                "callout" => {
                    if let Some(body) = block.pointer("/payload/body").and_then(Value::as_str) {
                        let label = block
                            .pointer("/payload/label")
                            .and_then(Value::as_str)
                            .unwrap_or("Note");
                        output.push_str(&format!("> **{label}:** {body}\n\n"));
                    }
                }
                "quiz" => {
                    if let Some(questions) = block
                        .pointer("/payload/questions")
                        .and_then(Value::as_array)
                    {
                        for (index, question) in questions.iter().enumerate() {
                            output.push_str(&format!(
                                "**Q{}.** {}\n\n",
                                index + 1,
                                question
                                    .get("question")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                            ));
                            if let Some(options) =
                                question.get("options").and_then(Value::as_object)
                            {
                                for (key, value) in options {
                                    output.push_str(&format!(
                                        "- {key}. {}\n",
                                        value.as_str().unwrap_or("")
                                    ));
                                }
                                output.push('\n');
                            }
                            if let Some(answer) = question
                                .get("correct_answer")
                                .and_then(Value::as_str)
                                .filter(|value| !value.is_empty())
                            {
                                output.push_str(&format!(
                                    "<details><summary>答案</summary>\n\n{answer}\n\n</details>\n\n"
                                ));
                            }
                        }
                    }
                }
                "code" => {
                    let language = block
                        .pointer("/payload/language")
                        .and_then(Value::as_str)
                        .unwrap_or("text");
                    let code = block
                        .pointer("/payload/code")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    output.push_str(&format!("```{language}\n{code}\n```\n\n"));
                    if let Some(explanation) = block
                        .pointer("/payload/explanation")
                        .and_then(Value::as_str)
                    {
                        output.push_str(explanation);
                        output.push_str("\n\n");
                    }
                }
                "timeline" => {
                    for event in block
                        .pointer("/payload/events")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        output.push_str(&format!(
                            "- **{}** {} — {}\n",
                            event.get("date").and_then(Value::as_str).unwrap_or(""),
                            event.get("title").and_then(Value::as_str).unwrap_or(""),
                            event
                                .get("description")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                        ));
                    }
                    output.push('\n');
                }
                "flash_cards" => {
                    for card in block
                        .pointer("/payload/cards")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        output.push_str(&format!(
                            "- **{}** → {}\n",
                            card.get("front").and_then(Value::as_str).unwrap_or(""),
                            card.get("back").and_then(Value::as_str).unwrap_or("")
                        ));
                    }
                    output.push('\n');
                }
                "figure" | "interactive" | "animation" => {
                    if let Some(description) = block
                        .pointer("/payload/description")
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty())
                    {
                        output.push_str(description);
                        output.push_str("\n\n");
                    }
                }
                "user_note" => {
                    if let Some(body) = block.pointer("/payload/body").and_then(Value::as_str) {
                        output.push_str(&format!("> **读者笔记:** {body}\n\n"));
                    }
                }
                _ => {}
            }
        }
    }
    output
}
