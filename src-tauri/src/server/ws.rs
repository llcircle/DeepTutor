use super::ScopedState;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use tokio::sync::broadcast::error::RecvError;

use super::{engine, AppState};

const PROTOCOL_VERSION: &str = "2.0";
type SocketSender = SplitSink<WebSocket, Message>;

#[derive(Default)]
struct Subscriptions {
    /// 每个回合已经发送给客户端的最大事件序号。
    turns: HashMap<String, u64>,
    /// 会话订阅用于接收该会话后续创建的回合。
    sessions: HashSet<String>,
}

impl Subscriptions {
    fn subscribe_turn(&mut self, turn_id: &str, after_seq: u64) {
        self.turns
            .entry(turn_id.to_string())
            .and_modify(|cursor| *cursor = (*cursor).max(after_seq))
            .or_insert(after_seq);
    }

    fn cursor(&self, turn_id: &str) -> u64 {
        self.turns.get(turn_id).copied().unwrap_or(0)
    }

    fn accepts(&self, event: &Value) -> bool {
        let Some(turn_id) = event.get("turn_id").and_then(Value::as_str) else {
            return false;
        };
        if self.turns.contains_key(turn_id) {
            return true;
        }
        event
            .get("session_id")
            .and_then(Value::as_str)
            .is_some_and(|session_id| self.sessions.contains(session_id))
    }
}

/// 统一回合协议入口。
pub async fn websocket(ws: WebSocketUpgrade, ScopedState(state): ScopedState) -> impl IntoResponse {
    ws.max_message_size(64 * 1024 * 1024)
        .max_frame_size(16 * 1024 * 1024)
        .on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    let (mut sender, mut receiver) = socket.split();
    let mut live_events = state.turn_events.subscribe();
    let mut subscriptions = Subscriptions::default();

    loop {
        tokio::select! {
            incoming = receiver.next() => {
                let Some(incoming) = incoming else {
                    break;
                };
                let message = match incoming {
                    Ok(message) => message,
                    Err(error) => {
                        tracing::debug!(%error, "客户端 WebSocket 读取失败");
                        break;
                    }
                };

                match message {
                    Message::Text(text) => {
                        if !handle_command(&mut sender, &state, &mut subscriptions, &text).await {
                            break;
                        }
                    }
                    Message::Binary(_) => {
                        if send_protocol_error(
                            &mut sender,
                            "binary_not_supported",
                            "回合协议只接受 UTF-8 JSON 文本帧。",
                            false,
                            "",
                            "",
                        )
                        .await
                        .is_err()
                        {
                            break;
                        }
                    }
                    Message::Ping(value) => {
                        if sender.send(Message::Pong(value)).await.is_err() {
                            break;
                        }
                    }
                    Message::Pong(_) => {}
                    Message::Close(_) => break,
                }
            }
            event = live_events.recv() => {
                match event {
                    Ok(event) => {
                        if let Err(error) = deliver_live_event(
                            &mut sender,
                            &state,
                            &mut subscriptions,
                            event,
                        )
                        .await
                        {
                            tracing::debug!(%error, "客户端 WebSocket 写入失败");
                            break;
                        }
                    }
                    Err(RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "WebSocket 实时事件积压，改用 SQLite 回放");
                        if replay_all_subscriptions(
                            &mut sender,
                            &state,
                            &mut subscriptions,
                        )
                        .await
                        .is_err()
                        {
                            break;
                        }
                    }
                    Err(RecvError::Closed) => {
                        tracing::debug!("回合实时事件通道已关闭");
                    }
                }
            }
        }
    }
}

async fn handle_command(
    sender: &mut SocketSender,
    state: &AppState,
    subscriptions: &mut Subscriptions,
    raw: &str,
) -> bool {
    let command = match serde_json::from_str::<Value>(raw) {
        Ok(command) if command.is_object() => command,
        Ok(_) => {
            return send_protocol_error(
                sender,
                "invalid_command",
                "WebSocket 命令必须是 JSON 对象。",
                false,
                "",
                "",
            )
            .await
            .is_ok();
        }
        Err(_) => {
            return send_protocol_error(
                sender,
                "invalid_json",
                "WebSocket 命令必须是合法 JSON。",
                false,
                "",
                "",
            )
            .await
            .is_ok();
        }
    };

    if command.get("protocol_version").and_then(Value::as_str) != Some(PROTOCOL_VERSION) {
        return send_protocol_error(
            sender,
            "unsupported_protocol_version",
            "缺少或不支持的 protocol_version，应为 2.0。",
            false,
            command
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or(""),
            command.get("turn_id").and_then(Value::as_str).unwrap_or(""),
        )
        .await
        .is_ok();
    }

    let command_type = command
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_else(|| {
            if command.get("content").is_some() {
                "start_turn"
            } else {
                ""
            }
        });

    let outcome = match command_type {
        "message" | "start_turn" => start_turn(sender, state, subscriptions, command).await,
        "subscribe_turn" => subscribe_turn_command(sender, state, subscriptions, &command).await,
        "resume_from" => {
            let after_seq = command
                .get("seq")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            let mut normalized = command.clone();
            normalized["after_seq"] = Value::from(after_seq);
            subscribe_turn_command(sender, state, subscriptions, &normalized).await
        }
        "subscribe_session" => {
            subscribe_session_command(sender, state, subscriptions, &command).await
        }
        "unsubscribe" => {
            let turn_id = command.get("turn_id").and_then(Value::as_str).unwrap_or("");
            let session_id = command
                .get("session_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            if turn_id.is_empty() && session_id.is_empty() {
                send_protocol_error(
                    sender,
                    "missing_subscription",
                    "unsubscribe 至少需要 turn_id 或 session_id。",
                    false,
                    "",
                    "",
                )
                .await
            } else {
                if !turn_id.is_empty() {
                    subscriptions.turns.remove(turn_id);
                }
                if !session_id.is_empty() {
                    subscriptions.sessions.remove(session_id);
                }
                Ok(())
            }
        }
        "cancel_turn" => cancel_turn_command(sender, state, &command).await,
        "regenerate" => regenerate_command(sender, state, subscriptions, command).await,
        "submit_user_reply" => submit_reply_command(sender, state, &command).await,
        "user_input" => user_input_command(sender, state, &command).await,
        "check_active_turn" => check_active_turn_command(sender, state, &command).await,
        "ping" => {
            send_value(
                sender,
                json!({
                    "type": "pong",
                    "protocol_version": PROTOCOL_VERSION
                }),
            )
            .await
        }
        _ => {
            send_protocol_error(
                sender,
                "unknown_message_type",
                "未知的 WebSocket 命令类型。",
                false,
                command
                    .get("session_id")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                command.get("turn_id").and_then(Value::as_str).unwrap_or(""),
            )
            .await
        }
    };

    if let Err(error) = outcome {
        tracing::debug!(%error, "处理 WebSocket 命令失败");
        return false;
    }
    true
}

async fn start_turn(
    sender: &mut SocketSender,
    state: &AppState,
    subscriptions: &mut Subscriptions,
    command: Value,
) -> Result<(), String> {
    match engine::start_turn(state.clone(), command).await {
        Ok(receipt) => {
            let turn_id = receipt
                .get("turn_id")
                .and_then(Value::as_str)
                .ok_or_else(|| "Rust 回合引擎未返回 turn_id。".to_string())?;
            subscriptions.subscribe_turn(turn_id, 0);
            replay_turn(sender, state, subscriptions, turn_id).await
        }
        Err(error) => {
            send_protocol_error(sender, "start_turn_rejected", &error, true, "", "").await
        }
    }
}

async fn regenerate_command(
    sender: &mut SocketSender,
    state: &AppState,
    subscriptions: &mut Subscriptions,
    command: Value,
) -> Result<(), String> {
    let session_id = command
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "regenerate 缺少 session_id。".to_string())?;
    let overrides = command
        .get("overrides")
        .filter(|value| value.is_object())
        .cloned();
    match engine::regenerate(state.clone(), session_id, overrides).await {
        Ok(receipt) => {
            let turn_id = receipt
                .get("turn_id")
                .and_then(Value::as_str)
                .ok_or_else(|| "Rust 回合引擎未返回 turn_id。".to_string())?;
            subscriptions.subscribe_turn(turn_id, 0);
            replay_turn(sender, state, subscriptions, turn_id).await
        }
        Err(error) => {
            send_protocol_error(sender, "regenerate_rejected", &error, true, session_id, "").await
        }
    }
}

async fn subscribe_turn_command(
    sender: &mut SocketSender,
    state: &AppState,
    subscriptions: &mut Subscriptions,
    command: &Value,
) -> Result<(), String> {
    let Some(turn_id) = command
        .get("turn_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return send_protocol_error(
            sender,
            "missing_turn_id",
            "订阅回合时缺少 turn_id。",
            false,
            "",
            "",
        )
        .await;
    };
    if state
        .database
        .get_turn(turn_id)
        .map_err(|error| format!("无法查询回合：{error}"))?
        .is_none()
    {
        return send_protocol_error(
            sender,
            "turn_not_found",
            "找不到指定回合。",
            false,
            "",
            turn_id,
        )
        .await;
    }
    let after_seq = command
        .get("after_seq")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    subscriptions.subscribe_turn(turn_id, after_seq);
    replay_turn(sender, state, subscriptions, turn_id).await
}

async fn subscribe_session_command(
    sender: &mut SocketSender,
    state: &AppState,
    subscriptions: &mut Subscriptions,
    command: &Value,
) -> Result<(), String> {
    let Some(session_id) = command
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return send_protocol_error(
            sender,
            "missing_session_id",
            "订阅会话时缺少 session_id。",
            false,
            "",
            "",
        )
        .await;
    };
    if state
        .database
        .get_session_meta(session_id)
        .map_err(|error| format!("无法查询会话：{error}"))?
        .is_none()
    {
        return send_protocol_error(
            sender,
            "session_not_found",
            "找不到指定会话。",
            false,
            session_id,
            "",
        )
        .await;
    }
    let after_seq = command
        .get("after_seq")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    subscriptions.sessions.insert(session_id.to_string());
    replay_session(sender, state, subscriptions, session_id, after_seq).await
}

async fn cancel_turn_command(
    sender: &mut SocketSender,
    state: &AppState,
    command: &Value,
) -> Result<(), String> {
    let Some(command_id) = required_command_id(sender, command).await? else {
        return Ok(());
    };
    let Some(turn_id) = required_turn_id(sender, command, "取消回合").await? else {
        return Ok(());
    };
    let accepted = state.cancel_turn(&turn_id);
    let (error_code, message) = if accepted {
        ("", "")
    } else {
        ("turn_not_active", "该回合当前不处于可取消的运行状态。")
    };
    send_command_ack(
        sender,
        &command_id,
        "cancel_turn",
        accepted,
        &turn_id,
        error_code,
        message,
    )
    .await
}

async fn submit_reply_command(
    sender: &mut SocketSender,
    state: &AppState,
    command: &Value,
) -> Result<(), String> {
    let Some(command_id) = required_command_id(sender, command).await? else {
        return Ok(());
    };
    let Some(turn_id) = required_turn_id(sender, command, "提交回答").await? else {
        return Ok(());
    };
    let text = command
        .get("text")
        .and_then(Value::as_str)
        .map(str::to_string);
    let answers = normalize_answers(command.get("answers"));
    if text.as_deref().unwrap_or("").trim().is_empty() && answers.is_none() {
        send_command_ack(
            sender,
            &command_id,
            "submit_user_reply",
            false,
            &turn_id,
            "missing_reply",
            "回答内容不能为空。",
        )
        .await?;
        return Ok(());
    }

    match engine::submit_reply(state.clone(), &turn_id, text, answers).await {
        Ok(true) => {
            send_command_ack(
                sender,
                &command_id,
                "submit_user_reply",
                true,
                &turn_id,
                "",
                "",
            )
            .await
        }
        Ok(false) => {
            send_command_ack(
                sender,
                &command_id,
                "submit_user_reply",
                false,
                &turn_id,
                "turn_not_waiting_input",
                "该回合当前没有等待用户回答。",
            )
            .await
        }
        Err(error) => {
            send_command_ack(
                sender,
                &command_id,
                "submit_user_reply",
                false,
                &turn_id,
                "reply_failed",
                &error,
            )
            .await
        }
    }
}

async fn user_input_command(
    sender: &mut SocketSender,
    state: &AppState,
    command: &Value,
) -> Result<(), String> {
    let Some(command_id) = required_command_id(sender, command).await? else {
        return Ok(());
    };
    let Some(turn_id) = required_turn_id(sender, command, "提交回合输入").await? else {
        return Ok(());
    };
    let content = command
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if content.is_empty() {
        send_command_ack(
            sender,
            &command_id,
            "user_input",
            false,
            &turn_id,
            "empty_input",
            "补充输入不能为空。",
        )
        .await?;
        return Ok(());
    }
    let accepted = engine::submit_user_input(state, &turn_id, content);
    let (error_code, message) = if accepted {
        ("", "")
    } else {
        ("turn_not_active", "该回合当前不处于运行状态。")
    };
    send_command_ack(
        sender,
        &command_id,
        "user_input",
        accepted,
        &turn_id,
        error_code,
        message,
    )
    .await
}

async fn check_active_turn_command(
    sender: &mut SocketSender,
    state: &AppState,
    command: &Value,
) -> Result<(), String> {
    let Some(session_id) = command
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return send_protocol_error(
            sender,
            "missing_session_id",
            "检查活动回合时缺少 session_id。",
            false,
            "",
            "",
        )
        .await;
    };
    let active = state
        .database
        .get_active_turn(session_id)
        .map_err(|error| format!("无法查询活动回合：{error}"))?;
    let turn_id = active
        .as_ref()
        .and_then(|turn| turn.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let status = active
        .as_ref()
        .and_then(|turn| turn.get("status"))
        .and_then(Value::as_str)
        .unwrap_or("none");
    send_value(
        sender,
        json!({
            "type": "active_turn_info",
            "protocol_version": PROTOCOL_VERSION,
            "turn_id": turn_id,
            "status": status,
            "owner_id": if turn_id.is_empty() { "" } else { "rust-local" }
        }),
    )
    .await
}

async fn required_command_id(
    sender: &mut SocketSender,
    command: &Value,
) -> Result<Option<String>, String> {
    let command_id = command
        .get("command_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if command_id.is_none() {
        send_protocol_error(
            sender,
            "missing_command_id",
            "该命令缺少 command_id。",
            false,
            "",
            command.get("turn_id").and_then(Value::as_str).unwrap_or(""),
        )
        .await?;
    }
    Ok(command_id.map(str::to_string))
}

async fn required_turn_id(
    sender: &mut SocketSender,
    command: &Value,
    action: &str,
) -> Result<Option<String>, String> {
    let turn_id = command
        .get("turn_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if turn_id.is_none() {
        send_protocol_error(
            sender,
            "missing_turn_id",
            &format!("{action}时缺少 turn_id。"),
            false,
            "",
            "",
        )
        .await?;
    }
    Ok(turn_id.map(str::to_string))
}

async fn replay_all_subscriptions(
    sender: &mut SocketSender,
    state: &AppState,
    subscriptions: &mut Subscriptions,
) -> Result<(), String> {
    let turn_ids = subscriptions.turns.keys().cloned().collect::<Vec<_>>();
    for turn_id in turn_ids {
        replay_turn(sender, state, subscriptions, &turn_id).await?;
    }
    let session_ids = subscriptions.sessions.iter().cloned().collect::<Vec<_>>();
    for session_id in session_ids {
        replay_session(sender, state, subscriptions, &session_id, 0).await?;
    }
    Ok(())
}

async fn replay_turn(
    sender: &mut SocketSender,
    state: &AppState,
    subscriptions: &mut Subscriptions,
    turn_id: &str,
) -> Result<(), String> {
    let after_seq = subscriptions.cursor(turn_id);
    let events = state
        .database
        .get_turn_events(turn_id, after_seq)
        .map_err(|error| format!("无法读取回合回放：{error}"))?;
    for event in events {
        if !send_replay_event(sender, subscriptions, event).await? {
            return Ok(());
        }
    }
    Ok(())
}

async fn replay_session(
    sender: &mut SocketSender,
    state: &AppState,
    subscriptions: &mut Subscriptions,
    session_id: &str,
    after_seq: u64,
) -> Result<(), String> {
    let events = state
        .database
        .get_session_turn_events(session_id, after_seq)
        .map_err(|error| format!("无法读取会话回放：{error}"))?;
    for event in events {
        if !send_replay_event(sender, subscriptions, event).await? {
            return Ok(());
        }
    }
    Ok(())
}

async fn send_replay_event(
    sender: &mut SocketSender,
    subscriptions: &mut Subscriptions,
    mut event: Value,
) -> Result<bool, String> {
    normalize_stream_event(&mut event);
    let Some(turn_id) = event
        .get("turn_id")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return Ok(true);
    };
    let Some(seq) = event.get("seq").and_then(Value::as_u64) else {
        return Ok(true);
    };
    let cursor = subscriptions.cursor(&turn_id);
    if seq <= cursor {
        return Ok(true);
    }
    send_value(sender, event).await?;
    subscriptions
        .turns
        .entry(turn_id)
        .and_modify(|cursor| *cursor = (*cursor).max(seq))
        .or_insert(seq);
    Ok(true)
}

async fn deliver_live_event(
    sender: &mut SocketSender,
    state: &AppState,
    subscriptions: &mut Subscriptions,
    mut event: Value,
) -> Result<(), String> {
    normalize_stream_event(&mut event);
    if !subscriptions.accepts(&event) {
        return Ok(());
    }
    let turn_id = event
        .get("turn_id")
        .and_then(Value::as_str)
        .ok_or_else(|| "实时事件缺少 turn_id。".to_string())?
        .to_string();
    let seq = event
        .get("seq")
        .and_then(Value::as_u64)
        .ok_or_else(|| "实时事件缺少 seq。".to_string())?;
    let cursor = subscriptions.cursor(&turn_id);
    if seq <= cursor {
        return Ok(());
    }
    if seq > cursor.saturating_add(1) {
        // 广播容量耗尽或数据库写入与广播时序发生交错时，以持久化事件为准补齐。
        replay_turn(sender, state, subscriptions, &turn_id).await?;
        return Ok(());
    }
    send_value(sender, event).await?;
    subscriptions
        .turns
        .entry(turn_id)
        .and_modify(|cursor| *cursor = (*cursor).max(seq))
        .or_insert(seq);
    Ok(())
}

fn normalize_stream_event(event: &mut Value) {
    let Some(object) = event.as_object_mut() else {
        return;
    };
    object
        .entry("protocol_version")
        .or_insert_with(|| Value::String(PROTOCOL_VERSION.to_string()));
    object
        .entry("content")
        .or_insert_with(|| Value::String(String::new()));
    object.entry("metadata").or_insert_with(|| json!({}));
    object
        .entry("timestamp")
        .or_insert_with(|| Value::from(crate::storage::now()));
}

fn normalize_answers(value: Option<&Value>) -> Option<Value> {
    let answers = value?.as_array()?;
    let cleaned = answers
        .iter()
        .filter_map(|answer| {
            let object = answer.as_object()?;
            let question_id = object
                .get("questionId")
                .or_else(|| object.get("id"))
                .and_then(Value::as_str)?
                .trim();
            if question_id.is_empty() {
                return None;
            }
            Some(json!({
                "questionId": question_id,
                "text": object.get("text").and_then(Value::as_str).unwrap_or("")
            }))
        })
        .collect::<Vec<_>>();
    (!cleaned.is_empty()).then(|| Value::Array(cleaned))
}

async fn send_command_ack(
    sender: &mut SocketSender,
    command_id: &str,
    command_type: &str,
    accepted: bool,
    turn_id: &str,
    error_code: &str,
    message: &str,
) -> Result<(), String> {
    let mut payload = json!({
        "type": "command_ack",
        "protocol_version": PROTOCOL_VERSION,
        "command_id": command_id,
        "command_type": command_type,
        "accepted": accepted,
        "turn_id": turn_id
    });
    if !error_code.is_empty() {
        payload["error_code"] = Value::String(error_code.to_string());
    }
    if !message.is_empty() {
        payload["message"] = Value::String(message.to_string());
    }
    send_value(sender, payload).await
}

async fn send_protocol_error(
    sender: &mut SocketSender,
    error_code: &str,
    message: &str,
    retryable: bool,
    session_id: &str,
    turn_id: &str,
) -> Result<(), String> {
    let mut payload = json!({
        "type": "protocol_error",
        "protocol_version": PROTOCOL_VERSION,
        "error_code": error_code,
        "message": message,
        "retryable": retryable
    });
    if !session_id.is_empty() {
        payload["session_id"] = Value::String(session_id.to_string());
    }
    if !turn_id.is_empty() {
        payload["turn_id"] = Value::String(turn_id.to_string());
    }
    send_value(sender, payload).await
}

async fn send_value(sender: &mut SocketSender, value: Value) -> Result<(), String> {
    let mut value = value;
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "protocol_version".to_string(),
            Value::String(PROTOCOL_VERSION.to_string()),
        );
    }
    let payload =
        serde_json::to_string(&value).map_err(|error| format!("序列化事件失败：{error}"))?;
    sender
        .send(Message::Text(payload.into()))
        .await
        .map_err(|error| format!("发送 WebSocket 事件失败：{error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 会话订阅可以接收后续回合事件() {
        let mut subscriptions = Subscriptions::default();
        subscriptions.sessions.insert("session-1".to_string());
        assert!(subscriptions.accepts(&json!({
            "turn_id": "turn-1",
            "session_id": "session-1",
            "seq": 1
        })));
        assert!(!subscriptions.accepts(&json!({
            "turn_id": "turn-2",
            "session_id": "session-2",
            "seq": 1
        })));
    }

    #[test]
    fn 回答格式会被清洗并保留问题编号() {
        let answers = normalize_answers(Some(&json!([
            {"questionId": "q1", "text": "甲"},
            {"id": "q2", "text": "乙"},
            {"text": "没有编号"},
            "非法项"
        ])));
        assert_eq!(
            answers,
            Some(json!([
                {"questionId": "q1", "text": "甲"},
                {"questionId": "q2", "text": "乙"}
            ]))
        );
    }
}
