use super::engine;
use super::error::{ApiError, ApiResult};
use super::ScopedState;
use super::{llm, AppState};
use axum::body::Body;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use chrono::{SecondsFormat, Utc};
use reqwest::redirect::Policy;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::net::IpAddr;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use url::Url;

const MATERIAL_DOMAIN: &str = "timed_media";
const NOTE_DOMAIN: &str = "video_note";
const NOTEBOOK_DOMAIN: &str = "notebook";
const NOTEBOOK_ID: &str = "video-learning";
const MAX_TRANSCRIPT_CUES: usize = 20_000;
const MAX_TRANSCRIPT_BYTES: usize = 2 * 1024 * 1024;
const MIN_SEGMENT_SECONDS: f64 = 20.0;
const MAX_SEGMENT_SECONDS: f64 = 90.0;
const MAX_MARKS: usize = 500;
const MAX_QUOTE_CHARS: usize = 4_000;
const MAX_NOTE_CHARS: usize = 2_000;
const MAX_SUGGESTIONS: usize = 5;
const NEARBY_WINDOW_SECONDS: f64 = 60.0;
const AUTH_FLOW_TIMEOUT_SECONDS: i64 = 600;
const ACCOUNT_SCOPES: [&str; 5] = [
    "GET:preferences",
    "POST:tokens/unregister",
    "GET:feed",
    "GET:playlists",
    "GET:playlists/*",
];
const DEFAULT_SETTINGS: &str = r#"{
  "version": 1,
  "default_provider": "youtube",
  "youtube": {"transcript_provider": "youtube_transcript_api"},
  "invidious": {"api_base_url": "", "public_base_url": ""}
}"#;

#[derive(Debug, Deserialize)]
struct ResolveRequest {
    url: String,
    #[serde(default)]
    language: String,
    provider_override: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProgressRequest {
    time_seconds: f64,
    #[serde(default)]
    duration_seconds: f64,
}

#[derive(Debug, Deserialize)]
struct CreateNoteRequest {
    body: String,
    time_seconds: f64,
}

#[derive(Debug, Deserialize)]
struct UpdateNoteRequest {
    body: String,
}

#[derive(Debug, Deserialize)]
struct MarkCreateRequest {
    kind: String,
    start_seconds: f64,
    #[serde(default)]
    end_seconds: Option<f64>,
    #[serde(default)]
    start_locator: usize,
    #[serde(default)]
    end_locator: usize,
    #[serde(default)]
    quote: Option<String>,
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    metadata: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct MarkPatchRequest {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    start_seconds: Option<f64>,
    #[serde(default)]
    end_seconds: Option<f64>,
    #[serde(default)]
    start_locator: Option<usize>,
    #[serde(default)]
    end_locator: Option<usize>,
    #[serde(default)]
    quote: Option<String>,
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    reviewed: Option<bool>,
    #[serde(default)]
    metadata: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct SuggestionRequest {
    #[serde(default)]
    time_seconds: f64,
}

#[derive(Debug, Deserialize)]
struct BrowseQuery {
    #[serde(default)]
    q: String,
    #[serde(default = "default_page")]
    page: u32,
    #[serde(default)]
    playlist_id: String,
}

#[derive(Debug, Deserialize, Default)]
struct CallbackQuery {
    #[serde(default)]
    token: String,
    #[serde(default)]
    state: String,
}

#[derive(Clone, Debug)]
struct YouTubeRequest {
    video_id: String,
    canonical_url: String,
    entry_time_seconds: u64,
}

#[derive(Clone, Debug)]
struct ProviderResolution {
    metadata: Value,
    cues: Vec<Value>,
    transcript_language: String,
    transcript_source: String,
    formats: Vec<Value>,
}

fn default_page() -> u32 {
    1
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/settings/video-learning",
            get(get_settings).put(put_settings),
        )
        .route(
            "/settings/video-learning/test-invidious",
            post(test_invidious),
        )
        .route(
            "/video-learning/invidious/account/status",
            get(account_status),
        )
        .route(
            "/video-learning/invidious/account/authorize",
            post(account_authorize),
        )
        .route(
            "/video-learning/invidious/account/callback",
            get(account_callback),
        )
        .route(
            "/video-learning/invidious/account/disconnect",
            post(account_disconnect),
        )
        .route(
            "/video-learning/invidious/browse/{kind}",
            get(browse_invidious),
        )
        .route("/video-learning/materials/resolve", post(resolve_video))
        .route(
            "/video-learning/materials/{material_id}",
            get(get_video_material),
        )
        .route(
            "/video-learning/materials/{material_id}/transcript/refresh",
            post(refresh_transcript),
        )
        .route(
            "/video-learning/materials/{material_id}/progress",
            put(save_progress),
        )
        .route(
            "/video-learning/materials/{material_id}/notes",
            get(list_notes).post(create_note),
        )
        .route(
            "/video-learning/materials/{material_id}/notes.md",
            get(export_notes),
        )
        .route(
            "/video-learning/materials/{material_id}/notes/{note_id}",
            put(update_note).delete(delete_note),
        )
        .route(
            "/video-learning/materials/{material_id}/marks",
            get(list_marks).post(create_mark),
        )
        .route(
            "/video-learning/materials/{material_id}/marks/{mark_id}",
            get(get_mark).patch(update_mark).delete(delete_mark),
        )
        .route(
            "/video-learning/materials/{material_id}/mark-suggestions",
            post(suggest_marks),
        )
        .route(
            "/video-learning/materials/{material_id}/subtitles.vtt",
            get(get_subtitles),
        )
        .route(
            "/video-learning/materials/{material_id}/stream/{format_id}",
            get(stream_video).head(head_video),
        )
}

async fn get_settings(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(load_settings(&state)?))
}

async fn put_settings(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let normalized = normalize_settings(&body)?;
    state.database.set_setting("video_learning", &normalized)?;
    Ok(Json(normalized))
}

async fn test_invidious(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let normalized = normalize_settings(&body)?;
    let Some(base) = normalized
        .pointer("/invidious/api_base_url")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    else {
        return Ok(Json(
            json!({"ok": false, "message": "尚未配置 Invidious API 地址"}),
        ));
    };
    let client = network_client(Duration::from_secs(8), false)?;
    let result = client.get(format!("{base}/api/v1/stats")).send().await;
    Ok(Json(match result {
        Ok(response) => json!({
            "ok": response.status().is_success(),
            "message": format!("HTTP {}", response.status().as_u16())
        }),
        Err(error) => json!({"ok": false, "message": error.to_string()}),
    }))
}

async fn resolve_video(
    ScopedState(state): ScopedState,
    Json(body): Json<ResolveRequest>,
) -> ApiResult<Json<Value>> {
    if body.url.chars().count() > 2_048 || body.language.chars().count() > 32 {
        return Err(ApiError::bad_request("视频地址或语言参数过长"));
    }
    let request = parse_youtube_url(&body.url)?;
    let settings = load_settings(&state)?;
    let provider = body
        .provider_override
        .as_deref()
        .unwrap_or_else(|| {
            settings
                .get("default_provider")
                .and_then(Value::as_str)
                .unwrap_or("youtube")
        })
        .to_ascii_lowercase();
    if provider != "youtube" && provider != "invidious" {
        return Err(ApiError::bad_request("不支持的视频提供方"));
    }

    let resolution = resolve_provider(&state, &request, &body.language, &provider).await?;
    let material_id = material_id_for(&request.video_id);
    let existing = state
        .database
        .get_resource(MATERIAL_DOMAIN, &material_id)?
        .unwrap_or_else(|| json!({}));
    let duration = resolution
        .metadata
        .get("lengthSeconds")
        .and_then(value_as_f64)
        .or_else(|| {
            existing
                .pointer("/metadata/duration_seconds")
                .and_then(value_as_f64)
        })
        .unwrap_or(0.0)
        .max(0.0)
        .min(86_400.0) as u64;
    let mut learning = existing
        .get("learning")
        .filter(|value| value.is_object())
        .cloned()
        .unwrap_or_else(|| json!({"last_position": request.entry_time_seconds}));
    if !learning.get("last_position").is_some_and(Value::is_number) {
        learning["last_position"] = Value::from(request.entry_time_seconds);
    }
    if !learning.get("marks").is_some_and(Value::is_array) {
        learning["marks"] = json!([]);
    }

    let title = resolution
        .metadata
        .get("title")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .or_else(|| existing.pointer("/metadata/title").and_then(Value::as_str))
        .unwrap_or(&request.video_id);
    let author = resolution
        .metadata
        .get("author_name")
        .or_else(|| resolution.metadata.get("author"))
        .and_then(Value::as_str)
        .or_else(|| existing.pointer("/metadata/author").and_then(Value::as_str))
        .unwrap_or("");
    let thumbnail = resolution
        .metadata
        .get("thumbnail_url")
        .and_then(Value::as_str)
        .or_else(|| {
            resolution
                .metadata
                .get("videoThumbnails")
                .and_then(Value::as_array)
                .and_then(|rows| rows.first())
                .and_then(|row| row.get("url"))
                .and_then(Value::as_str)
        })
        .unwrap_or("");
    let transcript_status = if resolution.cues.is_empty() {
        "unavailable"
    } else {
        "ready"
    };
    let created = existing
        .get("_created_at")
        .cloned()
        .unwrap_or_else(|| Value::String(now_iso()));
    let material = json!({
        "version": 1,
        "type": "timed_media",
        "material_id": material_id,
        "_created_at": created,
        "source": {
            "provider": "youtube",
            "video_id": request.video_id,
            "url": format!("https://youtu.be/{}", request.video_id),
            "entry_time_seconds": request.entry_time_seconds
        },
        "metadata": {
            "title": title,
            "author": author,
            "duration_seconds": duration,
            "thumbnail_url": thumbnail
        },
        "transcript": {
            "status": transcript_status,
            "reason": if resolution.cues.is_empty() { resolution.transcript_source.clone() } else { String::new() },
            "language": resolution.transcript_language,
            "source": resolution.transcript_source,
            "cues": resolution.cues
        },
        "segments": build_segments(&resolution.cues),
        "learning": learning,
        "provider_cache": {
            "invidious_formats": resolution.formats
        }
    });
    state
        .database
        .put_resource(MATERIAL_DOMAIN, &material_id, &material)?;
    Ok(Json(public_material(&material, &provider)))
}

async fn get_video_material(
    ScopedState(state): ScopedState,
    AxumPath(material_id): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    let mut material = get_material(&state, &material_id)?;
    let provider = load_settings(&state)?
        .get("default_provider")
        .and_then(Value::as_str)
        .unwrap_or("youtube")
        .to_string();
    let has_formats = material
        .pointer("/provider_cache/invidious_formats")
        .and_then(Value::as_array)
        .is_some_and(|rows| !rows.is_empty());
    if provider == "invidious" && !has_formats {
        let source_url = material
            .pointer("/source/url")
            .and_then(Value::as_str)
            .unwrap_or("");
        let request = parse_youtube_url(source_url)?;
        let resolution = resolve_invidious(&state, &request, "").await?;
        material["metadata"] = json!({
            "title": resolution.metadata.get("title").cloned()
                .unwrap_or_else(|| material.pointer("/metadata/title").cloned().unwrap_or(Value::String(request.video_id.clone()))),
            "author": resolution.metadata.get("author").cloned()
                .unwrap_or_else(|| material.pointer("/metadata/author").cloned().unwrap_or(Value::String(String::new()))),
            "duration_seconds": resolution.metadata.get("lengthSeconds").and_then(value_as_f64).unwrap_or(0.0) as u64,
            "thumbnail_url": thumbnail_from_metadata(&resolution.metadata)
        });
        if !resolution.cues.is_empty() {
            material["transcript"] = json!({
                "status": "ready",
                "reason": "",
                "language": resolution.transcript_language,
                "source": "invidious",
                "cues": resolution.cues
            });
            material["segments"] = Value::Array(build_segments(
                material
                    .pointer("/transcript/cues")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or_default(),
            ));
        }
        material["provider_cache"] = json!({"invidious_formats": resolution.formats});
        state
            .database
            .put_resource(MATERIAL_DOMAIN, &material_id, &material)?;
    }
    Ok(Json(public_material(&material, &provider)))
}

async fn refresh_transcript(
    ScopedState(state): ScopedState,
    AxumPath(material_id): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    let mut material = get_material(&state, &material_id)?;
    let video_id = material
        .pointer("/source/video_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !valid_video_id(video_id) {
        return Err(ApiError::not_found("视频材料不存在"));
    }
    if !material
        .pointer("/provider_cache/invidious_formats")
        .and_then(Value::as_array)
        .is_some_and(|rows| !rows.is_empty())
    {
        return Err(ApiError::bad_request(
            "这个材料没有可刷新的 Invidious 播放来源",
        ));
    }
    let settings = load_settings(&state)?;
    let base = settings
        .pointer("/invidious/api_base_url")
        .and_then(Value::as_str)
        .unwrap_or("");
    if base.is_empty() {
        return Err(ApiError::bad_request("尚未配置 Invidious"));
    }
    let language = material
        .pointer("/transcript/language")
        .and_then(Value::as_str)
        .unwrap_or("");
    let client = network_client(Duration::from_secs(20), false)?;
    let metadata = fetch_invidious_metadata(&client, base, video_id).await?;
    let (cues, transcript_language, source) = fetch_invidious_transcript(
        &client,
        base,
        video_id,
        metadata.get("captions"),
        language,
        true,
    )
    .await?;
    material["transcript"] = json!({
        "status": if cues.is_empty() { "unavailable" } else { "ready" },
        "reason": if cues.is_empty() { source.clone() } else { String::new() },
        "language": transcript_language,
        "source": source,
        "cues": cues
    });
    material["segments"] = Value::Array(build_segments(
        material
            .pointer("/transcript/cues")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default(),
    ));
    state
        .database
        .put_resource(MATERIAL_DOMAIN, &material_id, &material)?;
    Ok(Json(public_material(&material, "invidious")))
}

async fn save_progress(
    ScopedState(state): ScopedState,
    AxumPath(material_id): AxumPath<String>,
    Json(body): Json<ProgressRequest>,
) -> ApiResult<Json<Value>> {
    validate_seconds(body.time_seconds, "播放进度")?;
    validate_seconds(body.duration_seconds, "视频时长")?;
    let mut material = get_material(&state, &material_id)?;
    let known_duration = material
        .pointer("/metadata/duration_seconds")
        .and_then(value_as_f64)
        .unwrap_or(0.0);
    let duration = if known_duration > 0.0 {
        known_duration
    } else {
        body.duration_seconds
    };
    let position = if duration > 0.0 {
        body.time_seconds.min(duration)
    } else {
        body.time_seconds
    };
    material["learning"]["last_position"] = json!(position);
    if duration > 0.0 {
        material["metadata"]["duration_seconds"] = json!(duration);
    }
    state
        .database
        .put_resource(MATERIAL_DOMAIN, &material_id, &material)?;
    Ok(Json(json!({
        "time_seconds": position,
        "duration_seconds": duration
    })))
}

async fn list_notes(
    ScopedState(state): ScopedState,
    AxumPath(material_id): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    get_material(&state, &material_id)?;
    Ok(Json(Value::Array(notes_for_material(
        &state,
        &material_id,
    )?)))
}

async fn export_notes(
    ScopedState(state): ScopedState,
    AxumPath(material_id): AxumPath<String>,
) -> ApiResult<Response> {
    let material = get_material(&state, &material_id)?;
    let notes = notes_for_material(&state, &material_id)?;
    let title = clip_text(
        material
            .pointer("/metadata/title")
            .and_then(Value::as_str)
            .unwrap_or("视频"),
        140,
    );
    let mut lines = vec![format!("# 视频笔记：{title}")];
    if notes.is_empty() {
        lines.extend(["".to_string(), "_尚未记录时间点笔记。_".to_string()]);
    } else {
        for note in notes {
            let time = note
                .get("time_seconds")
                .and_then(value_as_f64)
                .unwrap_or(0.0);
            lines.extend([
                String::new(),
                format!("## {}", format_time(time)),
                String::new(),
                note.get("body")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string(),
            ]);
            let quote = clip_text(
                note.get("quote").and_then(Value::as_str).unwrap_or(""),
                MAX_QUOTE_CHARS,
            );
            if !quote.is_empty() {
                lines.extend([String::new(), format!("> {quote}")]);
            }
        }
    }
    let markdown = format!("{}\n", lines.join("\n").trim());
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/markdown; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .header(
            header::CONTENT_DISPOSITION,
            "attachment; filename=\"video-notes.md\"",
        )
        .body(Body::from(markdown))
        .map_err(|error| ApiError::internal(format!("无法生成笔记导出：{error}")))
}

async fn create_note(
    ScopedState(state): ScopedState,
    AxumPath(material_id): AxumPath<String>,
    Json(body): Json<CreateNoteRequest>,
) -> ApiResult<Json<Value>> {
    let clean_body = body.body.trim();
    if clean_body.is_empty() || clean_body.chars().count() > 20_000 {
        return Err(ApiError::bad_request("视频笔记必须包含 1 至 20000 个字符"));
    }
    validate_seconds(body.time_seconds, "笔记时间点")?;
    let material = get_material(&state, &material_id)?;
    let duration = material
        .pointer("/metadata/duration_seconds")
        .and_then(value_as_f64)
        .unwrap_or(0.0);
    if duration > 0.0 && body.time_seconds > duration {
        return Err(ApiError::bad_request("笔记时间点超出视频时长"));
    }
    let segment = segment_at(&material, body.time_seconds);
    let quote = segment
        .and_then(|row| row.get("text"))
        .and_then(Value::as_str)
        .map(|value| clip_text(value, MAX_QUOTE_CHARS))
        .unwrap_or_default();
    let locator = segment
        .and_then(|row| row.get("locator"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let timestamp = crate::storage::now();
    let note_id = format!("video-note-{}", uuid::Uuid::new_v4());
    let note = json!({
        "notebook_id": NOTEBOOK_ID,
        "note_id": note_id,
        "material_id": material_id,
        "body": clean_body,
        "time_seconds": body.time_seconds,
        "locator": locator,
        "quote": quote,
        "created_at": timestamp,
        "updated_at": timestamp
    });
    state.database.put_resource(NOTE_DOMAIN, &note_id, &note)?;
    sync_video_notebook(&state)?;
    Ok(Json(normalize_note(note)))
}

async fn update_note(
    ScopedState(state): ScopedState,
    AxumPath((material_id, note_id)): AxumPath<(String, String)>,
    Json(body): Json<UpdateNoteRequest>,
) -> ApiResult<Json<Value>> {
    get_material(&state, &material_id)?;
    let clean_body = body.body.trim();
    if clean_body.is_empty() || clean_body.chars().count() > 20_000 {
        return Err(ApiError::bad_request("视频笔记必须包含 1 至 20000 个字符"));
    }
    let mut note = state
        .database
        .get_resource(NOTE_DOMAIN, &note_id)?
        .ok_or_else(|| ApiError::not_found("视频笔记不存在"))?;
    if note.get("material_id").and_then(Value::as_str) != Some(material_id.as_str()) {
        return Err(ApiError::not_found("视频笔记不存在"));
    }
    note["body"] = Value::String(clean_body.to_string());
    note["updated_at"] = json!(crate::storage::now());
    state.database.put_resource(NOTE_DOMAIN, &note_id, &note)?;
    sync_video_notebook(&state)?;
    Ok(Json(normalize_note(note)))
}

async fn delete_note(
    ScopedState(state): ScopedState,
    AxumPath((material_id, note_id)): AxumPath<(String, String)>,
) -> ApiResult<Json<Value>> {
    get_material(&state, &material_id)?;
    let note = state
        .database
        .get_resource(NOTE_DOMAIN, &note_id)?
        .ok_or_else(|| ApiError::not_found("视频笔记不存在"))?;
    if note.get("material_id").and_then(Value::as_str) != Some(material_id.as_str()) {
        return Err(ApiError::not_found("视频笔记不存在"));
    }
    let deleted = state.database.delete_resource(NOTE_DOMAIN, &note_id)?;
    sync_video_notebook(&state)?;
    Ok(Json(
        json!({"status": if deleted { "deleted" } else { "missing" }}),
    ))
}

async fn list_marks(
    ScopedState(state): ScopedState,
    AxumPath(material_id): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    let material = get_material(&state, &material_id)?;
    Ok(Json(Value::Array(marks_from_material(&material))))
}

async fn get_mark(
    ScopedState(state): ScopedState,
    AxumPath((material_id, mark_id)): AxumPath<(String, String)>,
) -> ApiResult<Json<Value>> {
    let material = get_material(&state, &material_id)?;
    find_mark(&material, &mark_id)
        .cloned()
        .map(Json)
        .ok_or_else(|| ApiError::not_found("视频标记不存在"))
}

async fn create_mark(
    ScopedState(state): ScopedState,
    AxumPath(material_id): AxumPath<String>,
    Json(body): Json<MarkCreateRequest>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let mut material = get_material(&state, &material_id)?;
    let mut marks = marks_from_material(&material);
    if marks.len() >= MAX_MARKS {
        return Err(ApiError::bad_request("这个视频已达到标记数量上限"));
    }
    let mark = normalize_mark(
        &material,
        &body.kind,
        body.start_seconds,
        body.end_seconds,
        body.start_locator,
        body.end_locator,
        body.quote.as_deref(),
        body.note.as_deref(),
        body.metadata.as_ref(),
        None,
    )?;
    marks.push(mark.clone());
    material["learning"]["marks"] = Value::Array(marks);
    state
        .database
        .put_resource(MATERIAL_DOMAIN, &material_id, &material)?;
    Ok((StatusCode::CREATED, Json(mark)))
}

async fn update_mark(
    ScopedState(state): ScopedState,
    AxumPath((material_id, mark_id)): AxumPath<(String, String)>,
    Json(body): Json<MarkPatchRequest>,
) -> ApiResult<Json<Value>> {
    let mut material = get_material(&state, &material_id)?;
    let mut marks = marks_from_material(&material);
    let index = marks
        .iter()
        .position(|mark| mark.get("mark_id").and_then(Value::as_str) == Some(mark_id.as_str()))
        .ok_or_else(|| ApiError::not_found("视频标记不存在"))?;
    let existing = marks[index].clone();
    let kind = body
        .kind
        .as_deref()
        .or_else(|| existing.get("kind").and_then(Value::as_str))
        .unwrap_or("");
    let start = body
        .start_seconds
        .or_else(|| existing.get("start_seconds").and_then(value_as_f64))
        .unwrap_or(0.0);
    let end = body
        .end_seconds
        .or_else(|| existing.get("end_seconds").and_then(value_as_f64));
    let start_locator = body.start_locator.or_else(|| {
        existing
            .get("start_locator")
            .and_then(Value::as_u64)
            .map(|value| value as usize)
    });
    let end_locator = body.end_locator.or_else(|| {
        existing
            .get("end_locator")
            .and_then(Value::as_u64)
            .map(|value| value as usize)
    });
    let quote = body
        .quote
        .as_deref()
        .or_else(|| existing.get("quote").and_then(Value::as_str));
    let note = body
        .note
        .as_deref()
        .or_else(|| existing.get("note").and_then(Value::as_str));
    let metadata = body.metadata.as_ref().or_else(|| existing.get("metadata"));
    let mut updated = normalize_mark(
        &material,
        kind,
        start,
        end,
        start_locator.unwrap_or(0),
        end_locator.unwrap_or(0),
        quote,
        note,
        metadata,
        Some(&existing),
    )?;
    if let Some(reviewed) = body.reviewed {
        if reviewed {
            updated["reviewed_at"] = Value::String(now_iso());
        } else if let Some(object) = updated.as_object_mut() {
            object.remove("reviewed_at");
        }
    }
    marks[index] = updated.clone();
    material["learning"]["marks"] = Value::Array(marks);
    state
        .database
        .put_resource(MATERIAL_DOMAIN, &material_id, &material)?;
    Ok(Json(updated))
}

async fn delete_mark(
    ScopedState(state): ScopedState,
    AxumPath((material_id, mark_id)): AxumPath<(String, String)>,
) -> ApiResult<Json<Value>> {
    let mut material = get_material(&state, &material_id)?;
    let mut marks = marks_from_material(&material);
    let index = marks
        .iter()
        .position(|mark| mark.get("mark_id").and_then(Value::as_str) == Some(mark_id.as_str()))
        .ok_or_else(|| ApiError::not_found("视频标记不存在"))?;
    marks.remove(index);
    material["learning"]["marks"] = Value::Array(marks);
    state
        .database
        .put_resource(MATERIAL_DOMAIN, &material_id, &material)?;
    Ok(Json(json!({"ok": true})))
}

async fn suggest_marks(
    ScopedState(state): ScopedState,
    AxumPath(material_id): AxumPath<String>,
    Json(body): Json<SuggestionRequest>,
) -> ApiResult<Json<Value>> {
    validate_seconds(body.time_seconds, "标记建议时间点")?;
    let material = get_material(&state, &material_id)?;
    let segment = segment_at(&material, body.time_seconds).cloned();
    let nearby = nearby_cues(&material, body.time_seconds);
    if segment.is_none() && nearby.is_empty() {
        return Ok(Json(json!({"suggestions": []})));
    }
    let transcript = nearby
        .iter()
        .take(40)
        .map(|row| {
            format!(
                "[{:.1}-{:.1}] {}",
                row.get("start").and_then(value_as_f64).unwrap_or(0.0),
                row.get("end").and_then(value_as_f64).unwrap_or(0.0),
                row.get("text").and_then(Value::as_str).unwrap_or("")
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let prompt = format!(
        "从下面的视频片段提取最多 5 个学习标记。只返回 JSON 数组：\
         [{{\"kind\":\"key_point|question|review\",\"start_seconds\":数字,\
         \"end_seconds\":数字,\"quote\":\"字幕原文\",\"note\":\"简短说明\"}}]。\
         当前时间：{:.1}\n当前片段：{}\n附近字幕：\n{}",
        body.time_seconds,
        segment
            .as_ref()
            .map(Value::to_string)
            .unwrap_or_else(|| "{}".to_string()),
        transcript
    );
    let mut suggestions = Vec::new();
    if let Ok(settings) = engine::resolve_llm_settings(&state, &json!({})) {
        let cancel = CancellationToken::new();
        let messages = json!([
            {
                "role": "system",
                "content": "你帮助学习者标记视频。只输出 JSON 数组，不要 Markdown，不要虚构字幕时间。"
            },
            {"role": "user", "content": prompt}
        ]);
        if let Ok(response) = llm::stream_completion(
            &state.client,
            &settings,
            messages.as_array().map(Vec::as_slice).unwrap_or_default(),
            &[],
            None,
            &cancel,
            |_| {},
        )
        .await
        {
            for row in parse_json_array(&response.content)
                .into_iter()
                .take(MAX_SUGGESTIONS)
            {
                if let Ok(suggestion) = normalize_suggestion(&material, &row) {
                    suggestions.push(suggestion);
                }
            }
        }
    }
    if suggestions.is_empty() {
        suggestions = heuristic_suggestions(&material, body.time_seconds);
    }
    Ok(Json(json!({"suggestions": suggestions})))
}

async fn get_subtitles(
    ScopedState(state): ScopedState,
    AxumPath(material_id): AxumPath<String>,
) -> ApiResult<Response> {
    let material = get_material(&state, &material_id)?;
    let mut lines = vec!["WEBVTT".to_string(), String::new()];
    let cues = material
        .pointer("/transcript/cues")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|row| {
            let text = row.get("text").and_then(Value::as_str)?.trim();
            if text.is_empty() {
                None
            } else {
                Some((row, text))
            }
        })
        .collect::<Vec<_>>();
    for (index, cue) in cues.into_iter().enumerate() {
        lines.push((index + 1).to_string());
        lines.push(format!(
            "{} --> {}",
            vtt_timestamp(cue.0.get("start").and_then(value_as_f64).unwrap_or(0.0)),
            vtt_timestamp(cue.0.get("end").and_then(value_as_f64).unwrap_or(0.0))
        ));
        lines.push(xml_escape(
            &cue.1.split_whitespace().collect::<Vec<_>>().join(" "),
        ));
        lines.push(String::new());
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/vtt; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(lines.join("\n")))
        .map_err(|error| ApiError::internal(format!("无法生成字幕：{error}")))
}

async fn stream_video(
    ScopedState(state): ScopedState,
    AxumPath((material_id, format_id)): AxumPath<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    proxy_stream(state, material_id, format_id, headers, false).await
}

async fn head_video(
    ScopedState(state): ScopedState,
    AxumPath((material_id, format_id)): AxumPath<(String, String)>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    proxy_stream(state, material_id, format_id, headers, true).await
}

async fn proxy_stream(
    state: AppState,
    material_id: String,
    format_id: String,
    headers: HeaderMap,
    head_only: bool,
) -> ApiResult<Response> {
    if !format_id.chars().all(|value| value.is_ascii_digit())
        || format_id.is_empty()
        || format_id.len() > 6
    {
        return Err(ApiError::not_found("视频格式不存在"));
    }
    let material = get_material(&state, &material_id)?;
    let exists = material
        .pointer("/provider_cache/invidious_formats")
        .and_then(Value::as_array)
        .is_some_and(|rows| {
            rows.iter()
                .any(|row| row.get("format_id").and_then(Value::as_str) == Some(format_id.as_str()))
        });
    if !exists {
        return Err(ApiError::not_found("视频格式不存在"));
    }
    let video_id = material
        .pointer("/source/video_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let settings = load_settings(&state)?;
    let base = settings
        .pointer("/invidious/api_base_url")
        .and_then(Value::as_str)
        .unwrap_or("");
    let public_base = settings
        .pointer("/invidious/public_base_url")
        .and_then(Value::as_str)
        .unwrap_or("");
    if base.is_empty() {
        return Err(ApiError::bad_request("尚未配置 Invidious"));
    }
    let client = network_client(Duration::from_secs(30), true)?;
    let metadata = fetch_invidious_metadata(&client, base, video_id).await?;
    let mut stream_url = None;
    let mut mime = "video/mp4".to_string();
    for row in metadata
        .get("formatStreams")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        if row.get("itag").map(value_to_string).as_deref() != Some(format_id.as_str()) {
            continue;
        }
        let candidate = row.get("url").and_then(Value::as_str).unwrap_or("");
        let candidate = Url::parse(base)
            .ok()
            .and_then(|origin| origin.join(candidate).ok())
            .ok_or_else(|| ApiError::bad_request("Invidious 返回了无效的视频流地址"))?;
        stream_url = Some(allowed_stream_url(&candidate, base, public_base)?);
        mime = row
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("video/mp4")
            .split(';')
            .next()
            .unwrap_or("video/mp4")
            .to_string();
        break;
    }
    let mut current =
        stream_url.ok_or_else(|| ApiError::not_found("视频格式已被 Invidious 移除"))?;
    let mut request_headers = reqwest::header::HeaderMap::new();
    request_headers.insert(
        reqwest::header::USER_AGENT,
        HeaderValue::from_static("DeepTutor/1.7.0"),
    );
    request_headers.insert(
        reqwest::header::ACCEPT,
        HeaderValue::from_str(&mime).unwrap_or_else(|_| HeaderValue::from_static("video/mp4")),
    );
    if let Some(range) = headers.get(header::RANGE) {
        if range.to_str().is_ok_and(valid_range_header) {
            request_headers.insert(reqwest::header::RANGE, range.clone());
        }
    }
    let mut upstream = None;
    for _ in 0..=3 {
        let response = client
            .request(
                if head_only {
                    reqwest::Method::HEAD
                } else {
                    reqwest::Method::GET
                },
                current.clone(),
            )
            .headers(request_headers.clone())
            .send()
            .await
            .map_err(|error| ApiError::provider(format!("视频流请求失败：{error}")))?;
        if response.status().is_redirection() {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| ApiError::provider("视频流跳转缺少目标地址"))?;
            current = current
                .join(location)
                .map_err(|_| ApiError::provider("视频流返回了无效的跳转地址"))?;
            current = allowed_stream_url(&current, base, public_base)?;
            continue;
        }
        upstream = Some(response);
        break;
    }
    let upstream = upstream.ok_or_else(|| ApiError::provider("视频流跳转次数过多"))?;
    let status = StatusCode::from_u16(upstream.status().as_u16())
        .map_err(|_| ApiError::provider("视频流返回了无效状态码"))?;
    let mut builder = Response::builder().status(status);
    for name in [
        header::CONTENT_TYPE,
        header::CONTENT_LENGTH,
        header::CONTENT_RANGE,
        header::ACCEPT_RANGES,
        header::ETAG,
        header::LAST_MODIFIED,
        header::CACHE_CONTROL,
    ] {
        if let Some(value) = upstream.headers().get(&name) {
            builder = builder.header(name, value);
        }
    }
    if upstream.headers().get(header::CONTENT_TYPE).is_none() {
        builder = builder.header(header::CONTENT_TYPE, mime);
    }
    let body = if head_only {
        Body::empty()
    } else {
        Body::from_stream(upstream.bytes_stream())
    };
    builder
        .body(body)
        .map_err(|error| ApiError::internal(format!("无法构造视频流响应：{error}")))
}

async fn account_status(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(account_status_value(&state)?))
}

async fn account_authorize(
    ScopedState(state): ScopedState,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let settings = load_settings(&state)?;
    let api_base = settings
        .pointer("/invidious/api_base_url")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiError::bad_request("连接账户前请先配置 Invidious API 地址"))?;
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .unwrap_or("localhost");
    let callback_url = format!("http://{host}/api/video-learning/invidious/account/callback");
    let state_token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let expires_at = Utc::now().timestamp() + AUTH_FLOW_TIMEOUT_SECONDS;
    let mut flows = state
        .database
        .get_setting("invidious_auth_flows")?
        .unwrap_or_else(|| json!({}));
    prune_auth_flows(&mut flows);
    flows[&state_token] = json!({
        "api_base_url": api_base,
        "callback_url": callback_url,
        "expires_at": expires_at
    });
    state.database.set_setting("invidious_auth_flows", &flows)?;
    let public_base = settings
        .pointer("/invidious/public_base_url")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or(api_base);
    let mut authorize = Url::parse(&format!("{public_base}/authorize_token"))
        .map_err(|_| ApiError::bad_request("Invidious 公开地址无效"))?;
    authorize
        .query_pairs_mut()
        .append_pair("scopes", &ACCOUNT_SCOPES.join(","))
        .append_pair(
            "callback_url",
            &format!("{callback_url}?state={state_token}"),
        );
    Ok(Json(json!({"authorize_url": authorize.as_str()})))
}

async fn account_callback(
    ScopedState(state): ScopedState,
    Query(query): Query<CallbackQuery>,
) -> Response {
    let result = complete_account_callback(&state, &query).await;
    let code = match result {
        Ok(()) => "connected".to_string(),
        Err(error) => authorization_failure_code(&error, !query.token.is_empty()),
    };
    let mut response = Redirect::to(&format!("/reading?account={code}")).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}

async fn account_disconnect(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    let account = state
        .database
        .get_setting("invidious_account")?
        .unwrap_or_else(|| json!({}));
    let token = account.get("token").filter(|value| value.is_object());
    let base = account
        .get("api_base_url")
        .and_then(Value::as_str)
        .unwrap_or("");
    if token.is_none() || base.is_empty() {
        state
            .database
            .set_setting("invidious_account", &json!({}))?;
        return Ok(Json(json!({"connected": false})));
    }
    let token = token.unwrap();
    let client = network_client(Duration::from_secs(15), false)?;
    let response = client
        .post(format!("{base}/api/v1/auth/tokens/unregister"))
        .bearer_auth(compact_json(token))
        .json(&json!({"session": token.get("session").cloned().unwrap_or(Value::Null)}))
        .send()
        .await
        .map_err(|error| {
            ApiError::provider(format!(
                "Invidious 账户注销请求失败，已保留本地连接以便重试：{error}"
            ))
        })?;
    if !response.status().is_success() {
        return Err(ApiError::provider(format!(
            "Invidious 账户注销失败，HTTP {}",
            response.status().as_u16()
        )));
    }
    state
        .database
        .set_setting("invidious_account", &json!({}))?;
    Ok(Json(json!({"connected": false})))
}

async fn browse_invidious(
    ScopedState(state): ScopedState,
    AxumPath(kind): AxumPath<String>,
    Query(query): Query<BrowseQuery>,
) -> ApiResult<Json<Value>> {
    if query.q.chars().count() > 500 || !(1..=1_000).contains(&query.page) {
        return Err(ApiError::bad_request("视频浏览查询参数无效"));
    }
    let settings = load_settings(&state)?;
    let base = settings
        .pointer("/invidious/api_base_url")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiError::bad_request("尚未配置 Invidious"))?;
    let public_base = settings
        .pointer("/invidious/public_base_url")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or(base);
    let account = state
        .database
        .get_setting("invidious_account")?
        .unwrap_or_else(|| json!({}));
    let connected = account_status_value(&state)?.get("connected") == Some(&Value::Bool(true));
    let url = if kind == "search" {
        let mut url = Url::parse(&format!("{base}/api/v1/search"))
            .map_err(|_| ApiError::bad_request("Invidious 地址无效"))?;
        url.query_pairs_mut()
            .append_pair("q", &query.q)
            .append_pair("page", &query.page.to_string())
            .append_pair("type", "video");
        url
    } else {
        if !connected {
            return Err(ApiError::unauthorized(
                "请重新连接 Invidious 账户以浏览订阅和播放列表",
            ));
        }
        match kind.as_str() {
            "feed" => {
                let mut url = Url::parse(&format!("{base}/api/v1/auth/feed"))
                    .map_err(|_| ApiError::bad_request("Invidious 地址无效"))?;
                url.query_pairs_mut()
                    .append_pair("page", &query.page.to_string())
                    .append_pair("max_results", "24");
                url
            }
            "playlists" => Url::parse(&format!("{base}/api/v1/auth/playlists"))
                .map_err(|_| ApiError::bad_request("Invidious 地址无效"))?,
            "playlist" => {
                if query.playlist_id.is_empty()
                    || query.playlist_id.len() > 100
                    || !query
                        .playlist_id
                        .chars()
                        .all(|value| value.is_ascii_alphanumeric() || value == '_' || value == '-')
                {
                    return Err(ApiError::bad_request("播放列表标识无效"));
                }
                let mut url = Url::parse(&format!(
                    "{base}/api/v1/auth/playlists/{}",
                    query.playlist_id
                ))
                .map_err(|_| ApiError::bad_request("Invidious 地址无效"))?;
                url.query_pairs_mut()
                    .append_pair("page", &query.page.to_string());
                url
            }
            _ => return Err(ApiError::bad_request("未知的视频浏览视图")),
        }
    };
    let mut request = network_client(Duration::from_secs(20), false)?.get(url.clone());
    if kind != "search" {
        request = request.bearer_auth(compact_json(account.get("token").unwrap_or(&Value::Null)));
    }
    let response = request.send().await.map_err(|error| {
        ApiError::provider(format!("Invidious 无法加载视频，请重试或检查实例：{error}"))
    })?;
    if matches!(response.status().as_u16(), 401 | 403) {
        return Err(ApiError::unauthorized(
            "请重新连接 Invidious 账户以浏览订阅和播放列表",
        ));
    }
    if !response.status().is_success() {
        return Err(ApiError::provider(format!(
            "Invidious 无法加载视频，HTTP {}",
            response.status().as_u16()
        )));
    }
    let payload = response
        .json::<Value>()
        .await
        .map_err(|error| ApiError::provider(format!("Invidious 返回了无效目录：{error}")))?;
    Ok(Json(catalog_payload(&payload, &kind, public_base)))
}

fn load_settings(state: &AppState) -> ApiResult<Value> {
    let payload = state
        .database
        .get_setting("video_learning")?
        .unwrap_or_else(|| serde_json::from_str(DEFAULT_SETTINGS).unwrap_or_else(|_| json!({})));
    normalize_settings(&payload)
}

fn normalize_settings(payload: &Value) -> ApiResult<Value> {
    let provider = payload
        .get("default_provider")
        .and_then(Value::as_str)
        .unwrap_or("youtube")
        .trim()
        .to_ascii_lowercase();
    if provider != "youtube" && provider != "invidious" {
        return Err(ApiError::bad_request(
            "视频学习提供方必须是 youtube 或 invidious",
        ));
    }
    let transcript_provider = payload
        .pointer("/youtube/transcript_provider")
        .and_then(Value::as_str)
        .unwrap_or("youtube_transcript_api");
    if transcript_provider != "youtube_transcript_api" && transcript_provider != "none" {
        return Err(ApiError::bad_request("不支持的 YouTube 字幕提供方"));
    }
    let api_base = normalize_origin(
        payload
            .pointer("/invidious/api_base_url")
            .and_then(Value::as_str)
            .unwrap_or(""),
    )?;
    let public_base = normalize_origin(
        payload
            .pointer("/invidious/public_base_url")
            .and_then(Value::as_str)
            .unwrap_or(""),
    )?;
    if provider == "invidious" && api_base.is_empty() {
        return Err(ApiError::bad_request("选择 Invidious 前请先配置 API 地址"));
    }
    Ok(json!({
        "version": 1,
        "default_provider": provider,
        "youtube": {"transcript_provider": transcript_provider},
        "invidious": {"api_base_url": api_base, "public_base_url": public_base}
    }))
}

fn normalize_origin(raw: &str) -> ApiResult<String> {
    let raw = raw.trim().trim_end_matches('/');
    if raw.is_empty() {
        return Ok(String::new());
    }
    let parsed = Url::parse(raw)
        .map_err(|_| ApiError::bad_request("Invidious 地址必须是纯 HTTP(S) 源地址"))?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || !matches!(parsed.path(), "" | "/")
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(ApiError::bad_request(
            "Invidious 地址必须是纯 HTTP(S) 源地址且不能包含凭据",
        ));
    }
    let host = parsed.host_str().unwrap_or("").to_ascii_lowercase();
    if parsed.scheme() == "http" && !is_local_host(&host) {
        return Err(ApiError::bad_request("公网 Invidious 实例必须使用 HTTPS"));
    }
    Ok(parsed.origin().ascii_serialization())
}

fn is_local_host(host: &str) -> bool {
    if host == "localhost"
        || host == "invidious"
        || host == "host.docker.internal"
        || host.ends_with(".local")
        || host.ends_with(".ts.net")
    {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(address)) => {
            address.is_loopback()
                || address.is_private()
                || address.is_link_local()
                || (u32::from(address) & 0xffc0_0000)
                    == u32::from(std::net::Ipv4Addr::new(100, 64, 0, 0))
        }
        Ok(IpAddr::V6(address)) => {
            address.is_loopback() || address.is_unique_local() || address.is_unicast_link_local()
        }
        Err(_) => false,
    }
}

fn parse_youtube_url(value: &str) -> ApiResult<YouTubeRequest> {
    let value = value.trim().trim_matches(['`', '"', '\'']);
    let parsed = Url::parse(value)
        .map_err(|_| ApiError::bad_request("YouTube 地址必须使用 HTTP 或 HTTPS"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ApiError::bad_request("YouTube 地址必须使用 HTTP 或 HTTPS"));
    }
    let host = parsed
        .host_str()
        .unwrap_or("")
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let video_id = if host == "youtu.be" {
        parsed
            .path_segments()
            .and_then(|mut rows| rows.next())
            .unwrap_or("")
            .to_string()
    } else if [
        "youtube.com",
        "www.youtube.com",
        "m.youtube.com",
        "music.youtube.com",
    ]
    .contains(&host.as_str())
    {
        if parsed.path() == "/watch" {
            parsed
                .query_pairs()
                .find(|(key, _)| key == "v")
                .map(|(_, value)| value.into_owned())
                .unwrap_or_default()
        } else {
            ["/shorts/", "/live/", "/embed/"]
                .iter()
                .find_map(|prefix| parsed.path().strip_prefix(prefix))
                .and_then(|rest| rest.split('/').next())
                .unwrap_or("")
                .to_string()
        }
    } else {
        String::new()
    };
    if !valid_video_id(&video_id) {
        return Err(ApiError::bad_request("不支持或无效的 YouTube 地址"));
    }
    let entry_time = parsed
        .query_pairs()
        .find(|(key, _)| key == "t" || key == "start")
        .map(|(_, value)| parse_timestamp(&value))
        .unwrap_or(0);
    let canonical_url = if entry_time > 0 {
        format!("https://youtu.be/{video_id}?t={entry_time}")
    } else {
        format!("https://youtu.be/{video_id}")
    };
    Ok(YouTubeRequest {
        video_id,
        canonical_url,
        entry_time_seconds: entry_time,
    })
}

fn valid_video_id(value: &str) -> bool {
    value.len() == 11
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

fn parse_timestamp(value: &str) -> u64 {
    let raw = value.trim().to_ascii_lowercase();
    if raw.chars().all(|ch| ch.is_ascii_digit()) {
        return raw.parse::<u64>().unwrap_or(0);
    }
    let hours = capture_unit(&raw, 'h');
    let minutes = capture_unit(&raw, 'm');
    let seconds = capture_unit(&raw, 's');
    hours * 3_600 + minutes * 60 + seconds
}

fn capture_unit(value: &str, suffix: char) -> u64 {
    value
        .find(suffix)
        .and_then(|index| {
            value[..index]
                .chars()
                .rev()
                .take_while(|ch| ch.is_ascii_digit())
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>()
                .parse()
                .ok()
        })
        .unwrap_or(0)
}

fn material_id_for(video_id: &str) -> String {
    let digest = Sha256::digest(format!("youtube-resolve-{video_id}").as_bytes());
    hex::encode(digest)[..32].to_string()
}

async fn resolve_provider(
    state: &AppState,
    request: &YouTubeRequest,
    language: &str,
    provider: &str,
) -> ApiResult<ProviderResolution> {
    if provider == "invidious" {
        resolve_invidious(state, request, language).await
    } else {
        resolve_youtube(state, request, language).await
    }
}

async fn resolve_youtube(
    state: &AppState,
    request: &YouTubeRequest,
    language: &str,
) -> ApiResult<ProviderResolution> {
    let settings = load_settings(state)?;
    let mut metadata = fetch_youtube_oembed(&state.client, &request.canonical_url)
        .await
        .unwrap_or_else(|| json!({}));
    let disabled = settings
        .pointer("/youtube/transcript_provider")
        .and_then(Value::as_str)
        == Some("none");
    let mut cues = Vec::new();
    let mut transcript_language = String::new();
    let mut transcript_source = if disabled {
        "disabled".to_string()
    } else {
        "unavailable".to_string()
    };
    if !disabled {
        if let Ok((details, tracks)) = fetch_youtube_player(&state.client, &request.video_id).await
        {
            merge_metadata(&mut metadata, &details);
            if let Some(track) = choose_caption(&tracks, language) {
                transcript_language = track
                    .get("languageCode")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if let Some(base_url) = track.get("baseUrl").and_then(Value::as_str) {
                    if let Ok(caption) = fetch_youtube_caption(&state.client, base_url).await {
                        cues = normalize_cues(&caption);
                        if !cues.is_empty() {
                            transcript_source = "youtube".to_string();
                        }
                    }
                }
            }
        }
    }
    Ok(ProviderResolution {
        metadata,
        cues,
        transcript_language,
        transcript_source,
        formats: Vec::new(),
    })
}

async fn resolve_invidious(
    state: &AppState,
    request: &YouTubeRequest,
    language: &str,
) -> ApiResult<ProviderResolution> {
    let settings = load_settings(state)?;
    let base = settings
        .pointer("/invidious/api_base_url")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiError::bad_request("尚未配置 Invidious"))?;
    let client = network_client(Duration::from_secs(20), false)?;
    let metadata = fetch_invidious_metadata(&client, base, &request.video_id).await?;
    let (cues, transcript_language, transcript_source) = fetch_invidious_transcript(
        &client,
        base,
        &request.video_id,
        metadata.get("captions"),
        language,
        false,
    )
    .await?;
    let mut formats = Vec::new();
    for row in metadata
        .get("formatStreams")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let mime = row
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("");
        let format_id = row.get("itag").map(value_to_string).unwrap_or_default();
        if mime == "video/mp4"
            && !format_id.is_empty()
            && format_id.len() <= 6
            && format_id.chars().all(|ch| ch.is_ascii_digit())
        {
            formats.push(json!({
                "format_id": format_id,
                "mime_type": mime,
                "quality": row.get("qualityLabel").or_else(|| row.get("quality")).cloned().unwrap_or(Value::String(String::new()))
            }));
        }
    }
    if formats.is_empty() {
        return Err(ApiError::provider("Invidious 没有返回兼容的 MP4 视频流"));
    }
    Ok(ProviderResolution {
        metadata,
        cues,
        transcript_language,
        transcript_source,
        formats,
    })
}

async fn fetch_youtube_oembed(client: &reqwest::Client, url: &str) -> Option<Value> {
    let mut endpoint = Url::parse("https://www.youtube.com/oembed").ok()?;
    endpoint
        .query_pairs_mut()
        .append_pair("url", url)
        .append_pair("format", "json");
    let response = client.get(endpoint).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json::<Value>().await.ok().filter(Value::is_object)
}

async fn fetch_youtube_player(
    client: &reqwest::Client,
    video_id: &str,
) -> Result<(Value, Vec<Value>), String> {
    let body = json!({
        "context": {
            "client": {
                "clientName": "WEB",
                "clientVersion": "2.20241010.00.00",
                "hl": "zh-CN",
                "gl": "US"
            }
        },
        "videoId": video_id,
        "contentCheckOk": true,
        "racyCheckOk": true
    });
    let endpoint =
        "https://www.youtube.com/youtubei/v1/player?key=AIzaSyAO_FJ2SlqU8Q4STEHLGCilw_Y9_11qcW8";
    if let Ok(response) = client.post(endpoint).json(&body).send().await {
        if response.status().is_success() {
            if let Ok(payload) = response.json::<Value>().await {
                let tracks = caption_tracks(&payload);
                if !tracks.is_empty() {
                    return Ok((payload, tracks));
                }
            }
        }
    }
    let response = client
        .get(format!(
            "https://www.youtube.com/watch?v={video_id}&hl=zh-CN&has_verified=1"
        ))
        .header(reqwest::header::ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9,en;q=0.7")
        .send()
        .await
        .map_err(|error| format!("YouTube 页面请求失败：{error}"))?;
    let text = response
        .text()
        .await
        .map_err(|error| format!("YouTube 页面读取失败：{error}"))?;
    let payload = extract_embedded_json(&text, "ytInitialPlayerResponse")
        .ok_or_else(|| "YouTube 页面中没有播放器信息".to_string())?;
    Ok((payload.clone(), caption_tracks(&payload)))
}

fn caption_tracks(payload: &Value) -> Vec<Value> {
    payload
        .pointer("/captions/playerCaptionsTracklistRenderer/captionTracks")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn extract_embedded_json(text: &str, marker: &str) -> Option<Value> {
    let start = text.find(marker)?;
    let start = text[start..].find('{')? + start;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, ch) in text[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return serde_json::from_str(&text[start..start + offset + ch.len_utf8()]).ok();
                }
            }
            _ => {}
        }
    }
    None
}

fn choose_caption<'a>(tracks: &'a [Value], language: &str) -> Option<&'a Value> {
    let preferences = language_preferences(language);
    for preferred in &preferences {
        if let Some(track) = tracks.iter().find(|track| {
            track
                .get("languageCode")
                .or_else(|| track.get("language_code"))
                .and_then(Value::as_str)
                == Some(preferred.as_str())
        }) {
            return Some(track);
        }
    }
    for preferred in &preferences {
        if let Some(track) = tracks.iter().find(|track| {
            track
                .get("languageCode")
                .or_else(|| track.get("language_code"))
                .and_then(Value::as_str)
                .is_some_and(|value| {
                    value.eq_ignore_ascii_case(preferred)
                        || value.starts_with(&format!("{preferred}-"))
                })
        }) {
            return Some(track);
        }
    }
    tracks
        .iter()
        .find(|track| {
            !track
                .get("kind")
                .and_then(Value::as_str)
                .is_some_and(|value| value == "asr")
                && !track
                    .get("autoGenerated")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
        })
        .or_else(|| tracks.first())
}

fn language_preferences(language: &str) -> Vec<String> {
    let selected = language
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    if selected.is_empty() {
        vec![
            "zh-CN".to_string(),
            "zh-Hans".to_string(),
            "zh".to_string(),
            "en".to_string(),
        ]
    } else {
        selected
    }
}

async fn fetch_youtube_caption(client: &reqwest::Client, base_url: &str) -> Result<String, String> {
    let mut url = Url::parse(base_url).map_err(|error| format!("字幕地址无效：{error}"))?;
    let pairs = url
        .query_pairs()
        .filter(|(key, _)| key != "fmt")
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    url.set_query(None);
    {
        let mut query = url.query_pairs_mut();
        for (key, value) in pairs {
            query.append_pair(&key, &value);
        }
        query.append_pair("fmt", "vtt");
    }
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|error| format!("字幕请求失败：{error}"))?;
    if !response.status().is_success() {
        return Err(format!("字幕请求返回 HTTP {}", response.status().as_u16()));
    }
    if response.content_length().unwrap_or(0) > MAX_TRANSCRIPT_BYTES as u64 {
        return Err("字幕文件超过安全上限".to_string());
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|error| format!("字幕读取失败：{error}"))?;
    if bytes.len() > MAX_TRANSCRIPT_BYTES {
        return Err("字幕文件超过安全上限".to_string());
    }
    String::from_utf8(bytes.to_vec()).map_err(|_| "字幕不是有效的 UTF-8".to_string())
}

async fn fetch_invidious_metadata(
    client: &reqwest::Client,
    base: &str,
    video_id: &str,
) -> ApiResult<Value> {
    let response = client
        .get(format!("{base}/api/v1/videos/{video_id}"))
        .send()
        .await
        .map_err(|error| ApiError::provider(format!("Invidious 请求失败：{error}")))?;
    if !response.status().is_success() {
        return Err(ApiError::provider(format!(
            "Invidious 请求返回 HTTP {}",
            response.status().as_u16()
        )));
    }
    response
        .json::<Value>()
        .await
        .ok()
        .filter(Value::is_object)
        .ok_or_else(|| ApiError::provider("Invidious 返回了无效视频元数据"))
}

async fn fetch_invidious_transcript(
    client: &reqwest::Client,
    base: &str,
    video_id: &str,
    captions: Option<&Value>,
    language: &str,
    fail_on_error: bool,
) -> ApiResult<(Vec<Value>, String, String)> {
    let rows = captions
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let Some(caption) = choose_caption(rows, language) else {
        return Ok((Vec::new(), String::new(), "unavailable".to_string()));
    };
    let label = caption.get("label").and_then(Value::as_str).unwrap_or("");
    let fallback_language = language_preferences(language)
        .first()
        .cloned()
        .unwrap_or_default();
    let language_code = caption
        .get("languageCode")
        .or_else(|| caption.get("language_code"))
        .and_then(Value::as_str)
        .unwrap_or(&fallback_language)
        .to_string();
    let mut endpoint = Url::parse(&format!("{base}/api/v1/captions/{video_id}"))
        .map_err(|_| ApiError::bad_request("Invidious 地址无效"))?;
    if !label.is_empty() {
        endpoint.query_pairs_mut().append_pair("label", label);
    }
    let response = match client.get(endpoint).send().await {
        Ok(response) => response,
        Err(error) if fail_on_error => {
            return Err(ApiError::provider(format!(
                "Invidious 字幕请求失败：{error}"
            )));
        }
        Err(_) => return Ok((Vec::new(), language_code, "unavailable".to_string())),
    };
    if !response.status().is_success() {
        if fail_on_error {
            return Err(ApiError::provider(format!(
                "Invidious 字幕请求返回 HTTP {}",
                response.status().as_u16()
            )));
        }
        return Ok((Vec::new(), language_code, "unavailable".to_string()));
    }
    if response.content_length().unwrap_or(0) > MAX_TRANSCRIPT_BYTES as u64 {
        if fail_on_error {
            return Err(ApiError::bad_request("Invidious 字幕超过安全上限"));
        }
        return Ok((Vec::new(), language_code, "unavailable".to_string()));
    }
    let text = match response.text().await {
        Ok(text) if text.len() <= MAX_TRANSCRIPT_BYTES => text,
        _ if fail_on_error => return Err(ApiError::provider("Invidious 字幕读取失败")),
        _ => return Ok((Vec::new(), language_code, "unavailable".to_string())),
    };
    let cues = normalize_cues(&text);
    let source = if cues.is_empty() {
        "unavailable"
    } else {
        "invidious"
    };
    Ok((cues, language_code, source.to_string()))
}

fn normalize_cues(raw: &str) -> Vec<Value> {
    let trimmed = raw.trim_start();
    let rows = if trimmed.starts_with("WEBVTT") {
        parse_webvtt(raw)
    } else if trimmed.starts_with('{') {
        parse_json3_cues(raw)
    } else {
        parse_xml_cues(raw)
    };
    let mut result = Vec::new();
    let mut total_bytes = 0usize;
    for row in rows.into_iter().take(MAX_TRANSCRIPT_CUES) {
        let Some(text) = row.get("text").and_then(Value::as_str) else {
            continue;
        };
        let text = clean_transcript_text(text);
        if text.is_empty() {
            continue;
        }
        total_bytes += text.len();
        if total_bytes > MAX_TRANSCRIPT_BYTES {
            break;
        }
        let start = row
            .get("start")
            .and_then(value_as_f64)
            .unwrap_or(0.0)
            .max(0.0);
        let mut end = row
            .get("end")
            .and_then(value_as_f64)
            .unwrap_or_else(|| start + row.get("duration").and_then(value_as_f64).unwrap_or(0.0));
        if end <= start {
            end = start;
        }
        result.push(json!({"start": start, "end": end, "text": text}));
    }
    result
}

fn parse_json3_cues(raw: &str) -> Vec<Value> {
    let Ok(payload) = serde_json::from_str::<Value>(raw) else {
        return Vec::new();
    };
    payload
        .get("events")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|event| {
            let start = event.get("tStartMs").and_then(value_as_f64)? / 1_000.0;
            let duration = event
                .get("dDurationMs")
                .and_then(value_as_f64)
                .unwrap_or(0.0)
                / 1_000.0;
            let text = event
                .get("segs")
                .and_then(Value::as_array)
                .map(|rows| {
                    rows.iter()
                        .filter_map(|row| row.get("utf8").and_then(Value::as_str))
                        .collect::<String>()
                })
                .unwrap_or_default();
            (!text.trim().is_empty()).then(|| {
                json!({"start": start, "end": start + duration, "duration": duration, "text": text})
            })
        })
        .collect()
}

fn parse_webvtt(raw: &str) -> Vec<Value> {
    let normalized = raw.replace("\r\n", "\n").replace('\r', "\n");
    let lines = normalized.lines().collect::<Vec<_>>();
    let mut result = Vec::new();
    let mut index = 0usize;
    while index < lines.len() {
        let Some((start, end)) = parse_vtt_timing(lines[index]) else {
            index += 1;
            continue;
        };
        index += 1;
        let mut body = Vec::new();
        while index < lines.len() {
            if parse_vtt_timing(lines[index]).is_some() {
                break;
            }
            if lines[index].trim().is_empty() {
                index += 1;
                if !body.is_empty() {
                    break;
                }
                continue;
            }
            body.push(strip_vtt_markup(lines[index]));
            index += 1;
        }
        let text = body.join(" ");
        if !text.trim().is_empty() {
            result.push(json!({"start": start, "end": end, "text": text}));
        }
    }
    result
}

fn parse_vtt_timing(line: &str) -> Option<(f64, f64)> {
    let (left, right) = line.split_once("-->")?;
    let start = parse_vtt_time(left.trim())?;
    let end_text = right.trim().split_whitespace().next()?;
    let end = parse_vtt_time(end_text)?;
    Some((start, end))
}

fn parse_vtt_time(value: &str) -> Option<f64> {
    let value = value.replace(',', ".");
    let parts = value.split(':').collect::<Vec<_>>();
    match parts.as_slice() {
        [minutes, seconds] => {
            Some(minutes.parse::<f64>().ok()? * 60.0 + seconds.parse::<f64>().ok()?)
        }
        [hours, minutes, seconds] => Some(
            hours.parse::<f64>().ok()? * 3_600.0
                + minutes.parse::<f64>().ok()? * 60.0
                + seconds.parse::<f64>().ok()?,
        ),
        _ => None,
    }
}

fn strip_vtt_markup(value: &str) -> String {
    let mut result = String::new();
    let mut inside = false;
    for ch in value.chars() {
        match ch {
            '<' => inside = true,
            '>' => inside = false,
            _ if !inside => result.push(ch),
            _ => {}
        }
    }
    result
}

fn parse_xml_cues(raw: &str) -> Vec<Value> {
    let pattern = regex::Regex::new(
        r#"(?is)<text\b[^>]*\bstart="([^"]+)"[^>]*?(?:\bdur="([^"]+)")?[^>]*>(.*?)</text>"#,
    )
    .expect("有效的 XML 字幕正则");
    pattern
        .captures_iter(raw)
        .filter_map(|capture| {
            let start = capture.get(1)?.as_str().parse::<f64>().ok()?;
            let duration = capture
                .get(2)
                .and_then(|value| value.as_str().parse::<f64>().ok())
                .unwrap_or(0.0);
            let text = decode_entities(capture.get(3)?.as_str());
            (!text.trim().is_empty()).then(|| {
                json!({"start": start, "end": start + duration, "duration": duration, "text": text})
            })
        })
        .collect()
}

fn clean_transcript_text(value: &str) -> String {
    decode_entities(value)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn decode_entities(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
}

fn build_segments(cues: &[Value]) -> Vec<Value> {
    let mut segments: Vec<Value> = Vec::new();
    let mut current: Option<Value> = None;
    for cue in cues {
        if current.is_none() {
            current = Some(cue.clone());
            continue;
        }
        let row = current.as_ref().unwrap();
        let start = row.get("start").and_then(value_as_f64).unwrap_or(0.0);
        let end = row.get("end").and_then(value_as_f64).unwrap_or(0.0);
        let cue_start = cue.get("start").and_then(value_as_f64).unwrap_or(0.0);
        let cue_end = cue.get("end").and_then(value_as_f64).unwrap_or(cue_start);
        let gap = (cue_start - end).max(0.0);
        let length = end - start;
        let sentence_end = row
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim_end()
            .chars()
            .last()
            .is_some_and(|ch| ['.', '!', '?', '。', '！', '？'].contains(&ch));
        if length < MAX_SEGMENT_SECONDS
            && gap <= 4.0
            && !(length >= MIN_SEGMENT_SECONDS && sentence_end)
        {
            let mut merged = current.take().unwrap();
            merged["end"] = json!(cue_end);
            let previous = merged.get("text").and_then(Value::as_str).unwrap_or("");
            let next = cue.get("text").and_then(Value::as_str).unwrap_or("");
            merged["text"] = Value::String(format!("{previous} {next}").trim().to_string());
            current = Some(merged);
        } else {
            segments.push(current.take().unwrap());
            current = Some(cue.clone());
        }
    }
    if let Some(row) = current {
        segments.push(row);
    }
    for (index, row) in segments.iter_mut().enumerate() {
        row["locator"] = json!(index + 1);
    }
    segments
}

fn public_material(material: &Value, provider: &str) -> Value {
    let mut payload = material.clone();
    if let Some(object) = payload.as_object_mut() {
        object.remove("provider_cache");
        object.remove("_caption_text_version");
    }
    let start = payload
        .pointer("/learning/last_position")
        .and_then(value_as_f64)
        .or_else(|| {
            payload
                .pointer("/source/entry_time_seconds")
                .and_then(value_as_f64)
        })
        .unwrap_or(0.0);
    let material_id = payload
        .get("material_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    if provider == "invidious" {
        let best = material
            .pointer("/provider_cache/invidious_formats/0")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let format_id = best.get("format_id").and_then(Value::as_str).unwrap_or("");
        payload["playback"] = json!({
            "provider": "invidious",
            "kind": "html5",
            "format_id": format_id,
            "mime_type": best.get("mime_type").and_then(Value::as_str).unwrap_or("video/mp4"),
            "stream_url": format!("/api/video-learning/materials/{material_id}/stream/{format_id}"),
            "subtitles_url": format!("/api/video-learning/materials/{material_id}/subtitles.vtt"),
            "start_seconds": start
        });
    } else {
        payload["playback"] = json!({
            "provider": "youtube",
            "kind": "youtube_iframe",
            "video_id": payload.pointer("/source/video_id").and_then(Value::as_str).unwrap_or(""),
            "start_seconds": start
        });
    }
    payload
}

fn get_material(state: &AppState, material_id: &str) -> ApiResult<Value> {
    if material_id.len() < 16
        || material_id.len() > 64
        || !material_id.chars().all(|value| value.is_ascii_hexdigit())
    {
        return Err(ApiError::not_found("视频材料不存在"));
    }
    state
        .database
        .get_resource(MATERIAL_DOMAIN, material_id)?
        .filter(|value| value.get("type").and_then(Value::as_str) == Some("timed_media"))
        .ok_or_else(|| ApiError::not_found("视频材料不存在"))
}

fn notes_for_material(state: &AppState, material_id: &str) -> ApiResult<Vec<Value>> {
    let mut notes = state
        .database
        .list_resources(NOTE_DOMAIN)?
        .into_iter()
        .filter(|note| note.get("material_id").and_then(Value::as_str) == Some(material_id))
        .map(normalize_note)
        .collect::<Vec<_>>();
    notes.sort_by(|left, right| {
        left.get("time_seconds")
            .and_then(value_as_f64)
            .partial_cmp(&right.get("time_seconds").and_then(value_as_f64))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                left.get("created_at")
                    .and_then(value_as_f64)
                    .partial_cmp(&right.get("created_at").and_then(value_as_f64))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });
    Ok(notes)
}

fn normalize_note(mut note: Value) -> Value {
    let note_id = note
        .get("note_id")
        .and_then(Value::as_str)
        .or_else(|| note.get("id").and_then(Value::as_str))
        .unwrap_or("")
        .to_string();
    note["note_id"] = Value::String(note_id);
    note
}

fn sync_video_notebook(state: &AppState) -> ApiResult<()> {
    let notes = state.database.list_resources(NOTE_DOMAIN)?;
    let records = notes
        .iter()
        .map(|note| {
            let note = normalize_note(note.clone());
            let body = note.get("body").and_then(Value::as_str).unwrap_or("");
            let time = note
                .get("time_seconds")
                .and_then(value_as_f64)
                .unwrap_or(0.0);
            json!({
                "id": note.get("note_id").and_then(Value::as_str).unwrap_or(""),
                "type": "video_learning",
                "title": format!("视频笔记 - {}", format_time(time)),
                "summary": clip_text(body, 160),
                "user_query": note.get("quote").and_then(Value::as_str).unwrap_or(""),
                "output": body,
                "metadata": {
                    "material_id": note.get("material_id").cloned().unwrap_or(Value::Null),
                    "time_seconds": time,
                    "locator": note.get("locator").cloned().unwrap_or(Value::from(0)),
                    "quote": note.get("quote").cloned().unwrap_or(Value::String(String::new())),
                    "note_id": note.get("note_id").cloned().unwrap_or(Value::Null)
                },
                "created_at": note.get("created_at").cloned().unwrap_or(Value::Null)
            })
        })
        .collect::<Vec<_>>();
    let notebook = json!({
        "notebook_id": NOTEBOOK_ID,
        "name": "视频学习",
        "description": "在视频学习过程中记录的时间点笔记。",
        "color": "#EF4444",
        "icon": "video",
        "record_count": records.len(),
        "records": records
    });
    state
        .database
        .put_resource(NOTEBOOK_DOMAIN, NOTEBOOK_ID, &notebook)?;
    Ok(())
}

fn segment_at<'a>(material: &'a Value, time_seconds: f64) -> Option<&'a Value> {
    material
        .get("segments")
        .and_then(Value::as_array)?
        .iter()
        .rev()
        .find(|row| {
            row.get("start").and_then(value_as_f64).unwrap_or(0.0) <= time_seconds
                && row.get("end").and_then(value_as_f64).unwrap_or(0.0) >= time_seconds
        })
        .or_else(|| {
            material
                .get("segments")
                .and_then(Value::as_array)?
                .iter()
                .filter(|row| {
                    row.get("start").and_then(value_as_f64).unwrap_or(0.0) <= time_seconds
                })
                .last()
        })
}

fn nearby_cues(material: &Value, time_seconds: f64) -> Vec<Value> {
    material
        .pointer("/transcript/cues")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter(|row| {
            (row.get("start").and_then(value_as_f64).unwrap_or(0.0) - time_seconds).abs()
                <= NEARBY_WINDOW_SECONDS
        })
        .cloned()
        .collect()
}

fn marks_from_material(material: &Value) -> Vec<Value> {
    material
        .pointer("/learning/marks")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn find_mark<'a>(material: &'a Value, mark_id: &str) -> Option<&'a Value> {
    material
        .pointer("/learning/marks")
        .and_then(Value::as_array)?
        .iter()
        .find(|mark| mark.get("mark_id").and_then(Value::as_str) == Some(mark_id))
}

#[allow(clippy::too_many_arguments)]
fn normalize_mark(
    material: &Value,
    kind: &str,
    start_seconds: f64,
    end_seconds: Option<f64>,
    start_locator: usize,
    end_locator: usize,
    quote: Option<&str>,
    note: Option<&str>,
    metadata: Option<&Value>,
    existing: Option<&Value>,
) -> ApiResult<Value> {
    if !["key_point", "question", "review"].contains(&kind) {
        return Err(ApiError::bad_request(
            "标记类型必须是 key_point、question 或 review",
        ));
    }
    validate_seconds(start_seconds, "标记开始时间")?;
    let end = end_seconds.unwrap_or(start_seconds);
    validate_seconds(end, "标记结束时间")?;
    if end < start_seconds {
        return Err(ApiError::bad_request("标记结束时间不能早于开始时间"));
    }
    let duration = material
        .pointer("/metadata/duration_seconds")
        .and_then(value_as_f64)
        .unwrap_or(0.0);
    if duration > 0.0 && start_seconds > duration {
        return Err(ApiError::bad_request("标记开始时间超出视频时长"));
    }
    let end = if duration > 0.0 {
        end.min(duration)
    } else {
        end
    };
    let (derived_start, derived_end) = locators_for_range(material, start_seconds, end);
    let start_locator = if start_locator == 0 {
        derived_start
    } else {
        start_locator
    };
    let end_locator = if end_locator == 0 {
        derived_end.max(start_locator)
    } else {
        end_locator
    };
    let quote = quote
        .map(str::to_string)
        .unwrap_or_else(|| quote_for_range(material, start_seconds, end));
    let metadata = metadata
        .filter(|value| value.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
    if metadata.as_object().is_some_and(|object| object.len() > 16)
        || metadata.to_string().len() > 4_000
    {
        return Err(ApiError::bad_request("标记元数据过多或过大"));
    }
    let stamp = now_iso();
    let existing_mark_id = existing
        .and_then(|value| value.get("mark_id"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let mark_id = if existing_mark_id.len() >= 16
        && existing_mark_id.chars().all(|ch| ch.is_ascii_hexdigit())
    {
        existing_mark_id.to_string()
    } else {
        let digest = Sha256::digest(
            format!(
                "{}-{kind}-{start_seconds}-{end}-{stamp}",
                material
                    .get("material_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
            )
            .as_bytes(),
        );
        hex::encode(digest)[..24].to_string()
    };
    let created_at = existing
        .and_then(|value| value.get("created_at"))
        .and_then(Value::as_str)
        .unwrap_or(&stamp);
    let mut mark = json!({
        "mark_id": mark_id,
        "kind": kind,
        "start_seconds": start_seconds,
        "end_seconds": end,
        "start_locator": start_locator,
        "end_locator": end_locator,
        "quote": clip_text(&quote, MAX_QUOTE_CHARS),
        "note": clip_text(note.unwrap_or(""), MAX_NOTE_CHARS),
        "author": "user",
        "source": "immersive",
        "metadata": metadata,
        "created_at": created_at,
        "updated_at": stamp
    });
    if let Some(reviewed_at) = existing
        .and_then(|value| value.get("reviewed_at"))
        .filter(|value| !value.is_null())
    {
        mark["reviewed_at"] = reviewed_at.clone();
    }
    Ok(mark)
}

fn locators_for_range(material: &Value, start: f64, end: f64) -> (usize, usize) {
    let rows = material
        .get("segments")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter(|row| {
            row.get("end").and_then(value_as_f64).unwrap_or(0.0) >= start
                && row.get("start").and_then(value_as_f64).unwrap_or(0.0) <= end
        })
        .collect::<Vec<_>>();
    if rows.is_empty() {
        return (0, 0);
    }
    let first = rows[0].get("locator").and_then(Value::as_u64).unwrap_or(0) as usize;
    let last = rows
        .last()
        .and_then(|row| row.get("locator"))
        .and_then(Value::as_u64)
        .unwrap_or(first as u64) as usize;
    (first, last)
}

fn quote_for_range(material: &Value, start: f64, end: f64) -> String {
    clip_text(
        &material
            .pointer("/transcript/cues")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter(|row| {
                row.get("end").and_then(value_as_f64).unwrap_or(0.0) >= start
                    && row.get("start").and_then(value_as_f64).unwrap_or(0.0) <= end
            })
            .filter_map(|row| row.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        MAX_QUOTE_CHARS,
    )
}

fn normalize_suggestion(material: &Value, payload: &Value) -> ApiResult<Value> {
    let kind = payload
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("key_point");
    let start = payload
        .get("start_seconds")
        .and_then(value_as_f64)
        .ok_or_else(|| ApiError::bad_request("标记建议缺少开始时间"))?;
    let end = payload.get("end_seconds").and_then(value_as_f64);
    let mut mark = normalize_mark(
        material,
        kind,
        start,
        end,
        payload
            .get("start_locator")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize,
        payload
            .get("end_locator")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize,
        payload.get("quote").and_then(Value::as_str),
        payload.get("note").and_then(Value::as_str),
        payload.get("metadata"),
        None,
    )?;
    if let Some(object) = mark.as_object_mut() {
        object.remove("mark_id");
        object.remove("created_at");
        object.remove("updated_at");
        object.remove("reviewed_at");
    }
    mark["author"] = Value::String("assistant".to_string());
    let quote = quote_for_range(material, start, end.unwrap_or(start));
    if quote.is_empty() {
        return Err(ApiError::bad_request("标记建议没有对应字幕"));
    }
    mark["quote"] = Value::String(quote);
    Ok(mark)
}

fn heuristic_suggestions(material: &Value, time_seconds: f64) -> Vec<Value> {
    let (start, end, text) = if let Some(segment) = segment_at(material, time_seconds) {
        (
            segment.get("start").and_then(value_as_f64).unwrap_or(0.0),
            segment.get("end").and_then(value_as_f64).unwrap_or(0.0),
            segment
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        )
    } else {
        let nearby = nearby_cues(material, time_seconds);
        if nearby.is_empty() {
            return Vec::new();
        }
        (
            nearby
                .first()
                .and_then(|row| row.get("start"))
                .and_then(value_as_f64)
                .unwrap_or(time_seconds),
            nearby
                .last()
                .and_then(|row| row.get("end"))
                .and_then(value_as_f64)
                .unwrap_or(time_seconds),
            nearby
                .iter()
                .filter_map(|row| row.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(" "),
        )
    };
    if text.trim().is_empty() {
        return Vec::new();
    }
    let mut rows = vec![json!({
        "kind": "key_point",
        "start_seconds": start,
        "end_seconds": end.max(start),
        "quote": text,
        "note": ""
    })];
    if text.contains('?') || text.contains('？') {
        rows.push(json!({
            "kind": "question",
            "start_seconds": start,
            "end_seconds": end.max(start),
            "quote": text,
            "note": ""
        }));
    }
    rows.into_iter()
        .filter_map(|row| normalize_suggestion(material, &row).ok())
        .take(MAX_SUGGESTIONS)
        .collect()
}

fn parse_json_array(raw: &str) -> Vec<Value> {
    let trimmed = raw
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    if let Ok(Value::Array(rows)) = serde_json::from_str::<Value>(trimmed) {
        return rows;
    }
    let Some(start) = trimmed.find('[') else {
        return Vec::new();
    };
    let Some(end) = trimmed.rfind(']') else {
        return Vec::new();
    };
    serde_json::from_str::<Value>(&trimmed[start..=end])
        .ok()
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
}

fn thumbnail_from_metadata(metadata: &Value) -> String {
    metadata
        .get("videoThumbnails")
        .and_then(Value::as_array)
        .and_then(|rows| rows.first())
        .and_then(|row| row.get("url"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn merge_metadata(target: &mut Value, details: &Value) {
    if let Some(title) = details.pointer("/videoDetails/title") {
        target["title"] = title.clone();
    }
    if let Some(author) = details.pointer("/videoDetails/author") {
        target["author"] = author.clone();
    }
    if let Some(length) = details.pointer("/videoDetails/lengthSeconds") {
        target["lengthSeconds"] = length.clone();
    }
    if target.get("thumbnail_url").is_none() {
        if let Some(url) = details
            .pointer("/videoDetails/thumbnail/thumbnails")
            .and_then(Value::as_array)
            .and_then(|rows| rows.last())
            .and_then(|row| row.get("url"))
        {
            target["thumbnail_url"] = url.clone();
        }
    }
}

fn allowed_stream_url(candidate: &Url, base: &str, public_base: &str) -> ApiResult<Url> {
    if !matches!(candidate.scheme(), "http" | "https")
        || candidate.host_str().is_none()
        || !candidate.username().is_empty()
        || candidate.password().is_some()
    {
        return Err(ApiError::bad_request("Invidious 返回了无效的视频流地址"));
    }
    let host = candidate
        .host_str()
        .unwrap_or("")
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let same_origin = [base, public_base]
        .iter()
        .filter(|value| !value.is_empty())
        .filter_map(|value| Url::parse(value).ok())
        .any(|origin| {
            candidate.scheme() == origin.scheme()
                && candidate.host_str() == origin.host_str()
                && candidate.port_or_known_default() == origin.port_or_known_default()
        });
    let google_media = candidate.scheme() == "https"
        && (host == "googlevideo.com" || host.ends_with(".googlevideo.com"))
        && matches!(candidate.port(), None | Some(443));
    if !same_origin && !google_media {
        return Err(ApiError::bad_request(
            "Invidious 返回了可信媒体来源之外的视频地址",
        ));
    }
    Ok(candidate.clone())
}

fn account_status_value(state: &AppState) -> ApiResult<Value> {
    let account = state
        .database
        .get_setting("invidious_account")?
        .unwrap_or_else(|| json!({}));
    let token = account.get("token").filter(|value| value.is_object());
    let base = account
        .get("api_base_url")
        .and_then(Value::as_str)
        .unwrap_or("");
    let scopes = account
        .get("scopes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let configured_base = load_settings(state)?
        .pointer("/invidious/api_base_url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let token_usable = token.is_some_and(token_is_usable);
    if !token_usable || base.is_empty() {
        return Ok(json!({"connected": false}));
    }
    if !scopes_include_required(&scopes) || base != configured_base {
        return Ok(json!({"connected": false, "needs_reauthorization": true}));
    }
    Ok(json!({
        "connected": true,
        "api_base_url": base,
        "scopes": scopes,
        "connected_at": account.get("connected_at").cloned().unwrap_or(Value::Null)
    }))
}

async fn complete_account_callback(
    state: &AppState,
    query: &CallbackQuery,
) -> Result<(), ApiError> {
    if query.state.is_empty() {
        return Err(ApiError::bad_request("缺少 Invidious 授权状态"));
    }
    let mut flows = state
        .database
        .get_setting("invidious_auth_flows")?
        .unwrap_or_else(|| json!({}));
    prune_auth_flows(&mut flows);
    let flow = flows
        .get(&query.state)
        .cloned()
        .ok_or_else(|| ApiError::bad_request("Invidious 授权已过期或已使用"))?;
    if let Some(object) = flows.as_object_mut() {
        object.remove(&query.state);
    }
    state.database.set_setting("invidious_auth_flows", &flows)?;
    let token = parse_invidious_token(&query.token)?;
    let base = flow
        .get("api_base_url")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad_request("Invidious 授权流程无效"))?;
    let client = network_client(Duration::from_secs(15), false)?;
    let response = client
        .get(format!("{base}/api/v1/auth/preferences"))
        .bearer_auth(compact_json(&token))
        .send()
        .await
        .map_err(|error| ApiError::provider(format!("Invidious 账户验证请求失败：{error}")))?;
    if !response.status().is_success() {
        return Err(ApiError::provider(format!(
            "Invidious 账户验证失败，HTTP {}",
            response.status().as_u16()
        )));
    }
    if !response
        .json::<Value>()
        .await
        .is_ok_and(|value| value.is_object())
    {
        return Err(ApiError::provider("Invidious 返回了无效账户首选项"));
    }
    state.database.set_setting(
        "invidious_account",
        &json!({
            "version": 1,
            "api_base_url": base,
            "scopes": ACCOUNT_SCOPES,
            "connected_at": now_iso(),
            "token": token
        }),
    )?;
    Ok(())
}

fn parse_invidious_token(raw: &str) -> Result<Value, ApiError> {
    if raw.is_empty() || raw.len() > 8_192 {
        return Err(ApiError::bad_request("Invidious 返回了无效账户令牌"));
    }
    let decoded = if raw.trim_start().starts_with('{') {
        raw.to_string()
    } else {
        percent_decode_plus(raw)
    };
    let token = serde_json::from_str::<Value>(&decoded)
        .map_err(|_| ApiError::bad_request("Invidious 返回了无效账户令牌"))?;
    if !token.is_object()
        || token
            .get("session")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        || token
            .get("signature")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
    {
        return Err(ApiError::bad_request("Invidious 返回了不完整账户令牌"));
    }
    let scopes = token
        .get("scopes")
        .and_then(Value::as_array)
        .ok_or_else(|| ApiError::bad_request("Invidious 账户令牌缺少权限范围"))?;
    if !scopes_include_required(scopes) {
        return Err(ApiError::bad_request("Invidious 账户令牌缺少必需权限"));
    }
    if let Some(expire) = token.get("expire") {
        if !expire.as_i64().is_some_and(|value| value > 0) {
            return Err(ApiError::bad_request("Invidious 账户令牌过期时间无效"));
        }
    }
    Ok(token)
}

fn percent_decode_plus(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'+' {
            output.push(b' ');
            index += 1;
        } else if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = &raw[index + 1..index + 3];
            if let Ok(value) = u8::from_str_radix(hex, 16) {
                output.push(value);
                index += 3;
            } else {
                output.push(bytes[index]);
                index += 1;
            }
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn token_is_usable(token: &Value) -> bool {
    if token
        .get("session")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
        || token
            .get("signature")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
    {
        return false;
    }
    token
        .get("expire")
        .and_then(Value::as_i64)
        .is_none_or(|expire| expire > Utc::now().timestamp())
}

fn scopes_include_required(scopes: &[Value]) -> bool {
    let actual = scopes
        .iter()
        .filter_map(Value::as_str)
        .collect::<HashSet<_>>();
    ACCOUNT_SCOPES.iter().all(|scope| actual.contains(scope))
}

fn prune_auth_flows(flows: &mut Value) {
    let now = Utc::now().timestamp();
    if let Some(object) = flows.as_object_mut() {
        object.retain(|_, flow| {
            flow.get("expires_at")
                .and_then(Value::as_i64)
                .is_some_and(|expires| expires > now)
        });
    }
}

fn authorization_failure_code(error: &ApiError, has_token: bool) -> String {
    if !has_token {
        return "authorization_cancelled".to_string();
    }
    if error.message.contains("过期或已使用") {
        "authorization_expired"
    } else if error.message.contains("权限") {
        "authorization_scopes"
    } else if error.message.contains("令牌") {
        "authorization_token_invalid"
    } else if error.status == StatusCode::UNAUTHORIZED || error.status == StatusCode::FORBIDDEN {
        "authorization_token_rejected"
    } else if error.status == StatusCode::BAD_GATEWAY {
        "authorization_unavailable"
    } else {
        "authorization_failed"
    }
    .to_string()
}

fn catalog_payload(payload: &Value, kind: &str, public_base: &str) -> Value {
    let origin = Url::parse(public_base).ok();
    let video = |raw: &Value| -> Option<Value> {
        let video_id = raw.get("videoId").and_then(Value::as_str)?;
        if !valid_video_id(video_id) {
            return None;
        }
        let mut thumbnail = format!("https://i.ytimg.com/vi/{video_id}/hqdefault.jpg");
        if let (Some(rows), Some(origin)) = (
            raw.get("videoThumbnails").and_then(Value::as_array),
            origin.as_ref(),
        ) {
            for row in rows {
                let Some(candidate) = row
                    .get("url")
                    .and_then(Value::as_str)
                    .and_then(|value| origin.join(value).ok())
                else {
                    continue;
                };
                if candidate.origin() == origin.origin() {
                    thumbnail = candidate.to_string();
                    break;
                }
            }
        }
        Some(json!({
            "videoId": video_id,
            "title": raw.get("title").and_then(Value::as_str).unwrap_or(""),
            "author": raw.get("author").and_then(Value::as_str).unwrap_or(""),
            "lengthSeconds": raw.get("lengthSeconds").and_then(Value::as_u64).unwrap_or(0),
            "videoThumbnails": [{"url": thumbnail}]
        }))
    };
    if kind == "playlists" {
        return Value::Array(
            payload
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .filter_map(|item| {
                    let playlist_id = item.get("playlistId").and_then(Value::as_str)?;
                    if playlist_id.is_empty()
                        || playlist_id.len() > 100
                        || !playlist_id
                            .chars()
                            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
                    {
                        return None;
                    }
                    Some(json!({
                        "playlistId": playlist_id,
                        "title": item.get("title").and_then(Value::as_str).unwrap_or(""),
                        "videoCount": item.get("videoCount").and_then(Value::as_u64).unwrap_or(0),
                        "videos": item.get("videos").and_then(Value::as_array)
                            .map(|rows| rows.iter().filter_map(&video).collect::<Vec<_>>())
                            .unwrap_or_default()
                    }))
                })
                .collect(),
        );
    }
    let rows = payload.as_array().map(Vec::as_slice).unwrap_or_else(|| {
        payload
            .get("videos")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
    });
    json!({
        "videos": rows.iter().filter_map(video).collect::<Vec<_>>()
    })
}

fn network_client(timeout: Duration, no_redirect: bool) -> ApiResult<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .user_agent(format!("DeepTutor/{}", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .timeout(timeout);
    if no_redirect {
        builder = builder.redirect(Policy::none());
    }
    builder
        .build()
        .map_err(|error| ApiError::internal(format!("无法初始化网络客户端：{error}")))
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string())
}

fn valid_range_header(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("bytes=") else {
        return false;
    };
    if rest.contains(',') {
        return false;
    }
    let Some((start, end)) = rest.split_once('-') else {
        return false;
    };
    (start.is_empty() || start.chars().all(|ch| ch.is_ascii_digit()))
        && (end.is_empty() || end.chars().all(|ch| ch.is_ascii_digit()))
        && !(start.is_empty() && end.is_empty())
}

fn validate_seconds(value: f64, label: &str) -> ApiResult<()> {
    if !value.is_finite() || !(0.0..=86_400.0).contains(&value) {
        return Err(ApiError::bad_request(format!(
            "{label}必须在 0 至 86400 秒之间"
        )));
    }
    Ok(())
}

fn value_as_f64(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Number(value) => value.to_string(),
        _ => String::new(),
    }
}

fn clip_text(value: &str, limit: usize) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(limit)
        .collect()
}

fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn format_time(seconds: f64) -> String {
    let total = seconds.max(0.0).floor() as u64;
    let hours = total / 3_600;
    let minutes = (total % 3_600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

fn vtt_timestamp(seconds: f64) -> String {
    let milliseconds = (seconds.max(0.0) * 1_000.0).round() as u64;
    let hours = milliseconds / 3_600_000;
    let minutes = (milliseconds % 3_600_000) / 60_000;
    let seconds = (milliseconds % 60_000) / 1_000;
    let milliseconds = milliseconds % 1_000;
    format!("{hours:02}:{minutes:02}:{seconds:02}.{milliseconds:03}")
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 解析_youtube_短链接和时间点() {
        let request = parse_youtube_url("https://youtu.be/dQw4w9WgXcQ?t=1m30s").unwrap();
        assert_eq!(request.video_id, "dQw4w9WgXcQ");
        assert_eq!(request.entry_time_seconds, 90);
        assert_eq!(request.canonical_url, "https://youtu.be/dQw4w9WgXcQ?t=90");
    }

    #[test]
    fn 解析_webvtt_并合并分段() {
        let cues = normalize_cues(
            "WEBVTT\n\n00:00:00.000 --> 00:00:03.000\n第一句\n\n\
             00:00:03.100 --> 00:00:06.000\n第二句\n",
        );
        assert_eq!(cues.len(), 2);
        let segments = build_segments(&cues);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0]["locator"], 1);
        assert_eq!(segments[0]["text"], "第一句 第二句");
    }

    #[test]
    fn 拒绝不安全的视频流来源() {
        let base = "https://invidious.example";
        let allowed = Url::parse("https://invidious.example/videoplayback?id=1").unwrap();
        assert!(allowed_stream_url(&allowed, base, "").is_ok());
        let google = Url::parse("https://r1.googlevideo.com/videoplayback?id=1").unwrap();
        assert!(allowed_stream_url(&google, base, "").is_ok());
        let blocked = Url::parse("https://attacker.example/video").unwrap();
        assert!(allowed_stream_url(&blocked, base, "").is_err());
    }

    #[test]
    fn 校验_invidious_令牌必需权限() {
        let scopes = ACCOUNT_SCOPES
            .iter()
            .map(|value| Value::String((*value).to_string()))
            .collect::<Vec<_>>();
        assert!(scopes_include_required(&scopes));
        assert!(!scopes_include_required(&scopes[..4]));
    }

    #[test]
    fn 生成稳定的材料标识() {
        assert_eq!(material_id_for("dQw4w9WgXcQ").len(), 32);
        assert_eq!(
            material_id_for("dQw4w9WgXcQ"),
            material_id_for("dQw4w9WgXcQ")
        );
    }
}
