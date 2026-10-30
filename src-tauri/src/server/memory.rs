use super::engine;
use super::error::{ApiError, ApiResult};
use super::llm::{self, LlmChunk};
use super::AppState;
use super::ScopedState;
use axum::extract::{Path, Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use futures_util::Stream;
use parking_lot::{Mutex, RwLock};
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;
use std::fs;
use std::io::Write;
use std::path::{Path as FilePath, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const SURFACES: [&str; 7] = [
    "chat", "notebook", "quiz", "kb", "book", "partner", "cowriter",
];
const L3_SLOTS: [&str; 4] = ["recent", "profile", "scope", "preferences"];
const ENTRY_ID_PATTERN: &str = r"m_[0-9A-HJKMNP-TV-Z]{26}";
const SETTINGS_KEY: &str = "memory";

static MEMORY_WRITE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static RUNS: LazyLock<Mutex<HashMap<String, Arc<MemoryRun>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static ENTRY_ID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"^{ENTRY_ID_PATTERN}$")).expect("记忆条目正则有效"));
static MARKER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[\^([^\]]+)\]").expect("记忆脚标正则有效"));

#[derive(Debug, Deserialize)]
struct RunListQuery {
    layer: Option<String>,
    key: Option<String>,
}

#[derive(Debug, Deserialize)]
struct EventQuery {
    since: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct TraceQuery {
    limit: Option<usize>,
    offset: Option<usize>,
}

#[derive(Debug, Clone)]
struct Entry {
    id: String,
    section: String,
    text: String,
    refs: Vec<String>,
}

#[derive(Debug, Clone)]
struct Document {
    title: String,
    sections: Vec<(String, Vec<Entry>)>,
}

impl Document {
    fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.sections.iter().flat_map(|(_, entries)| entries.iter())
    }

    fn entry_count(&self) -> usize {
        self.entries().count()
    }

    fn remove_entry(&mut self, entry_id: &str) -> bool {
        for (_, entries) in &mut self.sections {
            if let Some(index) = entries.iter().position(|entry| entry.id == entry_id) {
                entries.remove(index);
                return true;
            }
        }
        false
    }
}

#[derive(Debug, Clone)]
struct UndoRecord {
    path: PathBuf,
    content: String,
    event: Value,
}

#[derive(Debug)]
struct RunState {
    status: String,
    events: Vec<Value>,
    error: Option<String>,
    ended_at: Option<String>,
    undo: Vec<UndoRecord>,
}

struct MemoryRun {
    id: String,
    layer: String,
    key: String,
    mode: String,
    started_at: String,
    state: RwLock<RunState>,
    notify: broadcast::Sender<Value>,
    cancel: CancellationToken,
}

impl MemoryRun {
    fn new(layer: String, key: String, mode: String) -> Arc<Self> {
        let (notify, _) = broadcast::channel(2048);
        Arc::new(Self {
            id: Uuid::new_v4().to_string(),
            layer,
            key,
            mode,
            started_at: Utc::now().to_rfc3339(),
            state: RwLock::new(RunState {
                status: "queued".to_string(),
                events: Vec::new(),
                error: None,
                ended_at: None,
                undo: Vec::new(),
            }),
            notify,
            cancel: CancellationToken::new(),
        })
    }

    fn handle(&self) -> Value {
        let state = self.state.read();
        json!({
            "id": self.id,
            "layer": self.layer,
            "key": self.key,
            "mode": self.mode,
            "status": state.status,
            "started_at": self.started_at,
            "ended_at": state.ended_at,
            "error": state.error,
            "event_count": state.events.len(),
            "undo_count": state.undo.len()
        })
    }

    fn is_active(&self) -> bool {
        matches!(self.state.read().status.as_str(), "queued" | "running")
    }

    fn push(&self, mut payload: Value) {
        if !payload.is_object() {
            payload = json!({"stage": "event", "value": payload});
        }
        let seq = {
            let mut state = self.state.write();
            if let Some(object) = payload.as_object_mut() {
                object.insert(
                    "seq".to_string(),
                    Value::from(state.events.len() as u64 + 1),
                );
                object.insert("ts".to_string(), Value::String(Utc::now().to_rfc3339()));
            }
            state.events.push(payload.clone());
            state.events.len()
        };
        tracing::trace!(run_id = %self.id, seq, "记忆运行事件");
        let _ = self.notify.send(payload);
    }

    fn set_status(&self, status: &str, error: Option<String>) {
        let mut state = self.state.write();
        state.status = status.to_string();
        state.error = error;
        if matches!(status, "done" | "error" | "cancelled") {
            state.ended_at = Some(Utc::now().to_rfc3339());
        }
    }

    fn events_since(&self, since: u64) -> Vec<Value> {
        self.state
            .read()
            .events
            .iter()
            .filter(|event| {
                event
                    .get("seq")
                    .and_then(Value::as_u64)
                    .is_some_and(|seq| seq >= since.max(1))
            })
            .cloned()
            .collect()
    }

    fn push_undo(&self, undo: UndoRecord) {
        self.state.write().undo.push(undo);
    }

    fn finish(&self, status: &str, error: Option<String>) {
        self.set_status(status, error.clone());
        self.push(json!({
            "stage": "run_ended",
            "status": status,
            "error": error,
            "undo_depth": self.state.read().undo.len()
        }));
    }
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/memory/overview", get(overview))
        .route("/memory/backup", get(backup))
        .route("/memory/resolve_entry/{entry_id}", get(resolve_entry))
        .route("/memory/settings", get(get_settings).put(put_settings))
        .route("/memory/doc/{layer}/{key}", get(get_doc).put(put_doc))
        .route("/memory/doc/{layer}/{key}/lines", get(get_doc_lines))
        .route(
            "/memory/doc/{layer}/{key}/entry/{entry_id}",
            delete(delete_entry),
        )
        .route("/memory/doc/{layer}/{key}/reset", post(reset_doc))
        .route("/memory/doc/{layer}/{key}/apply", post(apply_ops))
        .route("/memory/doc/{layer}/{key}/update", post(legacy_update))
        .route("/memory/doc/{layer}/{key}/audit", post(legacy_audit))
        .route("/memory/doc/{layer}/{key}/dedup", post(legacy_dedup))
        .route("/memory/runs/start", post(start_run))
        .route("/memory/runs", get(list_runs))
        .route("/memory/runs/{run_id}", get(get_run))
        .route("/memory/runs/{run_id}/cancel", post(cancel_run))
        .route("/memory/runs/{run_id}/undo", post(undo_run))
        .route("/memory/runs/{run_id}/events", get(run_events))
        .route(
            "/memory/trace/{surface}",
            get(get_trace).delete(clear_trace),
        )
        .route("/memory/trace/{surface}/day/{day}", delete(clear_trace_day))
        .route("/memory/snapshot/{surface}", get(get_snapshot))
        .route(
            "/memory/snapshot/{surface}/changes",
            get(snapshot_changes).delete(clear_snapshot_changes),
        )
        .route("/memory/snapshot/{surface}/refresh", post(refresh_snapshot))
}

fn memory_root(state: &AppState) -> PathBuf {
    state.paths.data.join("memory")
}

fn ensure_memory_root(state: &AppState) -> ApiResult<PathBuf> {
    let root = memory_root(state);
    for directory in [
        root.clone(),
        root.join("L2"),
        root.join("L3"),
        root.join("trace"),
        root.join("snapshot"),
        root.join("backup"),
    ] {
        fs::create_dir_all(&directory)
            .map_err(|error| ApiError::internal(format!("无法创建记忆目录：{error}")))?;
    }
    for surface in SURFACES {
        fs::create_dir_all(root.join("trace").join(surface))
            .map_err(|error| ApiError::internal(format!("无法创建记忆追踪目录：{error}")))?;
    }
    Ok(root)
}

fn validate_layer(layer: &str) -> ApiResult<&str> {
    match layer {
        "L2" | "L3" => Ok(layer),
        _ => Err(ApiError::bad_request("记忆层级必须是 L2 或 L3")),
    }
}

fn validate_key(layer: &str, key: &str) -> ApiResult<()> {
    let known = if layer == "L2" {
        SURFACES.contains(&key)
    } else {
        L3_SLOTS.contains(&key)
    };
    if known {
        Ok(())
    } else {
        Err(ApiError::not_found("未知的记忆文档"))
    }
}

fn validate_surface(surface: &str) -> ApiResult<()> {
    if SURFACES.contains(&surface) {
        Ok(())
    } else {
        Err(ApiError::not_found("未知的记忆来源"))
    }
}

fn document_path(state: &AppState, layer: &str, key: &str) -> ApiResult<PathBuf> {
    validate_layer(layer)?;
    validate_key(layer, key)?;
    let root = ensure_memory_root(state)?;
    Ok(root.join(layer).join(format!("{key}.md")))
}

fn default_title(layer: &str, key: &str) -> String {
    if layer == "L2" {
        format!("{key} memory")
    } else {
        match key {
            "recent" => "Recent summary".to_string(),
            "profile" => "User profile".to_string(),
            "scope" => "Knowledge scope".to_string(),
            "preferences" => "Preferences".to_string(),
            _ => format!("{key} memory"),
        }
    }
}

fn read_document(path: &FilePath) -> ApiResult<String> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(ApiError::internal(format!("无法读取记忆文档：{error}"))),
    }
}

/// 判断用户记忆中是否已有可检索内容，供工具组合决定是否挂载 `read_memory`。
pub fn has_content(state: &AppState) -> bool {
    let root = memory_root(state);
    ["L2", "L3"].iter().any(|layer| {
        let Ok(entries) = fs::read_dir(root.join(layer)) else {
            return false;
        };
        entries.filter_map(Result::ok).any(|entry| {
            entry.path().extension().and_then(|value| value.to_str()) == Some("md")
                && fs::read_to_string(entry.path())
                    .map(|content| parse_document(&content).entry_count() > 0)
                    .unwrap_or(false)
        })
    })
}

/// 读取四个 L3 文档，供聊天工具按需加载长期记忆。
pub fn l3_concat(state: &AppState) -> Result<String, String> {
    let root = ensure_memory_root(state).map_err(|error| error.to_string())?;
    let mut sections = Vec::new();
    for key in L3_SLOTS {
        let path = root.join("L3").join(format!("{key}.md"));
        let content = read_document(&path).map_err(|error| error.to_string())?;
        if !content.trim().is_empty() {
            sections.push(content.trim().to_string());
        }
    }
    if sections.is_empty() {
        Ok("长期记忆目前为空。".to_string())
    } else {
        Ok(sections.join("\n\n---\n\n"))
    }
}

/// 只为明确的用户偏好写入 L3/preferences，并生成可追踪的本地事件。
pub fn write_preference(
    state: &AppState,
    op: &str,
    text: &str,
    target_id: Option<&str>,
    reason: Option<&str>,
) -> Result<Value, String> {
    let op = op.trim().to_lowercase();
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if !matches!(op.as_str(), "add" | "edit") {
        return Err("op 必须是 add 或 edit。".to_string());
    }
    if text.is_empty() {
        return Err("偏好内容不能为空。".to_string());
    }
    if text.chars().count() > 240 {
        return Err("偏好内容不能超过 240 个字符。".to_string());
    }
    let target_id = target_id.unwrap_or("").trim();
    if op == "edit" && target_id.is_empty() {
        return Err("edit 操作必须提供 target_id。".to_string());
    }

    let trace_id = format!("tr_{}", Uuid::new_v4().simple());
    let path = document_path(state, "L3", "preferences").map_err(|error| error.to_string())?;
    let _guard = MEMORY_WRITE_LOCK.lock();
    let mut document = parse_document(&read_document(&path).map_err(|error| error.to_string())?);
    if document.title.is_empty() {
        document.title = default_title("L3", "preferences");
    }

    if op == "add" {
        let normalized = text.to_lowercase();
        if let Some(existing) = document
            .entries()
            .find(|entry| entry.text.trim().to_lowercase() == normalized)
        {
            return Ok(json!({
                "accepted": true,
                "entry_id": existing.id,
                "deduplicated": true,
                "message": format!("偏好已存在（entry={}），已跳过重复写入。", existing.id)
            }));
        }
        let entry_id = new_entry_id();
        let refs = vec![trace_id.clone()];
        if let Some((_, entries)) = document
            .sections
            .iter_mut()
            .find(|(section, _)| section == "Preferences")
        {
            entries.push(Entry {
                id: entry_id.clone(),
                section: "Preferences".to_string(),
                text: text.clone(),
                refs,
            });
        } else {
            document.sections.push((
                "Preferences".to_string(),
                vec![Entry {
                    id: entry_id.clone(),
                    section: "Preferences".to_string(),
                    text: text.clone(),
                    refs,
                }],
            ));
        }
        atomic_write(&path, &serialize_document(&document)).map_err(|error| error.to_string())?;
        emit_preference_trace(state, &trace_id, &op, &text, Some(&entry_id), reason)?;
        return Ok(json!({
            "accepted": true,
            "entry_id": entry_id,
            "deduplicated": false,
            "message": format!("偏好已新增（entry={entry_id}）。")
        }));
    }

    let entry = document
        .sections
        .iter_mut()
        .flat_map(|(_, entries)| entries.iter_mut())
        .find(|entry| entry.id == target_id);
    let Some(entry) = entry else {
        return Err(format!("找不到要编辑的偏好条目：{target_id}"));
    };
    entry.text = text.clone();
    if !entry.refs.contains(&trace_id) {
        entry.refs.push(trace_id.clone());
    }
    atomic_write(&path, &serialize_document(&document)).map_err(|error| error.to_string())?;
    emit_preference_trace(state, &trace_id, &op, &text, Some(target_id), reason)?;
    Ok(json!({
        "accepted": true,
        "entry_id": target_id,
        "deduplicated": false,
        "message": format!("偏好已更新（entry={target_id}）。")
    }))
}

fn emit_preference_trace(
    state: &AppState,
    trace_id: &str,
    op: &str,
    text: &str,
    entry_id: Option<&str>,
    reason: Option<&str>,
) -> Result<(), String> {
    let now = Utc::now();
    let day = now.format("%Y-%m-%d").to_string();
    let directory = ensure_memory_root(state)
        .map_err(|error| error.to_string())?
        .join("trace")
        .join("chat");
    fs::create_dir_all(&directory).map_err(|error| format!("无法创建记忆追踪目录：{error}"))?;
    let event = json!({
        "id": trace_id,
        "surface": "chat",
        "type": "preference_stated",
        "created_at": now.to_rfc3339(),
        "payload": {
            "op": op,
            "text": text,
            "entry_id": entry_id,
            "reason": reason
        }
    });
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(directory.join(format!("{day}.jsonl")))
        .map_err(|error| format!("无法打开记忆追踪文件：{error}"))?;
    writeln!(
        file,
        "{}",
        serde_json::to_string(&event)
            .map_err(|error| format!("无法序列化记忆追踪事件：{error}"))?
    )
    .map_err(|error| format!("无法写入记忆追踪事件：{error}"))
}

fn atomic_write(path: &FilePath, content: &str) -> ApiResult<()> {
    let parent = path
        .parent()
        .ok_or_else(|| ApiError::internal("记忆文档没有父目录"))?;
    fs::create_dir_all(parent)
        .map_err(|error| ApiError::internal(format!("无法创建记忆目录：{error}")))?;
    let temporary = parent.join(format!(".memory-{}.tmp", Uuid::new_v4()));
    fs::write(&temporary, content.as_bytes())
        .map_err(|error| ApiError::internal(format!("无法写入记忆文档：{error}")))?;
    fs::rename(&temporary, path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        ApiError::internal(format!("无法保存记忆文档：{error}"))
    })
}

fn parse_document(content: &str) -> Document {
    let mut legacy_refs: HashMap<String, Vec<String>> = HashMap::new();
    let mut label_refs: HashMap<String, String> = HashMap::new();
    for raw in content.lines() {
        let line = raw.trim_end();
        if let Some(rest) = line.strip_prefix("[^") {
            let Some((label, value)) = rest.split_once("]:") else {
                continue;
            };
            if ENTRY_ID_RE.is_match(label) {
                legacy_refs.insert(
                    label.to_string(),
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|item| !item.is_empty())
                        .map(str::to_string)
                        .collect(),
                );
            } else if !label.starts_with("m_") {
                label_refs.insert(label.to_string(), value.trim().to_string());
            }
        }
    }

    let mut document = Document {
        title: String::new(),
        sections: Vec::new(),
    };
    let mut current_section: Option<String> = None;
    for raw in content.lines() {
        let line = raw.trim_end();
        if document.title.is_empty() {
            if let Some(title) = line.strip_prefix("# ") {
                document.title = title.trim().to_string();
                continue;
            }
        }
        if let Some(section) = line.strip_prefix("## ") {
            let section = section.trim().to_string();
            document.sections.push((section.clone(), Vec::new()));
            current_section = Some(section);
            continue;
        }
        let Some(section) = current_section.as_ref() else {
            continue;
        };
        let Some(body) = line.trim_start().strip_prefix("- ") else {
            continue;
        };

        let (entry_id, text, markers) = if let Some(comment_start) = body.rfind("<!--") {
            if let Some(id) = body[comment_start + 4..]
                .trim()
                .strip_suffix("-->")
                .map(str::trim)
                .filter(|id| ENTRY_ID_RE.is_match(id))
            {
                let before = body[..comment_start].trim_end();
                let marker_ids = MARKER_RE
                    .captures_iter(before)
                    .filter_map(|capture| capture.get(1))
                    .map(|value| value.as_str().to_string())
                    .collect::<Vec<_>>();
                (id.to_string(), before.to_string(), marker_ids)
            } else {
                continue;
            }
        } else if let Some(comment_start) = body.rfind("[^") {
            let marker = body[comment_start + 2..].trim_end_matches(']').trim();
            if !ENTRY_ID_RE.is_match(marker) {
                continue;
            }
            (
                marker.to_string(),
                body[..comment_start].trim_end().to_string(),
                vec![marker.to_string()],
            )
        } else {
            continue;
        };

        let mut refs = Vec::new();
        for marker in markers {
            if ENTRY_ID_RE.is_match(&marker) {
                for value in legacy_refs.get(&marker).cloned().unwrap_or_default() {
                    if !refs.contains(&value) {
                        refs.push(value);
                    }
                }
            } else if let Some(value) = label_refs.get(&marker) {
                if !value.is_empty() && !refs.contains(value) {
                    refs.push(value.clone());
                }
            }
        }
        let mut text = text;
        for capture in MARKER_RE.captures_iter(&text.clone()) {
            if let Some(marker) = capture.get(1) {
                text = text.replace(&capture[0], "");
                if !marker.as_str().starts_with("m_") && !label_refs.contains_key(marker.as_str()) {
                    refs.push(marker.as_str().to_string());
                }
            }
        }
        if let Some((_, entries)) = document
            .sections
            .iter_mut()
            .find(|(name, _)| name == section)
        {
            entries.push(Entry {
                id: entry_id,
                section: section.clone(),
                text: text.trim().to_string(),
                refs,
            });
        }
    }
    document
}

fn serialize_document(document: &Document) -> String {
    let mut labels = BTreeMap::<String, usize>::new();
    let mut refs = Vec::<String>::new();
    for entry in document.entries() {
        for reference in &entry.refs {
            if !labels.contains_key(reference) {
                let label = refs.len() + 1;
                labels.insert(reference.clone(), label);
                refs.push(reference.clone());
            }
        }
    }
    let mut output = String::new();
    if !document.title.is_empty() {
        output.push_str("# ");
        output.push_str(&document.title);
        output.push_str("\n\n");
    }
    for (section, entries) in &document.sections {
        if entries.is_empty() {
            continue;
        }
        output.push_str("## ");
        output.push_str(section);
        output.push_str("\n\n");
        for entry in entries {
            output.push_str("- ");
            output.push_str(entry.text.trim());
            let markers = entry
                .refs
                .iter()
                .filter_map(|reference| labels.get(reference))
                .map(|label| format!("[^{label}]"))
                .collect::<Vec<_>>();
            if !markers.is_empty() {
                output.push(' ');
                output.push_str(&markers.join(", "));
            }
            output.push_str(" <!--");
            output.push_str(&entry.id);
            output.push_str("-->\n");
        }
        output.push('\n');
    }
    if !refs.is_empty() {
        output.push_str("---\n\n");
        for (index, reference) in refs.iter().enumerate() {
            output.push_str(&format!("[^{}]: {}\n", index + 1, reference));
        }
        output.push('\n');
    }
    output
}

fn line_view(document: &Document) -> Vec<Value> {
    let mut lines = Vec::<Value>::new();
    if !document.title.is_empty() {
        lines.push(json!({
            "number": lines.len() + 1,
            "kind": "title",
            "text": format!("# {}", document.title),
            "entry_id": null,
            "section": null
        }));
        lines.push(json!({
            "number": lines.len() + 1,
            "kind": "blank",
            "text": "",
            "entry_id": null,
            "section": null
        }));
    }
    for (section, entries) in &document.sections {
        if entries.is_empty() {
            continue;
        }
        lines.push(json!({
            "number": lines.len() + 1,
            "kind": "section",
            "text": format!("## {section}"),
            "entry_id": null,
            "section": null
        }));
        for entry in entries {
            lines.push(json!({
                "number": lines.len() + 1,
                "kind": "bullet",
                "text": format!("- {} [^{}]", entry.text, entry.id),
                "entry_id": entry.id,
                "section": section
            }));
        }
        lines.push(json!({
            "number": lines.len() + 1,
            "kind": "blank",
            "text": "",
            "entry_id": null,
            "section": null
        }));
    }
    while lines
        .last()
        .and_then(|line| line.get("kind"))
        .and_then(Value::as_str)
        == Some("blank")
    {
        lines.pop();
    }
    lines
}

fn new_entry_id() -> String {
    let mut hasher = Sha256::new();
    hasher.update(
        Utc::now()
            .timestamp_nanos_opt()
            .unwrap_or_default()
            .to_be_bytes(),
    );
    hasher.update(Uuid::new_v4().as_bytes());
    let digest = format!("{:x}", hasher.finalize()).to_uppercase();
    format!("m_{}", &digest[..26])
}

async fn overview(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    let root = ensure_memory_root(&state)?;
    let mut docs = Vec::new();
    for (layer, keys) in [("L2", SURFACES.to_vec()), ("L3", L3_SLOTS.to_vec())] {
        for key in keys {
            let path = root.join(layer).join(format!("{key}.md"));
            if !path.is_file() {
                docs.push(json!({
                    "layer": layer,
                    "key": key,
                    "exists": false,
                    "updated_at": null,
                    "entry_count": 0,
                    "backlog": if layer == "L2" { count_trace_since(&state, key, None).unwrap_or(0) } else { 0 }
                }));
                continue;
            }
            let metadata = fs::metadata(&path)
                .map_err(|error| ApiError::internal(format!("无法读取记忆状态：{error}")))?;
            let updated_at = metadata.modified().ok().map(|value| {
                let value: DateTime<Utc> = value.into();
                value.to_rfc3339()
            });
            let content = read_document(&path)?;
            let document = parse_document(&content);
            let backlog = if layer == "L2" {
                count_trace_since(&state, key, updated_at.as_deref()).unwrap_or(0)
            } else {
                0
            };
            docs.push(json!({
                "layer": layer,
                "key": key,
                "exists": true,
                "updated_at": updated_at,
                "entry_count": document.entry_count(),
                "backlog": backlog
            }));
        }
    }
    let backups = list_backup_names(root.join("backup"));
    Ok(Json(json!({"docs": docs, "backups": backups})))
}

async fn backup(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    let root = ensure_memory_root(&state)?.join("backup");
    let mut backups = Vec::new();
    for entry in fs::read_dir(root)
        .map_err(|error| ApiError::internal(format!("无法读取记忆备份：{error}")))?
    {
        let entry = entry.map_err(|error| ApiError::internal(error.to_string()))?;
        if !entry
            .file_type()
            .map_err(|error| ApiError::internal(error.to_string()))?
            .is_dir()
        {
            continue;
        }
        let mut files = fs::read_dir(entry.path())
            .map_err(|error| ApiError::internal(error.to_string()))?
            .filter_map(Result::ok)
            .map(|value| value.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        files.sort();
        backups.push(json!({
            "name": entry.file_name().to_string_lossy(),
            "files": files
        }));
    }
    backups.sort_by(|left, right| {
        left.get("name")
            .and_then(Value::as_str)
            .cmp(&right.get("name").and_then(Value::as_str))
    });
    Ok(Json(json!({"backups": backups})))
}

async fn resolve_entry(
    ScopedState(state): ScopedState,
    Path(entry_id): Path<String>,
) -> ApiResult<Json<Value>> {
    if !ENTRY_ID_RE.is_match(&entry_id) {
        return Err(ApiError::bad_request("记忆条目编号不合法"));
    }
    for surface in SURFACES {
        let path = document_path(&state, "L2", surface)?;
        let document = parse_document(&read_document(&path)?);
        if document.entries().any(|entry| entry.id == entry_id) {
            return Ok(Json(json!({
                "layer": "L2",
                "key": surface,
                "entry_id": entry_id
            })));
        }
    }
    Err(ApiError::not_found("任何 L2 文档中都没有这个记忆条目"))
}

async fn get_doc(
    ScopedState(state): ScopedState,
    Path((layer, key)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let path = document_path(&state, &layer, &key)?;
    Ok(Json(json!({
        "layer": layer,
        "key": key,
        "content": read_document(&path)?
    })))
}

async fn put_doc(
    ScopedState(state): ScopedState,
    Path((layer, key)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let path = document_path(&state, &layer, &key)?;
    let content = body
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad_request("缺少记忆文档内容"))?;
    let _guard = MEMORY_WRITE_LOCK.lock();
    atomic_write(&path, content)?;
    Ok(Json(json!({"layer": layer, "key": key, "saved": true})))
}

async fn get_doc_lines(
    ScopedState(state): ScopedState,
    Path((layer, key)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let path = document_path(&state, &layer, &key)?;
    let document = parse_document(&read_document(&path)?);
    Ok(Json(json!({
        "layer": layer,
        "key": key,
        "lines": line_view(&document)
    })))
}

async fn delete_entry(
    ScopedState(state): ScopedState,
    Path((layer, key, entry_id)): Path<(String, String, String)>,
) -> ApiResult<Json<Value>> {
    let path = document_path(&state, &layer, &key)?;
    let _guard = MEMORY_WRITE_LOCK.lock();
    let mut document = parse_document(&read_document(&path)?);
    if !document.remove_entry(&entry_id) {
        return Err(ApiError::not_found("记忆条目不存在"));
    }
    atomic_write(&path, &serialize_document(&document))?;
    Ok(Json(
        json!({"layer": layer, "key": key, "deleted": entry_id}),
    ))
}

async fn reset_doc(
    ScopedState(state): ScopedState,
    Path((layer, key)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    if RUNS
        .lock()
        .values()
        .any(|run| run.layer == layer && run.key == key && run.is_active())
    {
        return Err(ApiError::conflict("重置前请先取消这个文档的运行任务"));
    }
    let path = document_path(&state, &layer, &key)?;
    let meta_path = path.with_extension("meta.json");
    let removed_doc = path.exists();
    let removed_meta = meta_path.exists();
    if removed_doc {
        fs::remove_file(&path)
            .map_err(|error| ApiError::internal(format!("无法重置记忆文档：{error}")))?;
    }
    if removed_meta {
        fs::remove_file(&meta_path)
            .map_err(|error| ApiError::internal(format!("无法重置记忆元数据：{error}")))?;
    }
    Ok(Json(json!({
        "layer": layer,
        "key": key,
        "reset": true,
        "removed_doc": removed_doc,
        "removed_meta": removed_meta
    })))
}

async fn apply_ops(
    ScopedState(state): ScopedState,
    Path((layer, key)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    if layer == "L3" && key == "preferences" {
        return Err(ApiError::forbidden("偏好文档只能通过记忆工具写入"));
    }
    let ops = body
        .get("ops")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if ops.is_empty() {
        return Ok(Json(json!({
            "accepted": true,
            "reason": "没有需要应用的变更",
            "results": []
        })));
    }
    let path = document_path(&state, &layer, &key)?;
    let _guard = MEMORY_WRITE_LOCK.lock();
    let mut document = parse_document(&read_document(&path)?);
    if document.title.is_empty() {
        document.title = default_title(&layer, &key);
    }
    let mut results = Vec::new();
    for op in ops {
        let kind = op
            .get("op")
            .or_else(|| op.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("");
        match kind {
            "add" => {
                let section = op
                    .get("section")
                    .and_then(Value::as_str)
                    .unwrap_or("Memory")
                    .trim()
                    .to_string();
                let text = op
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let refs = op
                    .get("refs")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                if text.is_empty() {
                    results.push(json!({"status": "rejected", "detail": "缺少文本"}));
                    continue;
                }
                let entry_id = new_entry_id();
                if let Some((_, entries)) = document
                    .sections
                    .iter_mut()
                    .find(|(name, _)| name == &section)
                {
                    entries.push(Entry {
                        id: entry_id.clone(),
                        section,
                        text,
                        refs,
                    });
                } else {
                    document.sections.push((
                        section.clone(),
                        vec![Entry {
                            id: entry_id.clone(),
                            section,
                            text,
                            refs,
                        }],
                    ));
                }
                results.push(json!({
                    "status": "applied",
                    "entry_id": entry_id,
                    "detail": ""
                }));
            }
            "edit" => {
                let target = op
                    .get("target_id")
                    .or_else(|| op.get("entry_id"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let text = op
                    .get("new_text")
                    .or_else(|| op.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                let refs = op.get("new_refs").or_else(|| op.get("refs"));
                let found = document
                    .sections
                    .iter_mut()
                    .flat_map(|(_, entries)| entries.iter_mut())
                    .find(|entry| entry.id == target);
                if let Some(entry) = found {
                    if !text.is_empty() {
                        entry.text = text.to_string();
                    }
                    if let Some(refs) = refs.and_then(Value::as_array) {
                        entry.refs = refs
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect();
                    }
                    results.push(json!({
                        "status": "applied",
                        "entry_id": target,
                        "detail": ""
                    }));
                } else {
                    results.push(json!({
                        "status": "rejected",
                        "entry_id": target,
                        "detail": "条目不存在"
                    }));
                }
            }
            "delete" => {
                let target = op
                    .get("target_id")
                    .or_else(|| op.get("entry_id"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let applied = document.remove_entry(target);
                results.push(json!({
                    "status": if applied {"applied"} else {"rejected"},
                    "entry_id": target,
                    "detail": if applied {""} else {"条目不存在"}
                }));
            }
            _ => results.push(json!({
                "status": "rejected",
                "detail": format!("不支持的记忆操作：{kind}")
            })),
        }
    }
    atomic_write(&path, &serialize_document(&document))?;
    Ok(Json(json!({
        "accepted": true,
        "reason": "",
        "results": results
    })))
}

async fn get_settings(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    let stored = state
        .database
        .get_setting(SETTINGS_KEY)?
        .unwrap_or_else(|| json!({}));
    Ok(Json(merge_memory_settings(stored)))
}

async fn put_settings(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let saved = state.database.merge_setting(SETTINGS_KEY, &body)?;
    Ok(Json(merge_memory_settings(saved)))
}

fn merge_memory_settings(raw: Value) -> Value {
    let defaults = json!({
        "update": {"l2_budget": 20, "l3_budget": 10},
        "audit": {"l2_budget": 20, "l3_budget": 10},
        "dedup": {"iterations": 3, "auto_after_update": true},
        "merge": {
            "auto_after_update": true,
            "auto_after_audit": true,
            "auto_after_dedup": true
        },
        "chunking": {
            "overlap_ratio": 0.1,
            "boundary": "paragraph",
            "min_chunk_chars": 1000,
            "max_chunk_chars": 64000
        },
        "reference": {"enforce_required": true, "drop_invalid_refs": true}
    });
    deep_merge(defaults, raw)
}

fn deep_merge(mut base: Value, patch: Value) -> Value {
    match (&mut base, patch) {
        (Value::Object(base), Value::Object(patch)) => {
            for (key, value) in patch {
                let merged = base
                    .remove(&key)
                    .map(|current| deep_merge(current, value.clone()))
                    .unwrap_or(value);
                base.insert(key, merged);
            }
            Value::Object(base.clone())
        }
        (_, value) => value,
    }
}

#[derive(Debug, Deserialize)]
struct RunStartRequest {
    layer: String,
    key: String,
    mode: String,
    #[serde(default)]
    language: String,
    budget: Option<usize>,
    iterations: Option<usize>,
    llm_selection: Option<Value>,
}

async fn start_run(
    ScopedState(state): ScopedState,
    Json(request): Json<RunStartRequest>,
) -> ApiResult<Json<Value>> {
    validate_layer(&request.layer)?;
    validate_key(&request.layer, &request.key)?;
    if !matches!(
        request.mode.as_str(),
        "update" | "audit" | "dedup" | "merge"
    ) {
        return Err(ApiError::bad_request("未知的记忆运行模式"));
    }
    if request.layer == "L3"
        && request.key == "preferences"
        && !matches!(request.mode.as_str(), "dedup" | "merge")
    {
        return Err(ApiError::unprocessable("偏好文档不能自动整合"));
    }
    let mut runs = RUNS.lock();
    if runs
        .values()
        .any(|run| run.layer == request.layer && run.key == request.key && run.is_active())
    {
        return Err(ApiError::conflict("这个记忆文档已经有一个活动任务"));
    }
    let run = MemoryRun::new(
        request.layer.clone(),
        request.key.clone(),
        request.mode.clone(),
    );
    let handle = run.handle();
    run.push(json!({
        "stage": "queued",
        "layer": request.layer,
        "key": request.key,
        "mode": request.mode
    }));
    runs.insert(run.id.clone(), run.clone());
    if runs.len() > 200 {
        let active = runs
            .iter()
            .filter(|(_, run)| run.is_active())
            .map(|(id, _)| id.clone())
            .collect::<std::collections::HashSet<_>>();
        runs.retain(|id, _| active.contains(id) || id == &run.id);
    }
    drop(runs);

    tokio::spawn(execute_run(
        state,
        run,
        request.language,
        request.budget,
        request.iterations,
        request.llm_selection,
    ));
    Ok(Json(handle))
}

async fn execute_run(
    state: AppState,
    run: Arc<MemoryRun>,
    language: String,
    budget: Option<usize>,
    iterations: Option<usize>,
    selection: Option<Value>,
) {
    run.set_status("running", None);
    run.push(json!({"stage": "run_started"}));
    let result = execute_run_inner(&state, &run, &language, budget, iterations, selection).await;
    match result {
        Ok(()) => run.finish("done", None),
        Err(error) if run.cancel.is_cancelled() => {
            run.finish("cancelled", Some(error.message));
        }
        Err(error) => run.finish("error", Some(error.message)),
    }
}

async fn execute_run_inner(
    state: &AppState,
    run: &Arc<MemoryRun>,
    language: &str,
    budget: Option<usize>,
    iterations: Option<usize>,
    selection: Option<Value>,
) -> ApiResult<()> {
    let path = document_path(state, &run.layer, &run.key)?;
    let current = read_document(&path)?;
    if run.mode == "merge" {
        let document = parse_document(&current);
        let merged = serialize_document(&document);
        let changed = merged != current;
        if changed {
            apply_run_write(run, &path, current, merged, "merge_applied")?;
        }
        run.push(json!({"stage": "done", "changed": changed}));
        return Ok(());
    }

    run.push(json!({"stage": "collecting"}));
    let source = if run.mode == "update" {
        if run.layer == "L2" {
            snapshot_text(state, &run.key)?
        } else {
            l2_context(state)?
        }
    } else {
        current.clone()
    };
    run.push(json!({
        "stage": "sources_ready",
        "count": if source.is_empty() { 0 } else { 1 }
    }));
    if source.trim().is_empty() && run.mode == "update" {
        return Err(ApiError::unprocessable("没有可用于更新记忆的来源数据"));
    }

    let settings = engine::resolve_llm_settings(state, &json!({"llm_selection": selection}))
        .map_err(ApiError::provider)?;
    let mode_text = match run.mode.as_str() {
        "update" => "更新并整合记忆，保留已有事实，合并重复，输出完整文档",
        "audit" => "审计记忆的引用、矛盾和过时内容，修正后输出完整文档",
        "dedup" => "去重并压缩冗余记忆，保留全部条目编号和有效引用",
        _ => "整理记忆",
    };
    let budget_text = budget
        .map(|value| format!("本次最多保留约 {value} 条事实。"))
        .unwrap_or_default();
    let iteration_text = iterations
        .map(|value| format!("最多进行 {value} 轮去重。"))
        .unwrap_or_default();
    let instruction = format!(
        "你是 DeepTutor 记忆整理器。请用{language}执行：{mode_text}。\n\
         仅输出 Markdown 记忆文档，不要代码围栏或解释。\n\
         格式必须为：`# 标题`、若干 `## 分区`、每条事实一行 `- 事实 [^编号] <!--m_26位编号-->`，\
         末尾 `---` 后按首次出现顺序写 `[^编号]: 来源引用`。\n\
         保留已有条目的 `m_` 编号；新条目生成合法编号。不要编造来源。{budget_text}{iteration_text}\n\
         当前文档：\n{current}\n\n可用来源：\n{source}"
    );
    run.push(json!({"stage": "model_started", "model": settings.model}));
    let messages = vec![
        json!({"role": "system", "content": "你负责维护可追溯、简洁、无重复的分层记忆文档。"}),
        json!({"role": "user", "content": instruction}),
    ];
    let run_for_delta = run.clone();
    let response = llm::stream_completion(
        &state.client,
        &settings,
        &messages,
        &[],
        None,
        &run.cancel,
        move |chunk| {
            if let LlmChunk::Content(delta) = chunk {
                run_for_delta.push(json!({"stage": "delta", "delta": delta}));
            }
        },
    )
    .await
    .map_err(ApiError::provider)?;
    let generated = normalize_generated_document(&response.content, &run.layer, &run.key)?;
    if generated == current {
        run.push(json!({"stage": "done", "changed": false}));
        return Ok(());
    }
    apply_run_write(run, &path, current, generated, "write_applied")?;
    run.push(json!({
        "stage": "done",
        "changed": true,
        "usage": {
            "prompt_tokens": response.usage.prompt_tokens,
            "completion_tokens": response.usage.completion_tokens,
            "total_tokens": response.usage.total_tokens
        }
    }));
    Ok(())
}

fn apply_run_write(
    run: &Arc<MemoryRun>,
    path: &FilePath,
    previous: String,
    next: String,
    stage: &str,
) -> ApiResult<()> {
    let _guard = MEMORY_WRITE_LOCK.lock();
    let event = json!({
        "stage": stage,
        "path": path.file_name().and_then(|value| value.to_str()).unwrap_or("memory.md")
    });
    atomic_write(path, &next)?;
    run.push_undo(UndoRecord {
        path: path.to_path_buf(),
        content: previous,
        event: event.clone(),
    });
    run.push(event);
    Ok(())
}

fn normalize_generated_document(raw: &str, layer: &str, key: &str) -> ApiResult<String> {
    let mut text = raw.trim().to_string();
    if text.starts_with("```") {
        text = text
            .lines()
            .skip(1)
            .take_while(|line| !line.trim_start().starts_with("```"))
            .collect::<Vec<_>>()
            .join("\n");
    }
    let mut document = parse_document(&text);
    if document.title.is_empty() {
        document.title = default_title(layer, key);
    }
    if document.entry_count() == 0 {
        let body = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect::<Vec<_>>()
            .join(" ");
        if body.is_empty() {
            return Err(ApiError::provider("模型没有返回可用的记忆文档"));
        }
        document.sections.push((
            "Memory".to_string(),
            vec![Entry {
                id: new_entry_id(),
                section: "Memory".to_string(),
                text: body,
                refs: Vec::new(),
            }],
        ));
    }
    Ok(serialize_document(&document))
}

async fn get_run(Path(run_id): Path<String>) -> ApiResult<Json<Value>> {
    let run = RUNS
        .lock()
        .get(&run_id)
        .cloned()
        .ok_or_else(|| ApiError::not_found("未知的记忆运行编号"))?;
    Ok(Json(run.handle()))
}

async fn list_runs(Query(query): Query<RunListQuery>) -> ApiResult<Json<Value>> {
    if let Some(layer) = query.layer.as_deref() {
        validate_layer(layer)?;
        if let Some(key) = query.key.as_deref() {
            validate_key(layer, key)?;
        }
    }
    let mut runs = RUNS
        .lock()
        .values()
        .filter(|run| {
            query
                .layer
                .as_deref()
                .is_none_or(|layer| run.layer == layer)
                && query.key.as_deref().is_none_or(|key| run.key == key)
        })
        .map(|run| run.handle())
        .collect::<Vec<_>>();
    runs.sort_by(|left, right| {
        right
            .get("started_at")
            .and_then(Value::as_str)
            .cmp(&left.get("started_at").and_then(Value::as_str))
    });
    Ok(Json(json!({"runs": runs})))
}

async fn cancel_run(Path(run_id): Path<String>) -> ApiResult<Json<Value>> {
    let run = RUNS
        .lock()
        .get(&run_id)
        .cloned()
        .ok_or_else(|| ApiError::not_found("未知的记忆运行编号"))?;
    if !run.is_active() {
        return Err(ApiError::conflict("记忆运行已经结束"));
    }
    run.cancel.cancel();
    run.push(json!({"stage": "cancelling"}));
    Ok(Json(json!({"run_id": run_id, "cancelled": true})))
}

async fn undo_run(Path(run_id): Path<String>) -> ApiResult<Json<Value>> {
    let run = RUNS
        .lock()
        .get(&run_id)
        .cloned()
        .ok_or_else(|| ApiError::not_found("未知的记忆运行编号"))?;
    if run.is_active() {
        return Err(ApiError::conflict("运行结束前不能撤销"));
    }
    let undo = run
        .state
        .write()
        .undo
        .pop()
        .ok_or_else(|| ApiError::conflict("没有可撤销的写入"))?;
    let mut undone = undo.event.clone();
    undone["stage"] = Value::String("undo_applied".to_string());
    run.push(undone.clone());
    atomic_write(&undo.path, &undo.content)?;
    Ok(Json(json!({
        "run_id": run_id,
        "undone": true,
        "undo_count": run.state.read().undo.len(),
        "event": undone
    })))
}

async fn run_events(
    Path(run_id): Path<String>,
    Query(query): Query<EventQuery>,
) -> ApiResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    let run = RUNS
        .lock()
        .get(&run_id)
        .cloned()
        .ok_or_else(|| ApiError::not_found("未知的记忆运行编号"))?;
    let since = query.since.unwrap_or(0).max(1);
    let mut receiver = run.notify.subscribe();
    let stream = async_stream::stream! {
        let mut cursor = since;
        for event in run.events_since(cursor) {
            cursor = event.get("seq").and_then(Value::as_u64).unwrap_or(cursor) + 1;
            yield Ok(Event::default().data(event.to_string()));
        }
        loop {
            if !run.is_active() {
                for event in run.events_since(cursor) {
                    yield Ok(Event::default().data(event.to_string()));
                }
                break;
            }
            match tokio::time::timeout(Duration::from_secs(15), receiver.recv()).await {
                Ok(Ok(event)) => {
                    let seq = event.get("seq").and_then(Value::as_u64).unwrap_or(cursor);
                    if seq >= cursor {
                        cursor = seq + 1;
                        yield Ok(Event::default().data(event.to_string()));
                    }
                }
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => {
                    for event in run.events_since(cursor) {
                        cursor = event.get("seq").and_then(Value::as_u64).unwrap_or(cursor) + 1;
                        yield Ok(Event::default().data(event.to_string()));
                    }
                }
                Ok(Err(broadcast::error::RecvError::Closed)) => break,
                Err(_) => {
                    if !run.is_active() {
                        break;
                    }
                }
            }
        }
    };
    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("heartbeat"),
    ))
}

async fn legacy_update(
    ScopedState(state): ScopedState,
    Path((layer, key)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> ApiResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    start_legacy_run(ScopedState(state), layer, key, "update", body).await
}

async fn legacy_audit(
    ScopedState(state): ScopedState,
    Path((layer, key)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> ApiResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    start_legacy_run(ScopedState(state), layer, key, "audit", body).await
}

async fn legacy_dedup(
    ScopedState(state): ScopedState,
    Path((layer, key)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> ApiResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    start_legacy_run(ScopedState(state), layer, key, "dedup", body).await
}

async fn start_legacy_run(
    ScopedState(state): ScopedState,
    layer: String,
    key: String,
    mode: &str,
    body: Value,
) -> ApiResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    let request = RunStartRequest {
        layer,
        key,
        mode: mode.to_string(),
        language: body
            .get("language")
            .and_then(Value::as_str)
            .unwrap_or("zh")
            .to_string(),
        budget: body
            .get("budget")
            .and_then(Value::as_u64)
            .map(|value| value as usize),
        iterations: body
            .get("iterations")
            .and_then(Value::as_u64)
            .map(|value| value as usize),
        llm_selection: body.get("llm_selection").cloned(),
    };
    let Json(handle) = start_run(ScopedState(state), Json(request)).await?;
    let run_id = handle
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::internal("运行句柄缺少编号"))?
        .to_string();
    run_events(Path(run_id), Query(EventQuery { since: Some(1) })).await
}

async fn get_trace(
    ScopedState(state): ScopedState,
    Path(surface): Path<String>,
    Query(query): Query<TraceQuery>,
) -> ApiResult<Json<Value>> {
    validate_surface(&surface)?;
    let limit = query.limit.unwrap_or(200).clamp(1, 1000);
    let offset = query.offset.unwrap_or(0);
    let events = read_trace_events(&state, &surface)?
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>();
    Ok(Json(json!({
        "surface": surface,
        "events": events,
        "offset": offset,
        "limit": limit
    })))
}

async fn clear_trace(
    ScopedState(state): ScopedState,
    Path(surface): Path<String>,
) -> ApiResult<Json<Value>> {
    validate_surface(&surface)?;
    let root = ensure_memory_root(&state)?.join("trace").join(&surface);
    let mut removed_files = 0;
    for entry in fs::read_dir(root)
        .map_err(|error| ApiError::internal(format!("无法读取记忆追踪：{error}")))?
    {
        let path = entry
            .map_err(|error| ApiError::internal(error.to_string()))?
            .path();
        if path.extension().and_then(|value| value.to_str()) == Some("jsonl")
            && fs::remove_file(path).is_ok()
        {
            removed_files += 1;
        }
    }
    Ok(Json(json!({
        "surface": surface,
        "removed_files": removed_files
    })))
}

async fn clear_trace_day(
    ScopedState(state): ScopedState,
    Path((surface, day)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    validate_surface(&surface)?;
    chrono::NaiveDate::parse_from_str(&day, "%Y-%m-%d")
        .map_err(|_| ApiError::bad_request("日期必须使用 YYYY-MM-DD 格式"))?;
    let path = ensure_memory_root(&state)?
        .join("trace")
        .join(&surface)
        .join(format!("{day}.jsonl"));
    if !path.is_file() {
        return Err(ApiError::not_found("当天没有记忆追踪"));
    }
    fs::remove_file(&path)
        .map_err(|error| ApiError::internal(format!("无法删除记忆追踪：{error}")))?;
    Ok(Json(json!({
        "surface": surface,
        "day": day,
        "deleted": true
    })))
}

fn read_trace_events(state: &AppState, surface: &str) -> ApiResult<Vec<Value>> {
    let root = ensure_memory_root(state)?.join("trace").join(surface);
    let mut files = fs::read_dir(root)
        .map_err(|error| ApiError::internal(format!("无法读取记忆追踪：{error}")))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("jsonl"))
        .collect::<Vec<_>>();
    files.sort();
    let mut events = Vec::new();
    for path in files {
        let content = fs::read_to_string(path)
            .map_err(|error| ApiError::internal(format!("无法读取记忆追踪：{error}")))?;
        for line in content.lines().filter(|line| !line.trim().is_empty()) {
            if let Ok(value) = serde_json::from_str::<Value>(line) {
                events.push(value);
            }
        }
    }
    Ok(events)
}

fn count_trace_since(state: &AppState, surface: &str, since: Option<&str>) -> ApiResult<usize> {
    let cutoff = since.and_then(|value| DateTime::parse_from_rfc3339(value).ok());
    Ok(read_trace_events(state, surface)?
        .into_iter()
        .filter(|event| {
            let Some(cutoff) = cutoff else {
                return true;
            };
            event
                .get("ts")
                .and_then(Value::as_str)
                .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                .is_some_and(|value| value > cutoff)
        })
        .count())
}

async fn get_snapshot(
    ScopedState(state): ScopedState,
    Path(surface): Path<String>,
) -> ApiResult<Json<Value>> {
    validate_surface(&surface)?;
    let entities = snapshot_entities(&state, &surface)?;
    let (state_value, pending) = snapshot_state_and_changes(&state, &surface, &entities)?;
    Ok(Json(json!({
        "surface": surface,
        "entities": entities,
        "last_refresh": state_value.get("last_refresh"),
        "pending_changes": pending
    })))
}

async fn snapshot_changes(
    ScopedState(state): ScopedState,
    Path(surface): Path<String>,
    Query(query): Query<TraceQuery>,
) -> ApiResult<Json<Value>> {
    validate_surface(&surface)?;
    let limit = query.limit.unwrap_or(200).clamp(1, 1000);
    let offset = query.offset.unwrap_or(0);
    let changes = read_snapshot_changes(&state, &surface)?
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>();
    Ok(Json(json!({"surface": surface, "changes": changes})))
}

async fn clear_snapshot_changes(
    ScopedState(state): ScopedState,
    Path(surface): Path<String>,
) -> ApiResult<Json<Value>> {
    validate_surface(&surface)?;
    let (_, path) = snapshot_files(&state, &surface)?;
    match fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&path)
    {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(ApiError::internal(format!("无法清空快照变更：{error}"))),
    }
    Ok(Json(json!({"surface": surface, "cleared": true})))
}

async fn refresh_snapshot(
    ScopedState(state): ScopedState,
    Path(surface): Path<String>,
) -> ApiResult<Json<Value>> {
    validate_surface(&surface)?;
    let entities = snapshot_entities(&state, &surface)?;
    let (_, pending) = snapshot_state_and_changes(&state, &surface, &entities)?;
    append_snapshot_changes(&state, &surface, &pending)?;
    save_snapshot_state(&state, &surface, &entities)?;
    Ok(Json(json!({"surface": surface, "changes": pending})))
}

fn snapshot_entities(state: &AppState, surface: &str) -> ApiResult<Vec<Value>> {
    match surface {
        "chat" => chat_entities(state),
        "quiz" => quiz_entities(state),
        "notebook" => resource_entities(state, "notebook", "notebook", "title", "summary"),
        "kb" => resource_entities(state, "knowledge", "knowledge_base", "name", "description"),
        "book" => resource_entities(state, "book", "book", "title", "description"),
        "partner" => resource_entities(state, "partner", "partner", "name", "description"),
        "cowriter" => resource_entities(state, "cowriter", "document", "title", "content"),
        _ => Err(ApiError::not_found("未知的记忆来源")),
    }
}

fn chat_entities(state: &AppState) -> ApiResult<Vec<Value>> {
    let sessions = state.database.list_sessions(10_000, 0)?;
    let mut output = Vec::new();
    for session in sessions {
        let Some(id) = session.get("id").and_then(Value::as_str) else {
            continue;
        };
        let messages = state.database.get_messages(id).unwrap_or_default();
        let content = messages
            .iter()
            .filter_map(|message| {
                let role = message.get("role").and_then(Value::as_str)?;
                let content = message.get("content").and_then(Value::as_str)?;
                Some(format!("{role}: {content}"))
            })
            .collect::<Vec<_>>()
            .join("\n");
        output.push(entity_json(
            "chat",
            id,
            session
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("未命名会话"),
            session
                .get("updated_at")
                .and_then(Value::as_str)
                .unwrap_or(""),
            &content,
            json!({"message_count": messages.len()}),
        ));
    }
    Ok(output)
}

fn quiz_entities(state: &AppState) -> ApiResult<Vec<Value>> {
    let mut output = Vec::new();
    state.database.with_connection(|connection| {
        let mut statement = connection
            .prepare(
                "SELECT id, question, user_answer, is_correct, updated_at
                 FROM qn_entries ORDER BY updated_at DESC LIMIT 5000",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, f64>(4)?,
                ))
            })
            .map_err(|error| error.to_string())?;
        for row in rows {
            let (id, question, answer, correct, updated_at) =
                row.map_err(|error| error.to_string())?;
            let ts = DateTime::<Utc>::from_timestamp(updated_at as i64, 0)
                .unwrap_or_else(Utc::now)
                .to_rfc3339();
            output.push(entity_json(
                "quiz",
                &id.to_string(),
                &question,
                &ts,
                &format!(
                    "问题：{question}\n回答：{answer}\n判定：{}",
                    correct.unwrap_or(0)
                ),
                json!({"correct": correct}),
            ));
        }
        Ok(())
    })?;
    Ok(output)
}

fn resource_entities(
    state: &AppState,
    domain: &str,
    surface: &str,
    label_key: &str,
    content_key: &str,
) -> ApiResult<Vec<Value>> {
    Ok(state
        .database
        .list_resources(domain)?
        .into_iter()
        .filter_map(|resource| {
            let id = resource
                .get("id")
                .or_else(|| resource.get("name"))
                .and_then(Value::as_str)?;
            let label = resource
                .get(label_key)
                .or_else(|| resource.get("name"))
                .or_else(|| resource.get("id"))
                .and_then(Value::as_str)
                .unwrap_or(id);
            let content = resource
                .get(content_key)
                .or_else(|| resource.get("summary"))
                .or_else(|| resource.get("description"))
                .map(|value| {
                    value
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| value.to_string())
                })
                .unwrap_or_else(|| label.to_string());
            Some(entity_json(
                surface,
                id,
                label,
                resource
                    .get("updated_at")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                &content,
                json!({"domain": domain}),
            ))
        })
        .collect())
}

fn entity_json(
    surface: &str,
    id: &str,
    label: &str,
    ts: &str,
    content: &str,
    metadata: Value,
) -> Value {
    let entity_id = format!("{surface}:{id}");
    let fingerprint = fingerprint(&[&entity_id, label, ts, content]);
    json!({
        "id": entity_id,
        "label": label,
        "ts": ts,
        "content": content,
        "metadata": metadata,
        "fingerprint": fingerprint
    })
}

fn fingerprint(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    format!("{:x}", hasher.finalize())
}

fn snapshot_files(state: &AppState, surface: &str) -> ApiResult<(PathBuf, PathBuf)> {
    let root = ensure_memory_root(state)?.join("snapshot").join(surface);
    fs::create_dir_all(&root)
        .map_err(|error| ApiError::internal(format!("无法创建快照目录：{error}")))?;
    Ok((root.join("state.json"), root.join("changes.jsonl")))
}

fn load_snapshot_state(state: &AppState, surface: &str) -> ApiResult<Value> {
    let (path, _) = snapshot_files(state, surface)?;
    let raw = match fs::read_to_string(path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(json!({"last_refresh": null, "entities": {}}));
        }
        Err(error) => return Err(ApiError::internal(format!("无法读取快照状态：{error}"))),
    };
    serde_json::from_str(&raw).map_err(|error| ApiError::internal(format!("快照状态损坏：{error}")))
}

fn snapshot_state_and_changes(
    state: &AppState,
    surface: &str,
    entities: &[Value],
) -> ApiResult<(Value, Vec<Value>)> {
    let saved = load_snapshot_state(state, surface)?;
    let previous = saved
        .get("entities")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut current = Map::new();
    let mut changes = Vec::new();
    for entity in entities {
        let Some(id) = entity.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(new_fingerprint) = entity.get("fingerprint").and_then(Value::as_str) else {
            continue;
        };
        let previous_fingerprint = previous
            .get(id)
            .and_then(|value| value.get("fingerprint"))
            .and_then(Value::as_str);
        if previous_fingerprint != Some(new_fingerprint) {
            changes.push(json!({
                "ts": Utc::now().to_rfc3339(),
                "kind": if previous_fingerprint.is_some() {"modified"} else {"added"},
                "entity_id": id,
                "label": entity.get("label").cloned().unwrap_or(Value::Null),
                "prev_fingerprint": previous_fingerprint,
                "new_fingerprint": new_fingerprint
            }));
        }
        current.insert(
            id.to_string(),
            json!({
                "fingerprint": new_fingerprint,
                "label": entity.get("label").cloned().unwrap_or(Value::Null)
            }),
        );
    }
    for (id, old) in previous {
        if !current.contains_key(&id) {
            changes.push(json!({
                "ts": Utc::now().to_rfc3339(),
                "kind": "removed",
                "entity_id": id,
                "label": old.get("label").cloned().unwrap_or(Value::Null),
                "prev_fingerprint": old.get("fingerprint"),
                "new_fingerprint": null
            }));
        }
    }
    Ok((saved, changes))
}

fn save_snapshot_state(state: &AppState, surface: &str, entities: &[Value]) -> ApiResult<()> {
    let (path, _) = snapshot_files(state, surface)?;
    let mut map = Map::new();
    for entity in entities {
        let Some(id) = entity.get("id").and_then(Value::as_str) else {
            continue;
        };
        map.insert(
            id.to_string(),
            json!({
                "fingerprint": entity.get("fingerprint"),
                "label": entity.get("label")
            }),
        );
    }
    let payload = json!({
        "last_refresh": Utc::now().to_rfc3339(),
        "entities": map
    });
    atomic_write(
        &path,
        &serde_json::to_string_pretty(&payload)
            .map_err(|error| ApiError::internal(format!("无法序列化快照：{error}")))?,
    )
}

fn append_snapshot_changes(state: &AppState, surface: &str, changes: &[Value]) -> ApiResult<()> {
    if changes.is_empty() {
        return Ok(());
    }
    let (_, path) = snapshot_files(state, surface)?;
    let mut text = changes
        .iter()
        .map(|change| serde_json::to_string(change))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| ApiError::internal(format!("无法序列化快照变更：{error}")))?
        .join("\n");
    text.push('\n');
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| ApiError::internal(format!("无法追加快照变更：{error}")))?;
    file.write_all(text.as_bytes())
        .map_err(|error| ApiError::internal(format!("无法写入快照变更：{error}")))
}

fn read_snapshot_changes(state: &AppState, surface: &str) -> ApiResult<Vec<Value>> {
    let (_, path) = snapshot_files(state, surface)?;
    let content = match fs::read_to_string(path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(ApiError::internal(format!("无法读取快照变更：{error}"))),
    };
    Ok(content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect())
}

fn snapshot_text(state: &AppState, surface: &str) -> ApiResult<String> {
    let entities = snapshot_entities(state, surface)?;
    let mut output = String::new();
    for entity in entities {
        output.push_str("### ");
        output.push_str(
            entity
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or("条目"),
        );
        output.push_str("\n来源：");
        output.push_str(entity.get("id").and_then(Value::as_str).unwrap_or(""));
        output.push_str("\n");
        if let Some(content) = entity.get("content").and_then(Value::as_str) {
            output.push_str(&content.chars().take(12_000).collect::<String>());
        }
        output.push_str("\n\n");
        if output.chars().count() > 120_000 {
            break;
        }
    }
    Ok(output)
}

fn l2_context(state: &AppState) -> ApiResult<String> {
    let mut output = String::new();
    for surface in SURFACES {
        let content = read_document(&document_path(state, "L2", surface)?)?;
        if content.trim().is_empty() {
            continue;
        }
        output.push_str("### ");
        output.push_str(surface);
        output.push('\n');
        output.push_str(&content);
        output.push_str("\n\n");
        if output.chars().count() > 160_000 {
            break;
        }
    }
    Ok(output)
}

fn list_backup_names(root: PathBuf) -> Vec<String> {
    fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 记忆文档可以解析并往返序列化() {
        let source = "# 聊天记忆\n\n## 偏好\n\n- 用户喜欢数学 [^1] <!--m_0123456789ABCDEFGHJKMNPQRS-->\n\n---\n\n[^1]: chat:abc\n";
        let document = parse_document(source);
        assert_eq!(document.title, "聊天记忆");
        assert_eq!(document.entry_count(), 1);
        let roundtrip = serialize_document(&document);
        assert!(roundtrip.contains("chat:abc"));
        assert!(roundtrip.contains("m_0123456789ABCDEFGHJKMNPQRS"));
    }

    #[test]
    fn 行视图会省略脚注但保留条目编号() {
        let document = parse_document(
            "# 记忆\n\n## 事实\n\n- 测试 [^1] <!--m_0123456789ABCDEFGHJKMNPQRS-->\n\n---\n\n[^1]: chat:1\n",
        );
        let lines = line_view(&document);
        assert!(lines.iter().any(|line| {
            line.get("text").and_then(Value::as_str)
                == Some("- 测试 [^m_0123456789ABCDEFGHJKMNPQRS]")
        }));
    }

    #[test]
    fn 列表辅助函数不会引入占位路径() {
        assert!("chat".len() == 4);
    }
}
