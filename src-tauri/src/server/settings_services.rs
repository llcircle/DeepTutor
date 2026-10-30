use super::auth::{require_admin, Principal};
use super::catalog::{resolve_catalog_connections, restore_catalog_secrets, selected_catalog};
use super::media_services::{
    discover_voices, generate_image, generate_video, synthesize_tts, transcribe_audio, MediaError,
};
use super::tools::search_with_catalog;
use super::{ApiError, ApiResult, AppState, ScopedState};
use axum::body::Body;
use axum::extract::{Extension, Path};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const MAX_PREVIEW_AUDIO_BYTES: usize = 8 * 1024 * 1024;
const MAX_PREVIEW_RESULT_BYTES: usize = 40 * 1024 * 1024;
const PREVIEW_HEARTBEAT_SECONDS: u64 = 10;

#[derive(Debug, Deserialize)]
struct VoiceSelectionPayload {
    catalog: Value,
    profile_id: String,
    model_id: String,
}

#[derive(Debug, Deserialize)]
struct VoicePreviewPayload {
    catalog: Value,
    profile_id: String,
    model_id: String,
    text: String,
}

#[derive(Debug, Deserialize)]
struct ServicePreviewPayload {
    catalog: Value,
    profile_id: String,
    model_id: Option<String>,
    #[serde(default)]
    text: String,
    #[serde(default)]
    audio: String,
    #[serde(default)]
    content_type: String,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/settings/voice/voices", post(list_voice_choices))
        .route("/settings/voice/preview", post(preview_voice))
        .route(
            "/settings/services/{service}/preview",
            post(preview_service),
        )
}

async fn list_voice_choices(
    Extension(principal): Extension<Principal>,
    ScopedState(state): ScopedState,
    Json(payload): Json<VoiceSelectionPayload>,
) -> ApiResult<Response> {
    require_admin(&principal)?;
    let catalog = prepare_catalog(&state, &payload.catalog)?;
    let result = discover_voices(
        &state,
        &catalog,
        payload.profile_id.trim(),
        payload.model_id.trim(),
    )
    .await
    .map_err(map_media_error)?;
    let mut response = Json(result).into_response();
    set_no_store(&mut response);
    Ok(response)
}

async fn preview_voice(
    Extension(principal): Extension<Principal>,
    ScopedState(state): ScopedState,
    Json(payload): Json<VoicePreviewPayload>,
) -> ApiResult<Response> {
    require_admin(&principal)?;
    let text = payload.text.trim();
    if text.is_empty() || text.chars().count() > 500 {
        return Err(ApiError::bad_request(
            "试听文本长度必须为 1 到 500 个字符。",
        ));
    }
    let catalog = prepare_catalog(&state, &payload.catalog)?;
    let (audio, content_type) = synthesize_tts(
        &state,
        &catalog,
        payload.profile_id.trim(),
        payload.model_id.trim(),
        text,
    )
    .await
    .map_err(map_media_error)?;
    if audio.is_empty() {
        return Err(ApiError::provider("语音服务未返回可播放的音频。"));
    }
    audio_response(audio, &content_type)
}

async fn preview_service(
    Extension(principal): Extension<Principal>,
    ScopedState(state): ScopedState,
    Path(service): Path<String>,
    Json(payload): Json<ServicePreviewPayload>,
) -> ApiResult<Response> {
    require_admin(&principal)?;
    if !matches!(service.as_str(), "search" | "stt" | "imagegen" | "videogen") {
        return Err(ApiError::bad_request("不支持该服务预览。"));
    }
    let profile_id = payload.profile_id.trim().to_string();
    if profile_id.is_empty() {
        return Err(ApiError::bad_request("缺少服务配置 ID。"));
    }
    let model_id = payload
        .model_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let text = payload.text.trim().to_string();
    let audio = if service == "stt" {
        decode_preview_audio(&payload.audio, &payload.content_type)?
    } else {
        if text.is_empty() || text.chars().count() > 2000 {
            return Err(ApiError::bad_request(
                "试听文本长度必须为 1 到 2000 个字符。",
            ));
        }
        Vec::new()
    };
    let catalog = prepare_catalog(&state, &payload.catalog)?;
    let selected = selected_catalog(&catalog, &service, &profile_id, model_id.as_deref())
        .map_err(ApiError::bad_request)?;
    let selected_model = selected
        .pointer(&format!("/services/{service}/active_model_id"))
        .and_then(Value::as_str)
        .map(str::to_string);
    if service != "search" && selected_model.is_none() {
        return Err(ApiError::bad_request("请选择模型。"));
    }

    let timeout = Duration::from_secs(if service == "videogen" { 600 } else { 120 });
    let (events_tx, mut events_rx) = mpsc::unbounded_channel::<Value>();
    let cancel = CancellationToken::new();
    let worker_cancel = cancel.clone();
    let worker_state = state.clone();
    let worker_service = service.clone();
    let worker_text = text.clone();
    let worker_content_type = payload.content_type.clone();
    tokio::spawn(async move {
        let execution = tokio::select! {
            _ = worker_cancel.cancelled() => return,
            result = execute_preview(
                &worker_state,
                &catalog,
                &worker_service,
                &profile_id,
                selected_model.as_deref(),
                &worker_text,
                &audio,
                &worker_content_type,
                &events_tx,
            ) => result,
        };
        let event = match execution {
            Ok(result) => result,
            Err(PreviewFailure::Timeout) => {
                json!({"type": "error", "code": "timeout"})
            }
            Err(PreviewFailure::Failed) => {
                json!({"type": "error", "code": "failed"})
            }
        };
        let _ = events_tx.send(event);
    });

    let stream = async_stream::stream! {
        let _cancel_guard = CancelOnDrop(cancel);
        let mut heartbeat = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_secs(PREVIEW_HEARTBEAT_SECONDS),
            Duration::from_secs(PREVIEW_HEARTBEAT_SECONDS),
        );
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);
        yield Ok::<_, std::io::Error>(ndjson(json!({
            "type": "progress",
            "phase": "requesting"
        })));
        loop {
            tokio::select! {
                event = events_rx.recv() => {
                    match event {
                        Some(event) => {
                            let terminal = matches!(
                                event.get("type").and_then(Value::as_str),
                                Some("result" | "error")
                            );
                            yield Ok::<_, std::io::Error>(ndjson(event));
                            if terminal {
                                break;
                            }
                        }
                        None => break,
                    }
                }
                _ = heartbeat.tick() => {
                    yield Ok::<_, std::io::Error>(ndjson(json!({"type": "heartbeat"})));
                }
                _ = &mut deadline => {
                    yield Ok::<_, std::io::Error>(ndjson(json!({
                        "type": "error",
                        "code": "timeout"
                    })));
                    break;
                }
            }
        }
    };

    let mut response = Response::new(Body::from_stream(stream));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-ndjson"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
        .headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    Ok(response)
}

async fn execute_preview(
    state: &AppState,
    catalog: &Value,
    service: &str,
    profile_id: &str,
    model_id: Option<&str>,
    text: &str,
    audio: &[u8],
    content_type: &str,
    events: &mpsc::UnboundedSender<Value>,
) -> Result<Value, PreviewFailure> {
    match service {
        "search" => {
            let rows = search_with_catalog(text, 10, state, catalog)
                .await
                .map_err(|_| PreviewFailure::Failed)?;
            let compact_rows = rows
                .iter()
                .take(10)
                .map(|row| {
                    json!({
                        "title": limited_text(row.get("title").and_then(Value::as_str).unwrap_or_default(), 500),
                        "url": limited_text(row.get("url").and_then(Value::as_str).unwrap_or_default(), 4000),
                        "snippet": limited_text(row.get("snippet").and_then(Value::as_str).unwrap_or_default(), 2000)
                    })
                })
                .collect::<Vec<_>>();
            let answer = compact_rows
                .iter()
                .enumerate()
                .map(|(index, row)| {
                    format!(
                        "[{}] {}\n{}\n{}",
                        index + 1,
                        row["title"].as_str().unwrap_or_default(),
                        row["url"].as_str().unwrap_or_default(),
                        row["snippet"].as_str().unwrap_or_default()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n\n");
            Ok(json!({
                "type": "result",
                "kind": "search",
                "text": limited_owned_text(answer, 12000),
                "results": compact_rows
            }))
        }
        "stt" => {
            let (filename, normalized_type) =
                audio_format(content_type).ok_or(PreviewFailure::Failed)?;
            let transcript = transcribe_audio(
                state,
                catalog,
                profile_id,
                model_id.ok_or(PreviewFailure::Failed)?,
                audio,
                filename,
                normalized_type,
            )
            .await
            .map_err(preview_media_failure)?;
            Ok(json!({
                "type": "result",
                "kind": "transcript",
                "text": limited_owned_text(transcript, 24000)
            }))
        }
        "imagegen" => {
            let (media, content_type) = generate_image(
                state,
                catalog,
                profile_id,
                model_id.ok_or(PreviewFailure::Failed)?,
                text,
            )
            .await
            .map_err(preview_media_failure)?;
            media_result(media, &content_type, "image")
        }
        "videogen" => {
            let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<String>();
            let future = generate_video(
                state,
                catalog,
                profile_id,
                model_id.ok_or(PreviewFailure::Failed)?,
                text,
                Some(progress_tx),
            );
            tokio::pin!(future);
            loop {
                tokio::select! {
                    result = &mut future => {
                        let (media, content_type) = result.map_err(preview_media_failure)?;
                        return media_result(media, &content_type, "video");
                    }
                    message = progress_rx.recv() => {
                        if message.is_some() {
                            let _ = events.send(json!({
                                "type": "progress",
                                "phase": "rendering"
                            }));
                        }
                    }
                }
            }
        }
        _ => Err(PreviewFailure::Failed),
    }
}

pub(super) fn prepare_catalog(state: &AppState, proposed: &Value) -> ApiResult<Value> {
    if !proposed.is_object() {
        return Err(ApiError::bad_request("服务目录格式无效。"));
    }
    let current = state
        .database
        .get_setting("catalog")?
        .unwrap_or_else(|| json!({}));
    let saved = state
        .database
        .get_setting("settings_draft")?
        .and_then(|draft| draft.get("catalog").cloned())
        .filter(Value::is_object);
    let source = saved
        .as_ref()
        .map(|saved| restore_catalog_secrets(saved, &current))
        .unwrap_or(current);
    let restored = restore_catalog_secrets(proposed, &source);
    Ok(resolve_catalog_connections(&restored))
}

fn decode_preview_audio(encoded: &str, content_type: &str) -> ApiResult<Vec<u8>> {
    if audio_format(content_type).is_none() {
        return Err(ApiError::bad_request(
            "请选择 WAV、MP3、M4A、WebM、OGG 或 FLAC 音频文件。",
        ));
    }
    if encoded.is_empty() || encoded.len() > ((MAX_PREVIEW_AUDIO_BYTES + 2) / 3) * 4 {
        return Err(ApiError::bad_request("请选择不超过 8 MB 的非空音频样本。"));
    }
    let audio = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| ApiError::bad_request("无法读取音频样本。"))?;
    if audio.is_empty() || audio.len() > MAX_PREVIEW_AUDIO_BYTES {
        return Err(ApiError::bad_request("请选择不超过 8 MB 的非空音频样本。"));
    }
    Ok(audio)
}

fn audio_format(content_type: &str) -> Option<(&'static str, &'static str)> {
    let normalized = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    match normalized.as_str() {
        "audio/wav" | "audio/x-wav" => Some(("sample.wav", "audio/wav")),
        "audio/mpeg" | "audio/mp3" => Some(("sample.mp3", "audio/mpeg")),
        "audio/mp4" | "audio/x-m4a" => Some(("sample.m4a", "audio/mp4")),
        "audio/webm" => Some(("sample.webm", "audio/webm")),
        "audio/ogg" => Some(("sample.ogg", "audio/ogg")),
        "audio/flac" => Some(("sample.flac", "audio/flac")),
        _ => None,
    }
}

fn media_result(media: Vec<u8>, content_type: &str, kind: &str) -> Result<Value, PreviewFailure> {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let allowed = match kind {
        "image" => matches!(
            mime.as_str(),
            "image/png" | "image/jpeg" | "image/webp" | "image/gif"
        ),
        "video" => matches!(
            mime.as_str(),
            "video/mp4" | "video/webm" | "video/quicktime"
        ),
        _ => false,
    };
    if !allowed || media.is_empty() || media.len() > MAX_PREVIEW_RESULT_BYTES {
        return Err(PreviewFailure::Failed);
    }
    Ok(json!({
        "type": "result",
        "kind": kind,
        "content_type": mime,
        "data": base64::engine::general_purpose::STANDARD.encode(media)
    }))
}

fn audio_response(audio: Vec<u8>, content_type: &str) -> ApiResult<Response> {
    let content_type = HeaderValue::from_str(content_type)
        .map_err(|_| ApiError::internal("语音服务返回了无效的媒体类型。"))?;
    let mut response = audio.into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, content_type);
    set_no_store(&mut response);
    Ok(response)
}

fn set_no_store(response: &mut Response) {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
}

fn map_media_error(error: MediaError) -> ApiError {
    match error {
        MediaError::Invalid(message) => ApiError::bad_request(message),
        MediaError::Timeout => ApiError::new(
            StatusCode::GATEWAY_TIMEOUT,
            "provider_timeout",
            error.public_message(),
            true,
        ),
        _ => ApiError::provider(error.public_message()),
    }
}

fn preview_media_failure(error: MediaError) -> PreviewFailure {
    match error {
        MediaError::Timeout => PreviewFailure::Timeout,
        _ => PreviewFailure::Failed,
    }
}

fn ndjson(value: Value) -> Vec<u8> {
    let mut line = serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec());
    line.push(b'\n');
    line
}

fn limited_text(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn limited_owned_text(value: String, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

#[derive(Debug)]
enum PreviewFailure {
    Timeout,
    Failed,
}

struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
