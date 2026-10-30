use super::catalog::{extra_headers, service_selection, CATALOG_SECRET_MASK};
use super::AppState;
use base64::Engine;
use edge_tts_rust::{EdgeTtsClient, SpeakOptions, SynthesisEvent};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fmt;
use std::io::Cursor;
use std::path::Path;
use std::time::Duration;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::codecs::CodecParameters;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::FormatOptions;
use symphonia::core::formats::TrackType;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use url::Url;
use uuid::Uuid;

const DEFAULT_MAX_INPUT_CHARS: usize = 4096;
const MAX_MEDIA_BYTES: usize = 40 * 1024 * 1024;
const DASHSCOPE_PCM_RATE: u32 = 16000;

#[derive(Debug)]
pub enum MediaError {
    Invalid(String),
    HttpStatus(u16),
    Timeout,
    Provider,
}

impl MediaError {
    pub fn public_message(&self) -> String {
        match self {
            Self::Invalid(message) => message.clone(),
            Self::HttpStatus(401) => "服务鉴权失败，请检查 API Key 和账户区域。".to_string(),
            Self::HttpStatus(403) => "服务拒绝访问，请检查模型、语音和应用权限。".to_string(),
            Self::HttpStatus(404) => "服务地址或模型不存在，请检查 URL 与模型 ID。".to_string(),
            Self::HttpStatus(400 | 422) => {
                "服务拒绝了请求参数，请检查模型、语音、语言和媒体格式。".to_string()
            }
            Self::HttpStatus(429) => "服务配额或速率已达上限，请检查余额与配额后重试。".to_string(),
            Self::HttpStatus(status) if *status >= 500 => {
                "服务暂时不可用，请稍后重试。".to_string()
            }
            Self::HttpStatus(_) => "服务请求失败，请检查连接设置。".to_string(),
            Self::Timeout => "请求超时，请缩短输入或稍后重试。".to_string(),
            Self::Provider => "服务返回了无效结果，请检查凭据、模型和连接地址。".to_string(),
        }
    }
}

impl fmt::Display for MediaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.public_message())
    }
}

impl std::error::Error for MediaError {}

#[derive(Clone, Debug)]
struct RuntimeConfig {
    model: String,
    provider: String,
    adapter: String,
    auth_style: String,
    request_style: String,
    api_key: String,
    app_id: String,
    resource_id: String,
    base_url: String,
    api_version: String,
    extra_headers: BTreeMap<String, String>,
    voice: String,
    response_format: String,
    speed: Option<f64>,
    language: String,
    sample_rate: u32,
    instructions: String,
    max_input_chars: usize,
    request_timeout: Duration,
    size: String,
    quality: String,
    style: String,
    aspect_ratio: String,
    duration: String,
    resolution: String,
    poll_interval: Duration,
    poll_timeout: Duration,
}

impl RuntimeConfig {
    fn new(
        model: &Value,
        profile: &Value,
        provider: String,
        adapter: String,
        default_base_url: &str,
    ) -> Result<Self, MediaError> {
        let model_name = text(model, "model");
        if model_name.is_empty() {
            return Err(MediaError::Invalid("尚未配置模型。".into()));
        }
        let base_url = {
            let configured = text(profile, "base_url");
            if configured.is_empty() {
                default_base_url.to_string()
            } else {
                configured
            }
        };
        if base_url.is_empty() && adapter != "edge" {
            return Err(MediaError::Invalid("尚未配置服务地址。".into()));
        }
        let mut api_key = text(profile, "api_key");
        if api_key.is_empty() && matches!(provider.as_str(), "edge" | "vllm" | "custom") {
            api_key = "sk-no-key-required".into();
        }
        let speed = parse_optional_speed(model.get("speed"))?;
        let request_timeout = parse_tts_timeout(model.get("request_timeout"))?;
        let response_format = {
            let configured = text(model, "response_format");
            if configured.is_empty() {
                default_response_format(&adapter).to_string()
            } else {
                configured
            }
        };
        let max_input_chars = model
            .get("max_input_chars")
            .and_then(Value::as_u64)
            .map(|value| value.clamp(1, 12000) as usize)
            .unwrap_or_else(|| {
                if provider == "groq" {
                    200
                } else {
                    DEFAULT_MAX_INPUT_CHARS
                }
            });
        let mut auth_style = text(profile, "auth_style");
        if auth_style.is_empty() && provider == "azure_openai" {
            auth_style = "api_key_header".into();
        }
        let mut request_style = text(profile, "request_style");
        if request_style.is_empty() && provider == "openrouter" {
            request_style = "base64_json".into();
        }
        Ok(Self {
            model: model_name,
            provider,
            adapter,
            auth_style,
            request_style,
            api_key,
            app_id: text(profile, "app_id"),
            resource_id: text(model, "resource_id"),
            base_url,
            api_version: text(profile, "api_version"),
            extra_headers: header_map(profile)?,
            voice: text(model, "voice"),
            response_format,
            speed,
            language: text(model, "language"),
            sample_rate: model
                .get("sample_rate")
                .and_then(Value::as_u64)
                .unwrap_or(24000) as u32,
            instructions: text(model, "instructions"),
            max_input_chars,
            request_timeout,
            size: text(model, "size"),
            quality: text(model, "quality"),
            style: text(model, "style"),
            aspect_ratio: text(model, "aspect_ratio"),
            duration: text(model, "duration"),
            resolution: text(model, "resolution"),
            poll_interval: Duration::from_millis(
                model
                    .get("poll_interval_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(5000),
            ),
            poll_timeout: Duration::from_secs(
                model
                    .get("poll_timeout")
                    .and_then(Value::as_u64)
                    .unwrap_or(600)
                    .clamp(30, 1800),
            ),
        })
    }
}

pub async fn discover_voices(
    state: &AppState,
    catalog: &Value,
    profile_id: &str,
    model_id: &str,
) -> Result<Value, MediaError> {
    let Some((profile, Some(model))) =
        service_selection(catalog, "tts", Some(profile_id), Some(model_id))
    else {
        return Err(MediaError::Invalid("所选语音配置已不存在。".into()));
    };
    let (provider, adapter, default_base) = canonical_voice_provider(&text(&profile, "binding"));
    let config = RuntimeConfig::new(&model, &profile, provider.clone(), adapter, default_base)?;
    if config.adapter == "edge" {
        let client = EdgeTtsClient::builder()
            .connect_timeout(Duration::from_secs(10))
            .receive_timeout(Duration::from_secs(25))
            .ws_warmup(false)
            .build()
            .map_err(|_| MediaError::Provider)?;
        let voices = tokio::time::timeout(Duration::from_secs(25), client.list_voices())
            .await
            .map_err(|_| MediaError::Timeout)?
            .map_err(|_| MediaError::Provider)?;
        return Ok(json!({
            "status": "ready",
            "scope": "public",
            "voices": voices.into_iter().map(|voice| json!({
                "id": voice.short_name,
                "label": format!("{} ({})", voice.friendly_name.as_deref().unwrap_or(&voice.short_name), voice.gender),
                "languages": [voice.locale]
            })).collect::<Vec<_>>()
        }));
    }
    if !matches!(provider.as_str(), "minimax" | "dashscope") {
        return Ok(json!({"status": "unsupported", "scope": "none", "voices": []}));
    }
    if config.api_key.is_empty() || config.api_key == CATALOG_SECRET_MASK {
        return Err(MediaError::Invalid(
            "请先填写服务商凭据后再获取语音列表。".into(),
        ));
    }
    let native_dashscope = config.model.starts_with("cosyvoice-")
        || config.model.starts_with("qwen-audio-")
        || (config.model.starts_with("qwen")
            && (config.model.contains("-vc-") || config.model.contains("-vd-")));
    if provider == "dashscope" && !native_dashscope {
        return Ok(json!({"status": "unsupported", "scope": "none", "voices": []}));
    }
    let endpoint = discovery_url(&config.base_url, &provider)?;
    let mut voices = BTreeMap::<String, Value>::new();
    if provider == "minimax" {
        let response = authorized_request(&state.client, state.client.post(&endpoint), &config)?
            .json(&json!({"voice_type": "all"}))
            .send()
            .await
            .map_err(map_reqwest_error)?;
        ensure_http_success(response.status().as_u16())?;
        let payload = response
            .json::<Value>()
            .await
            .map_err(|_| MediaError::Provider)?;
        if payload
            .pointer("/base_resp/status_code")
            .and_then(Value::as_i64)
            != Some(0)
        {
            return Err(MediaError::Provider);
        }
        let mut found_group = false;
        for group in ["system_voice", "voice_cloning", "voice_generation"] {
            if let Some(rows) = payload.get(group).and_then(Value::as_array) {
                found_group = true;
                for row in rows {
                    if let Some(id) = row.get("voice_id").and_then(Value::as_str) {
                        insert_voice(
                            &mut voices,
                            id,
                            row.get("voice_name").and_then(Value::as_str),
                            None,
                        );
                    }
                }
            }
        }
        if !found_group {
            return Err(MediaError::Provider);
        }
        return Ok(json!({
            "status": "ready",
            "scope": "account",
            "voices": voices.into_values().collect::<Vec<_>>()
        }));
    }

    let qwen_audio = config.model.starts_with("qwen-audio-");
    let enrollment = if qwen_audio {
        "voice-enrollment"
    } else {
        "qwen-voice-enrollment"
    };
    for page in 0..100 {
        let response = authorized_request(&state.client, state.client.post(&endpoint), &config)?
            .json(&json!({
                "model": enrollment,
                "input": {
                    "action": if qwen_audio { "list_voice" } else { "list" },
                    "page_index": page,
                    "page_size": 100
                }
            }))
            .send()
            .await
            .map_err(map_reqwest_error)?;
        ensure_http_success(response.status().as_u16())?;
        let payload = response
            .json::<Value>()
            .await
            .map_err(|_| MediaError::Provider)?;
        validate_dashscope_response(&payload)?;
        let Some(rows) = payload
            .pointer("/output/voice_list")
            .and_then(Value::as_array)
        else {
            return Err(MediaError::Provider);
        };
        for row in rows {
            let id = row
                .get("voice_id")
                .or_else(|| row.get("voice"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if id.is_empty() {
                return Err(MediaError::Provider);
            }
            if qwen_audio && row.get("status").and_then(Value::as_str) != Some("OK") {
                continue;
            }
            if row
                .get("target_model")
                .and_then(Value::as_str)
                .is_some_and(|model| model != config.model)
                || row
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(|status| status != "OK")
            {
                continue;
            }
            insert_voice(
                &mut voices,
                id,
                None,
                row.get("language").and_then(Value::as_str),
            );
        }
        if rows.len() < 100 {
            return Ok(json!({
                "status": "ready",
                "scope": "custom",
                "voices": voices.into_values().collect::<Vec<_>>()
            }));
        }
    }
    Err(MediaError::Invalid(
        "语音列表过大，请直接填写语音 ID。".into(),
    ))
}

pub async fn synthesize_tts(
    state: &AppState,
    catalog: &Value,
    profile_id: &str,
    model_id: &str,
    spoken_text: &str,
) -> Result<(Vec<u8>, String), MediaError> {
    let input = spoken_text.trim();
    if input.is_empty() || input.chars().count() > 500 {
        return Err(MediaError::Invalid(
            "试听文本长度必须为 1 到 500 个字符。".into(),
        ));
    }
    synthesize_tts_inner(
        state, catalog, profile_id, model_id, input, false, None, None,
    )
    .await
}

pub async fn synthesize_active_tts(
    state: &AppState,
    catalog: &Value,
    profile_id: &str,
    model_id: &str,
    spoken_text: &str,
    voice: Option<&str>,
    response_format: Option<&str>,
) -> Result<(Vec<u8>, String), MediaError> {
    let input = spoken_text.trim();
    if input.is_empty() {
        return Err(MediaError::Invalid("语音文本不能为空。".into()));
    }
    synthesize_tts_inner(
        state,
        catalog,
        profile_id,
        model_id,
        input,
        true,
        voice,
        response_format,
    )
    .await
}

async fn synthesize_tts_inner(
    state: &AppState,
    catalog: &Value,
    profile_id: &str,
    model_id: &str,
    input: &str,
    truncate_to_limit: bool,
    voice_override: Option<&str>,
    format_override: Option<&str>,
) -> Result<(Vec<u8>, String), MediaError> {
    let Some((profile, Some(model))) =
        service_selection(catalog, "tts", Some(profile_id), Some(model_id))
    else {
        return Err(MediaError::Invalid("所选语音模型已不存在。".into()));
    };
    let (provider, adapter, default_base) = canonical_voice_provider(&text(&profile, "binding"));
    let mut config = RuntimeConfig::new(&model, &profile, provider, adapter, default_base)?;
    if let Some(voice) = voice_override
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        config.voice = voice.to_string();
    }
    if let Some(format) = format_override
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        config.response_format = format.to_ascii_lowercase();
    }
    if config.api_key == CATALOG_SECRET_MASK
        || config
            .extra_headers
            .values()
            .any(|value| value == CATALOG_SECRET_MASK)
    {
        return Err(MediaError::Invalid(
            "未找到已保存的语音凭据，请重新填写。".into(),
        ));
    }
    let prepared = if input.chars().count() > config.max_input_chars {
        if !truncate_to_limit {
            return Err(MediaError::Invalid(format!(
                "当前语音模型最多接受 {} 个字符。",
                config.max_input_chars
            )));
        }
        input
            .chars()
            .take(config.max_input_chars)
            .collect::<String>()
    } else {
        input.to_string()
    };
    if prepared.trim().is_empty() {
        return Err(MediaError::Invalid(format!("清理后的语音文本为空。")));
    }
    let (audio, content_type) = match config.adapter.as_str() {
        "openai_compat" => openai_tts(state, &prepared, &config).await?,
        "openrouter_tts" => openrouter_tts(state, &prepared, &config).await?,
        "dashscope" => dashscope_tts(state, &prepared, &config).await?,
        "volcengine" => volcengine_tts(state, &prepared, &config).await?,
        "minimax" => minimax_tts(state, &prepared, &config).await?,
        "gemini" => gemini_tts(state, &prepared, &config).await?,
        "mimo_tts" => mimo_tts(state, &prepared, &config).await?,
        "edge" => edge_tts(&prepared, &config).await?,
        _ => {
            return Err(MediaError::Invalid(format!(
                "不支持 TTS 适配器：{}",
                config.adapter
            )))
        }
    };
    if audio.is_empty() {
        return Err(MediaError::Provider);
    }
    if let Some((sample_rate, channels)) = parse_pcm_content_type(&content_type) {
        return Ok((
            pcm16_to_wav(&audio, sample_rate, channels),
            "audio/wav".into(),
        ));
    }
    Ok((audio, content_type))
}

pub async fn transcribe_audio(
    state: &AppState,
    catalog: &Value,
    profile_id: &str,
    model_id: &str,
    audio: &[u8],
    filename: &str,
    content_type: &str,
) -> Result<String, MediaError> {
    transcribe_audio_with_language(
        state,
        catalog,
        profile_id,
        model_id,
        audio,
        filename,
        content_type,
        None,
    )
    .await
}

pub async fn transcribe_audio_with_language(
    state: &AppState,
    catalog: &Value,
    profile_id: &str,
    model_id: &str,
    audio: &[u8],
    filename: &str,
    content_type: &str,
    language: Option<&str>,
) -> Result<String, MediaError> {
    if audio.is_empty() {
        return Err(MediaError::Invalid("音频样本不能为空。".into()));
    }
    let Some((profile, Some(model))) =
        service_selection(catalog, "stt", Some(profile_id), Some(model_id))
    else {
        return Err(MediaError::Invalid("所选转写模型已不存在。".into()));
    };
    let (provider, adapter, default_base) = canonical_stt_provider(&text(&profile, "binding"));
    let mut config = RuntimeConfig::new(&model, &profile, provider, adapter, default_base)?;
    if let Some(language) = language.map(str::trim).filter(|value| !value.is_empty()) {
        config.language = language.to_string();
    }
    let transcript = match config.adapter.as_str() {
        "openai_compat" => openai_stt(state, audio, filename, content_type, &config).await?,
        "dashscope" => dashscope_stt(state, audio, &config).await?,
        "volcengine" => volcengine_stt(state, audio, &config).await?,
        _ => {
            return Err(MediaError::Invalid(format!(
                "不支持 STT 适配器：{}",
                config.adapter
            )))
        }
    };
    Ok(transcript.trim().to_string())
}

pub async fn generate_image(
    state: &AppState,
    catalog: &Value,
    profile_id: &str,
    model_id: &str,
    prompt: &str,
) -> Result<(Vec<u8>, String), MediaError> {
    let Some((profile, Some(model))) =
        service_selection(catalog, "imagegen", Some(profile_id), Some(model_id))
    else {
        return Err(MediaError::Invalid("所选图像模型已不存在。".into()));
    };
    let (provider, adapter, default_base) =
        canonical_generation_provider(&text(&profile, "binding"), true);
    let config = RuntimeConfig::new(&model, &profile, provider, adapter, default_base)?;
    let result = match config.adapter.as_str() {
        "openai_compat" => openai_image(state, prompt, &config).await?,
        "chat_completions" => chat_image(state, prompt, &config).await?,
        "dashscope" => dashscope_image(state, prompt, &config).await?,
        _ => {
            return Err(MediaError::Invalid(format!(
                "不支持图像生成适配器：{}",
                config.adapter
            )))
        }
    };
    validate_media_result(result, "image")
}

pub async fn generate_video(
    state: &AppState,
    catalog: &Value,
    profile_id: &str,
    model_id: &str,
    prompt: &str,
    progress: Option<mpsc::UnboundedSender<String>>,
) -> Result<(Vec<u8>, String), MediaError> {
    let Some((profile, Some(model))) =
        service_selection(catalog, "videogen", Some(profile_id), Some(model_id))
    else {
        return Err(MediaError::Invalid("所选视频模型已不存在。".into()));
    };
    let (provider, adapter, default_base) =
        canonical_generation_provider(&text(&profile, "binding"), false);
    let config = RuntimeConfig::new(&model, &profile, provider, adapter, default_base)?;
    let result = match config.adapter.as_str() {
        "dashscope" => dashscope_video(state, prompt, &config, progress).await?,
        "async_task" => async_task_video(state, prompt, &config, progress).await?,
        _ => {
            return Err(MediaError::Invalid(format!(
                "不支持视频生成适配器：{}",
                config.adapter
            )))
        }
    };
    validate_media_result(result, "video")
}

async fn openai_tts(
    state: &AppState,
    text: &str,
    config: &RuntimeConfig,
) -> Result<(Vec<u8>, String), MediaError> {
    let endpoint = join_audio_path(&config.base_url, "audio/speech")?;
    let mut payload = json!({
        "model": config.model,
        "input": text,
        "response_format": config.response_format.to_ascii_lowercase()
    });
    if !config.voice.is_empty() {
        payload["voice"] = Value::String(config.voice.clone());
    }
    if let Some(speed) = config.speed {
        payload["speed"] = json!(speed);
    }
    if !config.instructions.is_empty() {
        payload["instructions"] = Value::String(config.instructions.clone());
    }
    let response = authorized_request(&state.client, state.client.post(endpoint), config)?
        .timeout(config.request_timeout)
        .json(&payload)
        .send()
        .await
        .map_err(map_reqwest_error)?;
    ensure_http_success(response.status().as_u16())?;
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    let audio = response.bytes().await.map_err(map_reqwest_error)?.to_vec();
    let mut content_type = if content_type.is_empty() {
        format_content_type(&config.response_format).to_string()
    } else {
        content_type
    };
    if content_type.contains("json") {
        content_type = format_content_type(&config.response_format).to_string();
    }
    Ok((audio, content_type))
}

async fn openrouter_tts(
    state: &AppState,
    text: &str,
    config: &RuntimeConfig,
) -> Result<(Vec<u8>, String), MediaError> {
    match openai_tts(state, text, config).await {
        Ok(audio) => Ok(audio),
        Err(MediaError::HttpStatus(status)) if !matches!(status, 401 | 403 | 429) => {
            let endpoint = join_api_path(&config.base_url, "chat/completions")?;
            let audio_format = if config.response_format.eq_ignore_ascii_case("pcm") {
                "pcm16"
            } else {
                config.response_format.as_str()
            };
            let mut messages = Vec::new();
            if !config.instructions.is_empty() {
                messages.push(json!({
                    "role": "system",
                    "content": config.instructions
                }));
            }
            messages.push(json!({"role": "user", "content": text}));
            let payload = json!({
                "model": config.model,
                "messages": messages,
                "modalities": ["text", "audio"],
                "audio": {"voice": config.voice, "format": audio_format},
                "stream": true
            });
            let response = authorized_request(&state.client, state.client.post(endpoint), config)?
                .timeout(config.request_timeout)
                .json(&payload)
                .send()
                .await
                .map_err(map_reqwest_error)?;
            ensure_http_success(response.status().as_u16())?;
            let body = response.bytes().await.map_err(map_reqwest_error)?;
            let text = String::from_utf8_lossy(&body);
            let mut encoded = String::new();
            for line in text.lines() {
                let Some(data) = line.trim().strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data.is_empty() || data == "[DONE]" {
                    continue;
                }
                let Ok(chunk) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                if let Some(audio) = chunk
                    .pointer("/choices/0/delta/audio/data")
                    .and_then(Value::as_str)
                {
                    encoded.push_str(audio);
                }
            }
            let audio = decode_base64(&encoded)?;
            Ok((audio, format_content_type(audio_format).to_string()))
        }
        Err(error) => Err(error),
    }
}

async fn dashscope_tts(
    state: &AppState,
    text: &str,
    config: &RuntimeConfig,
) -> Result<(Vec<u8>, String), MediaError> {
    let native_audio = config.model.starts_with("qwen-audio-") && config.model.contains("-tts-");
    if config.model.starts_with("qwen") && config.model.contains("realtime") {
        return Err(MediaError::Invalid(
            "不支持实时语音模型，请选择非实时 TTS 模型。".into(),
        ));
    }
    let endpoint = dashscope_tts_url(config, native_audio)?;
    let mut input = json!({"text": text});
    if !config.voice.is_empty() {
        input["voice"] = Value::String(config.voice.clone());
    } else if native_audio {
        return Err(MediaError::Invalid(
            "请填写 Qwen-Audio TTS 支持的语音 ID。".into(),
        ));
    }
    if !config.language.is_empty() {
        if native_audio {
            let language = match config.language.as_str() {
                "Chinese" => "zh",
                "English" => "en",
                "Auto" => "",
                value => value,
            };
            if !language.is_empty() {
                input["language_hints"] = json!([language]);
            }
        } else {
            input["language_type"] = Value::String(config.language.clone());
        }
    }
    if !config.instructions.is_empty() {
        input[if native_audio {
            "instruction"
        } else {
            "instructions"
        }] = Value::String(config.instructions.clone());
    }
    if native_audio {
        let format = config.response_format.to_ascii_lowercase();
        if !matches!(format.as_str(), "mp3" | "wav" | "opus" | "pcm") {
            return Err(MediaError::Invalid(
                "Qwen-Audio TTS 仅支持 MP3、WAV、Opus 和 PCM。".into(),
            ));
        }
        if !matches!(
            config.sample_rate,
            8000 | 12000 | 16000 | 22050 | 24000 | 44100 | 48000
        ) {
            return Err(MediaError::Invalid(
                "当前 Qwen-Audio TTS 采样率不受支持。".into(),
            ));
        }
        input["format"] = Value::String(format);
        input["sample_rate"] = json!(config.sample_rate);
        if let Some(speed) = config.speed {
            if !(0.5..=2.0).contains(&speed) {
                return Err(MediaError::Invalid(
                    "Qwen-Audio TTS 语速必须在 0.5 到 2 之间。".into(),
                ));
            }
            input["rate"] = json!(speed);
        }
    }
    let response = authorized_request(&state.client, state.client.post(endpoint), config)?
        .timeout(config.request_timeout)
        .json(&json!({"model": config.model, "input": input}))
        .send()
        .await
        .map_err(map_reqwest_error)?;
    ensure_http_success(response.status().as_u16())?;
    let payload = response
        .json::<Value>()
        .await
        .map_err(|_| MediaError::Provider)?;
    validate_dashscope_response(&payload)?;
    let audio_url = payload
        .pointer("/output/audio/url")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(MediaError::Provider)?;
    let response = state
        .client
        .get(audio_url)
        .timeout(config.request_timeout)
        .send()
        .await
        .map_err(map_reqwest_error)?;
    ensure_http_success(response.status().as_u16())?;
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.starts_with("audio/"))
        .map(str::to_string)
        .unwrap_or_else(|| format_content_type(&config.response_format).to_string());
    let audio = download_limited(response, 40 * 1024 * 1024).await?;
    if native_audio && config.response_format.eq_ignore_ascii_case("pcm") {
        return Ok((
            audio,
            format!("audio/pcm;rate={};channels=1", config.sample_rate),
        ));
    }
    Ok((audio, content_type))
}

async fn volcengine_tts(
    state: &AppState,
    text: &str,
    config: &RuntimeConfig,
) -> Result<(Vec<u8>, String), MediaError> {
    if config.voice.is_empty() {
        return Err(MediaError::Invalid("请选择火山引擎语音 ID。".into()));
    }
    let format = config.response_format.to_ascii_lowercase();
    if !matches!(format.as_str(), "mp3" | "wav" | "pcm" | "ogg_opus") {
        return Err(MediaError::Invalid(
            "火山引擎 TTS 仅支持 MP3、WAV、PCM 或 OGG Opus。".into(),
        ));
    }
    if !matches!(
        config.sample_rate,
        8000 | 16000 | 22050 | 24000 | 32000 | 44100 | 48000
    ) {
        return Err(MediaError::Invalid("不支持的火山引擎 TTS 采样率。".into()));
    }
    let mut params = json!({
        "format": if format == "wav" { "pcm" } else { &format },
        "sample_rate": config.sample_rate
    });
    if let Some(speed) = config.speed {
        if !(0.5..=2.0).contains(&speed) {
            return Err(MediaError::Invalid(
                "火山引擎语速必须在 0.5 到 2 之间。".into(),
            ));
        }
        params["speech_rate"] = json!(((speed - 1.0) * 100.0).round() as i64);
    }
    let mut request = json!({
        "text": text,
        "speaker": config.voice,
        "audio_params": params
    });
    let mut additions = serde_json::Map::new();
    if !config.language.is_empty() {
        additions.insert(
            "explicit_language".into(),
            Value::String(config.language.clone()),
        );
    }
    if !config.instructions.is_empty() {
        additions.insert("context_texts".into(), json!([config.instructions.clone()]));
    }
    if !additions.is_empty() {
        request["additions"] =
            Value::String(serde_json::to_string(&additions).map_err(|_| MediaError::Provider)?);
    }
    let endpoint = join_audio_path(&config.base_url, "tts/unidirectional/sse")?;
    let mut builder = state.client.post(endpoint).timeout(config.request_timeout);
    builder = volcengine_headers(builder, config, config.resource_id.as_str(), true)?;
    let response = builder
        .json(&json!({"user": {"uid": "deeptutor"}, "req_params": request}))
        .send()
        .await
        .map_err(map_reqwest_error)?;
    ensure_http_success(response.status().as_u16())?;
    let mut stream = response.bytes_stream();
    let mut buffer = Vec::<u8>::new();
    let mut audio = Vec::<u8>::new();
    let mut complete = false;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(map_reqwest_error)?;
        buffer.extend_from_slice(&chunk);
        while let Some(index) = buffer.iter().position(|byte| *byte == b'\n') {
            let line = buffer.drain(..=index).collect::<Vec<_>>();
            let line = String::from_utf8_lossy(&line);
            let Some(data) = line.trim().strip_prefix("data:") else {
                continue;
            };
            let event: Value =
                serde_json::from_str(data.trim()).map_err(|_| MediaError::Provider)?;
            let code = event.get("code").and_then(Value::as_i64).unwrap_or(-1);
            if !matches!(code, 0 | 20_000_000) {
                return Err(MediaError::Provider);
            }
            if let Some(data) = event.get("data").and_then(Value::as_str) {
                if !data.is_empty() {
                    audio.extend_from_slice(&decode_base64(data)?);
                }
            }
            if code == 20_000_000 {
                complete = true;
                break;
            }
        }
        if complete {
            break;
        }
    }
    if !complete {
        return Err(MediaError::Provider);
    }
    if format == "wav" {
        return Ok((
            pcm16_to_wav(&audio, config.sample_rate, 1),
            "audio/wav".into(),
        ));
    }
    let content_type = if format == "pcm" {
        format!("audio/pcm;rate={};channels=1", config.sample_rate)
    } else if format == "ogg_opus" {
        "audio/ogg".into()
    } else {
        "audio/mpeg".into()
    };
    Ok((audio, content_type))
}

async fn minimax_tts(
    state: &AppState,
    text: &str,
    config: &RuntimeConfig,
) -> Result<(Vec<u8>, String), MediaError> {
    if config.api_key.is_empty() || config.api_key == CATALOG_SECRET_MASK {
        return Err(MediaError::Invalid("请填写 MiniMax API Key。".into()));
    }
    if config.voice.is_empty() {
        return Err(MediaError::Invalid("请选择 MiniMax 语音 ID。".into()));
    }
    let format = config.response_format.to_ascii_lowercase();
    if !matches!(format.as_str(), "mp3" | "wav" | "flac" | "pcm") {
        return Err(MediaError::Invalid(
            "MiniMax TTS 仅支持 MP3、WAV、FLAC 或 PCM。".into(),
        ));
    }
    let mut voice = json!({"voice_id": config.voice});
    if let Some(speed) = config.speed {
        if !(0.5..=2.0).contains(&speed) {
            return Err(MediaError::Invalid(
                "MiniMax 语速必须在 0.5 到 2 之间。".into(),
            ));
        }
        voice["speed"] = json!(speed);
    }
    let payload = json!({
        "model": config.model,
        "text": text,
        "stream": false,
        "output_format": "hex",
        "voice_setting": voice,
        "audio_setting": {
            "format": format,
            "sample_rate": config.sample_rate,
            "channel": 1
        },
        "language_boost": if config.language.is_empty() { "auto" } else { &config.language }
    });
    let endpoint = join_audio_path(&config.base_url, "t2a_v2")?;
    let response = authorized_request(&state.client, state.client.post(endpoint), config)?
        .timeout(config.request_timeout)
        .json(&payload)
        .send()
        .await
        .map_err(map_reqwest_error)?;
    ensure_http_success(response.status().as_u16())?;
    let payload = response
        .json::<Value>()
        .await
        .map_err(|_| MediaError::Provider)?;
    if payload
        .pointer("/base_resp/status_code")
        .and_then(Value::as_i64)
        != Some(0)
        || payload.pointer("/data/status").and_then(Value::as_i64) != Some(2)
    {
        return Err(MediaError::Provider);
    }
    let encoded = payload
        .pointer("/data/audio")
        .and_then(Value::as_str)
        .ok_or(MediaError::Provider)?;
    let audio = decode_hex(encoded)?;
    let content_type = match format.as_str() {
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "pcm" => {
            return Ok((
                audio,
                format!("audio/pcm;rate={};channels=1", config.sample_rate),
            ))
        }
        _ => "audio/mpeg",
    };
    Ok((audio, content_type.into()))
}

async fn gemini_tts(
    state: &AppState,
    text: &str,
    config: &RuntimeConfig,
) -> Result<(Vec<u8>, String), MediaError> {
    let model = config
        .model
        .trim_matches('/')
        .strip_prefix("models/")
        .or_else(|| config.model.strip_prefix("google/"))
        .unwrap_or(config.model.trim_matches('/'));
    if config.voice.is_empty() {
        return Err(MediaError::Invalid(
            "请选择 Gemini 预置语音名称，例如 Kore 或 Puck。".into(),
        ));
    }
    let format = if config.response_format.is_empty() {
        "wav"
    } else {
        config.response_format.as_str()
    };
    if !matches!(format, "wav" | "pcm" | "pcm16") {
        return Err(MediaError::Invalid("Gemini TTS 仅支持 WAV 或 PCM。".into()));
    }
    if config.speed.is_some() {
        return Err(MediaError::Invalid("Gemini TTS 不支持语速参数。".into()));
    }
    let spoken = if config.instructions.is_empty() {
        text.to_string()
    } else {
        format!("{}: {text}", config.instructions)
    };
    let mut base = config.base_url.trim_end_matches('/').to_string();
    if let Some(value) = base.strip_suffix("/openai") {
        base = value.to_string();
    }
    let endpoint = format!("{base}/models/{model}:generateContent");
    let mut builder = state.client.post(endpoint).timeout(config.request_timeout);
    if Url::parse(&config.base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .as_deref()
        == Some("generativelanguage.googleapis.com")
    {
        if !config.api_key.is_empty() {
            builder = builder.header("x-goog-api-key", &config.api_key);
        }
    } else {
        builder = auth_headers(builder, config)?;
    }
    builder = extra_header_values(builder, config)?;
    let response = builder
        .json(&json!({
            "contents": [{"parts": [{"text": spoken}]}],
            "generationConfig": {
                "responseModalities": ["AUDIO"],
                "speechConfig": {
                    "voiceConfig": {
                        "prebuiltVoiceConfig": {"voiceName": config.voice}
                    }
                }
            }
        }))
        .send()
        .await
        .map_err(map_reqwest_error)?;
    ensure_http_success(response.status().as_u16())?;
    let payload = response
        .json::<Value>()
        .await
        .map_err(|_| MediaError::Provider)?;
    let mut audio = Vec::new();
    for part in payload
        .pointer("/candidates/0/content/parts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(data) = part.pointer("/inlineData/data").and_then(Value::as_str) {
            audio.extend_from_slice(&decode_base64(data)?);
        }
    }
    if audio.is_empty() {
        return Err(MediaError::Provider);
    }
    if matches!(format, "pcm" | "pcm16") {
        return Ok((
            audio,
            format!("audio/pcm;rate={DASHSCOPE_PCM_RATE};channels=1"),
        ));
    }
    Ok((
        pcm16_to_wav(&audio, DASHSCOPE_PCM_RATE, 1),
        "audio/wav".into(),
    ))
}

async fn mimo_tts(
    state: &AppState,
    text: &str,
    config: &RuntimeConfig,
) -> Result<(Vec<u8>, String), MediaError> {
    if config.model != "mimo-v2.5-tts" {
        return Err(MediaError::Invalid(
            "MiMo 预置语音适配器仅支持 mimo-v2.5-tts。".into(),
        ));
    }
    let format = if config.response_format.eq_ignore_ascii_case("pcm") {
        "pcm16".to_string()
    } else {
        config.response_format.to_ascii_lowercase()
    };
    if !matches!(format.as_str(), "wav" | "pcm16") {
        return Err(MediaError::Invalid("MiMo TTS 仅支持 WAV 或 PCM16。".into()));
    }
    if config.speed.is_some() {
        return Err(MediaError::Invalid(
            "MiMo TTS 请通过语音指令控制语速。".into(),
        ));
    }
    let mut messages = Vec::new();
    if !config.instructions.is_empty() {
        messages.push(json!({"role": "user", "content": config.instructions}));
    }
    messages.push(json!({"role": "assistant", "content": text}));
    let endpoint = join_api_path(&config.base_url, "chat/completions")?;
    let response = authorized_request(&state.client, state.client.post(endpoint), config)?
        .timeout(config.request_timeout)
        .json(&json!({
            "model": config.model,
            "messages": messages,
            "audio": {"format": format, "voice": config.voice},
            "stream": false
        }))
        .send()
        .await
        .map_err(map_reqwest_error)?;
    ensure_http_success(response.status().as_u16())?;
    let payload = response
        .json::<Value>()
        .await
        .map_err(|_| MediaError::Provider)?;
    let encoded = payload
        .pointer("/choices/0/message/audio/data")
        .and_then(Value::as_str)
        .ok_or(MediaError::Provider)?;
    let audio = decode_base64(encoded)?;
    let content_type = if format == "wav" {
        "audio/wav"
    } else {
        "audio/pcm;rate=24000;channels=1"
    };
    Ok((audio, content_type.into()))
}

async fn edge_tts(text: &str, config: &RuntimeConfig) -> Result<(Vec<u8>, String), MediaError> {
    if !config.response_format.eq_ignore_ascii_case("mp3") {
        return Err(MediaError::Invalid("Edge TTS 仅输出 MP3。".into()));
    }
    let rate = edge_rate(config.speed)?;
    let voice = if config.voice.is_empty() {
        "zh-CN-XiaoxiaoNeural"
    } else {
        config.voice.as_str()
    };
    let client = EdgeTtsClient::builder()
        .connect_timeout(Duration::from_secs(10))
        .receive_timeout(config.request_timeout)
        .ws_warmup(false)
        .build()
        .map_err(|_| MediaError::Provider)?;
    let options = SpeakOptions {
        voice: voice.to_string(),
        rate,
        output_format: "audio-24khz-48kbitrate-mono-mp3".into(),
        ..SpeakOptions::default()
    };
    let mut stream = tokio::time::timeout(
        config.request_timeout,
        client.stream(text.to_string(), options),
    )
    .await
    .map_err(|_| MediaError::Timeout)?
    .map_err(|_| MediaError::Provider)?;
    let mut audio = Vec::new();
    while let Some(event) = tokio::time::timeout(config.request_timeout, stream.next())
        .await
        .map_err(|_| MediaError::Timeout)?
    {
        match event.map_err(|_| MediaError::Provider)? {
            SynthesisEvent::Audio(chunk) => audio.extend_from_slice(&chunk),
            SynthesisEvent::Boundary(_) => {}
        }
    }
    Ok((audio, "audio/mpeg".into()))
}

async fn openai_stt(
    state: &AppState,
    audio: &[u8],
    filename: &str,
    content_type: &str,
    config: &RuntimeConfig,
) -> Result<String, MediaError> {
    let endpoint = join_audio_path(&config.base_url, "audio/transcriptions")?;
    let builder = authorized_request(&state.client, state.client.post(endpoint), config)?
        .timeout(config.request_timeout);
    let response = if config.request_style == "base64_json" {
        let format = Path::new(filename)
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("webm");
        let mut body = json!({
            "model": config.model,
            "input_audio": {
                "data": base64::engine::general_purpose::STANDARD.encode(audio),
                "format": format
            }
        });
        if !config.language.is_empty() {
            body["language"] = Value::String(config.language.clone());
        }
        builder.json(&body).send().await
    } else {
        let mime = content_type
            .split(';')
            .next()
            .unwrap_or("application/octet-stream")
            .trim()
            .to_string();
        let part = reqwest::multipart::Part::bytes(audio.to_vec())
            .file_name(filename.to_string())
            .mime_str(&mime)
            .map_err(|_| MediaError::Invalid("音频 MIME 类型无效。".into()))?;
        let mut form = reqwest::multipart::Form::new()
            .part("file", part)
            .text("model", config.model.clone())
            .text("response_format", "json");
        if !config.language.is_empty() {
            form = form.text("language", config.language.clone());
        }
        builder.multipart(form).send().await
    }
    .map_err(map_reqwest_error)?;
    ensure_http_success(response.status().as_u16())?;
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = response.bytes().await.map_err(map_reqwest_error)?;
    if content_type.contains("json") {
        let payload: Value = serde_json::from_slice(&body).map_err(|_| MediaError::Provider)?;
        if let Some(text) = payload.get("text").and_then(Value::as_str) {
            return Ok(text.trim().to_string());
        }
        if let Some(text) = payload
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
        {
            return Ok(text.trim().to_string());
        }
        return Err(MediaError::Provider);
    }
    Ok(String::from_utf8_lossy(&body).trim().to_string())
}

async fn dashscope_stt(
    state: &AppState,
    audio: &[u8],
    config: &RuntimeConfig,
) -> Result<String, MediaError> {
    if config.model == "paraformer-realtime-8k-v1" || config.model == "paraformer-realtime-8k-v2" {
        return Err(MediaError::Invalid(
            "DashScope 8k 实时模型需要 8 kHz 音频，请改用 paraformer-realtime-v2。".into(),
        ));
    }
    if config.api_key.is_empty() {
        return Err(MediaError::Invalid("尚未配置 DashScope API Key。".into()));
    }
    let wav = tokio::task::spawn_blocking({
        let audio = audio.to_vec();
        move || normalize_wav(&audio)
    })
    .await
    .map_err(|_| MediaError::Provider)??;
    let endpoint = dashscope_ws_url(&config.base_url)?;
    let mut request = endpoint
        .into_client_request()
        .map_err(|_| MediaError::Invalid("DashScope WebSocket 地址无效。".into()))?;
    {
        let headers = request.headers_mut();
        headers.insert(
            "Authorization",
            format!("Bearer {}", config.api_key)
                .parse()
                .map_err(|_| MediaError::Provider)?,
        );
        for (name, value) in &config.extra_headers {
            if let (Ok(name), Ok(value)) = (
                name.parse::<tokio_tungstenite::tungstenite::http::HeaderName>(),
                value.parse::<tokio_tungstenite::tungstenite::http::HeaderValue>(),
            ) {
                headers.insert(name, value);
            }
        }
    }
    let (mut socket, _) = tokio::time::timeout(
        config.request_timeout,
        tokio_tungstenite::connect_async(request),
    )
    .await
    .map_err(|_| MediaError::Timeout)?
    .map_err(|_| MediaError::Provider)?;
    let task_id = Uuid::new_v4().simple().to_string();
    let mut parameters = json!({
        "format": "wav",
        "sample_rate": DASHSCOPE_PCM_RATE
    });
    if !config.language.is_empty() {
        parameters["language_hints"] = json!([config.language]);
    }
    let start = json!({
        "header": {
            "task_id": task_id,
            "action": "run-task",
            "streaming": "duplex"
        },
        "payload": {
            "model": config.model,
            "task_group": "audio",
            "task": "asr",
            "function": "recognition",
            "input": {},
            "parameters": parameters
        }
    });
    socket
        .send(Message::Text(start.to_string().into()))
        .await
        .map_err(|_| MediaError::Provider)?;
    let started = tokio::time::timeout(config.request_timeout, socket.next())
        .await
        .map_err(|_| MediaError::Timeout)?
        .ok_or(MediaError::Provider)?
        .map_err(|_| MediaError::Provider)?;
    let Message::Text(started) = started else {
        return Err(MediaError::Provider);
    };
    let started: Value = serde_json::from_str(&started).map_err(|_| MediaError::Provider)?;
    if started.pointer("/header/task_id").and_then(Value::as_str) != Some(task_id.as_str())
        || started.pointer("/header/event").and_then(Value::as_str) != Some("task-started")
    {
        return Err(MediaError::Provider);
    }
    for chunk in wav.chunks(12_800) {
        socket
            .send(Message::Binary(chunk.to_vec().into()))
            .await
            .map_err(|_| MediaError::Provider)?;
    }
    socket
        .send(Message::Text(
            json!({
                "header": {
                    "task_id": task_id,
                    "action": "finish-task",
                    "streaming": "duplex"
                },
                "payload": {"input": {}}
            })
            .to_string()
            .into(),
        ))
        .await
        .map_err(|_| MediaError::Provider)?;
    let mut transcript = String::new();
    loop {
        let message = tokio::time::timeout(config.request_timeout, socket.next())
            .await
            .map_err(|_| MediaError::Timeout)?
            .ok_or(MediaError::Provider)?
            .map_err(|_| MediaError::Provider)?;
        let Message::Text(message) = message else {
            continue;
        };
        let payload: Value = serde_json::from_str(&message).map_err(|_| MediaError::Provider)?;
        match payload.pointer("/header/event").and_then(Value::as_str) {
            Some("result-generated") | Some("task-finished") => {
                append_dashscope_sentence(&mut transcript, payload.pointer("/payload/output"));
                if payload.pointer("/header/event").and_then(Value::as_str) == Some("task-finished")
                {
                    break;
                }
            }
            Some("task-failed") => return Err(MediaError::Provider),
            _ => {}
        }
    }
    let _ = socket.close(None).await;
    Ok(transcript.trim().to_string())
}

async fn volcengine_stt(
    state: &AppState,
    audio: &[u8],
    config: &RuntimeConfig,
) -> Result<String, MediaError> {
    let wav = tokio::task::spawn_blocking({
        let audio = audio.to_vec();
        move || normalize_wav(&audio)
    })
    .await
    .map_err(|_| MediaError::Provider)??;
    let endpoint = join_audio_path(&config.base_url, "auc/bigmodel/recognize/flash")?;
    let mut builder = state.client.post(endpoint).timeout(config.request_timeout);
    let resource = if config.resource_id.is_empty() {
        "volc.bigasr.auc_turbo"
    } else {
        config.resource_id.as_str()
    };
    builder = volcengine_headers(builder, config, resource, false)?;
    builder = builder.header("X-Api-Sequence", "-1");
    let response = builder
        .json(&json!({
            "user": {"uid": "deeptutor"},
            "audio": {
                "data": base64::engine::general_purpose::STANDARD.encode(&wav),
                "format": "wav",
                "rate": DASHSCOPE_PCM_RATE,
                "bits": 16,
                "channel": 1,
                "language": config.language
            },
            "request": {"model_name": config.model, "show_utterances": true}
        }))
        .send()
        .await
        .map_err(map_reqwest_error)?;
    ensure_http_success(response.status().as_u16())?;
    let status = response
        .headers()
        .get("X-Api-Status-Code")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if status == "20000003" {
        return Ok(String::new());
    }
    if status != "20000000" {
        return Err(MediaError::Provider);
    }
    let payload = response
        .json::<Value>()
        .await
        .map_err(|_| MediaError::Provider)?;
    payload
        .pointer("/result/text")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or(MediaError::Provider)
}

async fn openai_image(
    state: &AppState,
    prompt: &str,
    config: &RuntimeConfig,
) -> Result<(Vec<u8>, String), MediaError> {
    let endpoint = join_api_path(&config.base_url, "images/generations")?;
    let mut payload = json!({"model": config.model, "prompt": prompt, "n": 1});
    if !config.size.is_empty() {
        payload["size"] = Value::String(config.size.clone());
    }
    if !config.quality.is_empty() {
        payload["quality"] = Value::String(config.quality.clone());
    }
    if !config.style.is_empty() {
        payload["style"] = Value::String(config.style.clone());
    }
    if !config.response_format.is_empty() {
        payload["response_format"] = Value::String(config.response_format.clone());
    }
    let response = authorized_request(&state.client, state.client.post(endpoint), config)?
        .timeout(config.request_timeout)
        .json(&payload)
        .send()
        .await
        .map_err(map_reqwest_error)?;
    ensure_http_success(response.status().as_u16())?;
    let payload = response
        .json::<Value>()
        .await
        .map_err(|_| MediaError::Provider)?;
    let item = payload
        .get("data")
        .and_then(Value::as_array)
        .and_then(|rows| rows.iter().find(|row| row.is_object()))
        .ok_or(MediaError::Provider)?;
    if let Some(encoded) = item.get("b64_json").and_then(Value::as_str) {
        return Ok((decode_base64(encoded)?, "image/png".into()));
    }
    let url = item
        .get("url")
        .and_then(Value::as_str)
        .ok_or(MediaError::Provider)?;
    download_media(&state.client, url, config.request_timeout).await
}

async fn chat_image(
    state: &AppState,
    prompt: &str,
    config: &RuntimeConfig,
) -> Result<(Vec<u8>, String), MediaError> {
    let endpoint = join_api_path(&config.base_url, "chat/completions")?;
    let response = authorized_request(&state.client, state.client.post(&endpoint), config)?
        .timeout(config.request_timeout)
        .json(&json!({
            "model": config.model,
            "messages": [{"role": "user", "content": prompt}],
            "modalities": ["image", "text"]
        }))
        .send()
        .await
        .map_err(map_reqwest_error)?;
    let response = if response.status() == reqwest::StatusCode::NOT_FOUND {
        authorized_request(&state.client, state.client.post(endpoint), config)?
            .timeout(config.request_timeout)
            .json(&json!({
                "model": config.model,
                "messages": [{"role": "user", "content": prompt}],
                "modalities": ["image"]
            }))
            .send()
            .await
            .map_err(map_reqwest_error)?
    } else {
        response
    };
    ensure_http_success(response.status().as_u16())?;
    let payload = response
        .json::<Value>()
        .await
        .map_err(|_| MediaError::Provider)?;
    let source = first_image_source(&payload).ok_or(MediaError::Provider)?;
    if let Some(encoded) = source.strip_prefix("data:") {
        let (header, data) = encoded.split_once(',').ok_or(MediaError::Provider)?;
        let content_type = header
            .split(';')
            .next()
            .filter(|value| value.starts_with("image/"))
            .ok_or(MediaError::Provider)?;
        return Ok((decode_base64(data)?, content_type.to_string()));
    }
    download_media(&state.client, source, config.request_timeout).await
}

async fn dashscope_image(
    state: &AppState,
    prompt: &str,
    config: &RuntimeConfig,
) -> Result<(Vec<u8>, String), MediaError> {
    let submit = join_api_path(&config.base_url, "services/aigc/text2image/image-synthesis")?;
    let mut parameters = json!({"n": 1});
    if !config.size.is_empty() {
        parameters["size"] = Value::String(config.size.to_ascii_lowercase().replace('x', "*"));
    }
    if !config.style.is_empty() {
        parameters["style"] = Value::String(config.style.clone());
    }
    let response = authorized_request(&state.client, state.client.post(submit), config)?
        .timeout(config.request_timeout)
        .header("X-DashScope-Async", "enable")
        .json(&json!({
            "model": config.model,
            "input": {"prompt": prompt},
            "parameters": parameters
        }))
        .send()
        .await
        .map_err(map_reqwest_error)?;
    ensure_http_success(response.status().as_u16())?;
    let payload = response
        .json::<Value>()
        .await
        .map_err(|_| MediaError::Provider)?;
    validate_dashscope_response(&payload)?;
    let task_id = payload
        .pointer("/output/task_id")
        .and_then(Value::as_str)
        .ok_or(MediaError::Provider)?;
    let poll = join_api_path(&config.base_url, &format!("tasks/{task_id}"))?;
    let deadline = tokio::time::Instant::now() + config.poll_timeout;
    loop {
        let response = authorized_request(&state.client, state.client.get(&poll), config)?
            .timeout(config.request_timeout)
            .send()
            .await
            .map_err(map_reqwest_error)?;
        ensure_http_success(response.status().as_u16())?;
        let payload = response
            .json::<Value>()
            .await
            .map_err(|_| MediaError::Provider)?;
        validate_dashscope_response(&payload)?;
        let status = payload
            .pointer("/output/task_status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase();
        match status.as_str() {
            "succeeded" | "success" => {
                let url = payload
                    .pointer("/output/results/0/url")
                    .and_then(Value::as_str)
                    .ok_or(MediaError::Provider)?;
                return download_media(&state.client, url, config.request_timeout).await;
            }
            "failed" | "canceled" | "cancelled" | "expired" | "unknown" => {
                return Err(MediaError::Provider)
            }
            _ => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(MediaError::Timeout);
        }
        tokio::time::sleep(config.poll_interval).await;
    }
}

async fn async_task_video(
    state: &AppState,
    prompt: &str,
    config: &RuntimeConfig,
    progress: Option<mpsc::UnboundedSender<String>>,
) -> Result<(Vec<u8>, String), MediaError> {
    let submit = join_api_path(&config.base_url, "contents/generations/tasks")?;
    let mut commands = Vec::new();
    if !config.aspect_ratio.is_empty() {
        commands.push(format!("--ratio {}", config.aspect_ratio));
    }
    if !config.resolution.is_empty() {
        commands.push(format!("--resolution {}", config.resolution));
    }
    if !config.duration.is_empty() {
        commands.push(format!("--duration {}", config.duration));
    }
    let text = if commands.is_empty() {
        prompt.to_string()
    } else {
        format!("{prompt} {}", commands.join(" "))
    };
    let response = authorized_request(&state.client, state.client.post(submit), config)?
        .timeout(config.request_timeout)
        .json(&json!({
            "model": config.model,
            "content": [{"type": "text", "text": text}]
        }))
        .send()
        .await
        .map_err(map_reqwest_error)?;
    ensure_http_success(response.status().as_u16())?;
    let payload = response
        .json::<Value>()
        .await
        .map_err(|_| MediaError::Provider)?;
    let task_id = ["id", "task_id"]
        .iter()
        .find_map(|key| payload.get(*key).and_then(Value::as_str))
        .or_else(|| payload.pointer("/data/id").and_then(Value::as_str))
        .ok_or(MediaError::Provider)?;
    send_progress(&progress, "已提交视频任务，正在等待渲染…");
    let poll = join_api_path(
        &config.base_url,
        &format!("contents/generations/tasks/{task_id}"),
    )?;
    let deadline = tokio::time::Instant::now() + config.poll_timeout;
    for attempt in 0_u32.. {
        let response = authorized_request(&state.client, state.client.get(&poll), config)?
            .timeout(config.request_timeout)
            .send()
            .await
            .map_err(map_reqwest_error)?;
        ensure_http_success(response.status().as_u16())?;
        let payload = response
            .json::<Value>()
            .await
            .map_err(|_| MediaError::Provider)?;
        let status = payload
            .get("status")
            .or_else(|| payload.get("state"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase();
        let url = ["content", "data", ""]
            .iter()
            .filter_map(|key| {
                if key.is_empty() {
                    Some(&payload)
                } else {
                    payload.get(*key)
                }
            })
            .find_map(|value| {
                value
                    .get("video_url")
                    .or_else(|| value.get("url"))
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
            });
        if matches!(
            status.as_str(),
            "succeeded" | "success" | "completed" | "done"
        ) {
            let url = url.ok_or(MediaError::Provider)?;
            return download_media(&state.client, url, config.request_timeout).await;
        }
        if matches!(
            status.as_str(),
            "failed" | "error" | "cancelled" | "canceled" | "expired"
        ) {
            return Err(MediaError::Provider);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(MediaError::Timeout);
        }
        if attempt % 3 == 0 {
            send_progress(&progress, "视频仍在渲染，请稍候…");
        }
        tokio::time::sleep(config.poll_interval).await;
    }
    Err(MediaError::Timeout)
}

async fn dashscope_video(
    state: &AppState,
    prompt: &str,
    config: &RuntimeConfig,
    progress: Option<mpsc::UnboundedSender<String>>,
) -> Result<(Vec<u8>, String), MediaError> {
    let submit = join_api_path(
        &config.base_url,
        "services/aigc/video-generation/video-synthesis",
    )?;
    let mut parameters = serde_json::Map::new();
    if let Some(size) = dashscope_video_size(config)? {
        parameters.insert("size".into(), Value::String(size));
    }
    if !config.duration.is_empty() {
        let duration = config
            .duration
            .parse::<u64>()
            .map_err(|_| MediaError::Invalid("DashScope 视频时长必须是整数秒。".into()))?;
        if matches!(
            config.model.as_str(),
            "wanx2.1-t2v-turbo" | "wanx2.1-t2v-plus" | "wan2.2-t2v-plus"
        ) && duration != 5
        {
            return Err(MediaError::Invalid(format!(
                "{} 仅支持 5 秒视频。",
                config.model
            )));
        }
        if !matches!(
            config.model.as_str(),
            "wanx2.1-t2v-turbo" | "wanx2.1-t2v-plus" | "wan2.2-t2v-plus"
        ) {
            parameters.insert("duration".into(), json!(duration));
        }
    }
    let response = authorized_request(&state.client, state.client.post(submit), config)?
        .timeout(config.request_timeout)
        .header("X-DashScope-Async", "enable")
        .json(&json!({
            "model": config.model,
            "input": {"prompt": prompt},
            "parameters": parameters
        }))
        .send()
        .await
        .map_err(map_reqwest_error)?;
    ensure_http_success(response.status().as_u16())?;
    let payload = response
        .json::<Value>()
        .await
        .map_err(|_| MediaError::Provider)?;
    validate_dashscope_response(&payload)?;
    let task_id = payload
        .pointer("/output/task_id")
        .and_then(Value::as_str)
        .ok_or(MediaError::Provider)?;
    send_progress(&progress, "已提交 DashScope 视频任务，正在等待渲染…");
    let poll = join_api_path(&config.base_url, &format!("tasks/{task_id}"))?;
    let deadline = tokio::time::Instant::now() + config.poll_timeout;
    for attempt in 0_u32.. {
        let response = authorized_request(&state.client, state.client.get(&poll), config)?
            .timeout(config.request_timeout)
            .send()
            .await
            .map_err(map_reqwest_error)?;
        ensure_http_success(response.status().as_u16())?;
        let payload = response
            .json::<Value>()
            .await
            .map_err(|_| MediaError::Provider)?;
        validate_dashscope_response(&payload)?;
        let status = payload
            .pointer("/output/task_status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase();
        if matches!(status.as_str(), "succeeded" | "success") {
            let url = payload
                .pointer("/output/video_url")
                .and_then(Value::as_str)
                .ok_or(MediaError::Provider)?;
            return download_media(&state.client, url, config.request_timeout).await;
        }
        if matches!(
            status.as_str(),
            "failed" | "canceled" | "cancelled" | "expired" | "unknown"
        ) {
            return Err(MediaError::Provider);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(MediaError::Timeout);
        }
        if attempt % 3 == 0 {
            send_progress(&progress, "DashScope 视频仍在渲染，请稍候…");
        }
        tokio::time::sleep(config.poll_interval).await;
    }
    Err(MediaError::Timeout)
}

fn canonical_voice_provider(provider: &str) -> (String, String, &'static str) {
    let mut key = provider.trim().to_ascii_lowercase().replace('-', "_");
    key = match key.as_str() {
        "volcengine" | "doubao" => "volcengine_speech",
        "aliyun" | "bailian" => "dashscope",
        "azure" | "aoai" => "azure_openai",
        "openai_compatible" => "custom",
        "lmstudio" => "vllm",
        _ => &key,
    }
    .to_string();
    let value = match key.as_str() {
        "xiaomi_mimo" => ("xiaomi_mimo", "mimo_tts", "https://api.xiaomimimo.com/v1"),
        "minimax" => ("minimax", "minimax", "https://api.minimax.io/v1"),
        "volcengine_speech" => (
            "volcengine_speech",
            "volcengine",
            "https://openspeech.bytedance.com/api/v3",
        ),
        "dashscope" => (
            "dashscope",
            "dashscope",
            "https://dashscope.aliyuncs.com/api/v1",
        ),
        "openai" => ("openai", "openai_compat", "https://api.openai.com/v1"),
        "gemini" => (
            "gemini",
            "gemini",
            "https://generativelanguage.googleapis.com/v1beta",
        ),
        "openrouter" => (
            "openrouter",
            "openrouter_tts",
            "https://openrouter.ai/api/v1",
        ),
        "groq" => ("groq", "openai_compat", "https://api.groq.com/openai/v1"),
        "siliconflow" => (
            "siliconflow",
            "openai_compat",
            "https://api.siliconflow.cn/v1",
        ),
        "azure_openai" => ("azure_openai", "openai_compat", ""),
        "vllm" => ("vllm", "openai_compat", "http://localhost:8000/v1"),
        "edge" => ("edge", "edge", ""),
        _ => ("custom", "openai_compat", ""),
    };
    (value.0.into(), value.1.into(), value.2)
}

fn canonical_stt_provider(provider: &str) -> (String, String, &'static str) {
    let mut key = provider.trim().to_ascii_lowercase().replace('-', "_");
    key = match key.as_str() {
        "volcengine" | "doubao" => "volcengine_speech",
        "aliyun" | "bailian" => "dashscope",
        "azure" | "aoai" => "azure_openai",
        "openai_compatible" => "custom",
        "lmstudio" => "vllm",
        _ => &key,
    }
    .to_string();
    let (adapter, base) = match key.as_str() {
        "volcengine_speech" => ("volcengine", "https://openspeech.bytedance.com/api/v3"),
        "dashscope" => ("dashscope", "https://dashscope.aliyuncs.com/api/v1"),
        "openai" => ("openai_compat", "https://api.openai.com/v1"),
        "openrouter" => ("openai_compat", "https://openrouter.ai/api/v1"),
        "groq" => ("openai_compat", "https://api.groq.com/openai/v1"),
        "siliconflow" => ("openai_compat", "https://api.siliconflow.cn/v1"),
        "azure_openai" => ("openai_compat", ""),
        "vllm" => ("openai_compat", "http://localhost:8000/v1"),
        _ => ("openai_compat", ""),
    };
    (key, adapter.into(), base)
}

fn canonical_generation_provider(provider: &str, image: bool) -> (String, String, &'static str) {
    let mut key = provider.trim().to_ascii_lowercase().replace('-', "_");
    key = match key.as_str() {
        "aliyun" | "bailian" => "dashscope",
        "ark" | "volces" | "doubao" | "seedream" | "seedance" => "volcengine",
        "azure" | "aoai" => "azure_openai",
        "openai_compatible" => "custom",
        _ => &key,
    }
    .to_string();
    if image {
        let (adapter, base) = match key.as_str() {
            "dashscope" => ("dashscope", "https://dashscope.aliyuncs.com/api/v1"),
            "openai" => ("openai_compat", "https://api.openai.com/v1"),
            "volcengine" => ("openai_compat", "https://ark.cn-beijing.volces.com/api/v3"),
            "siliconflow" => ("openai_compat", "https://api.siliconflow.cn/v1"),
            "openrouter" => ("chat_completions", "https://openrouter.ai/api/v1"),
            "azure_openai" => ("openai_compat", ""),
            "custom_chat" => ("chat_completions", ""),
            _ => ("openai_compat", ""),
        };
        (key, adapter.into(), base)
    } else {
        let (adapter, base) = match key.as_str() {
            "dashscope" => ("dashscope", "https://dashscope.aliyuncs.com/api/v1"),
            "volcengine" => ("async_task", "https://ark.cn-beijing.volces.com/api/v3"),
            _ => ("async_task", ""),
        };
        (key, adapter.into(), base)
    }
}

fn default_response_format(adapter: &str) -> &'static str {
    match adapter {
        "edge" | "minimax" | "mimo_tts" => "mp3",
        "gemini" => "wav",
        _ => "mp3",
    }
}

fn discovery_url(base_url: &str, provider: &str) -> Result<String, MediaError> {
    let mut url = Url::parse(base_url).map_err(|_| MediaError::Invalid("服务地址无效。".into()))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(MediaError::Invalid("请检查服务地址后再获取语音。".into()));
    }
    let mut path = url.path().trim_end_matches('/').to_string();
    if provider == "minimax" {
        if let Some(value) = path.strip_suffix("/t2a_v2") {
            path = value.to_string();
        }
        path.push_str("/get_voice");
    } else {
        if path == "/compatible-mode/v1"
            && url
                .host_str()
                .is_some_and(|host| host.ends_with(".aliyuncs.com"))
        {
            path = "/api/v1".into();
        }
        for suffix in [
            "/services/aigc/multimodal-generation/generation",
            "/services/audio/tts/SpeechSynthesizer",
        ] {
            if let Some(value) = path.strip_suffix(suffix) {
                path = value.to_string();
            }
        }
        path.push_str("/services/audio/tts/customization");
    }
    url.set_path(&path);
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.to_string())
}

fn dashscope_tts_url(config: &RuntimeConfig, native_audio: bool) -> Result<String, MediaError> {
    let mut url =
        Url::parse(&config.base_url).map_err(|_| MediaError::Invalid("服务地址无效。".into()))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(MediaError::Invalid(
            "语音合成需要 HTTP 或 HTTPS 服务地址。".into(),
        ));
    }
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    let official = matches!(
        host.as_str(),
        "dashscope.aliyuncs.com" | "dashscope-intl.aliyuncs.com" | "dashscope-us.aliyuncs.com"
    ) || host.ends_with(".maas.aliyuncs.com");
    if native_audio
        && (matches!(
            host.as_str(),
            "dashscope-intl.aliyuncs.com" | "dashscope-us.aliyuncs.com"
        ) || (host.ends_with(".maas.aliyuncs.com")
            && !host.ends_with(".cn-beijing.maas.aliyuncs.com")))
    {
        return Err(MediaError::Invalid(
            "Qwen-Audio TTS 仅在北京区域提供，请改用北京 API Key 和地址。".into(),
        ));
    }
    if official && url.path().trim_end_matches('/') == "/compatible-mode/v1" {
        url.set_path("/api/v1");
    }
    let path = if native_audio {
        "services/audio/tts/SpeechSynthesizer"
    } else {
        "services/aigc/multimodal-generation/generation"
    };
    Ok(join_api_path(url.as_str(), path)?)
}

fn dashscope_ws_url(base_url: &str) -> Result<String, MediaError> {
    let mut url =
        Url::parse(base_url).map_err(|_| MediaError::Invalid("DashScope STT 地址无效。".into()))?;
    if matches!(url.scheme(), "ws" | "wss") {
        return Ok(url.to_string());
    }
    if url.path().contains("/api-ws/") {
        url.set_scheme("wss")
            .map_err(|_| MediaError::Invalid("DashScope STT 地址无效。".into()))?;
        return Ok(url.to_string());
    }
    let host = url
        .host_str()
        .ok_or_else(|| MediaError::Invalid("DashScope STT 地址无效。".into()))?;
    Ok(format!("wss://{host}/api-ws/v1/inference"))
}

fn dashscope_video_size(config: &RuntimeConfig) -> Result<Option<String>, MediaError> {
    if config.resolution.is_empty() && config.aspect_ratio.is_empty() {
        return Ok(None);
    }
    let sizes = dashscope_video_sizes();
    let resolution = config.resolution.trim().to_ascii_lowercase();
    let ratio = if config.aspect_ratio.trim().is_empty() {
        "16:9"
    } else {
        config.aspect_ratio.trim()
    };
    let mut selected_tier = "";
    let mut selected_size = "";
    for (tier, rows) in &sizes {
        if rows.values().any(|value| value == &resolution) {
            if !config.aspect_ratio.is_empty()
                && rows.get(ratio).map(String::as_str) != Some(resolution.as_str())
            {
                return Err(MediaError::Invalid(format!(
                    "DashScope 视频尺寸 {resolution:?} 与画幅比例 {ratio:?} 冲突。"
                )));
            }
            selected_tier = tier;
            selected_size = rows.get(ratio).map(String::as_str).unwrap_or("");
            break;
        }
    }
    if selected_size.is_empty() {
        let model_default = match config.model.as_str() {
            "wanx2.1-t2v-turbo" | "wanx2.1-t2v-plus" => "720p",
            "wan2.2-t2v-plus" | "wan2.5-t2v-preview" | "wan2.6-t2v" => "1080p",
            _ => "",
        };
        selected_tier = if resolution.is_empty() {
            model_default
        } else {
            &resolution
        };
        let rows = sizes.get(selected_tier).ok_or_else(|| {
            MediaError::Invalid(format!(
                "DashScope 视频分辨率或画幅不受支持：{selected_tier:?}, {ratio:?}。"
            ))
        })?;
        selected_size = rows.get(ratio).map(String::as_str).unwrap_or("");
        if selected_size.is_empty() {
            return Err(MediaError::Invalid("DashScope 视频画幅不受支持。".into()));
        }
    }
    let supported = match config.model.as_str() {
        "wanx2.1-t2v-turbo" => Some(["720p", "480p"].as_slice()),
        "wanx2.1-t2v-plus" => Some(["720p"].as_slice()),
        "wan2.2-t2v-plus" => Some(["1080p", "480p"].as_slice()),
        "wan2.5-t2v-preview" => Some(["1080p", "720p", "480p"].as_slice()),
        "wan2.6-t2v" => Some(["1080p", "720p"].as_slice()),
        _ => None,
    };
    if supported.is_some_and(|values| !values.contains(&selected_tier)) {
        return Err(MediaError::Invalid(format!(
            "DashScope 模型 {} 不支持 {selected_tier} 视频。",
            config.model
        )));
    }
    Ok(Some(selected_size.to_string()))
}

fn dashscope_video_sizes() -> BTreeMap<&'static str, BTreeMap<&'static str, String>> {
    let rows: &[(&str, &[(&str, &str)])] = &[
        (
            "480p",
            &[("16:9", "832*480"), ("9:16", "480*832"), ("1:1", "624*624")],
        ),
        (
            "720p",
            &[
                ("16:9", "1280*720"),
                ("9:16", "720*1280"),
                ("1:1", "960*960"),
                ("4:3", "1088*832"),
                ("3:4", "832*1088"),
            ],
        ),
        (
            "1080p",
            &[
                ("16:9", "1920*1080"),
                ("9:16", "1080*1920"),
                ("1:1", "1440*1440"),
                ("4:3", "1632*1248"),
                ("3:4", "1248*1632"),
            ],
        ),
    ];
    rows.iter()
        .map(|(tier, values)| {
            (
                *tier,
                values
                    .iter()
                    .map(|(ratio, size)| (*ratio, size.to_string()))
                    .collect(),
            )
        })
        .collect()
}

fn authorized_request(
    _client: &reqwest::Client,
    builder: reqwest::RequestBuilder,
    config: &RuntimeConfig,
) -> Result<reqwest::RequestBuilder, MediaError> {
    let builder = auth_headers(builder, config)?;
    extra_header_values(builder, config)
}

fn auth_headers(
    mut builder: reqwest::RequestBuilder,
    config: &RuntimeConfig,
) -> Result<reqwest::RequestBuilder, MediaError> {
    if config.api_key.is_empty() || config.api_key == CATALOG_SECRET_MASK {
        return Ok(builder);
    }
    builder = match config.auth_style.as_str() {
        "api_key_header" => builder.header("api-key", &config.api_key),
        "token" => builder.header(
            reqwest::header::AUTHORIZATION,
            format!("Token {}", config.api_key),
        ),
        _ => builder.header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", config.api_key),
        ),
    };
    Ok(builder)
}

fn extra_header_values(
    mut builder: reqwest::RequestBuilder,
    config: &RuntimeConfig,
) -> Result<reqwest::RequestBuilder, MediaError> {
    for (name, value) in &config.extra_headers {
        let name = name
            .parse::<reqwest::header::HeaderName>()
            .map_err(|_| MediaError::Invalid(format!("无效的请求头名称：{name}")))?;
        let value = value
            .parse::<reqwest::header::HeaderValue>()
            .map_err(|_| MediaError::Invalid(format!("请求头 {name} 的值无效。")))?;
        builder = builder.header(name, value);
    }
    Ok(builder)
}

fn volcengine_headers(
    mut builder: reqwest::RequestBuilder,
    config: &RuntimeConfig,
    resource: &str,
    tts: bool,
) -> Result<reqwest::RequestBuilder, MediaError> {
    if config.api_key.is_empty() || config.api_key == CATALOG_SECRET_MASK {
        return Err(MediaError::Invalid(
            "请配置火山引擎语音 API Key 或访问令牌。".into(),
        ));
    }
    builder = extra_header_values(builder, config)?
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(
            "X-Api-Resource-Id",
            if resource.is_empty() {
                &config.model
            } else {
                resource
            },
        )
        .header("X-Api-Request-Id", Uuid::new_v4().to_string());
    if config.app_id.is_empty() {
        builder = builder.header("X-Api-Key", &config.api_key);
    } else {
        builder = builder
            .header(
                if tts { "X-Api-App-Id" } else { "X-Api-App-Key" },
                &config.app_id,
            )
            .header("X-Api-Access-Key", &config.api_key);
    }
    Ok(builder)
}

fn join_api_path(base_url: &str, suffix: &str) -> Result<String, MediaError> {
    let base = base_url.trim();
    if base.is_empty() {
        return Err(MediaError::Invalid("尚未配置服务地址。".into()));
    }
    let (head, query) = base
        .split_once('?')
        .map(|(head, query)| (head, Some(query)))
        .unwrap_or((base, None));
    let suffix = suffix.trim_matches('/');
    if head.trim_end_matches('/').ends_with(&format!("/{suffix}")) {
        return Ok(base.to_string());
    }
    let joined = format!("{}/{}", head.trim_end_matches('/'), suffix);
    Ok(query
        .map(|query| format!("{joined}?{query}"))
        .unwrap_or(joined))
}

fn join_audio_path(base_url: &str, suffix: &str) -> Result<String, MediaError> {
    let base = base_url.trim();
    if base.is_empty() {
        return Err(MediaError::Invalid("尚未配置语音服务地址。".into()));
    }
    let head = base.split('?').next().unwrap_or(base);
    if head.contains("/audio/")
        || head
            .trim_end_matches('/')
            .ends_with(&format!("/{}", suffix.trim_matches('/')))
    {
        return Ok(base.to_string());
    }
    join_api_path(base, suffix)
}

fn ensure_http_success(status: u16) -> Result<(), MediaError> {
    if (200..400).contains(&status) {
        Ok(())
    } else {
        Err(MediaError::HttpStatus(status))
    }
}

fn map_reqwest_error(error: reqwest::Error) -> MediaError {
    if error.is_timeout() {
        MediaError::Timeout
    } else {
        MediaError::Provider
    }
}

async fn download_media(
    client: &reqwest::Client,
    url: &str,
    timeout: Duration,
) -> Result<(Vec<u8>, String), MediaError> {
    let parsed = Url::parse(url).map_err(|_| MediaError::Provider)?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(MediaError::Provider);
    }
    let response = client
        .get(url)
        .timeout(timeout)
        .send()
        .await
        .map_err(map_reqwest_error)?;
    ensure_http_success(response.status().as_u16())?;
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.split(';').next().unwrap_or(value).trim().to_string())
        .unwrap_or_else(|| "application/octet-stream".into());
    let bytes = download_limited(response, MAX_MEDIA_BYTES).await?;
    Ok((bytes, content_type))
}

async fn download_limited(
    response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, MediaError> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(MediaError::Provider);
    }
    let mut stream = response.bytes_stream();
    let mut result = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(map_reqwest_error)?;
        if result.len().saturating_add(chunk.len()) > limit {
            return Err(MediaError::Provider);
        }
        result.extend_from_slice(&chunk);
    }
    Ok(result)
}

fn first_image_source(payload: &Value) -> Option<&str> {
    for choice in payload.get("choices")?.as_array()? {
        let message = choice.get("message")?;
        if let Some(images) = message.get("images").and_then(Value::as_array) {
            for image in images {
                if let Some(source) = image
                    .pointer("/image_url/url")
                    .or_else(|| image.get("url"))
                    .and_then(Value::as_str)
                {
                    return Some(source);
                }
            }
        }
        if let Some(parts) = message.get("content").and_then(Value::as_array) {
            for part in parts {
                if let Some(source) = part
                    .pointer("/image_url/url")
                    .or_else(|| part.get("url"))
                    .and_then(Value::as_str)
                    .filter(|value| value.starts_with("data:image") || value.starts_with("http"))
                {
                    return Some(source);
                }
            }
        }
    }
    None
}

fn validate_media_result(
    (bytes, content_type): (Vec<u8>, String),
    kind: &str,
) -> Result<(Vec<u8>, String), MediaError> {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let allowed = if kind == "image" {
        matches!(
            mime.as_str(),
            "image/png" | "image/jpeg" | "image/webp" | "image/gif"
        )
    } else {
        matches!(
            mime.as_str(),
            "video/mp4" | "video/webm" | "video/quicktime"
        )
    };
    if bytes.is_empty() || bytes.len() > MAX_MEDIA_BYTES || !allowed {
        return Err(MediaError::Provider);
    }
    Ok((bytes, mime))
}

fn parse_pcm_content_type(value: &str) -> Option<(u32, u16)> {
    if !value.to_ascii_lowercase().starts_with("audio/pcm") {
        return None;
    }
    let mut sample_rate = 24000;
    let mut channels = 1;
    for part in value.split(';').skip(1) {
        let Some((name, value)) = part.split_once('=') else {
            continue;
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "rate" => sample_rate = value.trim().parse().unwrap_or(sample_rate),
            "channels" => channels = value.trim().parse().unwrap_or(channels),
            _ => {}
        }
    }
    Some((sample_rate, channels))
}

fn pcm16_to_wav(audio: &[u8], sample_rate: u32, channels: u16) -> Vec<u8> {
    let data_len = audio.len() as u32;
    let byte_rate = sample_rate
        .saturating_mul(channels as u32)
        .saturating_mul(2);
    let block_align = channels.saturating_mul(2);
    let mut wav = Vec::with_capacity(44 + audio.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend_from_slice(audio);
    wav
}

/// 将多种 TTS 返回值统一解码为 16 kHz 单声道 PCM，再拼成一个 WAV。
///
/// 不直接拼接 MP3/Opus 字节，避免编码器延迟、采样率和声道不一致导致
/// 播放时长或音高漂移，也让纯 Rust 发行版无需依赖 ffmpeg。
pub(super) fn join_audio_parts(parts: &[(Vec<u8>, String)]) -> Result<Vec<u8>, MediaError> {
    if parts.is_empty() {
        return Err(MediaError::Invalid("没有可拼接的语音片段。".into()));
    }
    let mut pcm = Vec::<u8>::new();
    for (audio, _) in parts {
        if audio.is_empty() {
            return Err(MediaError::Invalid("语音服务返回了空片段。".into()));
        }
        let normalized = normalize_wav(audio)?;
        if normalized.len() < 44 || &normalized[..4] != b"RIFF" || &normalized[8..12] != b"WAVE" {
            return Err(MediaError::Invalid("语音片段无法归一化为 WAV。".into()));
        }
        pcm.extend_from_slice(&normalized[44..]);
    }
    if pcm.is_empty() {
        return Err(MediaError::Invalid("拼接后的语音为空。".into()));
    }
    Ok(pcm16_to_wav(&pcm, DASHSCOPE_PCM_RATE, 1))
}

fn normalize_wav(audio: &[u8]) -> Result<Vec<u8>, MediaError> {
    if is_canonical_wav(audio) {
        return Ok(audio.to_vec());
    }
    let source = MediaSourceStream::new(Box::new(Cursor::new(audio.to_vec())), Default::default());
    let mut hint = Hint::new();
    for extension in ["wav", "mp3", "m4a", "aac", "flac", "ogg", "opus", "webm"] {
        hint.with_extension(extension);
    }
    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            source,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|_| MediaError::Invalid("无法读取该音频格式。".into()))?;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| MediaError::Invalid("音频中没有可解码轨道。".into()))?;
    let track_id = track.id;
    let codec_params = track
        .codec_params
        .as_ref()
        .and_then(CodecParameters::audio)
        .cloned()
        .ok_or_else(|| MediaError::Invalid("音频中没有可解码轨道。".into()))?;
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&codec_params, &AudioDecoderOptions::default())
        .map_err(|_| MediaError::Invalid("当前音频编码不受支持。".into()))?;
    let mut mono = Vec::<f32>::new();
    let mut input_rate = 0_u32;
    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(symphonia::core::errors::Error::IoError(error))
                if error.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break
            }
            Err(_) => return Err(MediaError::Invalid("音频数据损坏。".into())),
        };
        if packet.track_id != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
            Err(_) => return Err(MediaError::Invalid("音频解码失败。".into())),
        };
        let spec = decoded.spec();
        input_rate = spec.rate();
        let channels = spec.channels().count().max(1);
        let mut samples = Vec::<f32>::with_capacity(decoded.samples_interleaved());
        decoded.copy_to_vec_interleaved(&mut samples);
        for frame in samples.chunks(channels) {
            mono.push(frame.iter().copied().sum::<f32>() / channels as f32);
        }
    }
    if mono.is_empty() || input_rate == 0 {
        return Err(MediaError::Invalid("音频没有可识别的声音。".into()));
    }
    let target_rate = DASHSCOPE_PCM_RATE;
    let target_len = ((mono.len() as u64 * target_rate as u64) / input_rate as u64) as usize;
    let mut pcm = Vec::with_capacity(target_len * 2);
    for index in 0..target_len {
        let source_position = index as f64 * input_rate as f64 / target_rate as f64;
        let left = source_position.floor() as usize;
        let right = (left + 1).min(mono.len() - 1);
        let fraction = (source_position - left as f64) as f32;
        let sample = mono[left.min(mono.len() - 1)] * (1.0 - fraction) + mono[right] * fraction;
        let value = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        pcm.extend_from_slice(&value.to_le_bytes());
    }
    Ok(pcm16_to_wav(&pcm, target_rate, 1))
}

fn is_canonical_wav(audio: &[u8]) -> bool {
    audio.len() >= 44
        && &audio[..4] == b"RIFF"
        && &audio[8..12] == b"WAVE"
        && u32::from_le_bytes(audio[24..28].try_into().unwrap_or_default()) == DASHSCOPE_PCM_RATE
        && u16::from_le_bytes(audio[22..24].try_into().unwrap_or_default()) == 1
        && u16::from_le_bytes(audio[34..36].try_into().unwrap_or_default()) == 16
}

fn validate_dashscope_response(payload: &Value) -> Result<(), MediaError> {
    let invalid_code = payload.get("code").is_some_and(|code| {
        !matches!(code, Value::Null) && code.as_str() != Some("") && code.as_i64() != Some(0)
    });
    if invalid_code || payload.get("success").and_then(Value::as_bool) == Some(false) {
        Err(MediaError::Provider)
    } else {
        Ok(())
    }
}

fn append_dashscope_sentence(target: &mut String, output: Option<&Value>) {
    let Some(output) = output else {
        return;
    };
    let sentence = output.get("sentence");
    let rows = sentence
        .and_then(Value::as_array)
        .cloned()
        .or_else(|| sentence.cloned().map(|value| vec![value]))
        .unwrap_or_default();
    for row in rows {
        if let Some(text) = row.get("text").and_then(Value::as_str) {
            target.push_str(text);
        }
    }
}

fn insert_voice(
    voices: &mut BTreeMap<String, Value>,
    id: &str,
    label: Option<&str>,
    language: Option<&str>,
) {
    if id.trim().is_empty() {
        return;
    }
    let mut value = json!({"id": id, "label": label.unwrap_or(id)});
    if let Some(language) = language.filter(|value| !value.is_empty()) {
        value["languages"] = json!([language]);
    }
    voices.insert(id.to_string(), value);
}

fn format_content_type(format: &str) -> &'static str {
    match format.to_ascii_lowercase().as_str() {
        "opus" => "audio/opus",
        "aac" => "audio/aac",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "pcm" | "pcm16" => "audio/pcm",
        "ogg" | "ogg_opus" => "audio/ogg",
        _ => "audio/mpeg",
    }
}

fn edge_rate(speed: Option<f64>) -> Result<String, MediaError> {
    let Some(speed) = speed else {
        return Ok("+0%".into());
    };
    if !speed.is_finite() || !(0.5..=2.0).contains(&speed) {
        return Err(MediaError::Invalid(
            "Edge 语速必须在 0.5 到 2 之间。".into(),
        ));
    }
    Ok(format!("{:+.0}%", (speed - 1.0) * 100.0))
}

fn parse_optional_speed(value: Option<&Value>) -> Result<Option<f64>, MediaError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_null() || value.as_str().is_some_and(|value| value.trim().is_empty()) {
        return Ok(None);
    }
    let speed = value
        .as_f64()
        .or_else(|| value.as_str().and_then(|value| value.trim().parse().ok()))
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or_else(|| MediaError::Invalid("语速必须是正数。".into()))?;
    Ok(Some(speed))
}

fn parse_tts_timeout(value: Option<&Value>) -> Result<Duration, MediaError> {
    let Some(value) = value else {
        return Ok(Duration::from_secs(60));
    };
    if value.is_null() || value.as_str().is_some_and(|value| value.trim().is_empty()) {
        return Ok(Duration::from_secs(60));
    }
    let seconds = value
        .as_u64()
        .or_else(|| value.as_str().and_then(|value| value.trim().parse().ok()))
        .unwrap_or(0);
    if !(5..=600).contains(&seconds) {
        return Err(MediaError::Invalid(
            "语音请求超时必须是 5 到 600 秒的整数。".into(),
        ));
    }
    Ok(Duration::from_secs(seconds))
}

fn header_map(profile: &Value) -> Result<BTreeMap<String, String>, MediaError> {
    Ok(extra_headers(profile).into_iter().collect())
}

fn text(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

fn decode_base64(value: &str) -> Result<Vec<u8>, MediaError> {
    base64::engine::general_purpose::STANDARD
        .decode(value)
        .map_err(|_| MediaError::Provider)
}

fn decode_hex(value: &str) -> Result<Vec<u8>, MediaError> {
    let bytes = value.as_bytes();
    if bytes.len() % 2 != 0 {
        return Err(MediaError::Provider);
    }
    let mut decoded = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let text = std::str::from_utf8(pair).map_err(|_| MediaError::Provider)?;
        decoded.push(u8::from_str_radix(text, 16).map_err(|_| MediaError::Provider)?);
    }
    Ok(decoded)
}

fn send_progress(sender: &Option<mpsc::UnboundedSender<String>>, message: &str) {
    if let Some(sender) = sender {
        let _ = sender.send(message.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_is_wrapped_in_a_valid_wav_header() {
        let wav = pcm16_to_wav(&[0, 0, 1, 0], 16_000, 1);
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(wav.len(), 48);
    }

    #[test]
    fn api_paths_preserve_query_strings() {
        let value = join_api_path("https://example.com/v1?token=a", "audio/speech").unwrap();
        assert_eq!(value, "https://example.com/v1/audio/speech?token=a");
    }

    #[test]
    fn edge_rate_uses_signed_percent() {
        assert_eq!(edge_rate(Some(1.25)).unwrap(), "+25%");
        assert_eq!(edge_rate(Some(0.8)).unwrap(), "-20%");
    }
}
