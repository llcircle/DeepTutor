use futures_util::StreamExt;
use reqwest::header::{HeaderName, HeaderValue, CONTENT_TYPE};
use reqwest::Client;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
pub struct LlmSettings {
    pub provider: String,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub api_format: ApiFormat,
    pub headers: Vec<(String, String)>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
}

fn reasoning_model(settings: &LlmSettings) -> bool {
    let name = settings.model.to_ascii_lowercase();
    let name = name.rsplit('/').next().unwrap_or(&name);
    ["gpt-5", "o1", "o3", "o4"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApiFormat {
    OpenAiChat,
    OpenAiResponses,
    Anthropic,
}

impl ApiFormat {
    pub fn from_catalog(api_format: Option<&str>, wire_api: Option<&str>) -> Self {
        match api_format.unwrap_or_default() {
            "openai_responses" => Self::OpenAiResponses,
            "anthropic" => Self::Anthropic,
            "openai_chat" => Self::OpenAiChat,
            _ => match wire_api.unwrap_or_default() {
                "responses" => Self::OpenAiResponses,
                "chat_completions" => Self::OpenAiChat,
                _ => Self::OpenAiChat,
            },
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct LlmUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub reasoning_tokens: u64,
    pub cache_input_tokens: u64,
    pub cache_reported_calls: u64,
}

impl LlmUsage {
    fn merge(&mut self, other: &Self) {
        self.prompt_tokens = self.prompt_tokens.max(other.prompt_tokens);
        self.completion_tokens = self.completion_tokens.max(other.completion_tokens);
        self.total_tokens = self.total_tokens.max(other.total_tokens);
        self.cache_read_tokens = self.cache_read_tokens.max(other.cache_read_tokens);
        self.cache_creation_tokens = self.cache_creation_tokens.max(other.cache_creation_tokens);
        self.reasoning_tokens = self.reasoning_tokens.max(other.reasoning_tokens);
        self.cache_input_tokens = self.cache_input_tokens.max(other.cache_input_tokens);
        self.cache_reported_calls = self.cache_reported_calls.max(other.cache_reported_calls);
    }

    /// 将多个模型请求的用量累加成一个回合级汇总。
    pub fn accumulate(&mut self, other: &Self) {
        self.prompt_tokens += other.prompt_tokens;
        self.completion_tokens += other.completion_tokens;
        self.total_tokens += if other.total_tokens > 0 {
            other.total_tokens
        } else {
            other.prompt_tokens + other.completion_tokens
        };
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_creation_tokens += other.cache_creation_tokens;
        self.reasoning_tokens += other.reasoning_tokens;
        self.cache_input_tokens += other.cache_input_tokens;
        self.cache_reported_calls += other.cache_reported_calls;
    }

    pub fn to_json(&self, duration_seconds: f64) -> Value {
        self.summary_json(duration_seconds, 1)
    }

    pub fn summary_json(&self, duration_seconds: f64, total_calls: u64) -> Value {
        let prompt_tokens = self.prompt_tokens;
        let completion_tokens = self.completion_tokens;
        let total_tokens = if self.total_tokens > 0 {
            self.total_tokens
        } else {
            prompt_tokens + completion_tokens
        };
        json!({
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": total_tokens,
            "cache_read_input_tokens": self.cache_read_tokens,
            "cache_creation_input_tokens": self.cache_creation_tokens,
            "cache_input_tokens": self.cache_input_tokens,
            "cache_reported_calls": self.cache_reported_calls,
            "reasoning_tokens": self.reasoning_tokens,
            "total_calls": total_calls,
            "duration_seconds": duration_seconds,
            "generation_seconds": 0,
            "timed_completion_tokens": 0,
            "tokens_per_second": Value::Null
        })
    }
}

#[derive(Clone, Debug)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Clone, Debug, Default)]
pub struct LlmResponse {
    pub content: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: LlmUsage,
}

#[derive(Clone, Debug)]
pub enum LlmChunk {
    Content(String),
    Thinking(String),
    Usage(LlmUsage),
}

/// 发起一次支持三种主流线协议的流式请求，并把可见文本增量回调给回合引擎。
pub async fn stream_completion<F>(
    client: &Client,
    settings: &LlmSettings,
    messages: &[Value],
    tools: &[Value],
    tool_choice: Option<&str>,
    cancel: &CancellationToken,
    mut on_chunk: F,
) -> Result<LlmResponse, String>
where
    F: FnMut(LlmChunk),
{
    let started = Instant::now();
    let result = match settings.api_format {
        ApiFormat::OpenAiChat => {
            openai_chat_completion(
                client,
                settings,
                messages,
                tools,
                tool_choice,
                cancel,
                &mut on_chunk,
            )
            .await
        }
        ApiFormat::OpenAiResponses => {
            openai_responses_completion(
                client,
                settings,
                messages,
                tools,
                tool_choice,
                cancel,
                &mut on_chunk,
            )
            .await
        }
        ApiFormat::Anthropic => {
            anthropic_completion(
                client,
                settings,
                messages,
                tools,
                tool_choice,
                cancel,
                &mut on_chunk,
            )
            .await
        }
    }?;
    let mut result = result;
    if result.usage.total_tokens == 0 {
        result.usage.total_tokens = result.usage.prompt_tokens + result.usage.completion_tokens;
    }
    on_chunk(LlmChunk::Usage(result.usage.clone()));
    tracing::debug!(
        provider = %settings.provider,
        model = %settings.model,
        elapsed_ms = started.elapsed().as_millis(),
        "模型流式请求完成"
    );
    Ok(result)
}

async fn openai_chat_completion<F>(
    client: &Client,
    settings: &LlmSettings,
    messages: &[Value],
    tools: &[Value],
    tool_choice: Option<&str>,
    cancel: &CancellationToken,
    on_chunk: &mut F,
) -> Result<LlmResponse, String>
where
    F: FnMut(LlmChunk),
{
    let endpoint = endpoint(
        &settings.base_url,
        &["/chat/completions", "/v1/chat/completions"],
    )?;
    let mut body = json!({
        "model": settings.model,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true}
    });
    if let Some(value) = settings.temperature.filter(|_| !reasoning_model(settings)) {
        body["temperature"] = json!(value);
    }
    if let Some(value) = settings.max_tokens {
        body[if reasoning_model(settings) {
            "max_completion_tokens"
        } else {
            "max_tokens"
        }] = json!(value);
    }
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools.to_vec());
        body["tool_choice"] = Value::String(tool_choice.unwrap_or("auto").to_string());
    }
    let request = authorized_request(client.post(endpoint), settings)?
        .header(CONTENT_TYPE, "application/json")
        .json(&body);
    let response = send_request(request, cancel).await?;
    let response = ensure_success(response, "OpenAI Chat Completions").await?;

    let mut result = LlmResponse::default();
    let mut calls: BTreeMap<usize, ToolCallAccumulator> = BTreeMap::new();
    read_sse(response, cancel, |data| {
        if data.trim() == "[DONE]" {
            return Ok(false);
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            return Ok(true);
        };
        if let Some(error) = provider_error(&value) {
            return Err(error);
        }
        if let Some(usage) = value.get("usage").and_then(parse_openai_usage) {
            result.usage.merge(&usage);
            on_chunk(LlmChunk::Usage(usage));
        }
        let Some(choice) = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            append_top_level_text(&mut result, &value, on_chunk);
            return Ok(true);
        };
        if let Some(delta) = choice.get("delta") {
            append_content_value(
                &mut result,
                delta.get("content").unwrap_or(&Value::Null),
                on_chunk,
            );
            for key in ["reasoning_content", "reasoning", "thinking"] {
                if let Some(text) = delta.get(key).and_then(Value::as_str) {
                    if !text.is_empty() {
                        result.reasoning.push_str(text);
                        on_chunk(LlmChunk::Thinking(text.to_string()));
                    }
                    break;
                }
            }
            if let Some(function) = delta.get("function_call") {
                let call = calls.entry(0).or_default();
                if let Some(name) = function.get("name").and_then(Value::as_str) {
                    call.name.push_str(name);
                }
                append_tool_arguments(call, function.get("arguments"));
            }
            if let Some(tool_deltas) = delta.get("tool_calls").and_then(Value::as_array) {
                for (position, tool_delta) in tool_deltas.iter().enumerate() {
                    let index = tool_delta
                        .get("index")
                        .and_then(Value::as_u64)
                        .map(|value| value as usize)
                        .unwrap_or(position);
                    let call = calls.entry(index).or_default();
                    if let Some(id) = tool_delta.get("id").and_then(Value::as_str) {
                        if !id.is_empty() {
                            call.id = id.to_string();
                        }
                    }
                    if let Some(function) = tool_delta.get("function") {
                        if let Some(name) = function.get("name").and_then(Value::as_str) {
                            call.name.push_str(name);
                        }
                        append_tool_arguments(call, function.get("arguments"));
                    }
                }
            }
        }
        if let Some(message) = choice.get("message") {
            append_content_snapshot(
                &mut result,
                message.get("content").unwrap_or(&Value::Null),
                on_chunk,
            );
            absorb_chat_message_tool_calls(message, &mut calls);
            for key in ["reasoning_content", "reasoning", "thinking"] {
                if let Some(text) = message.get(key).and_then(Value::as_str) {
                    if !text.is_empty() {
                        result.reasoning.push_str(text);
                        on_chunk(LlmChunk::Thinking(text.to_string()));
                    }
                    break;
                }
            }
        }
        if result.content.is_empty() {
            append_content_value(
                &mut result,
                choice.get("text").unwrap_or(&Value::Null),
                on_chunk,
            );
        }
        Ok(true)
    })
    .await?;
    result.tool_calls = finish_tool_calls(calls);
    Ok(result)
}

async fn openai_responses_completion<F>(
    client: &Client,
    settings: &LlmSettings,
    messages: &[Value],
    tools: &[Value],
    tool_choice: Option<&str>,
    cancel: &CancellationToken,
    on_chunk: &mut F,
) -> Result<LlmResponse, String>
where
    F: FnMut(LlmChunk),
{
    let endpoint = endpoint(&settings.base_url, &["/responses", "/v1/responses"])?;
    let (instructions, input) = responses_input(messages);
    let mut body = json!({
        "model": settings.model,
        "instructions": instructions,
        "input": input,
        "stream": true
    });
    if let Some(value) = settings.temperature.filter(|_| !reasoning_model(settings)) {
        body["temperature"] = json!(value);
    }
    if let Some(value) = settings.max_tokens {
        body["max_output_tokens"] = json!(value);
    }
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools.iter().map(responses_tool_definition).collect());
        body["tool_choice"] = Value::String(tool_choice.unwrap_or("auto").to_string());
    }
    let request = authorized_request(client.post(endpoint), settings)?
        .header(CONTENT_TYPE, "application/json")
        .json(&body);
    let response = send_request(request, cancel).await?;
    let response = ensure_success(response, "OpenAI Responses").await?;

    let mut result = LlmResponse::default();
    let mut calls: BTreeMap<String, ToolCallAccumulator> = BTreeMap::new();
    read_sse(response, cancel, |data| {
        if data.trim() == "[DONE]" {
            return Ok(false);
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            return Ok(true);
        };
        if let Some(error) = provider_error(&value) {
            return Err(error);
        }
        let event_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        match event_type {
            "response.output_text.delta" | "response.refusal.delta" => {
                if let Some(text) = value.get("delta").and_then(Value::as_str) {
                    if !text.is_empty() {
                        result.content.push_str(text);
                        on_chunk(LlmChunk::Content(text.to_string()));
                    }
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(text) = value.get("delta").and_then(Value::as_str) {
                    if !text.is_empty() {
                        result.reasoning.push_str(text);
                        on_chunk(LlmChunk::Thinking(text.to_string()));
                    }
                }
            }
            "response.output_item.added" => {
                if let Some(item) = value.get("item") {
                    if item.get("type").and_then(Value::as_str) == Some("function_call") {
                        let key = output_item_key(item, &value);
                        let call = calls.entry(key).or_default();
                        call.id = item
                            .get("call_id")
                            .or_else(|| item.get("id"))
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        call.name = item
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        if let Some(arguments) = item.get("arguments").and_then(Value::as_str) {
                            call.arguments.push_str(arguments);
                        }
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                let key = value
                    .get("item_id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        value
                            .get("output_index")
                            .and_then(Value::as_u64)
                            .unwrap_or_default()
                            .to_string()
                    });
                let call = calls.entry(key).or_default();
                if let Some(text) = value.get("delta").and_then(Value::as_str) {
                    call.arguments.push_str(text);
                }
            }
            "response.completed" | "" => {
                let response = value.get("response").unwrap_or(&value);
                let text = response
                    .get("output")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter(|item| {
                                item.get("type").and_then(Value::as_str) == Some("message")
                            })
                            .map(|item| extract_text(&item["content"]))
                            .collect::<String>()
                    })
                    .unwrap_or_default();
                append_content_snapshot(&mut result, &Value::String(text), on_chunk);
                append_top_level_text(&mut result, response, on_chunk);
                if let Some(usage) = response.get("usage").and_then(parse_openai_usage) {
                    result.usage.merge(&usage);
                    on_chunk(LlmChunk::Usage(usage));
                }
                if let Some(items) = response.get("output").and_then(Value::as_array) {
                    for (index, item) in items.iter().enumerate() {
                        let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
                        if item_type == "function_call" {
                            let key = output_item_key(item, &Value::Null);
                            let call = calls.entry(key).or_default();
                            call.id = item
                                .get("call_id")
                                .or_else(|| item.get("id"))
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            call.name = item
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            if call.arguments.is_empty() {
                                if let Some(arguments) =
                                    item.get("arguments").and_then(Value::as_str)
                                {
                                    call.arguments.push_str(arguments);
                                }
                            }
                        } else if item_type == "message" && result.content.is_empty() {
                            if let Some(content) = item.get("content").and_then(Value::as_array) {
                                for block in content {
                                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                                        result.content.push_str(text);
                                        on_chunk(LlmChunk::Content(text.to_string()));
                                    }
                                }
                            }
                        } else if call_id_missing(index, item) {
                            tracing::trace!("Responses 输出项无需转换");
                        }
                    }
                }
            }
            _ => {}
        }
        let usage = value
            .get("response")
            .and_then(|response| response.get("usage"))
            .and_then(parse_openai_usage);
        if let Some(usage) = usage {
            result.usage.merge(&usage);
        }
        Ok(true)
    })
    .await?;
    result.tool_calls = finish_tool_calls(calls);
    Ok(result)
}

async fn anthropic_completion<F>(
    client: &Client,
    settings: &LlmSettings,
    messages: &[Value],
    tools: &[Value],
    tool_choice: Option<&str>,
    cancel: &CancellationToken,
    on_chunk: &mut F,
) -> Result<LlmResponse, String>
where
    F: FnMut(LlmChunk),
{
    let endpoint = endpoint(&settings.base_url, &["/v1/messages", "/messages"])?;
    let (system, messages) = anthropic_messages(messages);
    let mut body = json!({
        "model": settings.model,
        "system": system,
        "messages": messages,
        "max_tokens": settings.max_tokens.unwrap_or(8192),
        "stream": true
    });
    if let Some(value) = settings.temperature {
        body["temperature"] = json!(value);
    }
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools.iter().map(anthropic_tool_definition).collect());
        if let Some(name) = tool_choice {
            body["tool_choice"] = json!({"type": "tool", "name": name});
        }
    }
    let request = authorized_request(client.post(endpoint), settings)?
        .header(CONTENT_TYPE, "application/json")
        .header("anthropic-version", "2023-06-01")
        .json(&body);
    let response = send_request(request, cancel).await?;
    let response = ensure_success(response, "Anthropic Messages").await?;

    let mut result = LlmResponse::default();
    let mut calls: BTreeMap<usize, ToolCallAccumulator> = BTreeMap::new();
    read_sse(response, cancel, |data| {
        if data.trim() == "[DONE]" {
            return Ok(false);
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            return Ok(true);
        };
        if let Some(error) = provider_error(&value) {
            return Err(error);
        }
        let event_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        match event_type {
            "content_block_start" => {
                let index = value
                    .get("index")
                    .and_then(Value::as_u64)
                    .unwrap_or_default() as usize;
                if let Some(block) = value.get("content_block") {
                    if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                        let call = calls.entry(index).or_default();
                        call.id = block
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        call.name = block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                    }
                }
            }
            "content_block_delta" => {
                let index = value
                    .get("index")
                    .and_then(Value::as_u64)
                    .unwrap_or_default() as usize;
                if let Some(delta) = value.get("delta") {
                    match delta.get("type").and_then(Value::as_str).unwrap_or("") {
                        "text_delta" => {
                            if let Some(text) = delta.get("text").and_then(Value::as_str) {
                                if !text.is_empty() {
                                    result.content.push_str(text);
                                    on_chunk(LlmChunk::Content(text.to_string()));
                                }
                            }
                        }
                        "thinking_delta" => {
                            if let Some(text) = delta.get("thinking").and_then(Value::as_str) {
                                if !text.is_empty() {
                                    result.reasoning.push_str(text);
                                    on_chunk(LlmChunk::Thinking(text.to_string()));
                                }
                            }
                        }
                        "input_json_delta" => {
                            if let Some(text) = delta.get("partial_json").and_then(Value::as_str) {
                                calls.entry(index).or_default().arguments.push_str(text);
                            }
                        }
                        _ => {}
                    }
                }
            }
            "message_delta" => {
                if let Some(usage) = value.get("usage").and_then(parse_anthropic_usage) {
                    result.usage.merge(&usage);
                    on_chunk(LlmChunk::Usage(usage));
                }
            }
            "message_start" => {
                if let Some(usage) = value
                    .get("message")
                    .and_then(|message| message.get("usage"))
                    .and_then(parse_anthropic_usage)
                {
                    result.usage.merge(&usage);
                }
            }
            "message_stop" => return Ok(false),
            _ => {}
        }
        Ok(true)
    })
    .await?;
    result.tool_calls = finish_tool_calls(calls);
    Ok(result)
}

fn append_content_value<F>(result: &mut LlmResponse, value: &Value, on_chunk: &mut F)
where
    F: FnMut(LlmChunk),
{
    let text = extract_text(value);
    if text.is_empty() {
        return;
    }
    result.content.push_str(&text);
    on_chunk(LlmChunk::Content(text));
}

fn append_content_snapshot<F>(result: &mut LlmResponse, value: &Value, on_chunk: &mut F)
where
    F: FnMut(LlmChunk),
{
    let text = extract_text(value);
    if let Some(suffix) = text.strip_prefix(&result.content) {
        let suffix = suffix.to_string();
        append_content_value(result, &Value::String(suffix), on_chunk);
    }
}

fn append_top_level_text<F>(result: &mut LlmResponse, value: &Value, on_chunk: &mut F)
where
    F: FnMut(LlmChunk),
{
    if !result.content.is_empty() {
        return;
    }
    for key in ["output_text", "content", "text"] {
        if let Some(candidate) = value.get(key) {
            let before = result.content.len();
            append_content_value(result, candidate, on_chunk);
            if result.content.len() > before {
                break;
            }
        }
    }
}

fn extract_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts.iter().map(extract_text).collect(),
        Value::Object(object) => {
            if object
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| !matches!(kind, "text" | "output_text" | "refusal"))
            {
                return String::new();
            }
            for key in ["text", "output_text", "content", "refusal", "value"] {
                if let Some(value) = object.get(key) {
                    let text = extract_text(value);
                    if !text.is_empty() {
                        return text;
                    }
                }
            }
            String::new()
        }
        _ => String::new(),
    }
}

fn append_tool_arguments(call: &mut ToolCallAccumulator, value: Option<&Value>) {
    let Some(value) = value else {
        return;
    };
    match value {
        Value::String(arguments) => call.arguments.push_str(arguments),
        Value::Null => {}
        _ => call.arguments.push_str(&value.to_string()),
    }
}

fn absorb_chat_message_tool_calls(
    message: &Value,
    calls: &mut BTreeMap<usize, ToolCallAccumulator>,
) {
    if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
        for (position, tool_call) in tool_calls.iter().enumerate() {
            let index = tool_call
                .get("index")
                .and_then(Value::as_u64)
                .map(|value| value as usize)
                .unwrap_or(position);
            let call = calls.entry(index).or_default();
            if let Some(id) = tool_call.get("id").and_then(Value::as_str) {
                if !id.is_empty() {
                    call.id = id.to_string();
                }
            }
            let function = tool_call.get("function").unwrap_or(tool_call);
            if let Some(name) = function.get("name").and_then(Value::as_str) {
                call.name = name.to_string();
            }
            if call.arguments.is_empty() {
                append_tool_arguments(call, function.get("arguments"));
            }
        }
    }
    if let Some(function) = message.get("function_call") {
        let call = calls.entry(0).or_default();
        if let Some(name) = function.get("name").and_then(Value::as_str) {
            call.name = name.to_string();
        }
        if call.arguments.is_empty() {
            append_tool_arguments(call, function.get("arguments"));
        }
    }
}

#[derive(Default)]
struct ToolCallAccumulator {
    id: String,
    name: String,
    arguments: String,
}

fn finish_tool_calls<T>(calls: BTreeMap<T, ToolCallAccumulator>) -> Vec<ToolCall>
where
    T: Ord,
{
    calls
        .into_values()
        .filter(|call| !call.name.trim().is_empty())
        .map(|call| ToolCall {
            id: if call.id.trim().is_empty() {
                format!("call_{}", uuid::Uuid::new_v4().simple())
            } else {
                call.id
            },
            name: call.name,
            arguments: serde_json::from_str(call.arguments.trim())
                .unwrap_or_else(|_| json!({"_raw": call.arguments})),
        })
        .collect()
}

async fn send_request(
    request: reqwest::RequestBuilder,
    cancel: &CancellationToken,
) -> Result<reqwest::Response, String> {
    tokio::select! {
        _ = cancel.cancelled() => Err("回合已取消".to_string()),
        response = request.send() => response.map_err(|error| format!("无法连接模型服务：{error}")),
    }
}

async fn ensure_success(
    response: reqwest::Response,
    label: &str,
) -> Result<reqwest::Response, String> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    let detail = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .or_else(|| value.get("message"))
                .or_else(|| value.get("detail"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| text.chars().take(2000).collect());
    Err(format!(
        "{label} 返回 HTTP {status}：{}",
        if detail.trim().is_empty() {
            "服务未提供错误详情"
        } else {
            &detail
        }
    ))
}

async fn read_sse<F>(
    response: reqwest::Response,
    cancel: &CancellationToken,
    mut on_data: F,
) -> Result<(), String>
where
    F: FnMut(&str) -> Result<bool, String>,
{
    let mut stream = response.bytes_stream();
    let mut buffer = Vec::<u8>::new();
    let mut raw_body = Vec::<u8>::new();
    let mut saw_sse_data = false;
    let mut event_data = String::new();
    const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
    while let Some(chunk) = tokio::select! {
        _ = cancel.cancelled() => return Err("回合已取消".to_string()),
        chunk = stream.next() => chunk,
    } {
        let chunk = chunk.map_err(|error| format!("读取模型流失败：{error}"))?;
        if !saw_sse_data {
            raw_body.extend_from_slice(&chunk);
        }
        buffer.extend_from_slice(&chunk);
        if buffer.len() + event_data.len() > MAX_FRAME_BYTES || raw_body.len() > MAX_FRAME_BYTES {
            return Err("模型返回的单条消息超过 16 MiB 限制。".to_string());
        }
        while let Some(newline) = buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = buffer.drain(..=newline).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\r', '\n']);
            if !consume_sse_line(line, &mut event_data, &mut saw_sse_data, &mut on_data)? {
                return Ok(());
            }
            if saw_sse_data {
                raw_body.clear();
            }
        }
    }
    if !buffer.is_empty() {
        let line = String::from_utf8_lossy(&buffer);
        if !consume_sse_line(
            line.trim_end_matches('\r'),
            &mut event_data,
            &mut saw_sse_data,
            &mut on_data,
        )? {
            return Ok(());
        }
    }
    if !saw_sse_data && !raw_body.is_empty() {
        let body = String::from_utf8_lossy(&raw_body);
        let body = body.trim().trim_start_matches('\u{feff}');
        if !body.is_empty() {
            serde_json::from_str::<Value>(body)
                .map_err(|_| "模型服务没有返回有效的 SSE 或 JSON 数据。".to_string())?;
            on_data(body)?;
        }
    } else if !event_data.is_empty() {
        on_data(&event_data)?;
    }
    Ok(())
}

fn consume_sse_line<F>(
    line: &str,
    event_data: &mut String,
    saw_sse_data: &mut bool,
    on_data: &mut F,
) -> Result<bool, String>
where
    F: FnMut(&str) -> Result<bool, String>,
{
    let line = line.trim_start_matches('\u{feff}');
    if line.is_empty() {
        if event_data.is_empty() {
            return Ok(true);
        }
        let data = std::mem::take(event_data);
        return on_data(&data);
    }
    if let Some(data) = line.strip_prefix("data:") {
        *saw_sse_data = true;
        // 部分兼容网关省略事件之间的空行；完整 JSON 仍按独立事件处理。
        if !event_data.is_empty()
            && (event_data.trim() == "[DONE]" || serde_json::from_str::<Value>(event_data).is_ok())
        {
            let previous = std::mem::take(event_data);
            if !on_data(&previous)? {
                return Ok(false);
            }
        }
        if !event_data.is_empty() {
            event_data.push('\n');
        }
        event_data.push_str(data.strip_prefix(' ').unwrap_or(data));
    }
    Ok(true)
}

fn authorized_request(
    request: reqwest::RequestBuilder,
    settings: &LlmSettings,
) -> Result<reqwest::RequestBuilder, String> {
    let mut request = request;
    if !settings.api_key.trim().is_empty() {
        request = match settings.api_format {
            ApiFormat::Anthropic => request.header("x-api-key", &settings.api_key),
            _ => request.bearer_auth(&settings.api_key),
        };
    }
    for (name, value) in &settings.headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|error| format!("模型请求头名称无效（{name}）：{error}"))?;
        let value = HeaderValue::from_str(value)
            .map_err(|error| format!("模型请求头 {name} 的值无效：{error}"))?;
        request = request.header(name, value);
    }
    Ok(request)
}

pub(super) fn endpoint(base_url: &str, suffixes: &[&str]) -> Result<String, String> {
    let mut url = url::Url::parse(base_url.trim())
        .map_err(|_| "模型服务地址无效，请检查提供方 URL；请求尚未发送。".to_string())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("模型服务地址必须是不含账号密码的 HTTP 或 HTTPS URL。".to_string());
    }
    let mut path = url.path().trim_end_matches('/').to_string();
    if !suffixes.iter().any(|suffix| path.ends_with(suffix)) {
        // 仅裸域名补默认版本；用户填写的 /api/v3 等前缀必须原样保留。
        let suffix = suffixes[0];
        if path.is_empty() && !suffix.starts_with("/v1/") {
            path.push_str("/v1");
        }
        let suffix = if path.ends_with("/v1") && suffix.starts_with("/v1/") {
            &suffix[3..]
        } else {
            suffix
        };
        path.push_str(suffix);
    }
    url.set_path(&path);
    url.set_fragment(None);
    let mut logged_target = url.clone();
    logged_target.set_query(None);
    tracing::info!(target_url = %logged_target, "模型请求目标已解析");
    Ok(url.to_string())
}

fn provider_error(value: &Value) -> Option<String> {
    if let Some(error) = value.get("error") {
        if let Some(message) = error
            .get("message")
            .or_else(|| value.get("message"))
            .and_then(Value::as_str)
        {
            return Some(message.to_string());
        }
        if let Some(message) = error.as_str() {
            return Some(message.to_string());
        }
    }
    None
}

fn parse_openai_usage(value: &Value) -> Option<LlmUsage> {
    let object = value.as_object()?;
    let prompt_tokens = first_u64(object, &["prompt_tokens", "input_tokens"]);
    let completion_tokens = first_u64(object, &["completion_tokens", "output_tokens"]);
    let total_tokens = first_u64(object, &["total_tokens"]);
    let cache_read_tokens = first_u64(
        object,
        &[
            "cache_read_input_tokens",
            "cached_tokens",
            "cache_read_tokens",
        ],
    )
    .max(
        value
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
    )
    .max(
        value
            .pointer("/input_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
    );
    let cache_reported = [
        "cache_read_input_tokens",
        "cached_tokens",
        "cache_read_tokens",
    ]
    .iter()
    .any(|key| object.get(*key).is_some_and(Value::is_number))
        || value
            .pointer("/prompt_tokens_details/cached_tokens")
            .is_some_and(Value::is_number)
        || value
            .pointer("/input_tokens_details/cached_tokens")
            .is_some_and(Value::is_number);
    let reasoning_tokens = value
        .pointer("/completion_tokens_details/reasoning_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    Some(LlmUsage {
        prompt_tokens,
        completion_tokens,
        total_tokens,
        cache_read_tokens,
        cache_creation_tokens: 0,
        reasoning_tokens,
        cache_input_tokens: if cache_reported { prompt_tokens } else { 0 },
        cache_reported_calls: u64::from(cache_reported),
    })
}

fn parse_anthropic_usage(value: &Value) -> Option<LlmUsage> {
    let object = value.as_object()?;
    let prompt = first_u64(object, &["input_tokens"])
        + first_u64(object, &["cache_read_input_tokens"])
        + first_u64(object, &["cache_creation_input_tokens"]);
    let cache_reported = object
        .get("cache_read_input_tokens")
        .is_some_and(Value::is_number);
    Some(LlmUsage {
        prompt_tokens: prompt,
        completion_tokens: first_u64(object, &["output_tokens"]),
        total_tokens: 0,
        cache_read_tokens: first_u64(object, &["cache_read_input_tokens"]),
        cache_creation_tokens: first_u64(object, &["cache_creation_input_tokens"]),
        reasoning_tokens: 0,
        cache_input_tokens: if cache_reported { prompt } else { 0 },
        cache_reported_calls: u64::from(cache_reported),
    })
}

fn first_u64(object: &Map<String, Value>, keys: &[&str]) -> u64 {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(Value::as_u64))
        .unwrap_or_default()
}

fn responses_input(messages: &[Value]) -> (String, Vec<Value>) {
    let mut system = Vec::new();
    let mut input = Vec::new();
    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        if role == "system" {
            if let Some(content) = message.get("content").and_then(Value::as_str) {
                system.push(content.to_string());
            }
            continue;
        }
        if role == "tool" {
            input.push(json!({
                "type": "function_call_output",
                "call_id": message.get("tool_call_id").and_then(Value::as_str).unwrap_or_default(),
                "output": message.get("content").and_then(Value::as_str).unwrap_or_default()
            }));
            continue;
        }
        input.push(json!({
            "role": role,
            "content": responses_message_content(
                message.get("content").cloned().unwrap_or_else(|| Value::String(String::new()))
            )
        }));
        if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
            for call in tool_calls {
                input.push(json!({
                    "type": "function_call",
                    "call_id": call.get("id").and_then(Value::as_str).unwrap_or_default(),
                    "name": call.pointer("/function/name").and_then(Value::as_str).unwrap_or_default(),
                    "arguments": call.pointer("/function/arguments").and_then(Value::as_str).unwrap_or("{}")
                }));
            }
        }
    }
    (system.join("\n\n"), input)
}

fn anthropic_messages(messages: &[Value]) -> (String, Vec<Value>) {
    let mut system = Vec::new();
    let mut output = Vec::<Value>::new();
    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        match role {
            "system" => {
                if let Some(content) = message.get("content").and_then(Value::as_str) {
                    system.push(content.to_string());
                }
            }
            "tool" => {
                output.push(json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": message.get("tool_call_id").and_then(Value::as_str).unwrap_or_default(),
                        "content": message.get("content").and_then(Value::as_str).unwrap_or_default()
                    }]
                }));
            }
            "assistant" => {
                let mut blocks = Vec::new();
                blocks.extend(anthropic_content_blocks(
                    message
                        .get("content")
                        .cloned()
                        .unwrap_or_else(|| Value::String(String::new())),
                ));
                if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
                    for call in tool_calls {
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": call.get("id").and_then(Value::as_str).unwrap_or_default(),
                            "name": call.pointer("/function/name").and_then(Value::as_str).unwrap_or_default(),
                            "input": serde_json::from_str::<Value>(
                                call.pointer("/function/arguments").and_then(Value::as_str).unwrap_or("{}")
                            ).unwrap_or_else(|_| json!({}))
                        }));
                    }
                }
                output.push(json!({"role": "assistant", "content": blocks}));
            }
            _ => output.push(json!({
                "role": "user",
                "content": anthropic_content_blocks(
                    message
                        .get("content")
                        .cloned()
                        .unwrap_or_else(|| Value::String(String::new())),
                )
            })),
        }
    }
    (system.join("\n\n"), output)
}

fn responses_message_content(content: Value) -> Value {
    match content {
        Value::Array(parts) => Value::Array(
            parts
                .into_iter()
                .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                    Some("text") => Some(json!({
                        "type": "input_text",
                        "text": part.get("text").and_then(Value::as_str).unwrap_or_default()
                    })),
                    Some("image_url") => {
                        let url = part
                            .pointer("/image_url/url")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        (!url.is_empty()).then(|| json!({"type": "input_image", "image_url": url}))
                    }
                    _ => None,
                })
                .collect(),
        ),
        other => other,
    }
}

fn anthropic_content_blocks(content: Value) -> Vec<Value> {
    match content {
        Value::String(text) => {
            if text.is_empty() {
                Vec::new()
            } else {
                vec![json!({"type": "text", "text": text})]
            }
        }
        Value::Array(parts) => parts
            .into_iter()
            .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                Some("text") => {
                    let text = part.get("text").and_then(Value::as_str).unwrap_or_default();
                    (!text.is_empty()).then(|| json!({"type": "text", "text": text}))
                }
                Some("image_url") => {
                    let url = part
                        .pointer("/image_url/url")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    anthropic_image_block(url)
                }
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn anthropic_image_block(data_url: &str) -> Option<Value> {
    let payload = data_url.strip_prefix("data:")?;
    let (mime_type, data) = payload.split_once(";base64,")?;
    Some(json!({
        "type": "image",
        "source": {
            "type": "base64",
            "media_type": mime_type,
            "data": data
        }
    }))
}

fn responses_tool_definition(tool: &Value) -> Value {
    let function = tool.get("function").unwrap_or(tool);
    json!({
        "type": "function",
        "name": function.get("name").and_then(Value::as_str).unwrap_or_default(),
        "description": function.get("description").and_then(Value::as_str).unwrap_or_default(),
        "parameters": function.get("parameters").cloned().unwrap_or_else(|| json!({"type": "object"}))
    })
}

fn anthropic_tool_definition(tool: &Value) -> Value {
    let function = tool.get("function").unwrap_or(tool);
    json!({
        "name": function.get("name").and_then(Value::as_str).unwrap_or_default(),
        "description": function.get("description").and_then(Value::as_str).unwrap_or_default(),
        "input_schema": function.get("parameters").cloned().unwrap_or_else(|| json!({"type": "object"}))
    })
}

fn output_item_key(item: &Value, envelope: &Value) -> String {
    item.get("id")
        .or_else(|| item.get("call_id"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            envelope
                .get("item_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| {
            envelope
                .get("output_index")
                .and_then(Value::as_u64)
                .unwrap_or_default()
                .to_string()
        })
}

fn call_id_missing(_index: usize, _item: &Value) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn mock_completion(body: String, format: ApiFormat) -> (LlmResponse, String) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new().fallback(move || {
            let body = body.clone();
            async move { body }
        });
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let settings = LlmSettings {
            provider: "local-test".to_string(),
            base_url: format!("http://{address}/api/v3"),
            api_key: String::new(),
            model: "test-model".to_string(),
            api_format: format,
            headers: Vec::new(),
            temperature: None,
            max_tokens: None,
        };
        let mut visible = String::new();
        let response = stream_completion(
            &Client::builder().no_proxy().build().unwrap(),
            &settings,
            &[json!({"role": "user", "content": "测试"})],
            &[],
            None,
            &CancellationToken::new(),
            |chunk| {
                if let LlmChunk::Content(text) = chunk {
                    visible.push_str(&text)
                }
            },
        )
        .await;
        task.abort();
        (response.expect("解析本地模拟模型响应"), visible)
    }

    #[tokio::test]
    async fn 网关完整响应支持字符串数组及对象参数() {
        let body = json!({
            "choices": [{"message": {
                "content": [
                    {"type": "text", "text": "中文"},
                    {"type": "output_text", "text": {"value": "答案"}},
                    {"type": "thinking", "text": "不应显示推理块"}
                ],
                "tool_calls": [{"id": "call-1", "function": {"name": "read_material", "arguments": {"page": 2}}}]
            }}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 3}
        });
        let (response, visible) = mock_completion(
            serde_json::to_string_pretty(&body).unwrap(),
            ApiFormat::OpenAiChat,
        )
        .await;
        assert_eq!(response.content, "中文答案");
        assert_eq!(visible, response.content);
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, "read_material");
        assert_eq!(response.tool_calls[0].arguments, json!({"page": 2}));
        assert_eq!(response.usage.total_tokens, 8);
    }

    #[tokio::test]
    async fn 流式数组与结束快照不会丢字或重复显示() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":[{\"type\":\"text\",\"text\":\"中文\"}]}}]}\r\n\r\n",
            "data: {\"choices\":[{\"message\":{\"content\":\"中文答案\"}}]}\r\n\r\n",
            "data: [DONE]"
        );
        let (response, visible) = mock_completion(body.to_string(), ApiFormat::OpenAiChat).await;
        assert_eq!(response.content, "中文答案");
        assert_eq!(visible, "中文答案");
    }

    #[tokio::test]
    async fn 多行事件与无换行末尾事件均可解析() {
        let body = concat!(
            ": 保活\r\n",
            "event: message\r\n",
            "data: {\"choices\":\r\n",
            "data: [{\"delta\":{\"content\":\"多行\"}}]}\r\n\r\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"完整\"}}]}"
        );
        let (response, visible) = mock_completion(body.to_string(), ApiFormat::OpenAiChat).await;
        assert_eq!(response.content, "多行完整");
        assert_eq!(visible, response.content);
        let (response, _) = mock_completion(
            "data: {\"choices\":[{\"delta\":{\"content\":\"单条末帧\"}}]}".to_string(),
            ApiFormat::OpenAiChat,
        )
        .await;
        assert_eq!(response.content, "单条末帧");
    }

    #[tokio::test]
    async fn 工具增量与完整快照不会重复累加参数() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"read_\",\"arguments\":\"{\\\"page\\\":\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"material\",\"arguments\":\"2}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"message\":{\"tool_calls\":[{\"id\":\"c1\",\"function\":{\"name\":\"read_material\",\"arguments\":\"{\\\"page\\\":2}\"}}]}}]}\n\n",
            "data: [DONE]\n\n"
        );
        let (response, visible) = mock_completion(body.to_string(), ApiFormat::OpenAiChat).await;
        assert!(visible.is_empty());
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, "read_material");
        assert_eq!(response.tool_calls[0].arguments, json!({"page": 2}));
    }

    #[tokio::test]
    async fn Responses完整消息保留所有正文块() {
        let body = json!({"output": [
            {"type": "message", "content": [{"type": "output_text", "text": "第一段"}]},
            {"type": "message", "content": [{"type": "output_text", "text": "第二段"}]}
        ]});
        let (response, visible) =
            mock_completion(body.to_string(), ApiFormat::OpenAiResponses).await;
        assert_eq!(response.content, "第一段第二段");
        assert_eq!(visible, response.content);
    }

    #[test]
    fn endpoint_keeps_full_provider_paths() {
        assert_eq!(
            endpoint("https://example.test/v1", &["/chat/completions"]).expect("解析服务地址"),
            "https://example.test/v1/chat/completions"
        );
        assert_eq!(
            endpoint(
                "https://example.test/v1/chat/completions",
                &["/chat/completions"]
            )
            .expect("保留完整接口地址"),
            "https://example.test/v1/chat/completions"
        );
    }

    #[test]
    fn 自定义路径不会被追加错误版本且查询参数保持原位() {
        for (base, expected) in [
            (
                "https://gateway.example/api/v3",
                "https://gateway.example/api/v3/chat/completions",
            ),
            (
                "https://gateway.example/compatible-mode/v1/",
                "https://gateway.example/compatible-mode/v1/chat/completions",
            ),
            (
                "https://gateway.example/v1beta/openai",
                "https://gateway.example/v1beta/openai/chat/completions",
            ),
            (
                "https://gateway.example",
                "https://gateway.example/v1/chat/completions",
            ),
            (
                "https://gateway.example/api/v3?route=test",
                "https://gateway.example/api/v3/chat/completions?route=test",
            ),
        ] {
            assert_eq!(
                endpoint(base, &["/chat/completions"]).expect("解析地址"),
                expected
            );
        }
        assert_eq!(
            endpoint("https://gateway.example/api/v3", &["/responses"])
                .expect("解析 Responses 地址"),
            "https://gateway.example/api/v3/responses"
        );
        for (base, expected) in [
            (
                "https://gateway.example/anthropic",
                "https://gateway.example/anthropic/v1/messages",
            ),
            (
                "https://gateway.example/v1",
                "https://gateway.example/v1/messages",
            ),
        ] {
            assert_eq!(
                endpoint(base, &["/v1/messages", "/messages"]).expect("解析 Anthropic 地址"),
                expected
            );
        }
        for invalid in [
            "",
            "not-a-url",
            "file:///tmp/test",
            "https://user:password@gateway.example",
        ] {
            assert!(endpoint(invalid, &["/chat/completions"]).is_err());
        }
    }

    #[test]
    fn parse_usage_accepts_responses_shape() {
        let usage = parse_openai_usage(&json!({
            "input_tokens": 12,
            "output_tokens": 7,
            "total_tokens": 19,
            "input_tokens_details": {"cached_tokens": 3}
        }))
        .expect("usage");
        assert_eq!(usage.prompt_tokens, 12);
        assert_eq!(usage.completion_tokens, 7);
        assert_eq!(usage.total_tokens, 19);
    }

    #[test]
    fn 缓存命中率只统计明确报告缓存的请求() {
        let reported = parse_openai_usage(&json!({
            "input_tokens": 100, "output_tokens": 20,
            "input_tokens_details": {"cached_tokens": 40}
        }))
        .unwrap();
        let unreported =
            parse_openai_usage(&json!({"prompt_tokens": 50, "completion_tokens": 10})).unwrap();
        let mut total = LlmUsage::default();
        total.accumulate(&reported);
        total.accumulate(&unreported);
        let summary = total.summary_json(1.0, 2);
        assert_eq!(summary["cache_read_input_tokens"], 40);
        assert_eq!(summary["cache_input_tokens"], 100);
        assert_eq!(summary["cache_reported_calls"], 1);
        assert_eq!(summary["total_tokens"], 180);
        assert_eq!(LlmUsage::default().summary_json(0.0, 0)["total_calls"], 0);
    }
}
