use super::error::{ApiError, ApiResult};
use super::{AppState, ScopedState};
use axum::extract::{Extension, Query};
use axum::routing::{get, post};
use axum::{Json, Router};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const DOCUMENT_SETTING_KEY: &str = "document_parsing";
const LEGACY_MINERU_KEY: &str = "mineru";
const MAX_JOB_LINES: usize = 500;

#[derive(Clone, Debug)]
struct JobStatus {
    state: String,
    kind: String,
    engine: String,
    lines: Vec<String>,
    message: String,
}

impl Default for JobStatus {
    fn default() -> Self {
        Self {
            state: "idle".into(),
            kind: String::new(),
            engine: String::new(),
            lines: Vec::new(),
            message: String::new(),
        }
    }
}

#[derive(Clone)]
pub(super) struct DocumentJobManager {
    status: Arc<Mutex<JobStatus>>,
    cancel: Arc<Mutex<Option<CancellationToken>>>,
}

impl DocumentJobManager {
    pub(super) fn new() -> Self {
        Self {
            status: Arc::new(Mutex::new(JobStatus::default())),
            cancel: Arc::new(Mutex::new(None)),
        }
    }

    fn begin(&self, kind: &str, engine: &str) -> Result<CancellationToken, String> {
        let mut status = self.status.lock();
        if status.state == "running" {
            return Err("已有文档解析任务正在运行，请等待完成或先取消。".into());
        }
        let token = CancellationToken::new();
        *status = JobStatus {
            state: "running".into(),
            kind: kind.into(),
            engine: engine.into(),
            lines: Vec::new(),
            message: String::new(),
        };
        *self.cancel.lock() = Some(token.clone());
        Ok(token)
    }

    fn push_line(&self, line: String) {
        let mut status = self.status.lock();
        status.lines.push(line);
        if status.lines.len() > MAX_JOB_LINES {
            let remove = status.lines.len() - MAX_JOB_LINES;
            status.lines.drain(0..remove);
        }
    }

    fn finish(&self, state: &str, message: impl Into<String>) {
        let mut status = self.status.lock();
        // 取消接口会先写入终态；后台任务退出时不能把 cancelled 覆盖成 failed。
        if status.state == "cancelled" {
            return;
        }
        status.state = state.into();
        status.message = message.into();
        *self.cancel.lock() = None;
    }

    fn public_status(&self, cursor: usize) -> Value {
        let status = self.status.lock().clone();
        let start = cursor.min(status.lines.len());
        json!({
            "state": status.state,
            "kind": status.kind,
            "engine": status.engine,
            "lines": status.lines[start..],
            "next_cursor": status.lines.len(),
            "message": status.message
        })
    }

    fn cancel(&self) -> Value {
        let state = self.status.lock().state.clone();
        if state != "running" {
            return json!({"ok": false, "message": "当前没有正在运行的文档解析任务。"});
        }
        if let Some(token) = self.cancel.lock().as_ref() {
            token.cancel();
        }
        let mut status = self.status.lock();
        status.state = "cancelled".into();
        status.message = "任务已取消。".into();
        *self.cancel.lock() = None;
        json!({"ok": true, "message": "任务已取消。"})
    }

    async fn start_command(
        &self,
        kind: &str,
        engine: &str,
        program: PathBuf,
        args: Vec<String>,
        environment: Vec<(String, String)>,
    ) -> Result<(), String> {
        let token = self.begin(kind, engine)?;
        let manager = self.clone();
        tokio::spawn(async move {
            let result =
                run_command_with_logs(manager.clone(), token.clone(), program, args, environment)
                    .await;
            match result {
                Ok(true) if !token.is_cancelled() => {
                    manager.finish("done", "任务已完成。");
                }
                Ok(true) => manager.finish("cancelled", "任务已取消。"),
                Ok(false) if !token.is_cancelled() => {
                    manager.finish("failed", "命令执行失败，请查看上方日志。");
                }
                Ok(false) => manager.finish("cancelled", "任务已取消。"),
                Err(error) => manager.finish("failed", error),
            }
        });
        Ok(())
    }
}

async fn run_command_with_logs(
    manager: DocumentJobManager,
    cancel: CancellationToken,
    program: PathBuf,
    args: Vec<String>,
    environment: Vec<(String, String)>,
) -> Result<bool, String> {
    let mut command = tokio::process::Command::new(&program);
    command
        .args(&args)
        .envs(environment)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|error| format!("无法启动 {}：{error}", program.display()))?;
    manager.push_line(format!(
        "> {} {}",
        program.display(),
        args.iter()
            .map(|value| shell_display_arg(value))
            .collect::<Vec<_>>()
            .join(" ")
    ));

    let (line_tx, mut line_rx) = mpsc::unbounded_channel::<String>();
    if let Some(stdout) = child.stdout.take() {
        let sender = line_tx.clone();
        tokio::spawn(read_lines(stdout, sender));
    }
    if let Some(stderr) = child.stderr.take() {
        let sender = line_tx.clone();
        tokio::spawn(read_lines(stderr, sender));
    }
    drop(line_tx);

    let mut poll = tokio::time::interval(Duration::from_millis(250));
    let mut readers_done = false;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = child.kill().await;
                manager.push_line("任务已取消。".into());
                return Ok(true);
            }
            line = line_rx.recv(), if !readers_done => {
                match line {
                    Some(line) => manager.push_line(line),
                    None => readers_done = true,
                }
            }
            _ = poll.tick() => {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        while let Ok(line) = line_rx.try_recv() {
                            manager.push_line(line);
                        }
                        manager.push_line(format!("进程退出：{status}"));
                        return Ok(status.success());
                    }
                    Ok(None) => {}
                    Err(error) => return Err(format!("无法读取子进程状态：{error}")),
                }
            }
        }
    }
}

async fn read_lines<R>(reader: R, sender: mpsc::UnboundedSender<String>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let trimmed = line.trim_end().to_string();
        if !trimmed.is_empty() {
            let _ = sender.send(trimmed);
        }
    }
}

fn shell_display_arg(value: &str) -> String {
    if value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "-_./:\\".contains(character))
    {
        value.to_string()
    } else {
        format!("\"{}\"", value.replace('"', "\\\""))
    }
}

#[derive(Debug, Deserialize, Default)]
struct CursorQuery {
    cursor: Option<usize>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/settings/document-parsing",
            get(get_document_parsing).put(put_document_parsing),
        )
        .route(
            "/settings/document-parsing/test",
            post(test_document_parsing),
        )
        .route(
            "/settings/document-parsing/docling/test",
            post(test_docling),
        )
        .route("/settings/document-parsing/tika/test", post(test_tika))
        .route(
            "/settings/document-parsing/install",
            post(start_document_parsing_install),
        )
        .route(
            "/settings/document-parsing/models/download",
            post(start_document_parsing_model_download),
        )
        .route(
            "/settings/document-parsing/job/status",
            get(document_parsing_job_status),
        )
        .route(
            "/settings/document-parsing/job/cancel",
            post(cancel_document_parsing_job),
        )
        .route("/settings/mineru", get(get_mineru).put(put_mineru))
        .route(
            "/settings/mineru/models/download",
            post(start_mineru_model_download),
        )
        .route(
            "/settings/mineru/models/download/status",
            get(mineru_model_download_status),
        )
        .route(
            "/settings/mineru/models/download/cancel",
            post(cancel_mineru_model_download),
        )
        .route("/settings/mineru/test", post(test_mineru))
}

async fn get_document_parsing(
    ScopedState(state): ScopedState,
    Extension(principal): Extension<super::auth::Principal>,
) -> ApiResult<Json<Value>> {
    super::auth::require_admin(&principal)?;
    Ok(Json(document_payload(&state)?))
}

async fn put_document_parsing(
    ScopedState(state): ScopedState,
    Extension(principal): Extension<super::auth::Principal>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    super::auth::require_admin(&principal)?;
    let mut current = load_document_settings(&state)?;
    if let Some(engine) = body.get("engine").and_then(Value::as_str) {
        if !known_engine(engine) {
            return Err(ApiError::bad_request(format!(
                "未知的文档解析引擎：{engine}"
            )));
        }
        current["engine"] = Value::String(engine.into());
    }
    if let Some(engines) = body.get("engines").and_then(Value::as_object) {
        let target = current
            .get_mut("engines")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| ApiError::internal("文档解析配置结构损坏"))?;
        for (name, patch) in engines {
            if !known_engine(name) || !patch.is_object() {
                continue;
            }
            let mut patch = patch.clone();
            if patch.get("api_token").is_some_and(Value::is_null) {
                patch
                    .as_object_mut()
                    .map(|object| object.remove("api_token"));
            }
            crate::storage::merge_json(
                target
                    .entry(name.clone())
                    .or_insert_with(|| Value::Object(Default::default())),
                &patch,
            );
        }
    }
    if let Some(value) = body.get("image_caption").and_then(Value::as_bool) {
        current["image_caption"] = Value::Bool(value);
    }
    if body
        .as_object()
        .is_some_and(|object| object.contains_key("image_description_model"))
    {
        current["image_description_model"] = body
            .get("image_description_model")
            .cloned()
            .unwrap_or(Value::Null);
    }
    current["version"] = Value::from(2);
    state.database.set_setting(DOCUMENT_SETTING_KEY, &current)?;
    Ok(Json(document_payload(&state)?))
}

async fn get_mineru(
    ScopedState(state): ScopedState,
    Extension(principal): Extension<super::auth::Principal>,
) -> ApiResult<Json<Value>> {
    super::auth::require_admin(&principal)?;
    Ok(Json(mineru_payload(&state)?))
}

async fn put_mineru(
    ScopedState(state): ScopedState,
    Extension(principal): Extension<super::auth::Principal>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    super::auth::require_admin(&principal)?;
    let mut current = load_document_settings(&state)?;
    let allowed = [
        "mode",
        "api_base_url",
        "local_cli_path",
        "model_download_source",
        "model_download_endpoint",
        "model_version",
        "language",
        "enable_formula",
        "enable_table",
        "is_ocr",
        "allow_local_model_download",
    ];
    let mineru = current
        .pointer_mut("/engines/mineru")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| ApiError::internal("MinerU 配置结构损坏"))?;
    for key in allowed {
        if let Some(value) = body.get(key) {
            mineru.insert(key.into(), value.clone());
        }
    }
    if let Some(token) = body.get("api_token") {
        if !token.is_null() {
            mineru.insert("api_token".into(), token.clone());
        }
    }
    state.database.set_setting(DOCUMENT_SETTING_KEY, &current)?;
    Ok(Json(mineru_payload(&state)?))
}

async fn test_document_parsing(
    ScopedState(state): ScopedState,
    Extension(principal): Extension<super::auth::Principal>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    super::auth::require_admin(&principal)?;
    let settings = load_document_settings(&state)?;
    let engine = body
        .get("engine")
        .and_then(Value::as_str)
        .or_else(|| settings.get("engine").and_then(Value::as_str))
        .unwrap_or("text_only");
    let readiness = readiness(&state, &settings, engine);
    Ok(Json(json!({
        "ok": readiness["ready"],
        "message": readiness["message"]
    })))
}

async fn test_docling(
    ScopedState(state): ScopedState,
    Extension(principal): Extension<super::auth::Principal>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    super::auth::require_admin(&principal)?;
    let stored = load_document_settings(&state)?;
    let stored_token = stored
        .pointer("/engines/docling/api_token")
        .and_then(Value::as_str)
        .unwrap_or("");
    let base_url = body
        .get("api_base_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("http://localhost:5001")
        .trim_end_matches('/');
    let token = match body.get("api_token") {
        Some(Value::String(value)) => value.trim(),
        _ => stored_token,
    };
    let result = probe_http_service(&state, base_url, "/health", token).await;
    Ok(Json(result))
}

async fn test_tika(
    ScopedState(state): ScopedState,
    Extension(principal): Extension<super::auth::Principal>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    super::auth::require_admin(&principal)?;
    let base_url = body
        .get("server_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("http://localhost:9998")
        .trim_end_matches('/');
    Ok(Json(
        probe_http_service(&state, base_url, "/version", "").await,
    ))
}

async fn probe_http_service(state: &AppState, base_url: &str, path: &str, token: &str) -> Value {
    if !valid_http_url(base_url) {
        return json!({"ok": false, "message": "服务地址必须是有效的 HTTP 或 HTTPS 地址。"});
    }
    let mut request = state
        .client
        .get(format!("{base_url}{path}"))
        .timeout(Duration::from_secs(20));
    if !token.is_empty() {
        request = request.header("X-Api-Key", token);
    }
    match request.send().await {
        Ok(response) if response.status().is_success() => {
            let version = response.text().await.unwrap_or_default();
            json!({
                "ok": true,
                "message": if version.trim().is_empty() {
                    "连接成功，服务已就绪。".to_string()
                } else {
                    format!("连接成功：{}", version.trim().chars().take(200).collect::<String>())
                }
            })
        }
        Ok(response) => json!({
            "ok": false,
            "message": format!("服务返回 HTTP {}。", response.status())
        }),
        Err(error) => json!({
            "ok": false,
            "message": format!("连接失败：{error}")
        }),
    }
}

async fn start_document_parsing_install(
    ScopedState(state): ScopedState,
    Extension(principal): Extension<super::auth::Principal>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    super::auth::require_admin(&principal)?;
    let engine = normalize_engine(body.get("engine").and_then(Value::as_str).unwrap_or(""));
    Ok(Json(json!({
        "ok": false,
        "message": format!(
            "纯 Rust 便携版不安装 Python 解析包（请求引擎：{}）。内置解析器和远程 Tika、Docling、MinerU 服务仍可独立使用。",
            if engine.is_empty() { "unknown" } else { &engine }
        )
    })))
}

async fn start_document_parsing_model_download(
    ScopedState(state): ScopedState,
    Extension(principal): Extension<super::auth::Principal>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    super::auth::require_admin(&principal)?;
    let engine = normalize_engine(body.get("engine").and_then(Value::as_str).unwrap_or(""));
    if engine != "docling" {
        return Ok(Json(json!({
            "ok": false,
            "message": format!("引擎 {engine} 没有受支持的模型下载器。")
        })));
    }
    let Some(program) = find_executable("docling-tools") else {
        return Ok(Json(json!({
            "ok": false,
            "message": "未找到 docling-tools。请把 Docling 工具安装到 PATH，或在纯 Rust 模式下连接远程 Docling Serve。"
        })));
    };
    state
        .document_jobs
        .start_command(
            "model_download",
            &engine,
            program,
            vec!["models".into(), "download".into()],
            vec![("TMP".into(), state.paths.temp.display().to_string())],
        )
        .await
        .map_err(ApiError::conflict)?;
    Ok(Json(
        json!({"ok": true, "message": "Docling 模型下载已启动。"}),
    ))
}

async fn document_parsing_job_status(
    ScopedState(state): ScopedState,
    Extension(principal): Extension<super::auth::Principal>,
    Query(query): Query<CursorQuery>,
) -> ApiResult<Json<Value>> {
    super::auth::require_admin(&principal)?;
    let status = state
        .document_jobs
        .public_status(query.cursor.unwrap_or_default());
    Ok(Json(status))
}

async fn cancel_document_parsing_job(
    ScopedState(state): ScopedState,
    Extension(principal): Extension<super::auth::Principal>,
) -> ApiResult<Json<Value>> {
    super::auth::require_admin(&principal)?;
    Ok(Json(state.document_jobs.cancel()))
}

async fn start_mineru_model_download(
    ScopedState(state): ScopedState,
    Extension(principal): Extension<super::auth::Principal>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    super::auth::require_admin(&principal)?;
    let model_type = body
        .get("model_type")
        .and_then(Value::as_str)
        .unwrap_or("pipeline");
    let source = body
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("huggingface");
    if !matches!(model_type, "pipeline" | "vlm" | "all") {
        return Err(ApiError::bad_request("MinerU 模型类型无效。"));
    }
    if !matches!(source, "huggingface" | "modelscope") {
        return Err(ApiError::bad_request("MinerU 模型下载源无效。"));
    }
    let endpoint = body
        .get("endpoint")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .trim_end_matches('/');
    if !endpoint.is_empty() && !valid_http_url(endpoint) {
        return Err(ApiError::bad_request(
            "MinerU 下载地址必须是有效的 HTTP 或 HTTPS 地址。",
        ));
    }
    let cli_path = body
        .get("local_cli_path")
        .and_then(Value::as_str)
        .unwrap_or("");
    let Some(program) = resolve_mineru_downloader(cli_path) else {
        return Ok(Json(json!({
            "ok": false,
            "message": "未找到 mineru-models-download。请配置 MinerU CLI 路径或切换到云端 API。"
        })));
    };
    let mut environment = vec![("TMP".into(), state.paths.temp.display().to_string())];
    if !endpoint.is_empty() {
        environment.push(("HF_ENDPOINT".into(), endpoint.into()));
    }
    state
        .mineru_download_jobs
        .start_command(
            "model_download",
            "mineru",
            program,
            vec!["-s".into(), source.into(), "-m".into(), model_type.into()],
            environment,
        )
        .await
        .map_err(ApiError::conflict)?;
    Ok(Json(
        json!({"ok": true, "message": "MinerU 模型下载已启动。"}),
    ))
}

async fn mineru_model_download_status(
    ScopedState(state): ScopedState,
    Extension(principal): Extension<super::auth::Principal>,
    Query(query): Query<CursorQuery>,
) -> ApiResult<Json<Value>> {
    super::auth::require_admin(&principal)?;
    Ok(Json(
        state
            .mineru_download_jobs
            .public_status(query.cursor.unwrap_or_default()),
    ))
}

async fn cancel_mineru_model_download(
    ScopedState(state): ScopedState,
    Extension(principal): Extension<super::auth::Principal>,
) -> ApiResult<Json<Value>> {
    super::auth::require_admin(&principal)?;
    Ok(Json(state.mineru_download_jobs.cancel()))
}

async fn test_mineru(
    ScopedState(state): ScopedState,
    Extension(principal): Extension<super::auth::Principal>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    super::auth::require_admin(&principal)?;
    let mode = body.get("mode").and_then(Value::as_str).unwrap_or("local");
    if mode == "local" {
        return Ok(Json(test_mineru_local(&state, &body).await));
    }

    let stored = load_document_settings(&state)?;
    let stored_token = stored
        .pointer("/engines/mineru/api_token")
        .and_then(Value::as_str)
        .unwrap_or("");
    let token = match body.get("api_token") {
        Some(Value::String(value)) => value.trim(),
        _ => stored_token,
    };
    if token.is_empty() {
        return Ok(Json(
            json!({"ok": false, "message": "尚未配置 MinerU API token。"}),
        ));
    }
    let base_url = body
        .get("api_base_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("https://mineru.net")
        .trim_end_matches('/');
    if !valid_http_url(base_url) {
        return Ok(Json(
            json!({"ok": false, "message": "MinerU API 地址无效。"}),
        ));
    }
    let payload = json!({
        "files": [{"name": "connectivity-check.pdf", "is_ocr": false}],
        "model_version": body.get("model_version").and_then(Value::as_str).unwrap_or("pipeline"),
        "enable_formula": body.get("enable_formula").and_then(Value::as_bool).unwrap_or(true),
        "enable_table": body.get("enable_table").and_then(Value::as_bool).unwrap_or(true)
    });
    let response = state
        .client
        .post(format!("{base_url}/api/v4/file-urls/batch"))
        .bearer_auth(token)
        .json(&payload)
        .timeout(Duration::from_secs(30))
        .send()
        .await;
    let result = match response {
        Ok(response) if response.status().is_success() => {
            let body = response.json::<Value>().await.unwrap_or_else(|_| json!({}));
            if body.get("code").and_then(Value::as_i64).unwrap_or(0) == 0 {
                json!({"ok": true, "message": "MinerU API token 有效。"})
            } else {
                json!({
                    "ok": false,
                    "message": format!(
                        "MinerU API 返回错误：{}",
                        body.get("msg").and_then(Value::as_str).unwrap_or("未知错误")
                    )
                })
            }
        }
        Ok(response) if matches!(response.status().as_u16(), 401 | 403) => {
            json!({"ok": false, "message": "MinerU API 拒绝了该 token。"})
        }
        Ok(response) => {
            json!({"ok": false, "message": format!("MinerU API 返回 HTTP {}。", response.status())})
        }
        Err(error) => json!({"ok": false, "message": format!("MinerU API 连接失败：{error}")}),
    };
    Ok(Json(result))
}

async fn test_mineru_local(state: &AppState, body: &Value) -> Value {
    let cli_path = body
        .get("local_cli_path")
        .and_then(Value::as_str)
        .unwrap_or("");
    let probe = local_cli_probe(cli_path);
    if !probe.get("found").and_then(Value::as_bool).unwrap_or(false) {
        return json!({
            "ok": false,
            "message": if probe.get("source").and_then(Value::as_str) == Some("configured") {
                "配置的 MinerU CLI 路径不存在或不可执行。"
            } else {
                "未在 PATH 中找到 MinerU CLI，请配置路径或改用云端 API。"
            }
        });
    }
    let command = probe
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let version = command_version(state, Path::new(command)).await;
    match version {
        Some(version) if mineru_version_current(&version) => json!({
            "ok": true,
            "message": format!("MinerU CLI 已就绪：{version}")
        }),
        Some(version) => json!({
            "ok": false,
            "message": format!("MinerU 版本过低或不兼容：{version}，需要 3.4.5 或更高版本。")
        }),
        None => json!({
            "ok": false,
            "message": "MinerU CLI 无法执行 --version，请检查安装。"
        }),
    }
}

async fn command_version(state: &AppState, program: &Path) -> Option<String> {
    let output = tokio::process::Command::new(program)
        .arg("--version")
        .env("TMP", &state.paths.temp)
        .stdin(Stdio::null())
        .output();
    let output = tokio::time::timeout(Duration::from_secs(12), output)
        .await
        .ok()?
        .ok()?;
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    combined
        .lines()
        .find(|line| line.chars().any(|character| character.is_ascii_digit()))
        .map(|line| line.trim().chars().take(200).collect())
}

fn mineru_version_current(version: &str) -> bool {
    let Some(start) = version.find(|character: char| character.is_ascii_digit()) else {
        return false;
    };
    let numbers = version[start..]
        .split(|character: char| !character.is_ascii_digit())
        .filter(|value| !value.is_empty())
        .take(3)
        .filter_map(|value| value.parse::<u64>().ok())
        .collect::<Vec<_>>();
    numbers.as_slice().cmp(&[3, 4, 5]).is_ge()
}

fn document_payload(state: &AppState) -> Result<Value, String> {
    let settings = load_document_settings(state)?;
    let mut redacted = settings.clone();
    if let Some(engines) = redacted.get_mut("engines").and_then(Value::as_object_mut) {
        for engine in engines.values_mut() {
            if let Some(object) = engine.as_object_mut() {
                object.remove("api_token");
            }
        }
    }
    let engine_ids = [
        "text_only",
        "mineru",
        "docling",
        "markitdown",
        "pymupdf4llm",
        "liteparse",
        "tika",
    ];
    let engines = engine_ids
        .iter()
        .map(|engine| {
            let (description, local_models) = match *engine {
                "text_only" => ("内置 PDF、Office、HTML 与纯文本提取器。", false),
                "mineru" => ("MinerU 本地命令行或 mineru.net 云端高精度解析。", true),
                "docling" => ("Docling Serve 远程结构化解析。", true),
                "markitdown" => ("旧版 Python MarkItDown 解析器，纯 Rust 版不内置。", false),
                "pymupdf4llm" => ("旧版 Python PyMuPDF4LLM 解析器，纯 Rust 版不内置。", false),
                "liteparse" => ("旧版 Python LiteParse 包装器，纯 Rust 版不内置。", false),
                "tika" => ("Apache Tika 远程服务。", false),
                _ => ("未知解析器。", false),
            };
            let available = match *engine {
                "text_only" => true,
                "mineru" | "docling" | "tika" => true,
                _ => false,
            };
            json!({
                "id": engine,
                "name": match *engine {
                    "text_only" => "Text-only",
                    "mineru" => "MinerU",
                    "docling" => "Docling",
                    "markitdown" => "MarkItDown",
                    "pymupdf4llm" => "PyMuPDF4LLM",
                    "liteparse" => "LiteParse",
                    "tika" => "Apache Tika",
                    _ => engine,
                },
                "description": description,
                "needs_local_models": local_models,
                "available": available
            })
        })
        .collect::<Vec<_>>();
    let checks = engine_ids
        .iter()
        .map(|engine| ((*engine).to_string(), readiness(state, &settings, engine)))
        .collect::<serde_json::Map<_, _>>();
    let mineru_token_set = settings
        .pointer("/engines/mineru/api_token")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty());
    let docling_token_set = settings
        .pointer("/engines/docling/api_token")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty());
    Ok(json!({
        "version": 2,
        "engine": redacted.get("engine").cloned().unwrap_or_else(|| json!("text_only")),
        "image_caption": redacted.get("image_caption").cloned().unwrap_or_else(|| json!(false)),
        "image_description_model": redacted.get("image_description_model").cloned().unwrap_or(Value::Null),
        "engines": redacted.get("engines").cloned().unwrap_or_else(|| json!({})),
        "available_engines": engines,
        "readiness": checks,
        "installable": [],
        "model_downloadable": if find_executable("docling-tools").is_some() { json!(["docling"]) } else { json!([]) },
        "mineru": {
            "api_token_set": mineru_token_set,
            "local_cli": local_cli_probe(
                settings
                    .pointer("/engines/mineru/local_cli_path")
                    .and_then(Value::as_str)
                    .unwrap_or("")
            )
        },
        "docling": {"api_token_set": docling_token_set}
    }))
}

fn mineru_payload(state: &AppState) -> Result<Value, String> {
    let settings = load_document_settings(state)?;
    let mineru = settings
        .pointer("/engines/mineru")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let token_set = mineru
        .get("api_token")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty());
    let mut public = mineru.clone();
    if let Some(object) = public.as_object_mut() {
        object.remove("api_token");
    }
    Ok(json!({
        "settings": public,
        "api_token_set": token_set,
        "local_cli": local_cli_probe(
            mineru.get("local_cli_path").and_then(Value::as_str).unwrap_or("")
        )
    }))
}

fn readiness(_state: &AppState, settings: &Value, engine: &str) -> Value {
    match engine {
        "text_only" => {
            json!({"ready": true, "reason": "ready", "message": "内置解析器已就绪。"})
        }
        "mineru" => {
            let slice = settings
                .pointer("/engines/mineru")
                .cloned()
                .unwrap_or_else(|| json!({}));
            if slice.get("mode").and_then(Value::as_str) == Some("cloud") {
                if slice
                    .get("api_token")
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.is_empty())
                {
                    json!({"ready": true, "reason": "ready", "message": "MinerU 云端 API 已配置。"})
                } else {
                    json!({"ready": false, "reason": "token_missing", "message": "请先配置 MinerU API token。"})
                }
            } else {
                let probe = local_cli_probe(
                    slice
                        .get("local_cli_path")
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                );
                if probe.get("found").and_then(Value::as_bool).unwrap_or(false) {
                    json!({"ready": true, "reason": "ready", "message": "MinerU CLI 已找到。"})
                } else {
                    json!({"ready": false, "reason": "cli_missing", "message": "未找到 MinerU CLI；可配置路径或切换云端 API。"})
                }
            }
        }
        "docling" => {
            let slice = settings
                .pointer("/engines/docling")
                .cloned()
                .unwrap_or_else(|| json!({}));
            if slice.get("mode").and_then(Value::as_str) == Some("remote") {
                let url = slice
                    .get("api_base_url")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                json!({
                    "ready": valid_http_url(url),
                    "reason": if valid_http_url(url) {"ready"} else {"not_configured"},
                    "message": if valid_http_url(url) {
                        "Docling 远程服务地址已配置。"
                    } else {
                        "请配置 Docling Serve 地址。"
                    }
                })
            } else if find_executable("docling").is_some()
                || find_executable("docling-tools").is_some()
            {
                json!({"ready": true, "reason": "ready", "message": "Docling 工具已找到。"})
            } else {
                json!({"ready": false, "reason": "runtime_missing", "message": "纯 Rust 版不内置 Docling 本地运行时，请切换远程服务。"})
            }
        }
        "tika" => {
            let url = settings
                .pointer("/engines/tika/server_url")
                .and_then(Value::as_str)
                .unwrap_or("");
            json!({
                "ready": valid_http_url(url),
                "reason": if valid_http_url(url) {"ready"} else {"not_configured"},
                "message": if valid_http_url(url) {
                    "Tika 服务地址已配置。"
                } else {
                    "请配置 Apache Tika 服务地址。"
                }
            })
        }
        _ => json!({
            "ready": false,
            "reason": "external_parser_required",
            "message": "该解析器需要用户自行配置外部文档解析服务。"
        }),
    }
}

fn load_document_settings(state: &AppState) -> Result<Value, String> {
    let mut settings = default_document_settings();
    if let Some(legacy) = state.database.get_setting(LEGACY_MINERU_KEY)? {
        if legacy.is_object() && state.database.get_setting(DOCUMENT_SETTING_KEY)?.is_none() {
            if let Some(target) = settings.pointer_mut("/engines/mineru") {
                crate::storage::merge_json(target, &legacy);
            }
        }
    }
    if let Some(stored) = state.database.get_setting(DOCUMENT_SETTING_KEY)? {
        crate::storage::merge_json(&mut settings, &stored);
    }
    if let Some(engines) = settings.get_mut("engines").and_then(Value::as_object_mut) {
        for (name, defaults) in default_engines() {
            let target = engines
                .entry(name.to_string())
                .or_insert_with(|| Value::Object(Default::default()));
            let mut merged = defaults;
            crate::storage::merge_json(&mut merged, target);
            *target = merged;
        }
    }
    Ok(settings)
}

/// 供统一解析服务读取合并默认值后的文档解析设置。
pub(super) fn load_settings_for_state(state: &AppState) -> Result<Value, String> {
    load_document_settings(state)
}

fn default_document_settings() -> Value {
    json!({
        "version": 2,
        "engine": "text_only",
        "image_caption": false,
        "image_description_model": null,
        "engines": default_engines()
    })
}

fn default_engines() -> serde_json::Map<String, Value> {
    json!({
        "text_only": {},
        "mineru": {
            "mode": "local",
            "api_base_url": "https://mineru.net",
            "api_token": "",
            "local_cli_path": "",
            "model_download_source": "huggingface",
            "model_download_endpoint": "",
            "model_version": "pipeline",
            "language": "auto",
            "enable_formula": true,
            "enable_table": true,
            "is_ocr": false,
            "max_pages_per_part": 180,
            "normalize_tiny_scans": false,
            "allow_local_model_download": false
        },
        "docling": {
            "mode": "local",
            "api_base_url": "http://localhost:5001",
            "api_token": "",
            "do_ocr": false,
            "do_table_structure": true,
            "allow_local_model_download": false
        },
        "markitdown": {"enable_llm_image_description": false},
        "pymupdf4llm": {"write_images": true, "image_format": "png", "image_dpi": 150},
        "liteparse": {"image_mode": "placeholder", "extract_links": true, "extract_images": false, "max_pages": 0},
        "tika": {"server_url": "http://localhost:9998"}
    })
    .as_object()
    .cloned()
    .unwrap_or_default()
}

fn known_engine(engine: &str) -> bool {
    matches!(
        normalize_engine(engine).as_str(),
        "text_only" | "mineru" | "docling" | "markitdown" | "pymupdf4llm" | "liteparse" | "tika"
    )
}

fn normalize_engine(engine: &str) -> String {
    engine.trim().to_ascii_lowercase().replace(['-', ' '], "_")
}

fn valid_http_url(value: &str) -> bool {
    url::Url::parse(value)
        .ok()
        .is_some_and(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
}

fn local_cli_probe(configured: &str) -> Value {
    let configured = configured.trim();
    if !configured.is_empty() {
        let path = PathBuf::from(configured);
        let exists = path.is_file();
        return json!({
            "found": exists,
            "command": path.file_name().and_then(|value| value.to_str()).unwrap_or("mineru"),
            "path": path.display().to_string(),
            "source": "configured"
        });
    }
    if let Some(path) = find_executable("mineru")
        .or_else(|| find_executable("magic-pdf"))
        .or_else(|| find_executable("mineru.exe"))
    {
        return json!({
            "found": true,
            "command": path.file_name().and_then(|value| value.to_str()).unwrap_or("mineru"),
            "path": path.display().to_string(),
            "source": "path"
        });
    }
    json!({"found": false, "command": "mineru", "path": "", "source": "path"})
}

fn resolve_mineru_downloader(configured_cli: &str) -> Option<PathBuf> {
    if !configured_cli.trim().is_empty() {
        let cli = PathBuf::from(configured_cli.trim());
        if let Some(parent) = cli.parent() {
            for candidate in [
                parent.join(exe_name("mineru-models-download")),
                parent
                    .join("Scripts")
                    .join(exe_name("mineru-models-download")),
                parent
                    .parent()
                    .unwrap_or(parent)
                    .join("Scripts")
                    .join(exe_name("mineru-models-download")),
            ] {
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    find_executable("mineru-models-download")
}

fn find_executable(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let candidates = executable_names(name);
    for directory in std::env::split_paths(&path) {
        for candidate in &candidates {
            let path = directory.join(candidate);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

fn executable_names(name: &str) -> Vec<String> {
    if cfg!(windows) {
        let mut names = vec![name.to_string()];
        if !name.to_ascii_lowercase().ends_with(".exe") {
            names.insert(0, format!("{name}.exe"));
        }
        if let Some(extensions) = std::env::var_os("PATHEXT") {
            for extension in extensions
                .to_string_lossy()
                .split(';')
                .filter(|value| !value.trim().is_empty())
            {
                let candidate = format!("{name}{}", extension.trim().to_ascii_lowercase());
                if !names
                    .iter()
                    .any(|item| item.eq_ignore_ascii_case(&candidate))
                {
                    names.push(candidate);
                }
            }
        }
        names
    } else {
        vec![name.to_string()]
    }
}

fn exe_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mineru_versions_follow_the_supported_floor() {
        assert!(mineru_version_current("mineru 3.4.5"));
        assert!(mineru_version_current("3.10.0"));
        assert!(!mineru_version_current("3.4.4"));
        assert!(!mineru_version_current("unknown"));
    }

    #[test]
    fn document_defaults_do_not_echo_secrets() {
        let defaults = default_document_settings();
        assert_eq!(defaults["engine"], "text_only");
        assert!(defaults.pointer("/engines/mineru/api_token").is_some());
    }
}
