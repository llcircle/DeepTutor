use super::engine;
use super::llm::{self, LlmChunk};
use super::AppState;
use super::ScopedState;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub async fn websocket(ws: WebSocketUpgrade, ScopedState(state): ScopedState) -> impl IntoResponse {
    ws.max_message_size(64 * 1024 * 1024)
        .max_frame_size(16 * 1024 * 1024)
        .on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    let (mut sender, mut receiver) = socket.split();
    let payload = match receive_payload(&mut receiver).await {
        Ok(payload) => payload,
        Err(error) => {
            let _ = send_frame(&mut sender, json!({"type": "error", "content": error})).await;
            return;
        }
    };
    let request = json!({});
    let settings = match engine::resolve_llm_settings(&state, &request) {
        Ok(settings) => settings,
        Err(error) => {
            let _ = send_frame(&mut sender, json!({"type": "error", "content": error})).await;
            return;
        }
    };
    let messages = match judge_messages(&state, &payload) {
        Ok(messages) => messages,
        Err(error) => {
            let _ = send_frame(&mut sender, json!({"type": "error", "content": error})).await;
            return;
        }
    };
    if send_frame(&mut sender, json!({"type": "started"}))
        .await
        .is_err()
    {
        return;
    }

    let (chunk_tx, mut chunk_rx) = mpsc::unbounded_channel::<Value>();
    let cancel = CancellationToken::new();
    let worker_cancel = cancel.clone();
    let client = state.client.clone();
    let worker = tokio::spawn(async move {
        let result = llm::stream_completion(
            &client,
            &settings,
            &messages,
            &[],
            None,
            &worker_cancel,
            |chunk| {
                if let LlmChunk::Content(content) = chunk {
                    let _ = chunk_tx.send(json!({"type": "text", "content": content}));
                }
            },
        )
        .await;
        let _ = chunk_tx.send(match result {
            Ok(_) => json!({"type": "done"}),
            Err(error) => json!({"type": "error", "content": error}),
        });
    });

    loop {
        tokio::select! {
            frame = chunk_rx.recv() => {
                let Some(frame) = frame else {
                    break;
                };
                let terminal = matches!(
                    frame.get("type").and_then(Value::as_str),
                    Some("done" | "error")
                );
                if send_frame(&mut sender, frame).await.is_err() || terminal {
                    break;
                }
            }
            incoming = receiver.next() => {
                match incoming {
                    None | Some(Ok(Message::Close(_))) | Some(Err(_)) => {
                        cancel.cancel();
                        break;
                    }
                    Some(Ok(Message::Ping(value))) => {
                        if sender.send(Message::Pong(value)).await.is_err() {
                            cancel.cancel();
                            break;
                        }
                    }
                    Some(Ok(_)) => {}
                }
            }
        }
    }
    cancel.cancel();
    let _ = worker.await;
    let _ = sender.close().await;
}

async fn receive_payload(
    receiver: &mut futures_util::stream::SplitStream<WebSocket>,
) -> Result<Value, String> {
    let incoming = tokio::time::timeout(std::time::Duration::from_secs(30), receiver.next())
        .await
        .map_err(|_| "等待判题请求超时。".to_string())?
        .ok_or_else(|| "判题连接在发送请求前已关闭。".to_string())?
        .map_err(|error| format!("无法读取判题请求：{error}"))?;
    match incoming {
        Message::Text(text) => {
            serde_json::from_str(&text).map_err(|error| format!("判题请求不是合法 JSON：{error}"))
        }
        _ => Err("判题请求必须是 JSON 文本帧。".to_string()),
    }
}

fn judge_messages(state: &AppState, payload: &Value) -> Result<Vec<Value>, String> {
    let question = payload
        .get("question")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if question.is_empty() {
        return Err("判题请求缺少题目。".to_string());
    }
    let language = payload
        .get("language")
        .and_then(Value::as_str)
        .unwrap_or("zh");
    let system = if language.starts_with("zh") {
        "你是严格、耐心的学习评估助手。请判断学生的作答，指出正确或错误的原因，必要时给出简短订正步骤。只输出评估正文，不要输出 JSON。"
    } else {
        "You are a rigorous, patient learning assessor. Judge the learner's answer, explain why it is correct or incorrect, and give a concise correction when needed. Output only the assessment prose, not JSON."
    };
    let options = payload
        .get("options")
        .filter(|value| !value.is_null())
        .map(|value| serde_json::to_string_pretty(value).unwrap_or_default())
        .unwrap_or_else(|| "无".to_string());
    let prompt = format!(
        "题目：{question}\n题型：{}\n选项：{options}\n参考答案：{}\n参考解析：{}\n学生答案：{}",
        payload
            .get("question_type")
            .and_then(Value::as_str)
            .unwrap_or("unknown"),
        payload
            .get("correct_answer")
            .and_then(Value::as_str)
            .unwrap_or(""),
        payload
            .get("explanation")
            .and_then(Value::as_str)
            .unwrap_or(""),
        payload
            .get("user_answer")
            .and_then(Value::as_str)
            .unwrap_or("")
    );
    let mut parts = vec![json!({"type": "text", "text": prompt})];
    for (index, image) in payload
        .get("user_answer_images")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        if index >= 8 {
            break;
        }
        let mime = image
            .get("mime_type")
            .and_then(Value::as_str)
            .filter(|value| value.starts_with("image/"))
            .unwrap_or("image/png");
        let encoded = image
            .get("base64")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                image
                    .get("url")
                    .and_then(Value::as_str)
                    .and_then(|url| read_local_judge_image(state, url))
                    .map(|bytes| base64::engine::general_purpose::STANDARD.encode(bytes))
            });
        if let Some(encoded) = encoded {
            parts.push(json!({
                "type": "image_url",
                "image_url": {"url": format!("data:{mime};base64,{encoded}")}
            }));
        }
    }
    let content = if parts.len() == 1 {
        parts[0]
            .get("text")
            .cloned()
            .unwrap_or_else(|| Value::String(String::new()))
    } else {
        Value::Array(parts)
    };
    Ok(vec![
        json!({"role": "system", "content": system}),
        json!({"role": "user", "content": content}),
    ])
}

fn read_local_judge_image(state: &AppState, url: &str) -> Option<Vec<u8>> {
    if url.starts_with("http://") || url.starts_with("https://") || url.starts_with("data:") {
        return None;
    }
    let relative = url
        .split('?')
        .next()
        .unwrap_or(url)
        .strip_prefix("/files/attachments/")?;
    let root = state.paths.uploads.join("attachments");
    let path = safe_join(&root, relative)?;
    std::fs::read(path).ok()
}

fn safe_join(root: &Path, relative: &str) -> Option<PathBuf> {
    let path = relative
        .split('/')
        .filter(|segment| !segment.is_empty() && *segment != "." && *segment != "..")
        .fold(root.to_path_buf(), |path, segment| path.join(segment));
    path.starts_with(root).then_some(path)
}

async fn send_frame(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    value: Value,
) -> Result<(), ()> {
    sender
        .send(Message::Text(value.to_string().into()))
        .await
        .map_err(|_| ())
}
