use super::audio_overview;
use super::llm::{self, ApiFormat, LlmChunk, LlmSettings, LlmUsage};
use super::plugins;
use super::prompts;
use super::source_inventory;
use super::tool_policy;
use super::tools;
use super::AppState;
use parking_lot::Mutex;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const MAX_TOOL_ROUNDS: usize = 10;
const MAX_PARALLEL_TOOLS: usize = 4;
const MAX_CONTEXT_CHARS: usize = 180_000;

const CAPABILITIES: &[&str] = &[
    "chat",
    "ask_questions",
    "audio_overview",
    "deep_solve",
    "deep_question",
    "deep_research",
    "visualize",
    "math_animator",
    "mastery_path",
    "immersive_reading",
    "video_lesson",
];

#[derive(Clone)]
struct TurnEmitter {
    state: AppState,
    session_id: String,
    turn_id: String,
    source: String,
    stage: String,
}

impl TurnEmitter {
    fn with_stage(&self, stage: impl Into<String>) -> Self {
        let mut next = self.clone();
        next.stage = stage.into();
        next
    }

    fn emit(
        &self,
        event_type: &str,
        content: impl Into<String>,
        metadata: Value,
    ) -> Result<Value, String> {
        self.emit_at(event_type, content, metadata, &self.stage)
    }

    fn emit_at(
        &self,
        event_type: &str,
        content: impl Into<String>,
        metadata: Value,
        stage: &str,
    ) -> Result<Value, String> {
        self.emit_with_source(event_type, content, metadata, stage, &self.source)
    }

    fn emit_with_source(
        &self,
        event_type: &str,
        content: impl Into<String>,
        metadata: Value,
        stage: &str,
        source: &str,
    ) -> Result<Value, String> {
        let event = json!({
            "type": event_type,
            "protocol_version": "2.0",
            "content": content.into(),
            "source": source,
            "stage": stage,
            "session_id": self.session_id,
            "turn_id": self.turn_id,
            "timestamp": crate::storage::now(),
            "metadata": if metadata.is_object() { metadata } else { json!({}) }
        });
        let seq = self
            .state
            .database
            .append_turn_event(&self.turn_id, &event)?;
        let mut persisted = event;
        persisted["seq"] = Value::from(seq);
        self.state.broadcast_turn_event(persisted.clone());
        Ok(persisted)
    }
}

struct TurnContext {
    state: AppState,
    emitter: TurnEmitter,
    session_id: String,
    turn_id: String,
    capability: String,
    language: String,
    request: Value,
    user_message_id: Option<i64>,
    provider_messages: Vec<Value>,
    enabled_tools: Vec<String>,
    user_enabled_tools: Vec<String>,
    deferred_definitions: Vec<Value>,
    source_manifest: String,
    source_index: HashMap<String, String>,
    llm: LlmSettings,
    cancel: CancellationToken,
    input: mpsc::UnboundedReceiver<Value>,
    events: Vec<Value>,
    response_attachments: Vec<Value>,
    tool_attachments: Vec<Value>,
    sources: Vec<Value>,
    pending_ask_user_call: Option<String>,
    attachment_progress: Vec<AttachmentProgress>,
    capability_result: Option<Value>,
    capability_attachments: Vec<Value>,
}

struct AttachmentProgress {
    attachment_id: String,
    filename: String,
    phase: &'static str,
    detail: String,
}

/// 启动一个完整回合。函数在用户消息、会话和回合均已持久化后立即返回，
/// 模型执行在独立 Tokio 任务中继续。
pub async fn start_turn(state: AppState, request: Value) -> Result<Value, String> {
    if !state.turns_open() {
        return Err("DeepTutor 正在准备更新，请稍后重试。".to_string());
    }
    if request
        .get("protocol_version")
        .and_then(Value::as_str)
        .is_some_and(|version| version != "2.0")
    {
        return Err("不支持的回合协议版本，应为 2.0。".to_string());
    }

    let content = request
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if content.is_empty() && request.get("attachments").is_none() {
        return Err("消息内容不能为空。".to_string());
    }

    let capability = normalize_capability(
        &state,
        request
            .get("capability")
            .and_then(Value::as_str)
            .or_else(|| {
                request
                    .pointer("/config/capability")
                    .and_then(Value::as_str)
            })
            .unwrap_or("chat"),
    );
    let language = request
        .get("language")
        .and_then(Value::as_str)
        .or_else(|| request.pointer("/config/language").and_then(Value::as_str))
        .unwrap_or("zh")
        .to_string();

    let submission_id = non_empty(&request, "client_submission_id").map(str::to_string);
    let persist_user_requested = request
        .get("persist_user_message")
        .and_then(Value::as_bool)
        .unwrap_or(true)
        && !request
            .get("regenerate")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    let workspace_id = request
        .get("workspace_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let preferences = json!({
        "capability": capability,
        "language": language,
        "workspace_id": if workspace_id.is_empty() { Value::Null } else { Value::String(workspace_id.clone()) },
        "workspace_mode": request.get("workspace_mode").cloned().unwrap_or(Value::Null)
    });
    let mut request = request;
    let (session_id, session_created) = if let Some(submission_id) =
        submission_id.as_deref().filter(|_| persist_user_requested)
    {
        let digest = submission_digest(&request)?;
        let reservation = state.database.reserve_submission(
            submission_id,
            &digest,
            &workspace_id,
            non_empty(&request, "session_id"),
            &session_title(&content),
            &preferences,
        )?;
        let session_id = reservation
            .get("session_id")
            .and_then(Value::as_str)
            .ok_or_else(|| "提交保留结果缺少会话 ID。".to_string())?
            .to_string();
        request["session_id"] = Value::String(session_id.clone());
        if reservation.get("status").and_then(Value::as_str) == Some("replay") {
            let turn = reservation
                .get("turn")
                .cloned()
                .ok_or_else(|| "提交重放结果缺少回合。".to_string())?;
            let turn_status = turn
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if matches!(
                turn_status,
                "queued" | "running" | "waiting_input" | "completed"
            ) {
                return Ok(json!({
                    "session_id": session_id,
                    "turn_id": turn.get("id").cloned().unwrap_or(Value::Null),
                    "status": turn_status,
                    "capability": turn.get("capability").cloned().unwrap_or_else(|| Value::String(capability.clone())),
                    "replayed": true
                }));
            }
        }
        (
            session_id,
            reservation
                .get("session_created")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        )
    } else {
        ensure_session(&state, &request, &content, &capability)?
    };
    if let Some(active) = state.database.get_active_turn(&session_id)? {
        let requested_superseded = request
            .get("superseded_turn_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        let active_id = active.get("turn_id").and_then(Value::as_str).unwrap_or("");
        if !active_id.is_empty() && active_id != requested_superseded {
            return Err(format!("当前会话已有运行中的回合：{active_id}"));
        }
        if !requested_superseded.is_empty() {
            state.cancel_turn(requested_superseded);
        }
    }

    let attachment_progress = prepare_attachments(&state, &session_id, &mut request).await?;

    // 先完成所有可能失败的模型与上下文校验，再写入用户消息和回合记录。
    // 这样缺少模型配置时不会留下永远处于 running 状态的假回合。
    let llm = resolve_llm_settings(&state, &request)?;
    let parent_message_id = request.get("parent_message_id").and_then(Value::as_i64);
    let user_enabled_tools = enabled_tools(&state, &request)?;
    let (source_manifest, source_index) =
        source_inventory::build(&state, &request, &session_id, parent_message_id)?;
    let tool_surface = tool_policy::compose(
        &state,
        &request,
        &capability,
        &user_enabled_tools,
        !source_index.is_empty(),
    )?;
    let enabled_tools = tool_surface.names;
    let deferred_definitions = tool_surface.deferred_definitions;
    let provider_messages = build_provider_messages(
        &state,
        &request,
        &session_id,
        &content,
        &language,
        &source_manifest,
    )?;

    let persist_user = request
        .get("persist_user_message")
        .and_then(Value::as_bool)
        .unwrap_or(true)
        && request
            .get("regenerate")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            == false;
    let user_message_id = if persist_user {
        let attachments = request
            .get("attachments")
            .cloned()
            .unwrap_or_else(|| json!([]));
        Some(state.database.add_message(crate::storage::NewMessage {
            session_id: &session_id,
            role: "user",
            content: &content,
            capability: Some(&capability),
            events: json!([]),
            attachments,
            metadata: json!({
                "client_submission_id": request.get("client_submission_id"),
                "source": "unified_turn",
                "request_snapshot": source_inventory::request_snapshot(&request)
            }),
            parent_message_id,
        })?)
    } else {
        request
            .get("regenerated_from_message_id")
            .and_then(Value::as_i64)
    };

    let turn_id = new_id("turn");
    let cancel = state
        .register_turn(&turn_id)
        .ok_or_else(|| "DeepTutor 正在准备更新，请稍后重试。".to_string())?;
    if let Err(error) = state
        .database
        .create_turn(&turn_id, &session_id, &capability)
    {
        state.finish_turn(&turn_id);
        return Err(error);
    }
    if let Some(submission_id) = submission_id.as_deref().filter(|_| persist_user_requested) {
        if let Err(error) = state.database.bind_submission_turn(submission_id, &turn_id) {
            let _ = state.database.update_turn_status(
                &turn_id,
                "failed",
                &error,
                "submission_bind_failed",
                true,
            );
            state.finish_turn(&turn_id);
            return Err(error);
        }
    }
    let input = state.register_turn_input(&turn_id);
    let emitter = TurnEmitter {
        state: state.clone(),
        session_id: session_id.clone(),
        turn_id: turn_id.clone(),
        source: capability.clone(),
        stage: initial_stage(&capability).to_string(),
    };

    let session_event = match emitter.emit(
        "session",
        "",
        json!({
            "session_id": session_id,
            "turn_id": turn_id,
            "session_created": session_created
        }),
    ) {
        Ok(event) => event,
        Err(error) => {
            let _ = state.database.update_turn_status(
                &turn_id,
                "failed",
                &error,
                "event_persist_failed",
                false,
            );
            state.finish_turn(&turn_id);
            return Err(error);
        }
    };
    let events = vec![session_event];

    let result = json!({
        "session_id": session_id,
        "turn_id": turn_id,
        "status": "running",
        "capability": capability
    });

    let mut context = TurnContext {
        state: state.clone(),
        emitter,
        session_id,
        turn_id: turn_id.clone(),
        capability,
        language,
        request,
        user_message_id,
        provider_messages,
        enabled_tools,
        user_enabled_tools,
        deferred_definitions,
        source_manifest,
        source_index,
        llm,
        cancel: cancel.clone(),
        input,
        events,
        response_attachments: Vec::new(),
        tool_attachments: Vec::new(),
        sources: Vec::new(),
        pending_ask_user_call: None,
        attachment_progress,
        capability_result: None,
        capability_attachments: Vec::new(),
    };

    tokio::spawn(async move {
        run_turn(&mut context).await;
    });

    Ok(result)
}

/// 以新的客户端提交 ID 替换一次尚未完成的用户提交，并立即启动编辑后的回合。
///
/// 原始回合若仍在执行，会先取消并等待其持久化结束，再在同一个事务中把旧
/// 提交标记为已取代。这样可以避免旧回合和新回合同时修改同一分支。
pub async fn replace_submission(
    state: AppState,
    original_id: &str,
    request: Value,
) -> Result<Value, String> {
    let original_id = original_id.trim();
    if original_id.is_empty() || original_id.chars().count() > 100 {
        return Err("原始提交 ID 长度必须为 1 到 100 个字符。".to_string());
    }
    let mut request = request;
    let replacement_id = non_empty(&request, "client_submission_id")
        .ok_or_else(|| "编辑后的消息必须提供新的客户端提交 ID。".to_string())?
        .to_string();
    if replacement_id == original_id {
        return Err("编辑消息必须使用新的提交 ID。".to_string());
    }
    if replacement_id.chars().count() > 100 {
        return Err("替换提交 ID 不能超过 100 个字符。".to_string());
    }
    if !request
        .get("persist_user_message")
        .and_then(Value::as_bool)
        .unwrap_or(true)
        || request
            .get("regenerate")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        return Err("编辑后的消息必须作为新的用户消息保存。".to_string());
    }
    let workspace_id = request
        .get("workspace_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let content = request
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if content.is_empty() && request.get("attachments").is_none() {
        return Err("消息内容不能为空。".to_string());
    }
    let capability = normalize_capability(
        &state,
        request
            .get("capability")
            .and_then(Value::as_str)
            .or_else(|| {
                request
                    .pointer("/config/capability")
                    .and_then(Value::as_str)
            })
            .unwrap_or("chat"),
    );
    let language = request
        .get("language")
        .and_then(Value::as_str)
        .unwrap_or("zh")
        .to_string();
    let preferences = json!({
        "capability": capability,
        "language": language,
        "workspace_id": if workspace_id.is_empty() { Value::Null } else { Value::String(workspace_id.clone()) },
        "workspace_mode": request.get("workspace_mode").cloned().unwrap_or(Value::Null)
    });
    let digest = submission_digest(&request)?;
    let requested_session_id = non_empty(&request, "session_id").map(str::to_string);

    let reservation = loop {
        let receipt = state.database.replace_submission(
            original_id,
            &replacement_id,
            &digest,
            &workspace_id,
            requested_session_id.as_deref(),
            &session_title(&content),
            &preferences,
        )?;
        if receipt.get("status").and_then(Value::as_str) != Some("requires_cancel") {
            break receipt;
        }
        let turn_id = receipt
            .get("turn_id")
            .and_then(Value::as_str)
            .ok_or_else(|| "原始回合缺少 ID，无法安全替换。".to_string())?
            .to_string();
        state.cancel_turn(&turn_id);
        let stopped = wait_for_turn_terminal(&state, &turn_id, 200).await?;
        if !stopped {
            return Err("原始回合尚未停止，请稍后重试编辑。".to_string());
        }
    };

    let session_id = reservation
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or_else(|| "提交替换结果缺少会话 ID。".to_string())?
        .to_string();
    request["session_id"] = Value::String(session_id.clone());
    request["persist_user_message"] = Value::Bool(true);
    request["regenerate"] = Value::Bool(false);
    if request.get("parent_message_id").is_none() {
        if let Some(parent_message_id) =
            reservation.get("parent_message_id").and_then(Value::as_i64)
        {
            request["parent_message_id"] = Value::from(parent_message_id);
        }
    }
    let started = start_turn(state, request).await?;
    let turn_id = started
        .get("turn_id")
        .cloned()
        .ok_or_else(|| "编辑后的回合缺少 ID。".to_string())?;
    Ok(json!({
        "status": "started",
        "session_id": session_id,
        "submission_id": replacement_id,
        "turn": {
            "id": turn_id,
            "turn_id": turn_id,
            "session_id": session_id,
            "status": started.get("status").cloned().unwrap_or_else(|| Value::String("running".into())),
            "capability": started.get("capability").cloned().unwrap_or(Value::String(capability))
        }
    }))
}

async fn wait_for_turn_terminal(
    state: &AppState,
    turn_id: &str,
    attempts: usize,
) -> Result<bool, String> {
    for _ in 0..attempts {
        let Some(turn) = state.database.get_turn(turn_id)? else {
            return Ok(true);
        };
        let status = turn
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if matches!(status, "completed" | "failed" | "cancelled") {
            return Ok(true);
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    Ok(false)
}

/// 将前端对 `ask_user` 卡片的回答送回正在运行的原回合。
pub async fn submit_reply(
    state: AppState,
    turn_id: &str,
    text: Option<String>,
    answers: Option<Value>,
) -> Result<bool, String> {
    let payload = json!({
        "kind": "user_reply",
        "text": text,
        "answers": answers
    });
    if state.submit_turn_input(turn_id, payload.clone()) {
        state.database.delete_resource("pending_input", turn_id)?;
        return Ok(true);
    }
    if state
        .database
        .get_resource("pending_input", turn_id)?
        .is_none()
    {
        return Ok(false);
    }
    resume_persisted_turn(state, turn_id, payload).await?;
    Ok(true)
}

pub fn submit_user_input(state: &AppState, turn_id: &str, content: &str) -> bool {
    state.submit_turn_input(turn_id, json!({"kind": "user_input", "content": content}))
}

/// 重新生成会话中的最后一个用户回合，不会重复保存用户消息。
pub async fn regenerate(
    state: AppState,
    session_id: &str,
    overrides: Option<Value>,
) -> Result<Value, String> {
    let messages = state.database.get_messages(session_id)?;
    let last_user = messages
        .iter()
        .rev()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        .cloned()
        .ok_or_else(|| "没有可重新生成的消息。".to_string())?;
    let user_id = last_user
        .get("id")
        .and_then(Value::as_i64)
        .ok_or_else(|| "最后一条用户消息缺少数据库 ID。".to_string())?;
    let mut request = overrides.unwrap_or_else(|| json!({}));
    request["session_id"] = Value::String(session_id.to_string());
    request["content"] = last_user
        .get("content")
        .cloned()
        .unwrap_or_else(|| Value::String(String::new()));
    if request.get("attachments").is_none() {
        request["attachments"] = last_user
            .get("attachments")
            .cloned()
            .unwrap_or_else(|| json!([]));
    }
    request["persist_user_message"] = Value::Bool(false);
    request["regenerate"] = Value::Bool(true);
    request["regenerated_from_message_id"] = Value::from(user_id);
    if request.get("capability").is_none() {
        request["capability"] = messages
            .iter()
            .rev()
            .find(|message| message.get("role").and_then(Value::as_str) == Some("assistant"))
            .and_then(|message| message.get("capability").cloned())
            .unwrap_or_else(|| Value::String("chat".to_string()));
    }
    start_turn(state, request).await
}

async fn resume_persisted_turn(state: AppState, turn_id: &str, reply: Value) -> Result<(), String> {
    let pending = state
        .database
        .get_resource("pending_input", turn_id)?
        .ok_or_else(|| "该回合已不再等待用户回答。".to_string())?;
    let turn = state
        .database
        .get_turn(turn_id)?
        .ok_or_else(|| "找不到待恢复的回合。".to_string())?;
    if turn.get("status").and_then(Value::as_str) != Some("waiting_input") {
        return Err("回合不处于等待输入状态。".to_string());
    }
    let session_id = pending
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or_else(|| "待恢复回合缺少会话 ID。".to_string())?
        .to_string();
    let capability = pending
        .get("capability")
        .and_then(Value::as_str)
        .unwrap_or("chat")
        .to_string();
    let request = pending.get("request").cloned().unwrap_or_else(|| json!({}));
    let llm = resolve_llm_settings(&state, &request)?;
    let mut provider_messages = pending
        .get("provider_messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let ask_user_call_id = pending
        .get("ask_user_call_id")
        .and_then(Value::as_str)
        .unwrap_or("ask_user")
        .to_string();
    provider_messages.push(json!({
        "role": "tool",
        "tool_call_id": ask_user_call_id,
        "name": "ask_user",
        "content": format_user_reply(&reply)
    }));

    if !state.turns_open() {
        return Err("DeepTutor 正在准备更新，请稍后重试。".to_string());
    }
    let cancel = state
        .register_turn(&turn_id.to_string())
        .ok_or_else(|| "DeepTutor 正在准备更新，请稍后重试。".to_string())?;
    let input = state.register_turn_input(turn_id);
    let emitter = TurnEmitter {
        state: state.clone(),
        session_id: session_id.clone(),
        turn_id: turn_id.to_string(),
        source: capability.clone(),
        stage: "responding".to_string(),
    };
    emitter.emit(
        "progress",
        "已收到用户回答，继续处理。",
        json!({
            "trace_kind": "user_reply",
            "ask_user_resolved": true,
            "ask_user_tool_call_id": ask_user_call_id
        }),
    )?;
    state
        .database
        .update_turn_status(turn_id, "running", "", "", false)?;
    state.database.delete_resource("pending_input", turn_id)?;
    let user_enabled_tools = pending
        .get("user_enabled_tools")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or(enabled_tools(&state, &request)?);
    let source_index = pending
        .get("source_index")
        .and_then(Value::as_object)
        .map(|object| {
            object
                .iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|text| (key.clone(), text.to_string()))
                })
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or(
            source_inventory::build(
                &state,
                &request,
                &session_id,
                pending
                    .get("user_message_id")
                    .and_then(Value::as_i64)
                    .or_else(|| request.get("parent_message_id").and_then(Value::as_i64)),
            )?
            .1,
        );
    let tool_surface = tool_policy::compose(
        &state,
        &request,
        &capability,
        &user_enabled_tools,
        !source_index.is_empty(),
    )?;
    let enabled_tools = tool_surface.names;
    let deferred_definitions = tool_surface.deferred_definitions;

    let mut context = TurnContext {
        state: state.clone(),
        emitter,
        session_id,
        turn_id: turn_id.to_string(),
        capability,
        language: pending
            .get("language")
            .and_then(Value::as_str)
            .unwrap_or("zh")
            .to_string(),
        request,
        user_message_id: pending.get("user_message_id").and_then(Value::as_i64),
        provider_messages,
        enabled_tools,
        user_enabled_tools,
        deferred_definitions,
        source_manifest: String::new(),
        source_index,
        llm,
        cancel,
        input,
        events: state
            .database
            .get_turn_events(turn_id, 0)?
            .into_iter()
            .collect(),
        response_attachments: Vec::new(),
        tool_attachments: Vec::new(),
        sources: Vec::new(),
        pending_ask_user_call: None,
        attachment_progress: Vec::new(),
        capability_result: None,
        capability_attachments: Vec::new(),
    };
    tokio::spawn(async move {
        run_turn(&mut context).await;
    });
    Ok(())
}

async fn run_audio_overview(context: &mut TurnContext) -> Result<(String, LlmUsage), String> {
    let config = audio_overview::RequestConfig::from_request(&context.request)?;
    let retrieved = audio_overview::retrieve(&context.state, &context.request, &config).await?;
    if !retrieved.citations.is_empty() {
        let sources = retrieved
            .citations
            .iter()
            .map(|citation| {
                let mut source = citation.reference_json();
                source["type"] = Value::String("rag".to_string());
                source["kb_name"] = Value::String(retrieved.kb_name.clone());
                source
            })
            .collect::<Vec<_>>();
        let event = context.emitter.emit(
            "sources",
            "",
            json!({
                "sources": sources,
                "trace_kind": "sources"
            }),
        )?;
        context.sources.extend(sources);
        context.events.push(event);
    }
    end_current_stage(context, "completed");

    advance_stage(context, "script_writing");
    let (script, usage) = audio_overview::generate_script(
        &context.state,
        &context.llm,
        &context.request,
        &retrieved,
        &config,
        &context.language,
        &context.cancel,
    )
    .await?;
    end_current_stage(context, "completed");

    advance_stage(context, "voice_generation");
    let published = audio_overview::publish(
        &context.state,
        &retrieved,
        &script,
        &config,
        &context.turn_id,
        &context.language,
        &context.cancel,
    )
    .await?;
    end_current_stage(context, "completed");

    advance_stage(context, "publishing");
    let audio_url = format!("/files/outputs/{}", published.audio_relative);
    let transcript_url = format!("/files/outputs/{}", published.transcript_relative);
    let workspace_items = vec![
        json!({
            "path": published.audio_relative,
            "relative_path": published.audio_relative,
            "url": audio_url,
            "filename": published.audio_filename,
            "mime_type": "audio/wav",
            "size_bytes": published.audio_bytes,
            "title": published.title,
            "caption": "音频概览",
            "generated": true
        }),
        json!({
            "path": published.transcript_relative,
            "relative_path": published.transcript_relative,
            "url": transcript_url,
            "filename": published.transcript_filename,
            "mime_type": "text/markdown",
            "size_bytes": published.transcript_bytes,
            "title": published.title,
            "caption": "音频概览逐字稿",
            "generated": true
        }),
    ];
    let selected_kbs = context
        .request
        .get("knowledge_bases")
        .cloned()
        .unwrap_or_else(|| json!([]));
    context.capability_result = Some(json!({
        "response": published.transcript,
        "title": published.title,
        "kb_name": retrieved.kb_name,
        "selected_kbs": selected_kbs,
        "citations": retrieved
            .citations
            .iter()
            .map(audio_overview::Citation::reference_json)
            .collect::<Vec<_>>(),
        "segments": script
            .segments
            .iter()
            .map(audio_overview::ScriptSegment::json)
            .collect::<Vec<_>>(),
        "audio_path": published.audio_filename,
        "transcript_path": published.transcript_filename,
        "workspace_items": workspace_items,
        "metadata": {}
    }));
    context.capability_attachments = workspace_items
        .iter()
        .map(|item| {
            json!({
                "type": "document",
                "url": item.get("url").cloned().unwrap_or(Value::Null),
                "filename": item.get("filename").cloned().unwrap_or(Value::Null),
                "mime_type": item.get("mime_type").cloned().unwrap_or(Value::Null),
                "size_bytes": item.get("size_bytes").cloned().unwrap_or(Value::Null),
                "generated": true
            })
        })
        .collect();
    Ok((published.transcript, usage))
}

fn advance_stage(context: &mut TurnContext, stage: &str) {
    context.emitter = context.emitter.with_stage(stage);
    if let Ok(event) = context.emitter.emit(
        "stage_start",
        "",
        json!({
            "trace_kind": "stage",
            "stage": stage
        }),
    ) {
        context.events.push(event);
    }
}

fn end_current_stage(context: &mut TurnContext, status: &str) {
    if let Ok(event) = context.emitter.emit(
        "stage_end",
        "",
        json!({
            "trace_kind": "stage",
            "stage": context.emitter.stage,
            "status": status
        }),
    ) {
        context.events.push(event);
    }
}

async fn run_turn(context: &mut TurnContext) {
    let start = Instant::now();
    let mut usage = LlmUsage::default();
    let mut calls = 0u64;
    let mut final_content = String::new();
    let mut status = "completed";
    let mut failure = None::<(String, String, bool)>;
    let initial_stage = context.emitter.stage.clone();

    if let Err(error) = context.emitter.emit(
        "stage_start",
        "",
        json!({"trace_kind": "stage", "stage": initial_stage}),
    ) {
        finish_without_events(context, Err(error)).await;
        return;
    }
    for progress in std::mem::take(&mut context.attachment_progress) {
        let event = context.emitter.emit_with_source(
            "progress",
            progress.detail,
            json!({
                "trace_kind": "attachment_parsing",
                "attachment_id": progress.attachment_id,
                "filename": progress.filename,
                "phase": progress.phase
            }),
            &context.emitter.stage,
            "attachment_parsing",
        );
        let _ = event.and_then(|value| {
            context.events.push(value);
            Ok(())
        });
    }

    let plugin_capability = match plugins::execute_plugin_capability(
        &context.state,
        &context.capability,
        &context.request,
        &context.session_id,
        &context.turn_id,
        &context.language,
    )
    .await
    {
        Ok(Some(result)) => Some(result),
        Ok(None) => None,
        Err(error) => {
            status = "failed";
            failure = Some(("plugin_capability_failed".to_string(), error, false));
            None
        }
    };
    let capability_failed = failure.is_some();
    if let Some(result) = plugin_capability.filter(|_| !capability_failed) {
        for event in result.events {
            let event_type = event
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("progress")
                .to_string();
            let content = event
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let metadata = event.get("metadata").cloned().unwrap_or_else(|| json!({}));
            if let Ok(value) = context.emitter.emit(&event_type, content, metadata) {
                context.events.push(value);
            }
        }
        final_content = result.response;
        context.capability_result = Some(result.result);
        context.capability_attachments = result.attachments;
        calls += 1;
    } else if !capability_failed && context.capability == "audio_overview" {
        match run_audio_overview(context).await {
            Ok((transcript, audio_usage)) => {
                final_content = transcript;
                usage.accumulate(&audio_usage);
                calls += 1;
            }
            Err(error) => {
                if context.cancel.is_cancelled() {
                    status = "cancelled";
                    failure = Some(("cancelled".to_string(), error, false));
                } else {
                    status = "failed";
                    failure = Some(("audio_overview_failed".to_string(), error, true));
                }
            }
        }
    } else if !capability_failed {
        for round in 0..MAX_TOOL_ROUNDS {
            drain_queued_input(context);
            match tool_policy::compose(
                &context.state,
                &context.request,
                &context.capability,
                &context.user_enabled_tools,
                !context.source_index.is_empty(),
            ) {
                Ok(surface) => {
                    context.enabled_tools = surface.names;
                    context.deferred_definitions = surface.deferred_definitions;
                }
                Err(error) => {
                    failure = Some(("tool_composition_failed".to_string(), error, false));
                    break;
                }
            }
            let structured = is_structured_capability(&context.capability);
            let call_id = new_id("llm");
            let stage = context.emitter.stage.clone();
            let stream_metadata = json!({
                "call_id": call_id,
                "phase": stage,
                "label": if round == 0 { "理解问题" } else { "继续推理" },
                "call_kind": "agent_loop_round",
                "trace_id": call_id,
                "trace_role": "response",
                "trace_group": "llm",
                "trace_kind": "llm_chunk",
                "round": round + 1
            });
            let event_log = &mut context.events;
            let emitter = context.emitter.clone();
            let emit_error = Mutex::new(None::<String>);
            let chunk_handler = |chunk: LlmChunk| {
                if emit_error.lock().is_some() {
                    return;
                }
                let result = match chunk {
                    LlmChunk::Content(text) if !structured && !text.is_empty() => {
                        emitter.emit("content", text, stream_metadata.clone())
                    }
                    LlmChunk::Thinking(text) if !text.is_empty() => {
                        let mut metadata = stream_metadata.clone();
                        metadata["trace_kind"] = Value::String("llm_reasoning".to_string());
                        emitter.emit("thinking", text, metadata)
                    }
                    _ => return,
                };
                match result {
                    Ok(event) => event_log.push(event),
                    Err(error) => *emit_error.lock() = Some(error),
                }
            };
            let tool_choice = if context.capability == "ask_questions" && round == 0 {
                Some("ask_user")
            } else {
                None
            };
            let definitions = tools::definitions(
                &context.capability,
                &context.enabled_tools,
                Some(&context.request),
                &context.deferred_definitions,
            );
            let response = llm::stream_completion(
                &context.state.client,
                &context.llm,
                &context.provider_messages,
                &definitions,
                tool_choice,
                &context.cancel,
                chunk_handler,
            )
            .await;

            if let Some(error) = emit_error.lock().take() {
                failure = Some(("event_persist_failed".to_string(), error, false));
                break;
            }

            let response = match response {
                Ok(response) => response,
                Err(error) => {
                    if context.cancel.is_cancelled() {
                        status = "cancelled";
                        failure = Some(("cancelled".to_string(), error, false));
                    } else {
                        status = "failed";
                        failure = Some(("provider_error".to_string(), error, true));
                    }
                    break;
                }
            };
            usage.accumulate(&response.usage);
            calls += 1;
            final_content = response.content.trim().to_string();

            if response.tool_calls.is_empty() {
                break;
            }

            let tool_calls = response
                .tool_calls
                .into_iter()
                .take(MAX_PARALLEL_TOOLS)
                .collect::<Vec<_>>();
            context.provider_messages.push(json!({
                "role": "assistant",
                "content": response.content,
                "tool_calls": tool_calls.iter().map(|call| json!({
                    "id": call.id,
                    "type": "function",
                    "function": {
                        "name": call.name,
                        "arguments": call.arguments.to_string()
                    }
                })).collect::<Vec<_>>()
            }));

            let mut terminal_tool = false;
            for tool_call in tool_calls {
                if context.cancel.is_cancelled() {
                    break;
                }
                if tool_call.name == "ask_user" {
                    let reply = match pause_for_user(context, &tool_call).await {
                        Ok(reply) => reply,
                        Err(error) => {
                            if context.cancel.is_cancelled() {
                                status = "cancelled";
                                failure = Some(("cancelled".to_string(), error, false));
                            } else {
                                status = "failed";
                                failure = Some(("input_wait_failed".to_string(), error, true));
                            }
                            break;
                        }
                    };
                    context.provider_messages.push(json!({
                        "role": "tool",
                        "tool_call_id": tool_call.id,
                        "name": tool_call.name,
                        "content": format_user_reply(&reply)
                    }));
                    continue;
                }
                let outcome = tools::execute_with_runtime(
                    &tool_call.name,
                    &tool_call.arguments,
                    tools::ToolRuntime {
                        state: &context.state,
                        request: Some(&context.request),
                        llm: Some(&context.llm),
                        cancel: Some(&context.cancel),
                        turn_id: &context.turn_id,
                        session_id: &context.session_id,
                        language: &context.language,
                        source_index: Some(&context.source_index),
                    },
                )
                .await;
                if let Err(error) = emit_tool_events(context, &tool_call, &outcome) {
                    status = "failed";
                    failure = Some(("event_persist_failed".to_string(), error, false));
                    break;
                }
                collect_tool_sources(context, &outcome.metadata);
                collect_tool_attachments(context, &outcome.metadata);
                context.provider_messages.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_call.id,
                    "name": tool_call.name,
                    "content": outcome.content
                }));
                if outcome.terminate_turn {
                    terminal_tool = true;
                    break;
                }
            }
            if failure.is_some() || terminal_tool {
                break;
            }
        }
    }

    if context.cancel.is_cancelled() {
        status = "cancelled";
        failure = Some((
            "cancelled".to_string(),
            "回合已由用户取消。".to_string(),
            false,
        ));
    }

    if status == "completed" && final_content.is_empty() {
        status = "failed";
        failure = Some((
            "empty_response".to_string(),
            "模型没有返回可显示的内容。".to_string(),
            true,
        ));
    }

    let duration = start.elapsed().as_secs_f64();
    let usage_json = usage.summary_json(duration, calls);
    let mut assistant_content = final_content.clone();

    if status == "completed" {
        match finalize_capability(context, &final_content, &usage_json) {
            Ok((display, result_event, attachments)) => {
                assistant_content = display;
                if let Ok(event) = result_event {
                    context.events.push(event);
                }
                context.response_attachments = context.tool_attachments.clone();
                context.response_attachments.extend(attachments.into_iter());
                dedupe_attachments(&mut context.response_attachments);
            }
            Err(error) => {
                status = "failed";
                failure = Some(("invalid_result".to_string(), error, true));
            }
        }
    }

    if let Some((reason, message, retryable)) = &failure {
        if let Ok(event) = context.emitter.emit(
            "error",
            message,
            json!({
                "reason": reason,
                "error_code": reason,
                "retryable": retryable,
                "turn_terminal": status != "cancelled",
                "trace_kind": "error"
            }),
        ) {
            context.events.push(event);
        }
        if status != "cancelled" && assistant_content.is_empty() {
            assistant_content = message.clone();
        }
    }

    let assistant_message_id = context
        .state
        .database
        .add_message(crate::storage::NewMessage {
            session_id: &context.session_id,
            role: "assistant",
            content: &assistant_content,
            capability: Some(&context.capability),
            events: Value::Array(context.events.clone()),
            attachments: Value::Array(context.response_attachments.clone()),
            metadata: json!({
                "usage_summary": usage_json,
                "error_code": failure.as_ref().map(|item| item.0.clone())
            }),
            parent_message_id: context.user_message_id,
        })
        .ok();

    let mut done_metadata = json!({
        "status": status,
        "usage_summary": usage_json,
        "assistant_message_id": assistant_message_id,
        "user_message_id": context.user_message_id,
        "attachments": context.response_attachments
    });
    if let Some((reason, message, retryable)) = &failure {
        done_metadata["error_code"] = Value::String(reason.clone());
        done_metadata["error"] = Value::String(message.clone());
        done_metadata["retryable"] = Value::Bool(*retryable);
    }
    if let Ok(done) = context.emitter.emit("done", "", done_metadata) {
        context.events.push(done);
    }
    let failure_code = failure.as_ref().map(|item| item.0.as_str()).unwrap_or("");
    let retryable = failure.as_ref().map(|item| item.2).unwrap_or(false);
    let _ = context.state.database.update_turn_status(
        &context.turn_id,
        status,
        failure.as_ref().map(|item| item.1.as_str()).unwrap_or(""),
        failure_code,
        retryable,
    );
    finish_stage(context, status);
    publish_session_meta(context);
    context.state.finish_turn(&context.turn_id);
}

fn finish_stage(context: &TurnContext, status: &str) {
    let _ = context.emitter.emit(
        "stage_end",
        "",
        json!({"trace_kind": "stage", "stage": context.emitter.stage, "status": status}),
    );
}

fn publish_session_meta(context: &TurnContext) {
    if let Ok(Some(session)) = context.state.database.get_session_meta(&context.session_id) {
        let _ = context.emitter.emit(
            "session_meta",
            "",
            json!({
                "title": session.get("title").cloned().unwrap_or(Value::Null),
                "session_id": context.session_id
            }),
        );
    }
}

async fn finish_without_events(context: &TurnContext, error: Result<(), String>) {
    let message = error
        .err()
        .unwrap_or_else(|| "无法写入回合事件。".to_string());
    let _ = context.state.database.update_turn_status(
        &context.turn_id,
        "failed",
        &message,
        "event_persist_failed",
        false,
    );
    context.state.finish_turn(&context.turn_id);
}

fn drain_queued_input(context: &mut TurnContext) {
    while let Ok(input) = context.input.try_recv() {
        if input.get("kind").and_then(Value::as_str) == Some("user_input") {
            let content = input
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            if !content.is_empty() {
                context.provider_messages.push(json!({
                    "role": "user",
                    "content": content
                }));
                if let Ok(event) = context.emitter.emit(
                    "progress",
                    "已收到回合中的补充输入。",
                    json!({"trace_kind": "user_input"}),
                ) {
                    context.events.push(event);
                }
            }
        }
    }
}

async fn pause_for_user(context: &mut TurnContext, call: &llm::ToolCall) -> Result<Value, String> {
    let call_id = call.id.clone();
    context.pending_ask_user_call = Some(call_id.clone());
    let tool_call_event = context.emitter.emit(
        "tool_call",
        "ask_user",
        json!({
            "args": call.arguments,
            "tool_name": "ask_user",
            "tool_call_id": call_id,
            "call_id": call_id,
            "phase": context.emitter.stage,
            "label": "向用户提问",
            "call_kind": "ask_user",
            "trace_id": call_id,
            "trace_role": "tool",
            "trace_group": "tool_call",
            "trace_kind": "tool_call",
            "call_state": "running"
        }),
    )?;
    context.events.push(tool_call_event);

    let questions = call
        .arguments
        .get("questions")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let tool_result_event = context.emitter.emit(
        "tool_result",
        "等待用户回答。",
        json!({
            "tool": "ask_user",
            "tool_name": "ask_user",
            "tool_call_id": call_id,
            "call_id": call_id,
            "phase": context.emitter.stage,
            "label": "用户回答",
            "call_kind": "ask_user",
            "trace_id": call_id,
            "trace_role": "tool",
            "trace_group": "tool_call",
            "trace_kind": "tool_result",
            "call_state": "complete",
            "tool_metadata": {
                "ask_user": {
                    "intro": call.arguments.get("intro").cloned().unwrap_or(Value::Null),
                    "questions": questions
                }
            }
        }),
    )?;
    context.events.push(tool_result_event);
    let wait_event = context.emitter.emit(
        "wait_for_input",
        call.arguments
            .get("intro")
            .and_then(Value::as_str)
            .unwrap_or("请回答下面的问题后继续。"),
        json!({
            "tool_call_id": call_id,
            "questions": questions,
            "ask_user": call.arguments
        }),
    )?;
    context.events.push(wait_event);

    context
        .state
        .database
        .update_turn_status(&context.turn_id, "waiting_input", "", "", false)?;
    context.state.database.put_resource(
        "pending_input",
        &context.turn_id,
        &json!({
            "session_id": context.session_id,
            "turn_id": context.turn_id,
            "capability": context.capability,
            "language": context.language,
            "request": context.request,
            "user_message_id": context.user_message_id,
            "provider_messages": context.provider_messages,
            "user_enabled_tools": context.user_enabled_tools,
            "source_index": context.source_index,
            "ask_user_call_id": call_id
        }),
    )?;

    loop {
        let input = tokio::select! {
            _ = context.cancel.cancelled() => {
                return Err("回合已取消。".to_string());
            }
            input = context.input.recv() => {
                input.ok_or_else(|| "用户输入通道已关闭。".to_string())?
            }
        };
        match input.get("kind").and_then(Value::as_str) {
            Some("user_input") => {
                let content = input
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                if !content.is_empty() {
                    context.provider_messages.push(json!({
                        "role": "user",
                        "content": content
                    }));
                }
            }
            Some("user_reply") => {
                let resolved = context.emitter.emit(
                    "progress",
                    "已收到用户回答，继续处理。",
                    json!({
                        "trace_kind": "user_reply",
                        "ask_user_resolved": true,
                        "ask_user_tool_call_id": call_id
                    }),
                )?;
                context.events.push(resolved);
                context.pending_ask_user_call = None;
                context
                    .state
                    .database
                    .delete_resource("pending_input", &context.turn_id)?;
                context.state.database.update_turn_status(
                    &context.turn_id,
                    "running",
                    "",
                    "",
                    false,
                )?;
                return Ok(input);
            }
            _ => {}
        }
    }
}

fn emit_tool_events(
    context: &mut TurnContext,
    call: &llm::ToolCall,
    outcome: &tools::ToolOutcome,
) -> Result<(), String> {
    let call_id = call.id.clone();
    let mut call_metadata = json!({
        "args": call.arguments,
        "tool_name": call.name,
        "tool_call_id": call_id,
        "call_id": call_id,
        "phase": context.emitter.stage,
        "label": call.name,
        "call_kind": format!("tool_{}", call.name),
        "trace_id": call_id,
        "trace_role": "tool",
        "trace_group": "tool_call",
        "trace_kind": "tool_call",
        "call_state": "running",
        "tool_source": "builtin"
    });
    if let Some(provider) = outcome.metadata.get("provider").and_then(Value::as_str) {
        call_metadata["tool_provider"] = Value::String(provider.to_string());
    }
    let call_event = context
        .emitter
        .emit("tool_call", call.name.clone(), call_metadata)?;
    context.events.push(call_event);

    let result_event = context.emitter.emit(
        "tool_result",
        outcome.content.clone(),
        json!({
            "tool": call.name,
            "tool_name": call.name,
            "tool_call_id": call_id,
            "call_id": call_id,
            "phase": context.emitter.stage,
            "label": call.name,
            "call_kind": format!("tool_{}", call.name),
            "trace_id": call_id,
            "trace_role": "tool",
            "trace_group": "tool_call",
            "trace_kind": "tool_result",
            "call_state": if outcome.success { "complete" } else { "error" },
            "result_text": outcome.content,
            "tool_metadata": outcome.metadata,
            "tool_source": "builtin"
        }),
    )?;
    context.events.push(result_event);
    Ok(())
}

fn collect_tool_sources(context: &mut TurnContext, metadata: &Value) {
    let mut discovered = Vec::new();
    if let Some(sources) = metadata.get("sources").and_then(Value::as_array) {
        discovered.extend(sources.iter().cloned());
    }
    if let Some(results) = metadata.get("results").and_then(Value::as_array) {
        for item in results {
            if item.get("url").and_then(Value::as_str).is_some() {
                discovered.push(item.clone());
            }
        }
    }
    if discovered.is_empty() {
        return;
    }
    let mut seen = context
        .sources
        .iter()
        .filter_map(|item| item.get("url").and_then(Value::as_str).map(str::to_string))
        .collect::<BTreeSet<_>>();
    discovered.retain(|item| {
        item.get("url")
            .and_then(Value::as_str)
            .map(|url| seen.insert(url.to_string()))
            .unwrap_or(false)
    });
    if discovered.is_empty() {
        return;
    }
    context.sources.extend(discovered.clone());
    if let Ok(event) = context.emitter.emit(
        "sources",
        "",
        json!({
            "sources": discovered,
            "trace_kind": "sources"
        }),
    ) {
        context.events.push(event);
    }
}

fn collect_tool_attachments(context: &mut TurnContext, metadata: &Value) {
    let mut discovered = Vec::new();
    for key in ["artifacts", "workspace_items"] {
        for item in metadata
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if item.get("url").is_none()
                && item.get("relative_path").is_none()
                && item.get("path").is_none()
            {
                continue;
            }
            let mut normalized = item.clone();
            if normalized.get("relative_path").is_none() {
                if let Some(path) = normalized.get("path").and_then(Value::as_str) {
                    normalized["relative_path"] = Value::String(path.to_string());
                }
            }
            if normalized.get("type").is_none() {
                let mime = normalized
                    .get("mime_type")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                normalized["type"] = Value::String(if mime.starts_with("image/") {
                    "image".to_string()
                } else if mime.starts_with("video/") {
                    "video".to_string()
                } else {
                    "document".to_string()
                });
            }
            discovered.push(normalized);
        }
    }
    let mut seen = context
        .tool_attachments
        .iter()
        .filter_map(attachment_identity)
        .collect::<BTreeSet<_>>();
    discovered.retain(|item| {
        attachment_identity(item)
            .map(|identity| seen.insert(identity))
            .unwrap_or(false)
    });
    if discovered.is_empty() {
        return;
    }
    context.tool_attachments.extend(discovered.clone());
    if let Ok(event) = context.emitter.emit(
        "workspace_items",
        "",
        json!({
            "items": discovered,
            "trace_kind": "workspace_items"
        }),
    ) {
        context.events.push(event);
    }
}

fn dedupe_attachments(attachments: &mut Vec<Value>) {
    let mut seen = BTreeSet::new();
    attachments.retain(|item| {
        attachment_identity(item)
            .map(|identity| seen.insert(identity))
            .unwrap_or(false)
    });
}

fn attachment_identity(item: &Value) -> Option<String> {
    ["relative_path", "path", "url", "filename"]
        .iter()
        .find_map(|key| {
            item.get(*key)
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(|value| {
                    format!(
                        "{}:{value}",
                        item.get("type").and_then(Value::as_str).unwrap_or("file")
                    )
                })
        })
}

fn finalize_capability(
    context: &TurnContext,
    raw: &str,
    usage: &Value,
) -> Result<(String, Result<Value, String>, Vec<Value>), String> {
    match context.capability.as_str() {
        "deep_question" => finalize_question(context, raw, usage),
        "visualize" => finalize_visualize(context, raw, usage),
        "math_animator" => finalize_math_animator(context, raw, usage),
        "video_lesson" => finalize_video_lesson(context, raw, usage),
        "deep_research" => {
            let event = context.emitter.emit(
                "result",
                "",
                json!({
                    "response": raw,
                    "artifacts": context.tool_attachments,
                    "workspace_items": context.tool_attachments,
                    "metadata": {
                        "mode": "agentic_research",
                        "usage_summary": usage
                    }
                }),
            )?;
            Ok((raw.to_string(), Ok(event), Vec::new()))
        }
        "audio_overview" => {
            let mut payload = context
                .capability_result
                .clone()
                .ok_or_else(|| "音频概览结果没有生成。".to_string())?;
            payload["metadata"]["usage_summary"] = usage.clone();
            let display = context.emitter.emit(
                "content",
                raw,
                json!({
                    "call_kind": "llm_final_response",
                    "trace_kind": "llm_output"
                }),
            )?;
            let _ = display;
            let event = context.emitter.emit("result", "", payload)?;
            Ok((
                raw.to_string(),
                Ok(event),
                context.capability_attachments.clone(),
            ))
        }
        _ => {
            let mut payload = context
                .capability_result
                .clone()
                .unwrap_or_else(|| json!({}));
            if !payload.is_object() {
                payload = json!({});
            }
            if payload.get("response").is_none() {
                payload["response"] = Value::String(raw.to_string());
            }
            if payload.get("artifacts").is_none() {
                payload["artifacts"] = Value::Array(context.tool_attachments.clone());
            }
            if payload.get("workspace_items").is_none() {
                payload["workspace_items"] = Value::Array(context.tool_attachments.clone());
            }
            let metadata = payload
                .get_mut("metadata")
                .and_then(Value::as_object_mut)
                .map(|metadata| {
                    metadata
                        .entry("usage_summary")
                        .or_insert_with(|| usage.clone());
                });
            if metadata.is_none() {
                payload["metadata"] = json!({"usage_summary": usage});
            }
            let event = context.emitter.emit("result", "", payload)?;
            Ok((
                raw.to_string(),
                Ok(event),
                context.capability_attachments.clone(),
            ))
        }
    }
}

fn finalize_question(
    context: &TurnContext,
    raw: &str,
    usage: &Value,
) -> Result<(String, Result<Value, String>, Vec<Value>), String> {
    let parsed = parse_json_object(raw).ok_or_else(|| {
        "测验生成结果不是合法 JSON。请重试；模型必须只输出题目 JSON 对象。".to_string()
    })?;
    let questions = parsed
        .get("questions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if questions.is_empty() {
        return Err("测验生成结果中没有题目。".to_string());
    }
    let intro = parsed
        .get("intro")
        .and_then(Value::as_str)
        .unwrap_or("测验已生成。")
        .to_string();
    let total = questions.len();
    let mut results = Vec::with_capacity(total);
    for (index, mut question) in questions.into_iter().enumerate() {
        if question.get("question_id").is_none() {
            question["question_id"] = Value::String(format!("q_{}", index + 1));
        }
        let event = context.emitter.emit(
            "content",
            question
                .get("question")
                .and_then(Value::as_str)
                .unwrap_or(""),
            json!({
                "call_id": format!("{}-quiz-{}", context.turn_id, index + 1),
                "call_kind": "quiz_question_emitted",
                "trace_role": "quiz_question",
                "trace_group": "quizzing",
                "trace_kind": "quiz_question",
                "question_index": index,
                "total_questions": total,
                "qa_pair": question
            }),
        )?;
        let _ = event;
        results.push(json!({"qa_pair": question}));
    }
    let display_event = context.emitter.emit(
        "content",
        intro.clone(),
        json!({
            "call_kind": "llm_final_response",
            "trace_kind": "llm_output"
        }),
    )?;
    let result = context.emitter.emit(
        "result",
        "",
        json!({
            "response": intro,
            "metadata": {
                "summary": {
                    "results": results,
                    "num_questions": total
                },
                "usage_summary": usage
            }
        }),
    )?;
    let _ = display_event;
    Ok((intro, Ok(result), Vec::new()))
}

fn finalize_visualize(
    context: &TurnContext,
    raw: &str,
    usage: &Value,
) -> Result<(String, Result<Value, String>, Vec<Value>), String> {
    let parsed = parse_json_object(raw).ok_or_else(|| {
        "可视化结果不是合法 JSON。请重试并要求模型只输出单个 JSON 对象。".to_string()
    })?;
    let render_type = parsed
        .get("render_type")
        .and_then(Value::as_str)
        .unwrap_or("html");
    let code = parsed
        .get("code")
        .and_then(Value::as_str)
        .or_else(|| parsed.pointer("/payload/data").and_then(Value::as_str))
        .unwrap_or("");
    if code.trim().is_empty() {
        return Err("可视化结果缺少代码或数据载荷。".to_string());
    }
    let response = parsed
        .get("response")
        .and_then(Value::as_str)
        .unwrap_or("可视化已生成。")
        .to_string();
    let language = parsed
        .get("language")
        .and_then(Value::as_str)
        .unwrap_or(match render_type {
            "svg" => "svg",
            "chartjs" => "json",
            "mermaid" | "mindmap" => "mermaid",
            _ => "html",
        });
    let display = context.emitter.emit(
        "content",
        response.clone(),
        json!({
            "call_kind": "llm_final_response",
            "trace_kind": "llm_output"
        }),
    )?;
    let title = parsed
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("可视化");
    let description = parsed
        .get("description")
        .or_else(|| parsed.pointer("/analysis/description"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let alt_text = parsed
        .get("alt_text")
        .and_then(Value::as_str)
        .unwrap_or(title);
    let result = context.emitter.emit(
        "result",
        "",
        json!({
            "schema_version": "deeptutor.visualization/1",
            "response": response,
            "render_type": render_type,
            "renderer": {
                "id": render_type,
                "version": "1.0.0",
                "target": "native",
                "native_renderer": if render_type == "mindmap" { "mermaid" } else { render_type },
                "entry_url": ""
            },
            "payload": {
                "format": language,
                "data": code
            },
            "presentation": {
                "title": title,
                "description": description,
                "alt_text": alt_text,
                "aspect_ratio": ""
            },
            "interaction": {"events": []},
            "code": {"language": language, "content": code},
            "analysis": parsed.get("analysis").cloned().unwrap_or_else(|| json!({
                "render_type": render_type,
                "description": description,
                "visual_elements": []
            })),
            "review": {
                "changed": false,
                "review_notes": "Rust 原生输出已通过结构校验。"
            },
            "metadata": {"usage_summary": usage}
        }),
    )?;
    let _ = display;
    Ok((response, Ok(result), Vec::new()))
}

fn finalize_math_animator(
    context: &TurnContext,
    raw: &str,
    usage: &Value,
) -> Result<(String, Result<Value, String>, Vec<Value>), String> {
    let parsed = parse_json_object(raw).ok_or_else(|| "数学动画结果不是合法 JSON。".to_string())?;
    let response = parsed
        .get("response")
        .and_then(Value::as_str)
        .unwrap_or("数学动画已生成。")
        .to_string();
    let title = parsed
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("数学动画");
    let svg = parsed
        .get("svg")
        .and_then(Value::as_str)
        .or_else(|| parsed.get("html").and_then(Value::as_str))
        .unwrap_or("");
    if svg.trim().is_empty() {
        return Err("数学动画结果缺少 SVG。".to_string());
    }
    let filename = format!("{}.svg", safe_filename(&context.turn_id));
    let relative = write_output(&context.state, "math", &filename, svg.as_bytes())?;
    let artifact = json!({
        "type": "image",
        "url": format!("/files/outputs/{relative}"),
        "filename": filename,
        "content_type": "image/svg+xml",
        "label": title
    });
    let summary_text = parsed
        .get("summary")
        .and_then(Value::as_str)
        .unwrap_or(&response);
    let display = context.emitter.emit(
        "content",
        summary_text.to_string(),
        json!({
            "call_kind": "llm_final_response",
            "trace_kind": "llm_output"
        }),
    )?;
    let result = context.emitter.emit(
        "result",
        "",
        json!({
            "response": response,
            "output_mode": "image",
            "summary": {
                "summary_text": summary_text,
                "user_request": context.request.get("content"),
                "generated_output": title,
                "key_points": parsed.get("key_points").cloned().unwrap_or_else(|| json!([]))
            },
            "code": {"language": "svg", "content": svg},
            "artifacts": [artifact],
            "timings": {},
            "render": {
                "quality": "portable",
                "retry_attempts": 0,
                "retry_history": []
            },
            "metadata": {"usage_summary": usage}
        }),
    )?;
    let _ = display;
    Ok((
        response,
        Ok(result),
        vec![json!({
            "type": "image",
            "url": format!("/files/outputs/{relative}"),
            "filename": filename,
            "generated": true
        })],
    ))
}

fn finalize_video_lesson(
    context: &TurnContext,
    raw: &str,
    usage: &Value,
) -> Result<(String, Result<Value, String>, Vec<Value>), String> {
    let parsed = parse_json_object(raw).ok_or_else(|| "视频课件结果不是合法 JSON。".to_string())?;
    let response = parsed
        .get("response")
        .and_then(Value::as_str)
        .unwrap_or("视频课件已生成。")
        .to_string();
    let title = parsed
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("视频课件");
    let html = parsed.get("html").and_then(Value::as_str).unwrap_or("");
    if html.trim().is_empty() {
        return Err("视频课件结果缺少 HTML。".to_string());
    }
    let filename = format!("{}.html", safe_filename(&context.turn_id));
    let relative = write_output(&context.state, "lessons", &filename, html.as_bytes())?;
    let duration = parsed
        .get("duration_seconds")
        .and_then(Value::as_f64)
        .unwrap_or(60.0);
    let artifact = json!({
        "type": "html",
        "url": format!("/files/outputs/{relative}"),
        "filename": filename,
        "content_type": "text/html",
        "label": title
    });
    let display = context.emitter.emit(
        "content",
        response.clone(),
        json!({
            "call_kind": "llm_final_response",
            "trace_kind": "llm_output"
        }),
    )?;
    let result = context.emitter.emit(
        "result",
        "",
        json!({
            "response": response,
            "title": title,
            "artifacts": [artifact],
            "code": {"language": "html", "content": html},
            "render": {
                "attempts": [],
                "duration_seconds": duration,
                "narrated": true
            },
            "metadata": {"usage_summary": usage}
        }),
    )?;
    let _ = display;
    Ok((
        response,
        Ok(result),
        vec![json!({
            "type": "html",
            "url": format!("/files/outputs/{relative}"),
            "filename": filename,
            "generated": true
        })],
    ))
}

fn write_output(
    state: &AppState,
    category: &str,
    filename: &str,
    bytes: &[u8],
) -> Result<String, String> {
    let directory = state.paths.outputs.join(category);
    std::fs::create_dir_all(&directory).map_err(|error| format!("无法创建输出目录：{error}"))?;
    let path = directory.join(safe_filename(filename));
    std::fs::write(&path, bytes)
        .map_err(|error| format!("无法写入生成文件 {}：{error}", path.display()))?;
    Ok(format!(
        "{category}/{}",
        path.file_name().unwrap_or_default().to_string_lossy()
    ))
}

fn ensure_session(
    state: &AppState,
    request: &Value,
    content: &str,
    capability: &str,
) -> Result<(String, bool), String> {
    if let Some(session_id) = non_empty(request, "session_id") {
        let Some(session) = state.database.get_session_meta(session_id)? else {
            return Err(format!("会话不存在：{session_id}"));
        };
        let requested_workspace = request
            .get("workspace_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        let session_workspace = session
            .get("content_workspace_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        if session_workspace != requested_workspace {
            return Err("该会话属于另一个工作区。".to_string());
        }
        return Ok((session_id.to_string(), false));
    }
    let session_id = new_id("session");
    let title = session_title(content);
    let preferences = json!({
        "capability": capability,
        "language": request.get("language").cloned().unwrap_or_else(|| Value::String("zh".into())),
        "workspace_id": request.get("workspace_id").cloned().unwrap_or(Value::Null),
        "workspace_mode": request.get("workspace_mode").cloned().unwrap_or(Value::Null)
    });
    let workspace_id = request
        .get("workspace_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    state
        .database
        .create_session(&session_id, &title, &preferences, workspace_id)?;
    Ok((session_id, true))
}

fn build_provider_messages(
    state: &AppState,
    request: &Value,
    session_id: &str,
    content: &str,
    language: &str,
    source_manifest: &str,
) -> Result<Vec<Value>, String> {
    let capability = normalize_capability(
        state,
        request
            .get("capability")
            .and_then(Value::as_str)
            .unwrap_or("chat"),
    );
    let mut system = prompts::system_prompt(&capability, language);
    if let Some(custom_prompt) = non_empty(request, "system_prompt") {
        system = format!("{custom_prompt}\n\n{system}");
    }
    let extra_context = build_turn_context(state, request, session_id, source_manifest)?;
    let isolated_context = request
        .get("isolated_context")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut history = if isolated_context {
        Vec::new()
    } else {
        state.database.get_messages(session_id)?
    };
    if request
        .get("regenerate")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        if history
            .last()
            .and_then(|message| message.get("role"))
            .and_then(Value::as_str)
            == Some("assistant")
        {
            history.pop();
        }
    }

    let mut selected = Vec::<(String, Value)>::new();
    for message in history {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        if role != "user" && role != "assistant" {
            continue;
        }
        let text = message
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let content = provider_content(
            state,
            text,
            message.get("attachments").and_then(Value::as_array),
        );
        if content.as_str().is_some_and(str::is_empty)
            || content.as_array().is_some_and(Vec::is_empty)
        {
            continue;
        }
        selected.push((role.to_string(), content));
    }

    let current_text = if extra_context.trim().is_empty() {
        content.to_string()
    } else {
        format!("{content}\n\n[回合上下文]\n{extra_context}")
    };
    let current = provider_content(
        state,
        &current_text,
        request.get("attachments").and_then(Value::as_array),
    );
    let has_last_user = selected.last().is_some_and(|(role, _)| role == "user");
    if has_last_user {
        if let Some((_, message_content)) = selected.last_mut() {
            *message_content = current;
        }
    } else {
        selected.push(("user".to_string(), current));
    }

    let mut total = 0usize;
    let mut ordered = Vec::new();
    for (role, message_content) in selected.into_iter().rev() {
        let length = provider_content_text_len(&message_content);
        if total + length > MAX_CONTEXT_CHARS && !ordered.is_empty() {
            break;
        }
        total += length;
        ordered.push(json!({"role": role, "content": message_content}));
    }
    ordered.reverse();

    let mut messages = vec![json!({"role": "system", "content": system})];
    messages.extend(ordered);
    Ok(messages)
}

fn provider_content(state: &AppState, text: &str, attachments: Option<&Vec<Value>>) -> Value {
    let mut parts = Vec::new();
    if !text.is_empty() {
        parts.push(json!({"type": "text", "text": text}));
    }
    let mut image_bytes = 0usize;
    for attachment in attachments.into_iter().flatten() {
        let mime = attachment
            .get("mime_type")
            .and_then(Value::as_str)
            .unwrap_or("");
        let is_image = attachment.get("type").and_then(Value::as_str) == Some("image")
            || (mime.starts_with("image/") && mime != "image/svg+xml");
        if !is_image {
            continue;
        }
        let Some(bytes) = attachment_bytes(state, attachment) else {
            continue;
        };
        if bytes.is_empty() || image_bytes.saturating_add(bytes.len()) > 32 * 1024 * 1024 {
            continue;
        }
        image_bytes += bytes.len();
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        parts.push(json!({
            "type": "image_url",
            "image_url": {"url": format!("data:{mime};base64,{encoded}")}
        }));
    }
    if parts.len() == 1 {
        if let Some(text) = parts[0].get("text").cloned() {
            return text;
        }
    }
    Value::Array(parts)
}

fn attachment_bytes(state: &AppState, attachment: &Value) -> Option<Vec<u8>> {
    if let Some(base64) = attachment.get("base64").and_then(Value::as_str) {
        use base64::Engine;
        return base64::engine::general_purpose::STANDARD
            .decode(base64)
            .ok();
    }
    let relative = attachment
        .get("relative_path")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            attachment
                .get("url")
                .and_then(Value::as_str)
                .and_then(|url| url.strip_prefix("/files/attachments/").map(str::to_string))
        })?;
    let root = state.paths.uploads.join("attachments");
    let path = relative
        .split('/')
        .filter(|segment| !segment.is_empty() && *segment != "." && *segment != "..")
        .fold(root.clone(), |path, segment| path.join(segment));
    if !path.starts_with(&root) {
        return None;
    }
    std::fs::read(path).ok()
}

fn provider_content_text_len(content: &Value) -> usize {
    match content {
        Value::String(text) => text.chars().count(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .map(|text| text.chars().count())
            .sum(),
        _ => 0,
    }
}

fn build_turn_context(
    state: &AppState,
    request: &Value,
    session_id: &str,
    source_manifest: &str,
) -> Result<String, String> {
    let mut parts = Vec::new();
    if let Some(knowledge_bases) = request.get("knowledge_bases").and_then(Value::as_array) {
        let names = knowledge_bases
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();
        if !names.is_empty() {
            parts.push(format!(
                "本轮可检索知识库：{}。需要原文证据时调用 rag，逐字传入知识库名称。",
                names.join("、")
            ));
        }
    }
    if let Some(persona) = non_empty(request, "persona") {
        if let Some(value) = state.database.get_resource("persona", persona)? {
            if let Some(prompt) = value
                .get("system_prompt")
                .or_else(|| value.get("prompt"))
                .or_else(|| value.get("description"))
                .and_then(Value::as_str)
            {
                parts.push(format!("当前人格设定：{prompt}"));
            }
        }
    }
    if let Some(config) = request.get("config").filter(|value| value.is_object()) {
        parts.push(format!(
            "本轮能力参数（必须严格遵守）：{}",
            serde_json::to_string_pretty(config).unwrap_or_default()
        ));
    }
    if let Some(followup) = request
        .get("followup_question_context")
        .filter(|value| value.is_object())
    {
        parts.push(format!(
            "测验跟进上下文：{}",
            serde_json::to_string_pretty(followup).unwrap_or_default()
        ));
    }
    if !source_manifest.trim().is_empty() {
        parts.push(source_manifest.to_string());
    }
    if let Ok(journal) = super::learning_journal::load(state) {
        let journal_context = super::learning_journal::injection_markdown(&journal);
        if !journal_context.trim().is_empty() {
            parts.push(journal_context);
        }
    }
    if !session_id.is_empty() {
        if let Ok(Some(session)) = state.database.get_session_meta(session_id) {
            if let Some(workspace) = session
                .get("preferences")
                .and_then(|value| value.get("workspace_mode"))
                .and_then(Value::as_str)
            {
                if !workspace.is_empty() {
                    parts.push(format!("当前工作区模式：{workspace}"));
                }
            }
        }
    }
    Ok(parts.join("\n\n"))
}

fn append_resource_context(
    state: &AppState,
    parts: &mut Vec<String>,
    id: Option<&str>,
    domains: &[&str],
    label: &str,
) -> Result<(), String> {
    let Some(id) = id.filter(|value| !value.trim().is_empty()) else {
        return Ok(());
    };
    for domain in domains {
        if let Some(value) = state.database.get_resource(domain, id)? {
            let text = extract_resource_text(&value);
            if !text.trim().is_empty() {
                parts.push(format!("{label} {id}：\n{}", truncate(&text, 24000)));
                return Ok(());
            }
        }
    }
    Ok(())
}

fn extract_resource_text(value: &Value) -> String {
    for pointer in [
        "/content",
        "/text",
        "/markdown",
        "/body",
        "/description",
        "/summary",
        "/payload/render/content",
    ] {
        if let Some(text) = value.pointer(pointer).and_then(Value::as_str) {
            return text.to_string();
        }
    }
    truncated_json(value, 12000)
}

fn attachment_context(attachment: &Value) -> Option<String> {
    let filename = attachment
        .get("filename")
        .and_then(Value::as_str)
        .unwrap_or("附件");
    let mime = attachment
        .get("mime_type")
        .and_then(Value::as_str)
        .unwrap_or("");
    if let Some(text) = attachment
        .get("extracted_text")
        .or_else(|| attachment.get("text"))
        .and_then(Value::as_str)
    {
        return Some(format!("附件 {filename}：\n{}", truncate(text, 20000)));
    }
    if let Some(base64) = attachment.get("base64").and_then(Value::as_str) {
        if mime.starts_with("text/") || mime == "application/json" {
            use base64::Engine;
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(base64) {
                if let Ok(text) = String::from_utf8(bytes) {
                    return Some(format!("附件 {filename}：\n{}", truncate(&text, 20000)));
                }
            }
        }
    }
    Some(format!(
        "附件 {filename}（{mime}）已随回合上传；仅在其文本已可读取时引用其内容。"
    ))
}

async fn prepare_attachments(
    state: &AppState,
    session_id: &str,
    request: &mut Value,
) -> Result<Vec<AttachmentProgress>, String> {
    let Some(attachments) = request
        .get("attachments")
        .and_then(Value::as_array)
        .cloned()
    else {
        return Ok(Vec::new());
    };
    if attachments.is_empty() {
        return Ok(Vec::new());
    }

    let mut prepared = Vec::with_capacity(attachments.len());
    for (index, attachment) in attachments.into_iter().enumerate() {
        let id = attachment
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("att-{}", Uuid::new_v4()));
        let filename = sanitize_attachment_filename(
            attachment
                .get("filename")
                .and_then(Value::as_str)
                .unwrap_or("attachment.bin"),
            index,
        );
        let mime_type = attachment
            .get("mime_type")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                mime_guess::from_path(&filename)
                    .first_or_octet_stream()
                    .to_string()
            });
        let base64 = attachment.get("base64").and_then(Value::as_str);
        if base64.is_none() {
            prepared.push((index, id, filename, mime_type, None, None, attachment));
            continue;
        }
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(base64.unwrap_or_default())
            .map_err(|error| format!("附件 {filename} 不是有效的 Base64 数据：{error}"))?;
        if bytes.len() > 200 * 1024 * 1024 {
            return Err(format!("附件 {filename} 超过 200 MB 上限。"));
        }
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        let parsed = super::parsing::extract(state, &filename, &bytes)
            .await
            .map_err(|error| format!("附件 {filename} 解析失败：{error}"));
        let session = session_id.to_string();
        let id_for_write = id.clone();
        let filename_for_write = filename.clone();
        let root = state.paths.uploads.join("attachments");
        let size = tokio::task::spawn_blocking(move || {
            let directory = root.join(&session).join(&id_for_write);
            std::fs::create_dir_all(&directory)
                .map_err(|error| format!("无法创建附件目录 {}：{error}", directory.display()))?;
            let path = directory.join(&filename_for_write);
            std::fs::write(&path, &bytes)
                .map_err(|error| format!("无法保存附件 {filename_for_write}：{error}"))?;
            Ok::<_, String>(bytes.len() as i64)
        })
        .await
        .map_err(|error| format!("附件保存任务失败：{error}"))??;
        prepared.push((
            index,
            id,
            filename,
            mime_type,
            Some(sha256),
            Some((parsed, size)),
            attachment,
        ));
    }

    let mut enriched = Vec::with_capacity(prepared.len());
    let mut progress = Vec::new();
    for (index, id, filename, mime_type, sha256, extracted, mut attachment) in prepared {
        let relative_path = format!("{session_id}/{id}/{filename}");
        let (size_bytes, extracted_text, parse_result) = if let Some((parsed, size)) = extracted {
            match parsed {
                Ok(document) => (
                    size,
                    Some(document.text),
                    ("completed", "文档文本已提取并加入本轮上下文。".to_string()),
                ),
                Err(error) => (
                    size,
                    None,
                    ("fallback", format!("附件已保存，但未能提取文本：{error}")),
                ),
            }
        } else {
            (
                attachment
                    .get("size_bytes")
                    .and_then(Value::as_i64)
                    .unwrap_or(0),
                attachment
                    .get("extracted_text")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                ("received", "附件已接收。".to_string()),
            )
        };
        let sha256 = sha256.unwrap_or_else(|| {
            format!(
                "{:x}",
                Sha256::digest(
                    attachment
                        .get("base64")
                        .and_then(Value::as_str)
                        .unwrap_or(filename.as_str())
                        .as_bytes()
                )
            )
        });
        attachment["id"] = Value::String(id.clone());
        attachment["filename"] = Value::String(filename.clone());
        attachment["mime_type"] = Value::String(mime_type.clone());
        attachment["size_bytes"] = Value::from(size_bytes);
        attachment["sha256"] = Value::String(sha256.clone());
        attachment["url"] = Value::String(format!("/files/attachments/{relative_path}"));
        attachment["relative_path"] = Value::String(relative_path.clone());
        if let Some(text) = &extracted_text {
            attachment["extracted_text"] = Value::String(text.clone());
        }
        attachment
            .as_object_mut()
            .map(|object| object.remove("base64"));

        if size_bytes > 0 {
            state
                .database
                .add_attachment(crate::storage::NewAttachment {
                    id: &id,
                    session_id,
                    filename: &filename,
                    mime_type: &mime_type,
                    relative_path: &relative_path,
                    size_bytes,
                    sha256: &sha256,
                    metadata: &json!({
                        "attachment_id": id,
                        "index": index,
                        "type": attachment.get("type").cloned().unwrap_or(Value::Null),
                        "extracted_text": extracted_text
                    }),
                })?;
        }
        progress.push(AttachmentProgress {
            attachment_id: id,
            filename,
            phase: parse_result.0,
            detail: parse_result.1,
        });
        enriched.push(attachment);
    }
    request["attachments"] = Value::Array(enriched);
    Ok(progress)
}

fn sanitize_attachment_filename(value: &str, index: usize) -> String {
    let base = Path::new(value)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("attachment.bin")
        .chars()
        .map(|character| {
            if matches!(
                character,
                '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0'
            ) {
                '_'
            } else {
                character
            }
        })
        .take(180)
        .collect::<String>();
    if base.trim().is_empty() || base == "." || base == ".." {
        format!("attachment-{index}.bin")
    } else {
        base
    }
}

pub(super) fn resolve_llm_settings(
    state: &AppState,
    request: &Value,
) -> Result<LlmSettings, String> {
    let catalog = super::catalog::resolve_catalog_connections(
        &state
            .database
            .get_setting("catalog")?
            .unwrap_or_else(|| json!({})),
    );
    let service = catalog
        .pointer("/services/llm")
        .ok_or_else(|| "尚未配置语言模型。请先打开设置 → 语言模型添加服务。".to_string())?;
    let requested_profile = request
        .pointer("/llm_selection/profile_id")
        .and_then(Value::as_str);
    let requested_model = request
        .pointer("/llm_selection/model_id")
        .and_then(Value::as_str);
    let profiles = service
        .get("profiles")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let active_profile_id = service.get("active_profile_id").and_then(Value::as_str);
    let profile = profiles
        .iter()
        .find(|profile| {
            requested_profile.is_some()
                && profile.get("id").and_then(Value::as_str) == requested_profile
        })
        .or_else(|| {
            profiles.iter().find(|profile| {
                profile.get("id").and_then(Value::as_str) == active_profile_id
                    && profile.get("provider_only").and_then(Value::as_bool) != Some(true)
            })
        })
        .or_else(|| {
            profiles.iter().find(|profile| {
                profile
                    .get("models")
                    .and_then(Value::as_array)
                    .is_some_and(|models| !models.is_empty())
            })
        })
        .ok_or_else(|| "语言模型服务没有可用配置。".to_string())?;
    let models = profile
        .get("models")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let active_model_id = service.get("active_model_id").and_then(Value::as_str);
    let model = models
        .iter()
        .find(|model| {
            requested_model.is_some() && model.get("id").and_then(Value::as_str) == requested_model
        })
        .or_else(|| {
            models
                .iter()
                .find(|model| model.get("id").and_then(Value::as_str) == active_model_id)
        })
        .or_else(|| models.first())
        .ok_or_else(|| "当前模型服务没有配置模型。".to_string())?;
    let provider = profile
        .get("binding")
        .or_else(|| profile.get("provider"))
        .and_then(Value::as_str)
        .unwrap_or("openai")
        .to_string();
    let base_url = profile
        .get("base_url")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| default_base_url(&provider).to_string());
    let model_name = model
        .get("model")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .or_else(|| model.get("name").and_then(Value::as_str))
        .unwrap_or("")
        .to_string();
    if model_name.is_empty() {
        return Err("当前模型缺少模型标识。".to_string());
    }
    let headers = profile
        .get("extra_headers")
        .and_then(Value::as_object)
        .map(|headers| {
            headers
                .iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|value| (key.clone(), value.to_string()))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(LlmSettings {
        provider,
        base_url,
        api_key: profile
            .get("api_key")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        model: model_name,
        api_format: ApiFormat::from_catalog(
            profile.get("api_format").and_then(Value::as_str),
            profile.get("wire_api").and_then(Value::as_str),
        ),
        headers,
    })
}

fn default_base_url(provider: &str) -> &'static str {
    match provider.to_ascii_lowercase().as_str() {
        "anthropic" | "claude" => "https://api.anthropic.com/v1",
        "deepseek" => "https://api.deepseek.com/v1",
        "dashscope" | "aliyun" | "bailian" => "https://dashscope.aliyuncs.com/compatible-mode/v1",
        "openrouter" => "https://openrouter.ai/api/v1",
        "groq" => "https://api.groq.com/openai/v1",
        "xai" | "grok" => "https://api.x.ai/v1",
        "moonshot" | "kimi" => "https://api.moonshot.cn/v1",
        "zhipu" | "glm" => "https://open.bigmodel.cn/api/paas/v4",
        "siliconflow" => "https://api.siliconflow.cn/v1",
        "ollama" => "http://127.0.0.1:11434/v1",
        "lmstudio" => "http://127.0.0.1:1234/v1",
        "gemini" | "google" => "https://generativelanguage.googleapis.com/v1beta/openai",
        _ => "https://api.openai.com/v1",
    }
}

fn enabled_tools(state: &AppState, request: &Value) -> Result<Vec<String>, String> {
    if let Some(requested) = request.get("enabled_tools").and_then(Value::as_array) {
        return Ok(requested
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect());
    }
    Ok(state
        .database
        .get_setting("enabled_tools")?
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|value| value.as_str().map(str::to_string))
        .collect())
}

fn normalize_capability(state: &AppState, value: &str) -> String {
    let value = value.trim();
    if CAPABILITIES.contains(&value) {
        value.to_string()
    } else if plugins::plugin_capability_names(state)
        .map(|names| names.iter().any(|name| name == value))
        .unwrap_or(false)
    {
        value.to_string()
    } else {
        "chat".to_string()
    }
}

fn initial_stage(capability: &str) -> &'static str {
    match capability {
        "deep_research" => "researching",
        "visualize" => "generating",
        "math_animator" | "video_lesson" => "code_generation",
        "deep_question" => "generation",
        "audio_overview" => "retrieving",
        "chat" => "exploring",
        _ => "responding",
    }
}

fn is_structured_capability(capability: &str) -> bool {
    matches!(
        capability,
        "deep_question" | "visualize" | "math_animator" | "video_lesson" | "audio_overview"
    )
}

fn format_user_reply(reply: &Value) -> String {
    let text = reply
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let answers = reply.get("answers").cloned().unwrap_or(Value::Null);
    if answers.is_null() {
        text.to_string()
    } else if text.is_empty() {
        format!(
            "用户回答：{}",
            serde_json::to_string(&answers).unwrap_or_default()
        )
    } else {
        format!(
            "用户补充说明：{text}\n结构化回答：{}",
            serde_json::to_string(&answers).unwrap_or_default()
        )
    }
}

fn parse_json_object(raw: &str) -> Option<Value> {
    let trimmed = raw.trim();
    let without_fence = if trimmed.starts_with("```") {
        let content = trimmed
            .trim_start_matches("```json")
            .trim_start_matches("```JSON")
            .trim_start_matches("```");
        content.trim_end_matches("```").trim()
    } else {
        trimmed
    };
    if let Ok(value) = serde_json::from_str::<Value>(without_fence) {
        if value.is_object() {
            return Some(value);
        }
    }
    let start = without_fence.find('{')?;
    let end = without_fence.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str::<Value>(&without_fence[start..=end])
        .ok()
        .filter(Value::is_object)
}

fn non_empty<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// 生成跨进程稳定的请求摘要。
///
/// 会话和工作区属于传输绑定，不参与内容一致性判断；协议外壳字段也不应
/// 让同一份用户输入在重试时被视为不同请求。
fn submission_digest(request: &Value) -> Result<String, String> {
    let mut identity = request.clone();
    if let Some(object) = identity.as_object_mut() {
        for key in ["session_id", "workspace_id", "type", "protocol_version"] {
            object.remove(key);
        }
    }
    let encoded =
        serde_json::to_vec(&identity).map_err(|error| format!("无法计算提交摘要：{error}"))?;
    Ok(format!("{:x}", Sha256::digest(encoded)))
}

fn new_id(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
}

fn session_title(content: &str) -> String {
    let first_line = content
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    let title = first_line.trim().chars().take(48).collect::<String>();
    if title.is_empty() {
        "新对话".to_string()
    } else {
        title
    }
}

fn safe_filename(value: &str) -> String {
    let cleaned = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if cleaned.trim_matches(['.', '_']).is_empty() {
        format!("artifact-{}", Uuid::new_v4().simple())
    } else {
        cleaned
    }
}

fn truncate(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_string();
    }
    let mut output = value.chars().take(limit).collect::<String>();
    output.push_str("\n…（内容已截断）");
    output
}

fn truncated_json(value: &Value, limit: usize) -> String {
    truncate(
        &serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()),
        limit,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::portable::PortablePaths;
    use crate::storage::Database;
    use axum::body::Body;
    use axum::http::{header, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::post;
    use axum::Router;
    use std::path::PathBuf;
    use tempfile::TempDir;
    use tokio::net::TcpListener;

    fn test_paths(root: PathBuf) -> PortablePaths {
        PortablePaths {
            home: root.join("home"),
            data: root.join("data"),
            cache: root.join("cache"),
            temp: root.join("temp"),
            logs: root.join("logs"),
            webview: root.join("cache/webview2"),
            uploads: root.join("data/uploads"),
            outputs: root.join("data/outputs"),
            database: root.join("data/deeptutor.sqlite3"),
            root,
        }
    }

    fn test_state() -> (AppState, TempDir) {
        let temporary = tempfile::tempdir().expect("创建测试目录");
        let paths = test_paths(temporary.path().to_path_buf());
        let database = Database::new(&paths.database);
        database.initialize().expect("初始化测试数据库");
        let state = AppState::new(database, paths).expect("创建测试应用状态");
        (state, temporary)
    }

    fn create_session(state: &AppState) -> String {
        let session_id = "session-test".to_string();
        state
            .database
            .create_session(&session_id, "测试会话", &json!({}), "")
            .expect("创建测试会话");
        session_id
    }

    fn configure_mock_model(state: &AppState, base_url: String) {
        state
            .database
            .set_setting(
                "catalog",
                &json!({
                    "services": {
                        "llm": {
                            "active_profile_id": "profile-test",
                            "active_model_id": "model-test",
                            "profiles": [{
                                "id": "profile-test",
                                "binding": "openai",
                                "base_url": base_url,
                                "api_format": "openai_chat",
                                "models": [{
                                    "id": "model-test",
                                    "model": "mock-model"
                                }]
                            }]
                        }
                    }
                }),
            )
            .expect("写入测试模型配置");
    }

    #[test]
    fn linked_connection_address_overrides_stale_chat_address() {
        let (state, _temporary) = test_state();
        state
            .database
            .set_setting(
                "catalog",
                &json!({
                    "connections": [{
                        "id": "conn-tju",
                        "api_key": "linked-key",
                        "base_url": "https://gateway.example/v1"
                    }],
                    "services": {
                        "llm": {
                            "active_profile_id": "profile-test",
                            "active_model_id": "model-test",
                            "profiles": [{
                                "id": "profile-test",
                                "connection_id": "conn-tju",
                                "binding": "openai",
                                "base_url": "https://api.openai.com/v1",
                                "api_format": "openai_chat",
                                "models": [{
                                    "id": "model-test",
                                    "model": "deepseek-v4.1-flash"
                                }]
                            }]
                        }
                    }
                }),
            )
            .expect("写入连接测试配置");

        let settings = resolve_llm_settings(&state, &json!({})).expect("解析聊天模型设置");
        assert_eq!(settings.base_url, "https://gateway.example/v1");
        assert_eq!(settings.api_key, "linked-key");
    }

    async fn start_mock_openai() -> (String, tokio::task::JoinHandle<()>) {
        async fn completion() -> impl IntoResponse {
            let stream = concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"你好，\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"这是 Rust 引擎。\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],",
                "\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":8,\"total_tokens\":20}}\n\n",
                "data: [DONE]\n\n"
            );
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/event-stream")],
                Body::from(stream),
            )
        }

        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("绑定模拟模型端口");
        let address = listener.local_addr().expect("读取模拟模型地址");
        let app = Router::new().route("/v1/chat/completions", post(completion));
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{address}/v1"), task)
    }

    #[tokio::test]
    async fn 缺少模型配置时不会留下活动回合() {
        let (state, _temporary) = test_state();
        let session_id = create_session(&state);
        let error = start_turn(
            state.clone(),
            json!({
                "protocol_version": "2.0",
                "session_id": session_id,
                "content": "你好",
                "capability": "chat"
            }),
        )
        .await
        .expect_err("缺少模型配置时应失败");

        assert!(error.contains("语言模型"));
        assert!(state
            .database
            .get_active_turn("session-test")
            .expect("读取活动回合")
            .is_none());
        assert!(state
            .database
            .get_messages("session-test")
            .expect("读取消息")
            .is_empty());
    }

    #[tokio::test]
    async fn 原生流式回合会持久化正文结果和完成事件() {
        let (state, _temporary) = test_state();
        let session_id = create_session(&state);
        let (base_url, mock_task) = start_mock_openai().await;
        configure_mock_model(&state, base_url);
        let mut events = state.turn_events.subscribe();

        let receipt = start_turn(
            state.clone(),
            json!({
                "protocol_version": "2.0",
                "session_id": session_id,
                "content": "测试流式输出",
                "capability": "chat"
            }),
        )
        .await
        .expect("启动测试回合");
        let turn_id = receipt["turn_id"].as_str().expect("回合 ID");

        let done = tokio::time::timeout(std::time::Duration::from_secs(8), async {
            loop {
                let event = events.recv().await.expect("读取实时事件");
                if event.get("turn_id").and_then(Value::as_str) == Some(turn_id)
                    && event.get("type").and_then(Value::as_str) == Some("done")
                {
                    break event;
                }
            }
        })
        .await
        .expect("等待回合完成超时");

        assert_eq!(
            done.pointer("/metadata/status").and_then(Value::as_str),
            Some("completed")
        );
        let persisted = state
            .database
            .get_turn_events(turn_id, 0)
            .expect("读取持久化事件");
        assert!(persisted
            .iter()
            .any(|event| event.get("type").and_then(Value::as_str) == Some("session")));
        assert!(persisted
            .iter()
            .any(|event| event.get("type").and_then(Value::as_str) == Some("result")));
        assert_eq!(
            persisted
                .iter()
                .find(|event| event.get("type").and_then(Value::as_str) == Some("done"))
                .and_then(|event| event.pointer("/metadata/status"))
                .and_then(Value::as_str),
            Some("completed")
        );

        let messages = state
            .database
            .get_messages(&session_id)
            .expect("读取持久化消息");
        assert_eq!(messages.len(), 2);
        assert_eq!(
            messages[1].get("role").and_then(Value::as_str),
            Some("assistant")
        );
        assert_eq!(
            messages[1].get("content").and_then(Value::as_str),
            Some("你好，这是 Rust 引擎。")
        );
        mock_task.abort();
    }
}
