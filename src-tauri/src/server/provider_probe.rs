use super::catalog::{self, service_selection, CATALOG_SECRET_MASK};
use super::llm::{self, ApiFormat, LlmChunk, LlmSettings};
use super::{tools, AppState};
use base64::Engine;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

const PROBE_TIMEOUT_SECONDS: u64 = 660;

/// 一次设置页诊断运行的共享状态。
///
/// 事件只保存在当前桌面进程内；诊断结果不会覆盖用户正在编辑的草稿目录，
/// 因此即使探测失败也不会留下半套设置。
pub struct ServiceTestRun {
    service: String,
    events: Mutex<Vec<Value>>,
    done: AtomicBool,
    cancel: CancellationToken,
}

impl ServiceTestRun {
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            events: Mutex::new(Vec::new()),
            done: AtomicBool::new(false),
            cancel: CancellationToken::new(),
        }
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    pub fn snapshot(&self, start: usize) -> Vec<Value> {
        self.events.lock().iter().skip(start).cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.events.lock().len()
    }

    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    pub fn service_name(&self) -> &str {
        &self.service
    }

    fn emit(&self, kind: &str, message: impl Into<String>, mut extra: Value) {
        if !extra.is_object() {
            extra = json!({});
        }
        if let Some(object) = extra.as_object_mut() {
            object.insert("type".into(), Value::String(kind.into()));
            object.insert("message".into(), Value::String(message.into()));
            object.insert("timestamp".into(), Value::from(crate::storage::now()));
        }
        self.events.lock().push(extra);
    }

    fn finish(&self, kind: &str, message: impl Into<String>) {
        self.emit(kind, message, json!({}));
        self.done.store(true, Ordering::Release);
    }
}

pub async fn execute(
    state: AppState,
    run: Arc<ServiceTestRun>,
    catalog: Value,
) -> Arc<ServiceTestRun> {
    let catalog = catalog::resolve_catalog_connections(&catalog);
    let service = run.service.clone();
    run.emit("info", "正在准备配置快照。", json!({}));
    emit_config(&run, &catalog, &service);
    let probe = probe_service(&state, &run, &service, &catalog);
    let result = tokio::select! {
        _ = run.cancel.cancelled() => Err("诊断已取消。".to_string()),
        result = tokio::time::timeout(Duration::from_secs(PROBE_TIMEOUT_SECONDS), probe) => {
            match result {
                Ok(result) => result,
                Err(_) => Err("诊断超时，请检查服务地址、网络和模型配额。".to_string()),
            }
        }
    };
    if run.cancel.is_cancelled() {
        run.finish("failed", "诊断已取消。");
        return run;
    }
    match result {
        Ok(message) => run.finish("completed", message),
        Err(error) => run.finish("failed", error),
    }
    run
}

/// 为一个同步 HTTP 响应执行一次有界探测，并返回可序列化摘要。
pub async fn probe_once(state: AppState, service: &str, catalog: Value) -> Value {
    let run = Arc::new(ServiceTestRun::new(service));
    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(45),
        execute(state, run.clone(), catalog),
    )
    .await;
    if result.is_err() {
        run.cancel();
        run.finish("failed", "诊断超时，请检查服务地址、网络和模型配额。");
    }
    let events = run.snapshot(0);
    let terminal = events.iter().rev().find_map(|event| {
        let kind = event.get("type").and_then(Value::as_str)?;
        matches!(kind, "completed" | "failed").then_some(event)
    });
    let ok = terminal
        .and_then(|event| event.get("type"))
        .and_then(Value::as_str)
        == Some("completed");
    let message = terminal
        .and_then(|event| event.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("诊断未返回结果。");
    json!({
        "ok": ok,
        "message": message,
        "error": if ok { Value::Null } else { Value::String(message.to_string()) },
        "response_time_ms": started.elapsed().as_secs_f64() * 1000.0,
        "events": events
    })
}

fn emit_config(run: &ServiceTestRun, catalog: &Value, service: &str) {
    let Some((profile, model)) = service_selection(catalog, service, None, None) else {
        return;
    };
    let binding = profile
        .get("binding")
        .or_else(|| profile.get("provider"))
        .and_then(Value::as_str)
        .unwrap_or("");
    run.emit(
        "config",
        "正在使用当前选中的服务配置。",
        json!({
            "profile": {
                "name": profile.get("name").cloned().unwrap_or(Value::Null),
                "base_url": profile.get("base_url").cloned().unwrap_or(Value::Null),
                "binding": binding,
                "api_key": redact_secret(
                    profile.get("api_key").and_then(Value::as_str).unwrap_or("")
                ),
                "api_version": profile.get("api_version").cloned().unwrap_or(Value::Null)
            },
            "model": model
        }),
    );
}

async fn probe_service(
    state: &AppState,
    run: &ServiceTestRun,
    service: &str,
    catalog: &Value,
) -> Result<String, String> {
    match service {
        "llm" | "task" => probe_llm(state, run, service, catalog).await,
        "embedding" => probe_embedding(state, run, catalog).await,
        "search" => probe_search(state, run, catalog).await,
        "tts" => probe_tts(state, run, catalog).await,
        "stt" => probe_stt(state, run, catalog).await,
        "imagegen" => probe_imagegen(state, run, catalog).await,
        "videogen" => probe_videogen(state, run, catalog).await,
        _ => Err(format!("不支持的诊断服务：{service}")),
    }
}

async fn probe_llm(
    state: &AppState,
    run: &ServiceTestRun,
    service: &str,
    catalog: &Value,
) -> Result<String, String> {
    let effective_service = if service == "task" && !service_configured(catalog, "task") {
        run.emit(
            "info",
            "未配置全局任务模型，后台任务将继承主语言模型。",
            json!({}),
        );
        return Ok("任务模型未单独配置，已确认继承主语言模型。".to_string());
    } else {
        service
    };
    let (profile, model) = service_selection(catalog, effective_service, None, None)
        .ok_or_else(|| "尚未配置可用的语言模型。".to_string())?;
    let model = model.ok_or_else(|| "当前语言模型配置缺少模型。".to_string())?;
    let settings = llm_settings(&profile, &model)?;
    run.emit(
        "info",
        format!(
            "已解析模型 `{}`，协议为 `{}`。",
            settings.model,
            match settings.api_format {
                ApiFormat::OpenAiChat => "OpenAI Chat",
                ApiFormat::OpenAiResponses => "OpenAI Responses",
                ApiFormat::Anthropic => "Anthropic Messages",
            }
        ),
        json!({}),
    );
    run.emit(
        "info",
        format!("请求目标：{}", settings.base_url),
        json!({}),
    );
    let messages = vec![
        json!({"role": "system", "content": "这是一次连接诊断。请简短回答。"}),
        json!({"role": "user", "content": "Reply with OK."}),
    ];
    let response = llm::stream_completion(
        &state.client,
        &settings,
        &messages,
        &[],
        None,
        &run.cancel,
        |_chunk: LlmChunk| {},
    )
    .await?;
    let content = response.content.trim();
    if content.is_empty() {
        return Err("语言模型返回了空响应。".to_string());
    }
    run.emit(
        "response",
        "已收到语言模型响应。",
        json!({
            "snippet": truncate(content, 400),
            "usage": response.usage.to_json(0.0)
        }),
    );
    if service == "llm" {
        let (context_window, source, detail) = context_window(&model, &settings.model);
        run.emit(
            "context_window",
            if source == "default" {
                format!("上下文容量未知，暂按保守值 {context_window} tokens 计算。")
            } else {
                format!("检测到上下文窗口 {context_window} tokens（{source}）。")
            },
            json!({
                "context_window": context_window,
                "source": source,
                "detail": detail,
                "detected_at": crate::storage::now()
            }),
        );
    }
    if service == "task" {
        Ok("任务模型测试成功；标题与输入提示会复用此路径。".to_string())
    } else {
        Ok("语言模型连接、鉴权和流式响应均正常。".to_string())
    }
}

async fn probe_embedding(
    state: &AppState,
    run: &ServiceTestRun,
    catalog: &Value,
) -> Result<String, String> {
    let (profile, model) = service_selection(catalog, "embedding", None, None)
        .ok_or_else(|| "尚未配置可用的向量模型。".to_string())?;
    let model = model.ok_or_else(|| "当前向量模型配置缺少模型。".to_string())?;
    let model_name = model
        .get("model")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "向量模型缺少模型标识。".to_string())?;
    let endpoint = embedding_endpoint(&profile)?;
    run.emit("info", format!("请求目标：{endpoint}"), json!({}));
    run.emit(
        "info",
        "正在使用两条文本探测批量向量接口，不发送 dimensions 参数。",
        json!({}),
    );
    let body = json!({
        "model": model_name,
        "input": [
            "DeepTutor embedding smoke test",
            "DeepTutor retrieval batch probe"
        ]
    });
    let mut request = state.client.post(&endpoint).json(&body);
    if let Some(api_key) = profile
        .get("api_key")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && *value != CATALOG_SECRET_MASK)
    {
        request = request.bearer_auth(api_key);
    }
    for (name, value) in catalog::extra_headers(&profile) {
        request = request.header(name, value);
    }
    let response = tokio::select! {
        _ = run.cancel.cancelled() => return Err("诊断已取消。".to_string()),
        response = request.send() => response.map_err(|error| format!("无法连接向量服务：{error}"))?
    };
    let status = response.status();
    let payload = response
        .json::<Value>()
        .await
        .map_err(|error| format!("向量服务返回了无效 JSON：{error}"))?;
    if !status.is_success() {
        return Err(provider_http_error("向量服务", status.as_u16(), &payload));
    }
    let vectors = parse_embedding_vectors(&payload)
        .ok_or_else(|| "向量服务响应中没有可读取的向量。".to_string())?;
    if vectors.len() != 2 {
        return Err(format!(
            "向量服务返回的向量数量不正确：预期 2，实际 {}。",
            vectors.len()
        ));
    }
    if vectors.iter().any(Vec::is_empty) {
        return Err("向量服务返回了空向量。".to_string());
    }
    let detected_dim = vectors[0].len();
    if vectors.iter().any(|vector| vector.len() != detected_dim) {
        return Err("向量服务返回的批次维度不一致。".to_string());
    }
    let configured_dim = model.get("dimension").and_then(value_as_usize).unwrap_or(0);
    let supported = supported_dimensions(&model);
    run.emit(
        "capabilities",
        if configured_dim > 0 && configured_dim != detected_dim {
            format!("探测返回 {detected_dim} 维；当前目录记录 {configured_dim} 维，应用前请确认。")
        } else {
            format!("探测返回 {detected_dim} 维。")
        },
        json!({
            "detected_dim": detected_dim,
            "default_dim": detected_dim,
            "supported_dimensions": supported,
            "supports_variable_dimensions": !supported.is_empty(),
            "model_known": !supported.is_empty(),
            "active_dim": detected_dim,
            "active_dim_source": "detected"
        }),
    );
    run.emit(
        "response",
        "已收到批量向量响应。",
        json!({
            "actual_dimension": detected_dim,
            "expected_dimension": if configured_dim > 0 { Value::from(configured_dim) } else { Value::Null },
            "vector_count": vectors.len()
        }),
    );
    Ok(format!(
        "向量服务连接正常，两条测试文本均返回 {detected_dim} 维向量。"
    ))
}

async fn probe_search(
    state: &AppState,
    run: &ServiceTestRun,
    catalog: &Value,
) -> Result<String, String> {
    run.emit("info", "正在执行真实搜索探测。", json!({}));
    let results =
        tools::search_with_catalog_strict("DeepTutor health check", 5, state, catalog).await?;
    let citations = results
        .iter()
        .filter_map(|row| row.get("url").and_then(Value::as_str))
        .filter(|url| !url.trim().is_empty())
        .take(5)
        .map(str::to_string)
        .collect::<Vec<_>>();
    run.emit(
        "response",
        "已收到搜索结果。",
        json!({
            "search_result_count": results.len(),
            "citation_count": citations.len(),
            "citations": citations,
            "answer_preview": results.first().and_then(|row| row.get("snippet")).cloned().unwrap_or(Value::Null)
        }),
    );
    Ok(format!("搜索服务连接正常，返回 {} 条结果。", results.len()))
}

async fn probe_tts(
    state: &AppState,
    run: &ServiceTestRun,
    catalog: &Value,
) -> Result<String, String> {
    let (profile, model) = service_selection(catalog, "tts", None, None)
        .ok_or_else(|| "尚未配置可用的语音合成模型。".to_string())?;
    let model = model.ok_or_else(|| "当前语音合成配置缺少模型。".to_string())?;
    let profile_id = profile
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| "语音合成配置缺少 ID。".to_string())?;
    let model_id = model
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| "语音合成模型缺少 ID。".to_string())?;
    run.emit("info", "正在合成一段短试听。", json!({}));
    let (audio, content_type) = super::media_services::synthesize_tts(
        state,
        catalog,
        profile_id,
        model_id,
        "DeepTutor voice check. 这是一段语音合成测试。",
    )
    .await
    .map_err(|error| error.public_message())?;
    if audio.is_empty() {
        return Err("语音服务返回了空音频。".to_string());
    }
    run.emit(
        "response",
        format!("已收到 {} 字节 {} 音频。", audio.len(), content_type),
        json!({
            "audio_base64": base64::engine::general_purpose::STANDARD.encode(&audio),
            "content_type": content_type,
            "bytes": audio.len()
        }),
    );
    Ok("语音合成服务连接和鉴权正常。".to_string())
}

async fn probe_stt(
    state: &AppState,
    run: &ServiceTestRun,
    catalog: &Value,
) -> Result<String, String> {
    let (profile, model) = service_selection(catalog, "stt", None, None)
        .ok_or_else(|| "尚未配置可用的语音转写模型。".to_string())?;
    let model = model.ok_or_else(|| "当前语音转写配置缺少模型。".to_string())?;
    let profile_id = profile
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| "语音转写配置缺少 ID。".to_string())?;
    let model_id = model
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| "语音转写模型缺少 ID。".to_string())?;
    let wav = silent_wav()?;
    run.emit("info", "正在上传 1 秒静音探测音频。", json!({}));
    let transcript = super::media_services::transcribe_audio(
        state,
        catalog,
        profile_id,
        model_id,
        &wav,
        "probe.wav",
        "audio/wav",
    )
    .await
    .map_err(|error| error.public_message())?;
    run.emit(
        "response",
        "转写接口已返回响应。",
        json!({
            "snippet": if transcript.trim().is_empty() {
                "（静音样本返回空文本，符合预期）".to_string()
            } else {
                truncate(&transcript, 200)
            }
        }),
    );
    Ok("语音转写端点、鉴权和上传链路正常；静音样本不评估识别质量。".to_string())
}

async fn probe_imagegen(
    state: &AppState,
    run: &ServiceTestRun,
    catalog: &Value,
) -> Result<String, String> {
    let (profile, model) = service_selection(catalog, "imagegen", None, None)
        .ok_or_else(|| "尚未配置可用的图像生成模型。".to_string())?;
    let model = model.ok_or_else(|| "当前图像生成配置缺少模型。".to_string())?;
    let profile_id = profile
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| "图像生成配置缺少 ID。".to_string())?;
    let model_id = model
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| "图像生成模型缺少 ID。".to_string())?;
    run.emit(
        "info",
        "正在生成一张测试图，这会消耗一次真实配额。",
        json!({}),
    );
    let (image, content_type) = super::media_services::generate_image(
        state,
        catalog,
        profile_id,
        model_id,
        "A small minimalist test icon of a blue book on a white background.",
    )
    .await
    .map_err(|error| error.public_message())?;
    if image.is_empty() {
        return Err("图像服务返回了空文件。".to_string());
    }
    run.emit(
        "response",
        format!("已收到 {} 字节 {} 图像。", image.len(), content_type),
        json!({
            "image_base64": base64::engine::general_purpose::STANDARD.encode(&image),
            "content_type": content_type,
            "bytes": image.len()
        }),
    );
    Ok("图像生成服务连接、鉴权和返回格式正常。".to_string())
}

async fn probe_videogen(
    state: &AppState,
    run: &ServiceTestRun,
    catalog: &Value,
) -> Result<String, String> {
    let (profile, model) = service_selection(catalog, "videogen", None, None)
        .ok_or_else(|| "尚未配置可用的视频生成模型。".to_string())?;
    let model = model.ok_or_else(|| "当前视频生成配置缺少模型。".to_string())?;
    let profile_id = profile
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| "视频生成配置缺少 ID。".to_string())?;
    let model_id = model
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| "视频生成模型缺少 ID。".to_string())?;
    run.emit(
        "info",
        "正在提交并等待一段短视频探测，这会消耗一次真实配额。",
        json!({}),
    );
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let future = super::media_services::generate_video(
        state,
        catalog,
        profile_id,
        model_id,
        "A short test clip of a calm ocean wave.",
        Some(progress_tx),
    );
    tokio::pin!(future);
    let (video, content_type) = loop {
        tokio::select! {
            _ = run.cancel.cancelled() => return Err("诊断已取消。".to_string()),
            result = &mut future => {
                break result.map_err(|error| error.public_message())?;
            }
            message = progress_rx.recv() => {
                if let Some(message) = message {
                    run.emit("info", message, json!({}));
                }
            }
        }
    };
    if video.is_empty() {
        return Err("视频服务返回了空文件。".to_string());
    }
    run.emit(
        "response",
        format!("已收到 {} 字节 {} 视频。", video.len(), content_type),
        json!({
            "content_type": content_type,
            "bytes": video.len()
        }),
    );
    Ok("视频任务已提交并成功生成可下载文件。".to_string())
}

fn service_configured(catalog: &Value, service: &str) -> bool {
    service_selection(catalog, service, None, None).is_some()
}

pub(super) fn llm_settings(profile: &Value, model: &Value) -> Result<LlmSettings, String> {
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
        .ok_or_else(|| "当前模型缺少模型标识。".to_string())?
        .to_string();
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
        headers: catalog::extra_headers(profile),
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

fn embedding_endpoint(profile: &Value) -> Result<String, String> {
    let base = profile
        .get("base_url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .trim_end_matches('/');
    if base.is_empty() {
        return Err("向量服务缺少服务地址。".to_string());
    }
    if base.ends_with("/embeddings") {
        return Ok(base.to_string());
    }
    if base.ends_with("/v1")
        || base.ends_with("/api/paas/v4")
        || base.ends_with("/openai")
        || base.contains("/compatible-mode/")
    {
        return Ok(format!("{base}/embeddings"));
    }
    Ok(format!("{base}/v1/embeddings"))
}

fn parse_embedding_vectors(payload: &Value) -> Option<Vec<Vec<f64>>> {
    let rows = payload
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| payload.get("embeddings").and_then(Value::as_array))
        .or_else(|| {
            payload
                .pointer("/output/embeddings")
                .and_then(Value::as_array)
        })?;
    let mut vectors = Vec::with_capacity(rows.len());
    for row in rows {
        let raw = if row.is_array() {
            row.as_array()?
        } else {
            row.get("embedding")
                .or_else(|| row.get("vector"))
                .and_then(Value::as_array)?
        };
        let vector = raw
            .iter()
            .map(|value| value.as_f64())
            .collect::<Option<Vec<_>>>()?;
        vectors.push(vector);
    }
    Some(vectors)
}

fn supported_dimensions(model: &Value) -> Vec<usize> {
    match model.get("supported_dimensions") {
        Some(Value::Array(values)) => values.iter().filter_map(value_as_usize).collect(),
        Some(Value::String(value)) => value
            .split(',')
            .filter_map(|part| part.trim().parse::<usize>().ok())
            .filter(|value| *value > 0)
            .collect(),
        _ => Vec::new(),
    }
}

fn value_as_usize(value: &Value) -> Option<usize> {
    value
        .as_u64()
        .map(|value| value as usize)
        .or_else(|| value.as_str()?.trim().parse::<usize>().ok())
}

fn context_window(model: &Value, model_name: &str) -> (u64, &'static str, String) {
    for key in [
        "context_window",
        "context_length",
        "max_context_tokens",
        "max_input_tokens",
    ] {
        if let Some(value) = model.get(key).and_then(value_as_u64) {
            if value > 0 {
                return (value, "metadata", format!("来自模型目录字段 {key}。"));
            }
        }
    }
    let lower = model_name.to_ascii_lowercase();
    let known = if lower.contains("gemini-1.5") || lower.contains("gemini-2") {
        Some(1_048_576)
    } else if lower.contains("claude") {
        Some(200_000)
    } else if lower.contains("gpt-4.1") {
        Some(1_047_576)
    } else if lower.contains("gpt-4o") {
        Some(128_000)
    } else if lower.contains("deepseek") {
        Some(128_000)
    } else if lower.contains("qwen") {
        Some(131_072)
    } else if lower.contains("llama-4") {
        Some(1_048_576)
    } else {
        None
    };
    match known {
        Some(value) => (
            value,
            "model_metadata",
            "根据已知模型系列推断，应用前可手动覆盖。".to_string(),
        ),
        None => (
            32_768,
            "default",
            "服务未返回上下文窗口，采用保守预算。".to_string(),
        ),
    }
}

fn value_as_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str()?.trim().parse::<u64>().ok())
}

fn silent_wav() -> Result<Vec<u8>, String> {
    let data_size = 16_000u32 * 2;
    let mut wav = Vec::with_capacity(44 + data_size as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_size).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&16_000u32.to_le_bytes());
    wav.extend_from_slice(&32_000u32.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_size.to_le_bytes());
    wav.resize(44 + data_size as usize, 0);
    Ok(wav)
}

fn provider_http_error(label: &str, status: u16, payload: &Value) -> String {
    let detail = payload
        .pointer("/error/message")
        .or_else(|| payload.get("message"))
        .or_else(|| payload.get("detail"))
        .and_then(Value::as_str)
        .unwrap_or("服务未提供错误详情");
    format!("{label}返回 HTTP {status}：{}", truncate(detail, 400))
}

fn redact_secret(value: &str) -> String {
    if value.is_empty() {
        return "(empty)".to_string();
    }
    if value.chars().count() <= 8 {
        "****".to_string()
    } else {
        let start = value.chars().take(4).collect::<String>();
        let end = value
            .chars()
            .rev()
            .take(4)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();
        format!("{start}...{end}")
    }
}

fn truncate(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_openai_embedding_batches() {
        let payload = json!({
            "data": [
                {"embedding": [0.1, 0.2, 0.3]},
                {"embedding": [0.4, 0.5, 0.6]}
            ]
        });
        let vectors = parse_embedding_vectors(&payload).expect("解析向量");
        assert_eq!(vectors.len(), 2);
        assert_eq!(vectors[0].len(), 3);
    }

    #[test]
    fn silent_wav_has_valid_header() {
        let wav = silent_wav().expect("生成探测音频");
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(wav.len(), 32_044);
    }

    #[test]
    fn linked_connection_address_wins_for_llm_probe() {
        let catalog = json!({
            "connections": [{
                "id": "conn-tju",
                "api_key": "linked-key",
                "base_url": "https://gateway.example/v1"
            }],
            "services": {
                "llm": {
                    "active_profile_id": "p1",
                    "active_model_id": "m1",
                    "profiles": [{
                        "id": "p1",
                        "connection_id": "conn-tju",
                        "binding": "openai",
                        "base_url": "https://api.openai.com/v1",
                        "api_key": "stale-key",
                        "models": [{"id": "m1", "model": "deepseek-v4.1-flash"}]
                    }]
                }
            }
        });
        let (profile, model) =
            service_selection(&catalog, "llm", None, None).expect("读取语言模型配置");
        let settings = llm_settings(&profile, &model.expect("模型")).expect("解析模型设置");

        assert_eq!(settings.base_url, "https://gateway.example/v1");
        assert_eq!(settings.api_key, "linked-key");
    }
}
