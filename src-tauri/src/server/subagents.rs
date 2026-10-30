use super::auth::{require_admin, Principal};
use super::error::{ApiError, ApiResult};
use super::{AppState, ScopedState};
use axum::body::{Body, Bytes};
use axum::extract::Path;
use axum::http::{header, StatusCode};
use axum::response::Response;
use axum::routing::{delete, get, post};
use axum::{Extension, Json, Router};
use futures_util::future::join_all;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path as FilePath, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

const SETTINGS_KEY: &str = "subagent_settings";
const CONNECTION_DOMAIN: &str = "subagent_connection";
const SESSION_DOMAIN: &str = "subagent_session";
const DEFAULT_CONSULT_BUDGET: i64 = 5;
const MAX_EVENT_TEXT: usize = 12_000;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/subagents/detect", get(detect_subagents))
        .route("/subagents/backends/options", get(backend_options))
        .route(
            "/subagents/backends/{kind}/sync",
            post(sync_backend_options),
        )
        .route(
            "/subagents/connections",
            get(list_connections).post(create_connection),
        )
        .route("/subagents/connections/{name}", delete(delete_connection))
        .route(
            "/subagents/connections/{name}/message",
            post(message_connection),
        )
        .route(
            "/subagents/settings",
            get(get_settings).put(update_settings),
        )
}

#[derive(Clone, Copy, Debug)]
struct BackendSpec {
    kind: &'static str,
    display_name: &'static str,
    command: &'static str,
    local: bool,
    efforts: &'static [&'static str],
}

const BACKENDS: &[BackendSpec] = &[
    BackendSpec {
        kind: "claude_code",
        display_name: "Claude Code",
        command: "claude",
        local: true,
        efforts: &["low", "medium", "high", "xhigh", "max"],
    },
    BackendSpec {
        kind: "codex",
        display_name: "Codex",
        command: "codex",
        local: true,
        efforts: &["none", "minimal", "low", "medium", "high", "xhigh"],
    },
    BackendSpec {
        kind: "grok",
        display_name: "Grok CLI",
        command: "grok",
        local: true,
        efforts: &[],
    },
    BackendSpec {
        kind: "antigravity",
        display_name: "Antigravity CLI",
        command: "agy",
        local: true,
        efforts: &["low", "medium", "high"],
    },
    BackendSpec {
        kind: "kimi",
        display_name: "Kimi CLI",
        command: "kimi",
        local: true,
        efforts: &[],
    },
    BackendSpec {
        kind: "opencode",
        display_name: "opencode",
        command: "opencode",
        local: true,
        efforts: &["minimal", "high", "max"],
    },
    BackendSpec {
        kind: "mimo",
        display_name: "MiMo Code",
        command: "mimo",
        local: true,
        efforts: &["minimal", "high", "max"],
    },
    BackendSpec {
        kind: "hermes",
        display_name: "Hermes Agent",
        command: "hermes",
        local: true,
        efforts: &[
            "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
        ],
    },
    BackendSpec {
        kind: "hermes_remote",
        display_name: "Hermes Agent (remote)",
        command: "",
        local: false,
        efforts: &[
            "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
        ],
    },
    BackendSpec {
        kind: "openclaw",
        display_name: "OpenClaw",
        command: "openclaw",
        local: true,
        efforts: &["off", "minimal", "low", "medium", "high", "xhigh"],
    },
    BackendSpec {
        kind: "deepseek_harness",
        display_name: "DeepSeek Harness",
        command: "dsh",
        local: true,
        efforts: &[
            "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
        ],
    },
];

#[derive(Clone, Debug, Default)]
struct StreamContext {
    final_text: String,
    session_id: String,
    success: bool,
    error: String,
    error_emitted: bool,
    stdout: Vec<String>,
    cumulative: HashMap<String, String>,
    tools: HashMap<String, Value>,
}

async fn detect_subagents(ScopedState(state): ScopedState) -> Json<Value> {
    let results = join_all(BACKENDS.iter().map(|spec| detect_backend(&state, *spec))).await;
    Json(json!({"backends": results}))
}

async fn backend_options(ScopedState(state): ScopedState) -> Json<Value> {
    let settings = load_settings(&state).unwrap_or_else(|_| default_settings());
    let results = join_all(
        BACKENDS
            .iter()
            .map(|spec| options_for(&state, &settings, *spec, false)),
    )
    .await;
    Json(json!({"backends": results}))
}

async fn sync_backend_options(
    ScopedState(state): ScopedState,
    Path(kind): Path<String>,
) -> ApiResult<Json<Value>> {
    let spec = backend_spec(&kind)?;
    if !spec.local {
        return Err(ApiError::bad_request("远程代理没有本地模型目录可同步"));
    }
    let settings = load_settings(&state)?;
    Ok(Json(options_for(&state, &settings, spec, true).await))
}

async fn list_connections(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(json!({
        "connections": state.database.list_resources(CONNECTION_DOMAIN)?
    })))
}

async fn create_connection(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    validate_connection_name(&name)?;
    let kind = body
        .get("agent_kind")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let spec = backend_spec(&kind)?;
    if state
        .database
        .get_resource(CONNECTION_DOMAIN, &name)?
        .is_some()
    {
        return Err(ApiError::conflict(format!("子代理连接已存在：{name}")));
    }
    let raw_cwd = body.get("cwd").and_then(Value::as_str).unwrap_or("");
    let cwd = resolve_connection_cwd(&state, raw_cwd, spec.local)?;
    let now = chrono::Utc::now().to_rfc3339();
    let connection = json!({
        "name": name,
        "agent_kind": kind,
        "cwd": cwd,
        "description": body.get("description").and_then(Value::as_str).unwrap_or(""),
        "created_at": now,
        "updated_at": Value::Null
    });
    state
        .database
        .put_resource(CONNECTION_DOMAIN, &name, &connection)?;
    Ok(Json(json!({
        "status": "connected",
        "name": name,
        "agent_kind": kind,
        "cwd": cwd,
        "partner_id": ""
    })))
}

async fn delete_connection(
    ScopedState(state): ScopedState,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    if !state.database.delete_resource(CONNECTION_DOMAIN, &name)? {
        return Err(ApiError::not_found(format!("未找到子代理连接：{name}")));
    }
    for resource in state.database.list_resources(SESSION_DOMAIN)?.into_iter() {
        if resource.get("connection_name").and_then(Value::as_str) == Some(name.as_str()) {
            if let Some(id) = resource.get("id").and_then(Value::as_str) {
                let _ = state.database.delete_resource(SESSION_DOMAIN, id);
            }
        }
    }
    Ok(Json(json!({"status": "disconnected", "name": name})))
}

async fn get_settings(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(load_settings(&state)?))
}

async fn update_settings(
    Extension(principal): Extension<Principal>,
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    require_admin(&principal)?;
    let current = load_settings(&state)?;
    let updated = merge_settings(&current, &body);
    state.database.set_setting(SETTINGS_KEY, &updated)?;
    Ok(Json(updated))
}

async fn message_connection(
    ScopedState(state): ScopedState,
    Path(name): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Response> {
    let connection = state
        .database
        .get_resource(CONNECTION_DOMAIN, &name)?
        .ok_or_else(|| ApiError::not_found(format!("未找到子代理连接：{name}")))?;
    let message = body
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if message.is_empty() {
        return Err(ApiError::bad_request("消息不能为空"));
    }
    let chat_session_id = body
        .get("chat_session_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let kind = connection
        .get("agent_kind")
        .and_then(Value::as_str)
        .unwrap_or("");
    let spec = backend_spec(kind)?;
    let settings = load_settings(&state)?;
    let config = backend_config(&settings, kind);
    let cwd = connection
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let session_key = session_key(chat_session_id, &name);
    let resume_id = if session_key.is_empty() {
        String::new()
    } else {
        state
            .database
            .get_resource(SESSION_DOMAIN, &session_key)?
            .and_then(|value| {
                value
                    .get("session_id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_default()
    };

    let (sender, receiver) = mpsc::channel::<Result<Bytes, std::io::Error>>(128);
    let initial = ndjson_bytes(&json!({"channel": "user_question", "text": message}))?;
    sender
        .send(Ok(initial))
        .await
        .map_err(|_| ApiError::internal("无法启动子代理流"))?;

    let task_state = state.clone();
    let task_name = name.clone();
    let task_kind = kind.to_string();
    let task_cwd = cwd.clone();
    let task_message = message.clone();
    let task_session_key = session_key.clone();
    let resume_id = resume_id.clone();
    tokio::spawn(async move {
        let result = if spec.local {
            run_local_backend(
                task_state.clone(),
                spec,
                task_cwd.clone(),
                config,
                task_message,
                resume_id,
                sender.clone(),
            )
            .await
        } else {
            run_remote_hermes(
                task_state.clone(),
                config,
                task_message,
                resume_id,
                sender.clone(),
            )
            .await
        };
        let (success, session_id) = match result {
            Ok(result) => result,
            Err(message) => {
                let _ = send_json(&sender, &json!({"channel": "error", "text": message})).await;
                (false, String::new())
            }
        };
        if success && !task_session_key.is_empty() && !session_id.is_empty() {
            let _ = task_state.database.put_resource(
                SESSION_DOMAIN,
                &task_session_key,
                &json!({
                    "id": task_session_key,
                    "connection_name": task_name,
                    "agent_kind": task_kind,
                    "cwd": task_cwd,
                    "session_id": session_id,
                    "updated_at": chrono::Utc::now().to_rfc3339()
                }),
            );
        }
        let _ = send_json(
            &sender,
            &json!({
                "done": true,
                "success": success,
                "session_id": session_id
            }),
        )
        .await;
    });

    let stream = ReceiverStream::new(receiver);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-ndjson; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from_stream(stream))
        .map_err(|error| ApiError::internal(format!("无法构建子代理流响应：{error}")))
}

async fn detect_backend(state: &AppState, spec: BackendSpec) -> Value {
    if spec.kind == "hermes_remote" {
        return detect_hermes_remote(state).await;
    }
    let Some(cli) = find_cli(state, spec.command) else {
        return json!({
            "kind": spec.kind,
            "display_name": spec.display_name,
            "available": false,
            "version": "",
            "detail": format!("未在 PATH 中找到 {} CLI", spec.command)
        });
    };
    match probe_version(state, &cli, &["--version"]).await {
        Ok(text) => {
            if spec.kind == "grok" {
                let help = probe_version(state, &cli, &["--help"])
                    .await
                    .unwrap_or_default();
                let compatible = [
                    "--output-format",
                    "streaming-json",
                    "--single",
                    "--resume",
                    "--permission-mode",
                    "--no-memory",
                    "--verbatim",
                ]
                .iter()
                .all(|flag| help.contains(flag));
                if !compatible {
                    return json!({
                        "kind": spec.kind,
                        "display_name": spec.display_name,
                        "available": false,
                        "version": "",
                        "detail": "检测到同名 grok 命令，但缺少 DeepTutor 所需的 streaming-json 协议"
                    });
                }
            }
            json!({
                "kind": spec.kind,
                "display_name": spec.display_name,
                "available": true,
                "version": text,
                "detail": ""
            })
        }
        Err(detail) => json!({
            "kind": spec.kind,
            "display_name": spec.display_name,
            "available": false,
            "version": "",
            "detail": detail
        }),
    }
}

async fn detect_hermes_remote(state: &AppState) -> Value {
    let settings = load_settings(state).unwrap_or_else(|_| default_settings());
    let config = backend_config(&settings, "hermes_remote");
    let base_url = config
        .get("base_url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if base_url.is_empty() {
        return remote_detection(false, "not_configured", "");
    }
    let key_env = config
        .get("api_key_env")
        .and_then(Value::as_str)
        .unwrap_or("DEEPTUTOR_HERMES_REMOTE_API_KEY");
    let api_key = std::env::var(key_env).unwrap_or_default();
    if api_key.trim().is_empty() {
        return remote_detection(false, "key_missing", "");
    }
    let Some(url) = join_http_url(&base_url, "/v1/capabilities") else {
        return remote_detection(false, "invalid_base_url", "");
    };
    match state.client.get(url).bearer_auth(api_key).send().await {
        Ok(response) if response.status().is_success() => {
            let payload = response.json::<Value>().await.unwrap_or_else(|_| json!({}));
            let features = payload.get("features");
            let compatible = payload.get("object").and_then(Value::as_str)
                == Some("hermes.api_server.capabilities")
                && features.is_some_and(|value| {
                    [
                        "run_submission",
                        "run_events_sse",
                        "run_stop",
                        "run_approval_response",
                        "session_resources",
                    ]
                    .iter()
                    .all(|key| value.get(*key).and_then(Value::as_bool) == Some(true))
                });
            if compatible {
                remote_detection(
                    true,
                    "",
                    payload
                        .get("model")
                        .and_then(Value::as_str)
                        .unwrap_or("Hermes API"),
                )
            } else {
                remote_detection(false, "incompatible", "")
            }
        }
        Ok(response) if matches!(response.status().as_u16(), 401 | 403) => {
            remote_detection(false, "unauthorized", "")
        }
        Ok(_) => remote_detection(false, "incompatible", ""),
        Err(_) => remote_detection(false, "unreachable", ""),
    }
}

fn remote_detection(available: bool, detail: &str, version: &str) -> Value {
    json!({
        "kind": "hermes_remote",
        "display_name": "Hermes Agent (remote)",
        "available": available,
        "version": version,
        "detail": detail
    })
}

async fn options_for(
    state: &AppState,
    settings: &Value,
    spec: BackendSpec,
    refresh: bool,
) -> Value {
    let detection = detect_backend(state, spec).await;
    let available = detection
        .get("available")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let version = detection
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let detail = detection
        .get("detail")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut models = Vec::new();
    let mut default_model = String::new();
    let mut allow_custom = true;
    let mut synced_at = String::new();
    match spec.kind {
        "claude_code" => {
            models = vec![
                model_option("opus", "Opus", "", spec.efforts),
                model_option("sonnet", "Sonnet", "", spec.efforts),
                model_option("sonnet[1m]", "Sonnet · 1M context", "", spec.efforts),
                model_option("haiku", "Haiku", "", spec.efforts),
            ];
            if refresh {
                synced_at.clear();
            }
        }
        "codex" => {
            let cache = codex_home(state).join("models_cache.json");
            if let Ok(raw) = std::fs::read_to_string(cache) {
                if let Ok(payload) = serde_json::from_str::<Value>(&raw) {
                    synced_at = payload
                        .get("fetched_at")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if let Some(entries) = payload.get("models").and_then(Value::as_array) {
                        for entry in entries {
                            let Some(slug) = entry.get("slug").and_then(Value::as_str) else {
                                continue;
                            };
                            let efforts = entry
                                .get("supported_reasoning_levels")
                                .and_then(Value::as_array)
                                .map(|values| {
                                    values
                                        .iter()
                                        .filter_map(|level| {
                                            level.get("effort").and_then(Value::as_str)
                                        })
                                        .map(str::to_string)
                                        .collect::<Vec<_>>()
                                })
                                .unwrap_or_default();
                            models.push(model_option_owned(
                                slug,
                                entry
                                    .get("display_name")
                                    .and_then(Value::as_str)
                                    .unwrap_or(slug),
                                entry
                                    .get("default_reasoning_level")
                                    .and_then(Value::as_str)
                                    .unwrap_or(""),
                                &efforts,
                            ));
                        }
                    }
                }
            }
            default_model = read_codex_default_model(&codex_home(state));
        }
        "opencode" | "mimo" => {
            if available {
                if let Some(cli) = find_cli(state, spec.command) {
                    let mut args = vec!["models"];
                    if refresh {
                        args.push("--refresh");
                    }
                    if let Ok(output) = run_text_command(
                        state,
                        &cli,
                        &args,
                        None,
                        Duration::from_secs(if refresh { 60 } else { 25 }),
                    )
                    .await
                    {
                        models = output
                            .lines()
                            .map(str::trim)
                            .filter(|line| {
                                line.contains('/') && !line.contains(char::is_whitespace)
                            })
                            .map(|slug| model_option(slug, slug, "", spec.efforts))
                            .collect();
                    }
                }
            }
        }
        _ => {}
    }
    if spec.kind == "codex" || spec.kind == "opencode" || spec.kind == "mimo" {
        allow_custom = true;
    }
    let _ = settings;
    json!({
        "kind": spec.kind,
        "display_name": spec.display_name,
        "available": available,
        "version": version,
        "default_model": default_model,
        "models": models,
        "efforts": spec.efforts,
        "allow_custom_model": allow_custom,
        "synced_at": synced_at,
        "detail": detail
    })
}

fn model_option(slug: &str, display_name: &str, default_effort: &str, efforts: &[&str]) -> Value {
    json!({
        "slug": slug,
        "display_name": display_name,
        "default_effort": default_effort,
        "efforts": efforts
    })
}

fn model_option_owned(
    slug: &str,
    display_name: &str,
    default_effort: &str,
    efforts: &[String],
) -> Value {
    json!({
        "slug": slug,
        "display_name": display_name,
        "default_effort": default_effort,
        "efforts": efforts
    })
}

async fn run_local_backend(
    state: AppState,
    spec: BackendSpec,
    cwd: String,
    config: Value,
    message: String,
    resume_id: String,
    sender: mpsc::Sender<Result<Bytes, std::io::Error>>,
) -> Result<(bool, String), String> {
    let cli = find_cli(&state, spec.command)
        .ok_or_else(|| format!("未在 PATH 中找到 {} CLI", spec.command))?;
    let args = build_backend_args(spec.kind, &message, &resume_id, &config);
    let mut command = build_cli_command(&cli, &args);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command.envs(state.paths.environment());
    if !cwd.is_empty() {
        command.current_dir(&cwd);
    }
    hide_window(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| format!("无法启动 {}：{error}", spec.display_name))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "无法读取子代理标准输出".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "无法读取子代理标准错误".to_string())?;
    let (line_sender, mut line_receiver) = mpsc::channel::<(bool, String)>(256);
    spawn_line_reader(stdout, true, line_sender.clone());
    spawn_line_reader(stderr, false, line_sender);

    let mut context = StreamContext {
        success: true,
        session_id: initial_session_id(spec.kind, &resume_id),
        ..Default::default()
    };
    let idle_timeout = config
        .get("idle_timeout_seconds")
        .and_then(Value::as_u64)
        .unwrap_or(600)
        .clamp(1, 86_400);

    while let Some((is_stdout, line)) =
        tokio::time::timeout(Duration::from_secs(idle_timeout), line_receiver.recv())
            .await
            .map_err(|_| {
                format!(
                    "{} 超过 {idle_timeout} 秒没有输出，已终止",
                    spec.display_name
                )
            })?
    {
        let events = if is_stdout {
            handle_stdout_line(spec.kind, &line, &mut context)
        } else {
            handle_stderr_line(spec.kind, &line, &mut context)
        };
        for event in events {
            if !send_json(&sender, &event).await {
                let _ = child.kill().await;
                return Ok((context.success, context.session_id));
            }
        }
    }
    let status = child
        .wait()
        .await
        .map_err(|error| format!("等待 {} 退出失败：{error}", spec.display_name))?;
    finish_context(spec.kind, &mut context, status.success());
    if !context.error.is_empty() && !context.error_emitted {
        let _ = send_json(&sender, &json!({"channel": "error", "text": context.error})).await;
    }
    Ok((context.success, context.session_id))
}

async fn run_remote_hermes(
    state: AppState,
    config: Value,
    message: String,
    resume_id: String,
    sender: mpsc::Sender<Result<Bytes, std::io::Error>>,
) -> Result<(bool, String), String> {
    let base_url = config
        .get("base_url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if base_url.is_empty() {
        return Err("Hermes 远程网关尚未配置".to_string());
    }
    let key_env = config
        .get("api_key_env")
        .and_then(Value::as_str)
        .unwrap_or("DEEPTUTOR_HERMES_REMOTE_API_KEY");
    let api_key = std::env::var(key_env).unwrap_or_default();
    if api_key.trim().is_empty() {
        return Err(format!("环境变量 {key_env} 未设置 Hermes 网关密钥"));
    }
    let run_url =
        join_http_url(&base_url, "/v1/runs").ok_or_else(|| "Hermes 网关地址无效".to_string())?;
    let mut payload = json!({
        "input": message,
        "instructions": "调用方是 DeepTutor Connected Agents。请直接回答用户问题。"
    });
    if !resume_id.is_empty() {
        payload["session_id"] = json!(resume_id);
    }
    if let Some(model) = config
        .get("model")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        payload["model"] = json!(model);
    }
    if let Some(effort) = config
        .get("effort")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        payload["model_options"] = json!({"reasoning_effort": effort});
    }
    if let Some(system_prompt) = config
        .get("system_prompt")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        payload["instructions"] = json!(format!(
            "{}\n\n调用方是 DeepTutor Connected Agents。请直接回答用户问题。",
            system_prompt.trim()
        ));
    }
    let started = state
        .client
        .post(run_url)
        .bearer_auth(&api_key)
        .json(&payload)
        .send()
        .await
        .map_err(|error| format!("提交 Hermes 运行失败：{error}"))?;
    if !started.status().is_success() {
        return Err(format!("Hermes 网关返回 HTTP {}", started.status()));
    }
    let started_payload = started
        .json::<Value>()
        .await
        .map_err(|error| format!("Hermes 启动响应无效：{error}"))?;
    let run_id = started_payload
        .get("run_id")
        .and_then(Value::as_str)
        .ok_or_else(|| "Hermes 启动响应缺少 run_id".to_string())?
        .to_string();
    let mut session_id = started_payload
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or(&resume_id)
        .to_string();
    if session_id.is_empty() {
        session_id = run_id.clone();
    }
    let events_url = join_http_url(&base_url, &format!("/v1/runs/{run_id}/events"))
        .ok_or_else(|| "Hermes 事件地址无效".to_string())?;
    let mut response = state
        .client
        .get(events_url)
        .bearer_auth(&api_key)
        .send()
        .await
        .map_err(|error| format!("连接 Hermes 事件流失败：{error}"))?;
    if !response.status().is_success() {
        return Err(format!("Hermes 事件流返回 HTTP {}", response.status()));
    }

    let mut success = true;
    let mut final_text = String::new();
    let mut buffer = String::new();
    let mut terminal = false;
    while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(index) = buffer.find('\n') {
            let line = buffer[..index].trim_end_matches('\r').to_string();
            buffer.drain(..=index);
            if let Some(event) = parse_sse_data(&line) {
                let name = event.get("event").and_then(Value::as_str).unwrap_or("");
                match name {
                    "message.delta" => {
                        final_text
                            .push_str(event.get("delta").and_then(Value::as_str).unwrap_or(""));
                        if !send_json(
                            &sender,
                            &json!({
                                "channel": "text",
                                "text": final_text.trim(),
                                "merge_id": "hermes_remote:final"
                            }),
                        )
                        .await
                        {
                            return Ok((success, session_id));
                        }
                    }
                    "run.completed" => {
                        final_text = event
                            .get("output")
                            .and_then(Value::as_str)
                            .unwrap_or(&final_text)
                            .to_string();
                        terminal = true;
                    }
                    "run.failed" | "run.cancelled" => {
                        success = false;
                        let error = if name == "run.failed" {
                            "run_failed"
                        } else {
                            "run_cancelled"
                        };
                        let _ =
                            send_json(&sender, &json!({"channel": "error", "text": error})).await;
                        terminal = true;
                    }
                    "approval.request" => {
                        let auto = config
                            .get("auto_approve")
                            .and_then(Value::as_bool)
                            .unwrap_or(true);
                        let choice = if auto { "once" } else { "deny" };
                        let approval_url =
                            join_http_url(&base_url, &format!("/v1/runs/{run_id}/approval"))
                                .ok_or_else(|| "Hermes 审批地址无效".to_string())?;
                        let _ = state
                            .client
                            .post(approval_url)
                            .bearer_auth(&api_key)
                            .json(&json!({"choice": choice}))
                            .send()
                            .await;
                        let _ = send_json(
                            &sender,
                            &json!({
                                "channel": "log",
                                "text": if auto { "approval approved" } else { "approval denied" }
                            }),
                        )
                        .await;
                    }
                    "tool.started" => {
                        let tool = event.get("tool").and_then(Value::as_str).unwrap_or("tool");
                        let preview = event.get("preview").and_then(Value::as_str).unwrap_or("");
                        let _ = send_json(
                            &sender,
                            &json!({
                                "channel": "tool",
                                "text": format!("{tool}: {preview}").trim_matches(':').trim(),
                                "merge_id": format!("hermes_remote:tool:{tool}")
                            }),
                        )
                        .await;
                    }
                    "tool.completed" => {
                        let tool = event.get("tool").and_then(Value::as_str).unwrap_or("tool");
                        let status = if event.get("error").is_some_and(|value| !value.is_null()) {
                            "error"
                        } else {
                            "complete"
                        };
                        let _ = send_json(
                            &sender,
                            &json!({
                                "channel": "tool_result",
                                "text": format!("{tool}: {status}"),
                                "merge_id": format!("hermes_remote:tool:{tool}")
                            }),
                        )
                        .await;
                    }
                    "reasoning.available" => {
                        if let Some(text) = event.get("text").and_then(Value::as_str) {
                            let _ =
                                send_json(&sender, &json!({"channel": "reasoning", "text": text}))
                                    .await;
                        }
                    }
                    "" | "gateway.keepalive" => {}
                    other => {
                        let _ = send_json(&sender, &json!({"channel": "log", "text": other})).await;
                    }
                }
                if terminal {
                    break;
                }
            }
        }
        if terminal {
            break;
        }
    }
    if success && (!terminal || final_text.trim().is_empty()) {
        success = false;
        let _ = send_json(
            &sender,
            &json!({
                "channel": "error",
                "text": if !terminal { "incomplete_stream" } else { "empty_response" }
            }),
        )
        .await;
    }
    Ok((success, session_id))
}

fn handle_stdout_line(kind: &str, line: &str, context: &mut StreamContext) -> Vec<Value> {
    if line.trim().is_empty() {
        return Vec::new();
    }
    let parsed = serde_json::from_str::<Value>(line.trim()).ok();
    match kind {
        "claude_code" => handle_claude(parsed.as_ref(), line, context),
        "codex" => handle_codex(parsed.as_ref(), line, context),
        "grok" => handle_grok(parsed.as_ref(), line, context),
        "antigravity" => handle_antigravity(parsed.as_ref(), line, context),
        "kimi" => handle_kimi(parsed.as_ref(), line, context),
        "openclaw" => {
            context.stdout.push(line.to_string());
            Vec::new()
        }
        "hermes" | "deepseek_harness" => {
            context.final_text.push_str(line);
            context.final_text.push('\n');
            vec![json!({
                "channel": "text",
                "text": context.final_text.trim(),
                "merge_id": format!("{kind}:final")
            })]
        }
        "opencode" | "mimo" => handle_opencode(parsed.as_ref(), line, context),
        _ => vec![json!({"channel": "log", "text": line})],
    }
}

fn handle_stderr_line(kind: &str, line: &str, context: &mut StreamContext) -> Vec<Value> {
    if line.trim().is_empty() {
        return Vec::new();
    }
    if kind == "hermes" {
        if let Some(session) = line
            .trim()
            .strip_prefix("session_id=")
            .or_else(|| line.trim().strip_prefix("Session ID:"))
        {
            context.session_id = session.trim().to_string();
            return vec![json!({"channel": "log", "text": "Session started"})];
        }
    }
    if kind == "deepseek_harness" && line.to_ascii_lowercase().contains("reasoning:") {
        return Vec::new();
    }
    vec![json!({"channel": "log", "text": truncate(line, MAX_EVENT_TEXT)})]
}

fn handle_claude(event: Option<&Value>, raw: &str, context: &mut StreamContext) -> Vec<Value> {
    let Some(event) = event else {
        return if raw.trim().is_empty() {
            Vec::new()
        } else {
            vec![json!({"channel": "log", "text": raw})]
        };
    };
    if let Some(session) = event.get("session_id").and_then(Value::as_str) {
        if !session.is_empty() {
            context.session_id = session.to_string();
        }
    }
    let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
    match event_type {
        "stream_event" => {
            let Some(inner) = event.get("event") else {
                return Vec::new();
            };
            let inner_type = inner.get("type").and_then(Value::as_str).unwrap_or("");
            if inner_type == "message_start" {
                if let Some(message_id) = inner
                    .get("message")
                    .and_then(|value| value.get("id"))
                    .and_then(Value::as_str)
                {
                    context.session_id = context.session_id.clone();
                    context
                        .cumulative
                        .insert("message".into(), message_id.into());
                }
                return Vec::new();
            }
            if inner_type != "content_block_delta" {
                return Vec::new();
            }
            let index = inner.get("index").and_then(Value::as_i64).unwrap_or(0);
            let delta = inner.get("delta").unwrap_or(&Value::Null);
            let delta_type = delta.get("type").and_then(Value::as_str).unwrap_or("");
            let (field, channel, prefix) = match delta_type {
                "text_delta" => ("text", "text", "txt"),
                "thinking_delta" => ("thinking", "reasoning", "rsn"),
                _ => return Vec::new(),
            };
            let key = format!("{prefix}:{index}");
            let next = delta.get(field).and_then(Value::as_str).unwrap_or("");
            let text = context.cumulative.entry(key).or_default();
            text.push_str(next);
            if channel == "text" {
                context.final_text = text.clone();
            }
            vec![json!({
                "channel": channel,
                "text": text,
                "merge_id": format!("{prefix}:{}:{index}", context.session_id)
            })]
        }
        "assistant" => {
            let blocks = event
                .get("message")
                .and_then(|value| value.get("content"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let mut out = Vec::new();
            for block in blocks {
                let block_type = block.get("type").and_then(Value::as_str).unwrap_or("");
                match block_type {
                    "text" => {
                        let text = block
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .trim();
                        if !text.is_empty() {
                            context.final_text = text.to_string();
                            out.push(json!({"channel": "text", "text": text}));
                        }
                    }
                    "thinking" => {
                        let text = block
                            .get("thinking")
                            .or_else(|| block.get("text"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .trim();
                        if !text.is_empty() {
                            out.push(json!({"channel": "reasoning", "text": text}));
                        }
                    }
                    "tool_use" => {
                        out.push(json!({
                            "channel": "tool",
                            "text": render_tool_use(&block)
                        }));
                    }
                    _ => {}
                }
            }
            out
        }
        "user" => {
            let blocks = event
                .get("message")
                .and_then(|value| value.get("content"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            blocks
                .into_iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
                .map(|block| {
                    json!({
                        "channel": "tool_result",
                        "text": render_content_text(block.get("content"))
                    })
                })
                .collect()
        }
        "result" => {
            if let Some(text) = event.get("result").and_then(Value::as_str) {
                if !text.trim().is_empty() {
                    context.final_text = text.trim().to_string();
                }
            }
            if event.get("is_error").and_then(Value::as_bool) == Some(true) {
                context.success = false;
                context.error = event
                    .get("result")
                    .and_then(Value::as_str)
                    .unwrap_or("Claude Code 报告失败")
                    .to_string();
            }
            Vec::new()
        }
        "system" if event.get("subtype").and_then(Value::as_str) == Some("init") => {
            vec![json!({"channel": "log", "text": "Session started"})]
        }
        _ => Vec::new(),
    }
}

fn handle_codex(event: Option<&Value>, raw: &str, context: &mut StreamContext) -> Vec<Value> {
    let Some(event) = event else {
        return vec![json!({"channel": "log", "text": raw})];
    };
    for key in ["thread_id", "session_id", "threadId", "sessionId"] {
        if let Some(value) = event.get(key).and_then(Value::as_str) {
            if !value.is_empty() {
                context.session_id = value.to_string();
            }
        }
    }
    let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
    if event_type == "thread.started" {
        return vec![json!({"channel": "log", "text": "Session started"})];
    }
    if matches!(event_type, "turn.failed" | "error") {
        context.success = false;
        context.error = error_message(event, "Codex 报告失败");
        return vec![json!({"channel": "error", "text": context.error})];
    }
    let Some(item) = event.get("item").filter(|value| value.is_object()) else {
        return Vec::new();
    };
    let item_type = item
        .get("type")
        .or_else(|| item.get("item_type"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let item_id = item.get("id").and_then(Value::as_str).unwrap_or("");
    let completed = event_type.ends_with(".completed");
    let started = event_type.ends_with(".started");
    match item_type {
        "agent_message" | "assistant_message" => {
            let text = item_text(item);
            if completed && !text.is_empty() {
                context.final_text = text.clone();
            }
            if started || text.is_empty() {
                Vec::new()
            } else {
                vec![json!({
                    "channel": "text",
                    "text": text,
                    "merge_id": if item_id.is_empty() { Value::Null } else { json!(item_id) }
                })]
            }
        }
        "reasoning" => {
            let text = item_text(item);
            if text.is_empty() {
                Vec::new()
            } else {
                vec![json!({
                    "channel": "reasoning",
                    "text": text,
                    "merge_id": if item_id.is_empty() { Value::Null } else { json!(item_id) }
                })]
            }
        }
        "command_execution" => {
            let command = item
                .get("command")
                .or_else(|| item.get("cmd"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if started {
                vec![json!({"channel": "tool", "text": format!("$ {command}")})]
            } else {
                let output = item
                    .get("aggregated_output")
                    .or_else(|| item.get("output"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                vec![json!({
                    "channel": "tool_result",
                    "text": truncate(&format!("$ {command}\n{output}"), MAX_EVENT_TEXT)
                })]
            }
        }
        "web_search" => {
            let query = item.get("query").and_then(Value::as_str).unwrap_or("");
            vec![json!({
                "channel": "tool",
                "text": if query.is_empty() { "web search".to_string() } else { format!("web search · {query}") },
                "merge_id": if item_id.is_empty() { Value::Null } else { json!(item_id) }
            })]
        }
        "mcp_tool_call" | "tool_call" => vec![json!({
            "channel": "tool",
            "text": truncate(&format!(
                "{} · {}",
                item.get("name").or_else(|| item.get("tool")).and_then(Value::as_str).unwrap_or("tool"),
                compact_json(item.get("arguments").or_else(|| item.get("input")).unwrap_or(&Value::Null))
            ), MAX_EVENT_TEXT)
        })],
        _ if completed => vec![json!({
            "channel": "log",
            "text": truncate(&compact_json(item), MAX_EVENT_TEXT)
        })],
        _ => Vec::new(),
    }
}

fn handle_grok(event: Option<&Value>, _raw: &str, context: &mut StreamContext) -> Vec<Value> {
    let Some(event) = event else {
        context.success = false;
        context.error = "Grok CLI 输出了无效的 streaming JSON".to_string();
        return vec![json!({"channel": "error", "text": context.error})];
    };
    match event.get("type").and_then(Value::as_str).unwrap_or("") {
        "text" => {
            let key = "grok-text";
            let delta = event.get("data").and_then(Value::as_str).unwrap_or("");
            let text = context.cumulative.entry(key.into()).or_default();
            text.push_str(delta);
            context.final_text = text.clone();
            vec![json!({
                "channel": "text",
                "text": text,
                "merge_id": "grok-text"
            })]
        }
        "tool_call" | "tool_call_update" => {
            let tool_id = event
                .get("toolCallId")
                .and_then(Value::as_str)
                .unwrap_or("");
            let store = context
                .tools
                .entry(tool_id.to_string())
                .or_insert_with(|| json!({}));
            if let (Some(target), Some(source)) = (store.as_object_mut(), event.as_object()) {
                for (key, value) in source {
                    if !value.is_null() {
                        target.insert(key.clone(), value.clone());
                    }
                }
            }
            let name = store
                .get("toolName")
                .or_else(|| store.get("title"))
                .and_then(Value::as_str)
                .unwrap_or("Tool");
            vec![json!({
                "channel": if store.get("status").and_then(Value::as_str).is_some_and(|value| matches!(value, "completed" | "failed")) {
                    "tool_result"
                } else {
                    "tool"
                },
                "text": name,
                "merge_id": if tool_id.is_empty() { Value::Null } else { json!(format!("grok-tool-{tool_id}")) }
            })]
        }
        "end" => {
            if let Some(session) = event
                .get("sessionId")
                .or_else(|| event.get("session_id"))
                .and_then(Value::as_str)
            {
                context.session_id = session.to_string();
            }
            if event
                .get("stopReason")
                .and_then(Value::as_str)
                .is_some_and(|reason| reason != "end_turn")
            {
                context.success = false;
                context.error = "Grok CLI 未正常完成回合".to_string();
            }
            Vec::new()
        }
        "error" => {
            context.success = false;
            context.error = event
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Grok CLI 错误")
                .to_string();
            vec![json!({"channel": "error", "text": context.error})]
        }
        "thought" | "usage" | "available_commands" => Vec::new(),
        other if !other.is_empty() => {
            vec![json!({"channel": "log", "text": format!("Grok CLI: {other}")})]
        }
        _ => Vec::new(),
    }
}

fn handle_antigravity(event: Option<&Value>, raw: &str, context: &mut StreamContext) -> Vec<Value> {
    let Some(event) = event else {
        return vec![json!({"channel": "log", "text": raw})];
    };
    let name = event.get("event").and_then(Value::as_str).unwrap_or("");
    let body = event
        .get(name)
        .filter(|value| value.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
    if let Some(session) = event
        .get("conversation_id")
        .or_else(|| body.get("conversation_id"))
        .and_then(Value::as_str)
    {
        context.session_id = session.to_string();
    }
    match name {
        "init" => vec![json!({"channel": "log", "text": "Session started"})],
        "step_update" => {
            let index = body.get("step_index").and_then(Value::as_i64).unwrap_or(0);
            if let Some(delta) = body.get("text_delta").and_then(Value::as_str) {
                if !delta.is_empty() {
                    let text = context
                        .cumulative
                        .entry(format!("agy:{index}"))
                        .or_default();
                    text.push_str(delta);
                    context.final_text.push_str(delta);
                    return vec![json!({
                        "channel": "text",
                        "text": text,
                        "merge_id": format!("agy:{index}")
                    })];
                }
            }
            if let Some(tool) = body.get("tool_name").and_then(Value::as_str) {
                let state = body.get("state").and_then(Value::as_str).unwrap_or("");
                let info = body.get("tool_info").unwrap_or(&Value::Null);
                if state == "DONE" {
                    vec![json!({
                        "channel": "tool_result",
                        "text": truncate(&compact_json(info), MAX_EVENT_TEXT)
                    })]
                } else {
                    vec![json!({
                        "channel": "tool",
                        "text": render_tool_header(tool, info)
                    })]
                }
            } else {
                Vec::new()
            }
        }
        "result" => {
            let status = body
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_ascii_uppercase();
            if let Some(response) = body.get("response").and_then(Value::as_str) {
                if !response.is_empty() {
                    context.final_text = response.to_string();
                }
            }
            if matches!(
                status.as_str(),
                "ERROR" | "CANCELED" | "INTERRUPTED" | "INVALID"
            ) {
                context.success = false;
                context.error = format!("Antigravity CLI 状态：{status}");
                return vec![json!({"channel": "error", "text": context.error})];
            }
            Vec::new()
        }
        _ => Vec::new(),
    }
}

fn handle_kimi(event: Option<&Value>, raw: &str, context: &mut StreamContext) -> Vec<Value> {
    let Some(event) = event else {
        return vec![json!({"channel": "log", "text": raw})];
    };
    let role = event.get("role").and_then(Value::as_str).unwrap_or("");
    match role {
        "assistant" => {
            let mut out = Vec::new();
            if let Some(content) = event.get("content") {
                for part in content_parts(content) {
                    match part.get("type").and_then(Value::as_str).unwrap_or("") {
                        "text" => {
                            if let Some(text) = part.get("text").and_then(Value::as_str) {
                                if !text.trim().is_empty() {
                                    context.final_text = text.trim().to_string();
                                    out.push(json!({"channel": "text", "text": text.trim()}));
                                }
                            }
                        }
                        "think" => {
                            if let Some(text) = part
                                .get("think")
                                .or_else(|| part.get("text"))
                                .and_then(Value::as_str)
                            {
                                if !text.trim().is_empty() {
                                    out.push(json!({"channel": "reasoning", "text": text.trim()}));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            if let Some(calls) = event.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    out.push(json!({"channel": "tool", "text": render_tool_call(call)}));
                }
            }
            out
        }
        "tool" => vec![json!({
            "channel": "tool_result",
            "text": truncate(&content_text(event.get("content")), MAX_EVENT_TEXT)
        })],
        "" => {
            if event.get("category").is_some() || event.get("severity").is_some() {
                let text = [event.get("title"), event.get("body")]
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" · ");
                if text.is_empty() {
                    Vec::new()
                } else {
                    vec![json!({"channel": "log", "text": text})]
                }
            } else {
                vec![
                    json!({"channel": "log", "text": truncate(&compact_json(event), MAX_EVENT_TEXT)}),
                ]
            }
        }
        _ => Vec::new(),
    }
}

fn handle_opencode(event: Option<&Value>, raw: &str, context: &mut StreamContext) -> Vec<Value> {
    let Some(event) = event else {
        if raw.trim().is_empty() {
            return Vec::new();
        }
        context.final_text.push_str(raw);
        context.final_text.push('\n');
        return vec![json!({
            "channel": "text",
            "text": context.final_text.trim(),
            "merge_id": "opencode:final"
        })];
    };
    let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
    let properties = event.get("properties").unwrap_or(event);
    if let Some(session) = properties.get("sessionID").and_then(Value::as_str) {
        if !session.is_empty() {
            context.session_id = session.to_string();
        }
    }
    if event_type == "session.error" || event.get("error").is_some() {
        context.success = false;
        context.error = error_message(event, "opencode 报告失败");
        return vec![json!({"channel": "error", "text": context.error})];
    }
    if let Some(part) = properties.get("part").filter(|value| value.is_object()) {
        let part_type = part.get("type").and_then(Value::as_str).unwrap_or("");
        let part_id = part.get("id").and_then(Value::as_str).unwrap_or("");
        if matches!(part_type, "text" | "reasoning") {
            let text = part.get("text").and_then(Value::as_str).unwrap_or("");
            if text.trim().is_empty() {
                return Vec::new();
            }
            context
                .cumulative
                .insert(part_id.to_string(), text.to_string());
            if part_type == "text" {
                context.final_text = text.trim().to_string();
            }
            return vec![json!({
                "channel": if part_type == "text" { "text" } else { "reasoning" },
                "text": text,
                "merge_id": if part_id.is_empty() { Value::Null } else { json!(part_id) }
            })];
        }
        if part_type == "tool" {
            let tool_state = part.get("state").unwrap_or(&Value::Null);
            let status = tool_state
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("");
            let title = tool_state
                .get("title")
                .and_then(Value::as_str)
                .or_else(|| part.get("tool").and_then(Value::as_str))
                .unwrap_or("tool");
            if status == "running" {
                return vec![json!({"channel": "tool", "text": title})];
            }
            if status == "completed" {
                return vec![json!({
                    "channel": "tool_result",
                    "text": truncate(tool_state.get("output").and_then(Value::as_str).unwrap_or("(empty result)"), MAX_EVENT_TEXT)
                })];
            }
        }
    }
    if let Some(delta) = properties.get("delta").and_then(Value::as_str) {
        let part_id = properties
            .get("partID")
            .and_then(Value::as_str)
            .unwrap_or("");
        let text = context.cumulative.entry(part_id.to_string()).or_default();
        text.push_str(delta);
        context.final_text = text.clone();
        return vec![json!({
            "channel": "text",
            "text": text,
            "merge_id": part_id
        })];
    }
    Vec::new()
}

fn finish_context(kind: &str, context: &mut StreamContext, exit_success: bool) {
    if kind == "openclaw" && context.success {
        let raw = context.stdout.join("\n");
        if let Ok(payload) = serde_json::from_str::<Value>(raw.trim()) {
            context.session_id = payload
                .get("session_id")
                .or_else(|| payload.get("sessionId"))
                .and_then(Value::as_str)
                .unwrap_or(&context.session_id)
                .to_string();
            let result = payload.get("result").unwrap_or(&payload);
            context.final_text = response_text(result).unwrap_or_default();
            if payload.get("ok").and_then(Value::as_bool) == Some(false) {
                context.success = false;
                context.error =
                    response_error(&payload).unwrap_or_else(|| "OpenClaw 返回失败".to_string());
            }
        } else {
            context.success = false;
            context.error = "OpenClaw 未返回有效 JSON".to_string();
        }
    }
    if kind == "hermes" || kind == "deepseek_harness" {
        context.final_text = context.final_text.trim().to_string();
    }
    if !exit_success {
        context.success = false;
        if context.error.is_empty() {
            context.error = "子代理进程异常退出".to_string();
        }
    }
    if context.final_text.trim().is_empty() && context.success {
        context.success = false;
        context.error = "子代理未返回答案".to_string();
    }
}

fn build_backend_args(kind: &str, question: &str, session_id: &str, config: &Value) -> Vec<String> {
    let model = string_field(config, "model");
    let effort = string_field(config, "effort");
    let system_prompt = string_field(config, "system_prompt");
    let permission_mode = string_field(config, "permission_mode");
    let auto_approve = bool_field(config, "auto_approve", true);
    let extra_args = string_array(config.get("extra_args"));
    match kind {
        "claude_code" => {
            let mut args = vec![
                "-p".into(),
                question.into(),
                "--output-format".into(),
                "stream-json".into(),
                "--verbose".into(),
                "--include-partial-messages".into(),
            ];
            if !session_id.is_empty() {
                args.extend(["--resume".into(), session_id.into()]);
            }
            if !permission_mode.is_empty() {
                args.extend(["--permission-mode".into(), permission_mode]);
            }
            if !model.is_empty() {
                args.extend(["--model".into(), model]);
            }
            if !effort.is_empty() {
                args.extend(["--effort".into(), effort]);
            }
            if !system_prompt.trim().is_empty() {
                args.extend(["--append-system-prompt".into(), system_prompt]);
            }
            args.extend(extra_args);
            args
        }
        "codex" => {
            let mut args = vec!["exec".into()];
            if !session_id.is_empty() {
                args.extend(["resume".into(), session_id.into()]);
            }
            args.extend(["--json".into(), "--skip-git-repo-check".into()]);
            if bool_field(config, "ephemeral", false) && session_id.is_empty() {
                args.push("--ephemeral".into());
            }
            let sandbox = string_field(config, "sandbox");
            if matches!(
                sandbox.as_str(),
                "bypass" | "danger-full-access-bypass" | "dangerously-bypass-approvals-and-sandbox"
            ) {
                args.push("--dangerously-bypass-approvals-and-sandbox".into());
            } else if !sandbox.is_empty() {
                args.extend(["-c".into(), format!("sandbox_mode=\"{sandbox}\"")]);
                let approval = string_field(config, "approval");
                if !approval.is_empty() {
                    args.extend(["-c".into(), format!("approval_policy=\"{approval}\"")]);
                }
                if bool_field(config, "network_access", false) {
                    args.extend([
                        "-c".into(),
                        "sandbox_workspace_write.network_access=true".into(),
                    ]);
                }
            }
            if !model.is_empty() {
                args.extend(["-m".into(), model]);
            }
            if !effort.is_empty() {
                args.extend(["-c".into(), format!("model_reasoning_effort=\"{effort}\"")]);
            }
            args.extend(extra_args);
            args.push(question.into());
            args
        }
        "grok" => {
            let mut args = vec![
                "--output-format".into(),
                "streaming-json".into(),
                "--permission-mode".into(),
                if permission_mode.is_empty() {
                    "dontAsk".into()
                } else {
                    permission_mode
                },
                "--no-memory".into(),
                "--verbatim".into(),
            ];
            if !model.is_empty() {
                args.extend(["--model".into(), model]);
            }
            if !effort.is_empty() {
                args.extend(["--reasoning-effort".into(), effort]);
            }
            if !system_prompt.trim().is_empty() {
                args.extend(["--rules".into(), system_prompt]);
            }
            if !session_id.is_empty() {
                args.extend(["--resume".into(), session_id.into()]);
            }
            args.extend(extra_args);
            args.push(format!("--single={question}"));
            args
        }
        "antigravity" => {
            let prompt = if session_id.is_empty() && !system_prompt.trim().is_empty() {
                format!("{system_prompt}\n\n{question}")
            } else {
                question.to_string()
            };
            let mut args = vec![
                "-p".into(),
                prompt,
                "--output-format".into(),
                "stream-json".into(),
            ];
            if matches!(
                permission_mode.as_str(),
                "bypassPermissions" | "acceptEdits"
            ) {
                args.push("--dangerously-skip-permissions".into());
            }
            if !session_id.is_empty() {
                args.extend(["--conversation".into(), session_id.into()]);
            }
            if !model.is_empty() {
                args.extend(["--model".into(), model]);
            }
            if matches!(effort.as_str(), "low" | "medium" | "high") {
                args.extend(["--effort".into(), effort]);
            }
            args.extend(extra_args);
            args
        }
        "kimi" => {
            let sid = if session_id.is_empty() {
                Uuid::new_v4().to_string()
            } else {
                session_id.to_string()
            };
            let prompt = if session_id.is_empty() && !system_prompt.trim().is_empty() {
                format!("{}\n\n{question}", system_prompt.trim())
            } else {
                question.to_string()
            };
            let mut args = vec![
                "--print".into(),
                "--output-format".into(),
                "stream-json".into(),
                "--session".into(),
                sid,
            ];
            if auto_approve {
                args.push("--yolo".into());
            }
            args.push(
                if bool_field(config, "thinking", true) {
                    "--thinking"
                } else {
                    "--no-thinking"
                }
                .into(),
            );
            if !model.is_empty() {
                args.extend(["--model".into(), model]);
            }
            args.extend(extra_args);
            args.extend(["--prompt".into(), prompt]);
            args
        }
        "opencode" | "mimo" => {
            let mut args = vec!["run".into(), "--format".into(), "json".into()];
            if !session_id.is_empty() {
                args.extend(["--session".into(), session_id.into()]);
            }
            if !model.is_empty() {
                args.extend(["--model".into(), model]);
            }
            if !effort.is_empty() {
                args.extend(["--variant".into(), effort]);
            }
            if session_id.is_empty() && !system_prompt.trim().is_empty() {
                args.extend(["--system".into(), system_prompt]);
            }
            args.extend(extra_args);
            args.push(question.into());
            args
        }
        "hermes" => {
            let prompt = if session_id.is_empty() && !system_prompt.trim().is_empty() {
                format!("{}\n\n{question}", system_prompt.trim())
            } else {
                question.to_string()
            };
            let mut args = vec!["chat".into(), "--quiet".into()];
            if !session_id.is_empty() {
                args.extend(["--resume".into(), session_id.into()]);
            }
            if !model.is_empty() {
                args.extend(["--model".into(), model]);
            }
            if !effort.is_empty() {
                args.extend(["--reasoning".into(), effort]);
            }
            if auto_approve {
                args.push("--yolo".into());
            }
            args.extend(extra_args);
            args.extend(["--query".into(), prompt]);
            args
        }
        "openclaw" => {
            let sid = if session_id.is_empty() {
                format!("deeptutor-{}", Uuid::new_v4().simple())
            } else {
                session_id.to_string()
            };
            let prompt = if session_id.is_empty() && !system_prompt.trim().is_empty() {
                format!("{}\n\n{question}", system_prompt.trim())
            } else {
                question.to_string()
            };
            let mut args = vec![
                "agent".into(),
                "--session-key".into(),
                sid,
                "--json".into(),
                "--timeout".into(),
                "0".into(),
            ];
            if !model.is_empty() {
                args.extend(["--model".into(), model]);
            }
            if !effort.is_empty() {
                args.extend(["--thinking".into(), effort]);
            }
            args.extend(extra_args);
            args.extend(["--message".into(), prompt]);
            args
        }
        "deepseek_harness" => {
            let prompt = if !system_prompt.trim().is_empty() {
                format!("{}\n\n{question}", system_prompt.trim())
            } else {
                question.to_string()
            };
            let mut args = vec!["--profile".into(), "headless".into()];
            args.extend(extra_args);
            args.push(prompt);
            args
        }
        _ => vec![question.to_string()],
    }
}

fn load_settings(state: &AppState) -> ApiResult<Value> {
    let raw = state
        .database
        .get_setting(SETTINGS_KEY)?
        .unwrap_or_else(default_settings);
    Ok(normalize_settings(&raw))
}

fn default_settings() -> Value {
    json!({
        "consult_budget": DEFAULT_CONSULT_BUDGET,
        "backends": {}
    })
}

fn normalize_settings(raw: &Value) -> Value {
    let budget = raw
        .get("consult_budget")
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_CONSULT_BUDGET)
        .clamp(1, 12);
    let mut backends = Map::new();
    if let Some(values) = raw.get("backends").and_then(Value::as_object) {
        for spec in BACKENDS {
            if let Some(config) = values.get(spec.kind).and_then(Value::as_object) {
                backends.insert(
                    spec.kind.to_string(),
                    normalize_backend_config(spec.kind, config),
                );
            }
        }
    }
    json!({"consult_budget": budget, "backends": backends})
}

fn normalize_backend_config(kind: &str, raw: &Map<String, Value>) -> Value {
    let default_permission = if kind == "grok" {
        "dontAsk"
    } else {
        "bypassPermissions"
    };
    let mut config = json!({
        "enabled": raw.get("enabled").and_then(Value::as_bool).unwrap_or(true),
        "model": raw.get("model").and_then(Value::as_str).unwrap_or(""),
        "effort": raw.get("effort").and_then(Value::as_str).unwrap_or(""),
        "system_prompt": raw.get("system_prompt").and_then(Value::as_str).unwrap_or(""),
        "permission_mode": raw.get("permission_mode").and_then(Value::as_str).unwrap_or(default_permission),
        "sandbox": raw.get("sandbox").and_then(Value::as_str).unwrap_or("workspace-write"),
        "approval": raw.get("approval").and_then(Value::as_str).unwrap_or("never"),
        "network_access": raw.get("network_access").and_then(Value::as_bool).unwrap_or(false),
        "ephemeral": raw.get("ephemeral").and_then(Value::as_bool).unwrap_or(false),
        "auto_approve": raw.get("auto_approve").and_then(Value::as_bool).unwrap_or(true),
        "thinking": raw.get("thinking").and_then(Value::as_bool).unwrap_or(true),
        "forward_images": raw.get("forward_images").and_then(Value::as_bool).unwrap_or(false),
        "extra_args": string_array(raw.get("extra_args"))
    });
    if kind == "hermes_remote" {
        config["base_url"] = json!(raw.get("base_url").and_then(Value::as_str).unwrap_or(""));
        config["api_key_env"] = json!(raw
            .get("api_key_env")
            .and_then(Value::as_str)
            .unwrap_or("DEEPTUTOR_HERMES_REMOTE_API_KEY"));
        config["profile"] = json!(raw.get("profile").and_then(Value::as_str).unwrap_or(""));
        config["idle_timeout_seconds"] = json!(raw
            .get("idle_timeout_seconds")
            .and_then(Value::as_i64)
            .unwrap_or(600)
            .clamp(1, 86_400));
    }
    config
}

fn merge_settings(current: &Value, patch: &Value) -> Value {
    let budget = patch
        .get("consult_budget")
        .and_then(Value::as_i64)
        .or_else(|| current.get("consult_budget").and_then(Value::as_i64))
        .unwrap_or(DEFAULT_CONSULT_BUDGET)
        .clamp(1, 12);
    let mut backends = current
        .get("backends")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Some(incoming) = patch.get("backends").and_then(Value::as_object) {
        for (kind, value) in incoming {
            let Some(spec) = BACKENDS.iter().find(|spec| spec.kind == kind) else {
                continue;
            };
            let mut merged = backends
                .get(kind)
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            if let Some(values) = value.as_object() {
                for (key, item) in values {
                    merged.insert(key.clone(), item.clone());
                }
            }
            backends.insert(kind.clone(), normalize_backend_config(spec.kind, &merged));
        }
    }
    normalize_settings(&json!({"consult_budget": budget, "backends": backends}))
}

fn backend_config(settings: &Value, kind: &str) -> Value {
    settings
        .get("backends")
        .and_then(|value| value.get(kind))
        .cloned()
        .unwrap_or_else(|| normalize_backend_config(kind, &Map::new()))
}

fn backend_spec(kind: &str) -> ApiResult<BackendSpec> {
    BACKENDS
        .iter()
        .copied()
        .find(|spec| spec.kind == kind.trim())
        .ok_or_else(|| ApiError::bad_request(format!("未知子代理类型：{kind}")))
}

fn validate_connection_name(name: &str) -> ApiResult<()> {
    if name.is_empty()
        || name.chars().count() > 80
        || name
            .chars()
            .any(|character| character.is_control() || matches!(character, '/' | '\\' | ':' | '\0'))
    {
        return Err(ApiError::bad_request(
            "子代理名称必须在 1 到 80 个安全字符之间",
        ));
    }
    Ok(())
}

fn resolve_connection_cwd(state: &AppState, raw: &str, local: bool) -> ApiResult<String> {
    if raw.trim().is_empty() || !local {
        return Ok(String::new());
    }
    let path = FilePath::new(raw.trim())
        .canonicalize()
        .map_err(|_| ApiError::bad_request("子代理工作目录不存在"))?;
    let root = state
        .paths
        .root
        .canonicalize()
        .map_err(|error| ApiError::internal(format!("无法解析便携目录：{error}")))?;
    if !path.starts_with(&root) || !path.is_dir() {
        return Err(ApiError::forbidden(
            "子代理工作目录必须位于 DeepTutor 便携安装目录内",
        ));
    }
    Ok(path.to_string_lossy().into_owned())
}

fn session_key(chat_session_id: &str, name: &str) -> String {
    if chat_session_id.trim().is_empty() {
        return String::new();
    }
    let mut digest = Sha256::new();
    digest.update(chat_session_id.as_bytes());
    digest.update([0]);
    digest.update(name.as_bytes());
    hex::encode(digest.finalize())
}

fn initial_session_id(kind: &str, resume_id: &str) -> String {
    if !resume_id.is_empty() {
        return resume_id.to_string();
    }
    if matches!(kind, "kimi" | "openclaw") {
        Uuid::new_v4().to_string()
    } else {
        String::new()
    }
}

fn find_cli(state: &AppState, command: &str) -> Option<PathBuf> {
    if command.trim().is_empty() {
        return None;
    }
    let candidate = PathBuf::from(command);
    if candidate.is_absolute() || candidate.components().count() > 1 {
        return candidate.is_file().then_some(candidate);
    }
    let path = state
        .paths
        .environment()
        .get("PATH")
        .cloned()
        .or_else(|| std::env::var("PATH").ok())
        .unwrap_or_default();
    let extensions = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
            .split(';')
            .map(|value| value.trim().to_ascii_lowercase())
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    for directory in std::env::split_paths(&path) {
        let direct = directory.join(command);
        if direct.is_file() {
            return Some(direct);
        }
        for extension in &extensions {
            let candidate = directory.join(format!("{command}{extension}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

async fn probe_version(state: &AppState, cli: &FilePath, args: &[&str]) -> Result<String, String> {
    run_text_command(state, cli, args, None, Duration::from_secs(8)).await
}

async fn run_text_command(
    state: &AppState,
    cli: &FilePath,
    args: &[&str],
    cwd: Option<&FilePath>,
    timeout: Duration,
) -> Result<String, String> {
    let owned = args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
    let mut command = build_cli_command(cli, &owned);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .envs(state.paths.environment())
        .kill_on_drop(true);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    hide_window(&mut command);
    let output = tokio::time::timeout(timeout, command.output())
        .await
        .map_err(|_| "命令探测超时".to_string())?
        .map_err(|error| format!("命令启动失败：{error}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if output.status.success() {
        Ok(if stdout.is_empty() { stderr } else { stdout })
    } else {
        Err(if stderr.is_empty() { stdout } else { stderr })
    }
}

fn build_cli_command(cli: &FilePath, args: &[String]) -> Command {
    #[cfg(windows)]
    {
        let extension = cli
            .extension()
            .and_then(OsStr::to_str)
            .unwrap_or("")
            .to_ascii_lowercase();
        if matches!(extension.as_str(), "cmd" | "bat" | "ps1") {
            let mut command = Command::new("powershell.exe");
            command.args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                "& $args[0] @($args[1..($args.Length - 1)])",
            ]);
            command.arg(cli);
            command.args(args);
            return command;
        }
    }
    let mut command = Command::new(cli);
    command.args(args);
    command
}

fn hide_window(command: &mut Command) {
    #[cfg(windows)]
    {
        command.creation_flags(0x08000000);
    }
}

fn spawn_line_reader<R>(reader: R, stdout: bool, sender: mpsc::Sender<(bool, String)>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    if sender.send((stdout, line)).await.is_err() {
                        break;
                    }
                }
                Ok(None) | Err(_) => break,
            }
        }
    });
}

async fn send_json(sender: &mpsc::Sender<Result<Bytes, std::io::Error>>, value: &Value) -> bool {
    match ndjson_bytes(value) {
        Ok(bytes) => sender.send(Ok(bytes)).await.is_ok(),
        Err(_) => false,
    }
}

fn ndjson_bytes(value: &Value) -> ApiResult<Bytes> {
    let mut bytes = serde_json::to_vec(value)
        .map_err(|error| ApiError::internal(format!("无法编码子代理事件：{error}")))?;
    bytes.push(b'\n');
    Ok(Bytes::from(bytes))
}

fn codex_home(state: &AppState) -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .filter(|path| path.starts_with(&state.paths.root))
        .unwrap_or_else(|| state.paths.home.join(".codex"))
}

fn read_codex_default_model(home: &FilePath) -> String {
    let path = home.join("config.toml");
    let Ok(raw) = std::fs::read_to_string(path) else {
        return String::new();
    };
    for line in raw.lines() {
        let trimmed = line.trim();
        if let Some(value) = trimmed.strip_prefix("model") {
            let value = value.trim_start();
            if let Some(value) = value.strip_prefix('=') {
                return value
                    .trim()
                    .trim_matches('"')
                    .split('"')
                    .next()
                    .unwrap_or("")
                    .to_string();
            }
        }
    }
    String::new()
}

fn join_http_url(base: &str, path: &str) -> Option<url::Url> {
    let mut url = url::Url::parse(base.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    url.set_path(path);
    Some(url)
}

fn parse_sse_data(line: &str) -> Option<Value> {
    let raw = line.strip_prefix("data:")?.trim();
    if raw.is_empty() || raw == "[DONE]" {
        return None;
    }
    serde_json::from_str(raw).ok()
}

fn content_parts(value: &Value) -> Vec<Value> {
    match value {
        Value::String(text) => vec![json!({"type": "text", "text": text})],
        Value::Array(values) => values
            .iter()
            .filter(|value| value.is_object())
            .cloned()
            .collect(),
        _ => Vec::new(),
    }
}

fn content_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(value) => compact_json(value),
        None => String::new(),
    }
}

fn render_content_text(value: Option<&Value>) -> String {
    let text = content_text(value);
    truncate(&text, MAX_EVENT_TEXT)
}

fn render_tool_use(block: &Value) -> String {
    let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
    let input = block.get("input").unwrap_or(&Value::Null);
    for key in [
        "command",
        "file_path",
        "path",
        "pattern",
        "query",
        "url",
        "prompt",
        "description",
    ] {
        if let Some(value) = input.get(key).and_then(Value::as_str) {
            if !value.trim().is_empty() {
                return format!("{name}({})", truncate(value.trim(), 160));
            }
        }
    }
    format!("{name}({})", truncate(&compact_json(input), 160))
}

fn render_tool_header(name: &str, info: &Value) -> String {
    let args = info.get("args").unwrap_or(info);
    for key in [
        "command",
        "file_path",
        "path",
        "pattern",
        "query",
        "url",
        "prompt",
        "description",
    ] {
        if let Some(value) = args.get(key).and_then(Value::as_str) {
            if !value.trim().is_empty() {
                return format!("{name}({})", truncate(value.trim(), 160));
            }
        }
    }
    name.to_string()
}

fn render_tool_call(call: &Value) -> String {
    let function = call.get("function").unwrap_or(call);
    let name = function
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("tool");
    let arguments = function
        .get("arguments")
        .or_else(|| function.get("input"))
        .unwrap_or(&Value::Null);
    let args = arguments
        .as_str()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .unwrap_or_else(|| arguments.clone());
    render_tool_use(&json!({"name": name, "input": args}))
}

fn response_text(payload: &Value) -> Option<String> {
    if let Some(payloads) = payload.get("payloads").and_then(Value::as_array) {
        let text = payloads
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n\n");
        if !text.trim().is_empty() {
            return Some(text);
        }
    }
    for key in ["final", "text", "response", "output"] {
        if let Some(text) = payload.get(key).and_then(Value::as_str) {
            if !text.trim().is_empty() {
                return Some(text.trim().to_string());
            }
        }
    }
    None
}

fn response_error(payload: &Value) -> Option<String> {
    payload
        .get("error")
        .and_then(|value| {
            value
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| value.as_str())
        })
        .or_else(|| payload.get("summary").and_then(Value::as_str))
        .map(str::to_string)
}

fn error_message(event: &Value, fallback: &str) -> String {
    for key in ["message", "error", "reason", "detail"] {
        if let Some(value) = event.get(key) {
            if let Some(text) = value.as_str() {
                if !text.is_empty() {
                    return text.to_string();
                }
            }
            if let Some(text) = value.get("message").and_then(Value::as_str) {
                if !text.is_empty() {
                    return text.to_string();
                }
            }
        }
    }
    fallback.to_string()
}

fn item_text(item: &Value) -> String {
    for key in ["text", "message", "content", "summary"] {
        if let Some(text) = item.get(key).and_then(Value::as_str) {
            if !text.trim().is_empty() {
                return text.trim().to_string();
            }
        }
    }
    String::new()
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
}

fn truncate(value: &str, limit: usize) -> String {
    let trimmed = value.trim();
    if trimmed.chars().count() <= limit {
        return trimmed.to_string();
    }
    let mut output = trimmed.chars().take(limit).collect::<String>();
    output.push('…');
    output
}

fn string_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

fn bool_field(value: &Value, key: &str, default: bool) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(default)
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    value
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 设置合并只保留已知后端() {
        let current = default_settings();
        let updated = merge_settings(
            &current,
            &json!({
                "consult_budget": 99,
                "backends": {
                    "codex": {"model": "gpt-test"},
                    "unknown": {"model": "bad"}
                }
            }),
        );
        assert_eq!(updated["consult_budget"], 12);
        assert_eq!(updated["backends"]["codex"]["model"], "gpt-test");
        assert!(updated["backends"].get("unknown").is_none());
    }

    #[test]
    fn 会话键不暴露原始标识() {
        let key = session_key("chat-session", "my-agent");
        assert_eq!(key.len(), 64);
        assert!(!key.contains("chat"));
        assert!(session_key("", "my-agent").is_empty());
    }

    #[test]
    fn SSE数据仅接受data字段() {
        assert_eq!(
            parse_sse_data(r#"data: {"event":"message.delta"}"#),
            Some(json!({"event": "message.delta"}))
        );
        assert_eq!(parse_sse_data("event: x"), None);
    }
}
