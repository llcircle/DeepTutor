use super::documents::{self, ExtractedDocument};
use super::{document_settings, AppState};
use futures_util::StreamExt;
use reqwest::redirect::Policy;
use reqwest::{Client, Response};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{Cursor, Read};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::Command;
use walkdir::WalkDir;

const MAX_PARSE_CHARS: usize = 8_000_000;
const MAX_REMOTE_BODY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_MINERU_ARCHIVE_BYTES: u64 = 500 * 1024 * 1024;
const MAX_MINERU_ARCHIVE_ENTRIES: usize = 5_000;
const MAX_MINERU_UNCOMPRESSED_BYTES: u64 = 500 * 1024 * 1024;
const MINERU_POLL_INTERVAL: Duration = Duration::from_secs(4);
const MINERU_TIMEOUT: Duration = Duration::from_secs(300);
const CLI_TIMEOUT: Duration = Duration::from_secs(900);

#[derive(Debug, Deserialize, Serialize)]
struct CachedParse {
    text: String,
    metadata: Value,
}

struct ParserSelection {
    engine: String,
    settings: Value,
    native_fallback: bool,
}

/// 按当前文档解析设置执行一次解析，并复用安装目录内的内容寻址缓存。
pub async fn extract(
    state: &AppState,
    filename: &str,
    bytes: &[u8],
) -> Result<ExtractedDocument, String> {
    if bytes.is_empty() {
        return Err("待解析文档为空。".to_string());
    }
    let selection = select_engine(state, filename)?;
    let signature = parser_signature(&selection.engine, &selection.settings);
    let source_hash = format!("{:x}", Sha256::digest(bytes));
    let cache_dir = state
        .paths
        .cache
        .join("parse")
        .join(&source_hash[..2])
        .join(&source_hash)
        .join(&signature);
    if let Some(cached) = load_cache(&cache_dir) {
        return Ok(ExtractedDocument {
            text: cached.text,
            metadata: with_cache_metadata(cached.metadata, true, &selection.engine, &source_hash),
        });
    }

    let parsed = if selection.native_fallback {
        documents::extract(filename, bytes)?
    } else {
        match selection.engine.as_str() {
            "docling" => extract_docling(state, filename, bytes, &selection.settings).await?,
            "tika" => extract_tika(state, filename, bytes, &selection.settings).await?,
            "mineru" => extract_mineru(state, filename, bytes, &selection.settings).await?,
            _ => documents::extract(filename, bytes)?,
        }
    };
    let parsed = ExtractedDocument {
        text: limit_text(parsed.text),
        metadata: with_cache_metadata(parsed.metadata, false, &selection.engine, &source_hash),
    };
    if parsed.text.trim().is_empty() {
        return Err("解析引擎没有返回可读取文本。".to_string());
    }
    write_cache(
        &cache_dir,
        &CachedParse {
            text: parsed.text.clone(),
            metadata: parsed.metadata.clone(),
        },
    )?;
    Ok(parsed)
}

pub async fn extract_path(state: &AppState, path: &Path) -> Result<ExtractedDocument, String> {
    let bytes =
        std::fs::read(path).map_err(|error| format!("无法读取文档 {}：{error}", path.display()))?;
    let filename = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("document");
    extract(state, filename, &bytes).await
}

fn select_engine(state: &AppState, filename: &str) -> Result<ParserSelection, String> {
    let settings = document_settings::load_settings_for_state(state)?;
    let configured = settings
        .get("engine")
        .and_then(Value::as_str)
        .unwrap_or("text_only")
        .trim()
        .to_ascii_lowercase()
        .replace(['-', ' '], "_");
    let supported = engine_supports_format(&configured, filename);
    let native_fallback = !supported && native_supports(filename);
    if !supported && !native_fallback {
        return Err(format!(
            "解析引擎 `{configured}` 不支持 {} 文件，内置解析器也无法读取该格式。",
            extension(filename)
        ));
    }
    Ok(ParserSelection {
        engine: if native_fallback {
            "text_only".to_string()
        } else {
            configured
        },
        settings,
        native_fallback,
    })
}

fn engine_supports_format(engine: &str, filename: &str) -> bool {
    let extension = extension(filename);
    match engine {
        "tika" => true,
        "mineru" => matches!(
            extension.as_str(),
            "pdf"
                | "bmp"
                | "gif"
                | "jp2"
                | "jpeg"
                | "jpg"
                | "png"
                | "tiff"
                | "webp"
                | "docx"
                | "pptx"
                | "xlsx"
        ),
        "docling" => matches!(
            extension.as_str(),
            "aac"
                | "adoc"
                | "asc"
                | "asciidoc"
                | "avi"
                | "bmp"
                | "boxnote"
                | "csv"
                | "dclg"
                | "dclx"
                | "doc"
                | "docm"
                | "docx"
                | "dot"
                | "dotm"
                | "dotx"
                | "eml"
                | "epub"
                | "flac"
                | "htm"
                | "html"
                | "jpeg"
                | "jpg"
                | "json"
                | "latex"
                | "m4a"
                | "md"
                | "mkv"
                | "mov"
                | "mp3"
                | "mp4"
                | "msg"
                | "odp"
                | "ods"
                | "odt"
                | "ogg"
                | "otp"
                | "ots"
                | "ott"
                | "pages"
                | "pdf"
                | "png"
                | "pps"
                | "ppsm"
                | "ppsx"
                | "ppt"
                | "pptm"
                | "pptx"
                | "qmd"
                | "rmd"
                | "tex"
                | "text"
                | "tif"
                | "tiff"
                | "txt"
                | "vtt"
                | "wav"
                | "webm"
                | "webp"
                | "xbrl"
                | "xhtml"
                | "xls"
                | "xlsm"
                | "xlsx"
                | "xlt"
                | "xml"
        ),
        "text_only" | "markitdown" | "pymupdf4llm" | "liteparse" => native_supports(filename),
        _ => native_supports(filename),
    }
}

fn native_supports(filename: &str) -> bool {
    matches!(
        extension(filename).as_str(),
        "pdf"
            | "docx"
            | "pptx"
            | "epub"
            | "xlsx"
            | "xls"
            | "xlsb"
            | "ods"
            | "html"
            | "htm"
            | "xhtml"
            | "xml"
            | "svg"
            | "json"
            | "jsonc"
            | "json5"
            | "yaml"
            | "yml"
            | "toml"
            | "md"
            | "markdown"
            | "txt"
            | "text"
            | "log"
            | "csv"
            | "tsv"
            | "rst"
            | "tex"
            | "latex"
            | "bib"
            | "css"
            | "scss"
            | "sass"
            | "less"
            | "js"
            | "mjs"
            | "cjs"
            | "ts"
            | "mts"
            | "cts"
            | "jsx"
            | "tsx"
            | "vue"
            | "svelte"
            | "py"
            | "java"
            | "kt"
            | "kts"
            | "scala"
            | "groovy"
            | "gradle"
            | "c"
            | "h"
            | "cpp"
            | "cc"
            | "cxx"
            | "hpp"
            | "hh"
            | "hxx"
            | "cs"
            | "go"
            | "rs"
            | "zig"
            | "nim"
            | "swift"
            | "m"
            | "mm"
            | "rb"
            | "php"
            | "pl"
            | "pm"
            | "lua"
            | "r"
            | "jl"
            | "dart"
            | "hs"
            | "clj"
            | "cljs"
            | "cljc"
            | "ex"
            | "exs"
            | "erl"
            | "ml"
            | "mli"
            | "fs"
            | "fsx"
            | "lisp"
            | "lsp"
            | "scm"
            | "rkt"
            | "sol"
            | "sh"
            | "bash"
            | "zsh"
            | "fish"
            | "ps1"
            | "vim"
            | "sql"
            | "graphql"
            | "gql"
            | "proto"
            | "cmake"
            | "mk"
            | "tf"
            | "hcl"
            | "nginxconf"
            | "dockerfile"
            | "ini"
            | "cfg"
            | "conf"
            | "env"
            | "properties"
    )
}

fn extension(filename: &str) -> String {
    Path::new(filename)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

async fn extract_docling(
    state: &AppState,
    filename: &str,
    bytes: &[u8],
    settings: &Value,
) -> Result<ExtractedDocument, String> {
    let config = settings
        .pointer("/engines/docling")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if config.get("mode").and_then(Value::as_str) == Some("remote") {
        return extract_docling_remote(state, filename, bytes, &config).await;
    }
    extract_docling_cli(state, filename, bytes, &config).await
}

async fn extract_docling_remote(
    state: &AppState,
    filename: &str,
    bytes: &[u8],
    config: &Value,
) -> Result<ExtractedDocument, String> {
    let base_url = config
        .get("api_base_url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .trim_end_matches('/');
    validate_service_url(base_url)?;
    let token = config
        .get("api_token")
        .and_then(Value::as_str)
        .unwrap_or("");
    let form = reqwest::multipart::Form::new()
        .text("to_formats", "md")
        .text(
            "do_ocr",
            if config
                .get("do_ocr")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                "true"
            } else {
                "false"
            },
        )
        .text(
            "do_table_structure",
            if config
                .get("do_table_structure")
                .and_then(Value::as_bool)
                .unwrap_or(true)
            {
                "true"
            } else {
                "false"
            },
        )
        .part(
            "files",
            reqwest::multipart::Part::bytes(bytes.to_vec())
                .file_name(safe_remote_filename(filename))
                .mime_str("application/octet-stream")
                .map_err(|error| format!("无法设置 Docling 上传类型：{error}"))?,
        );
    let client = restricted_client(Duration::from_secs(300))?;
    let mut request = client
        .post(format!("{base_url}/v1/convert/file"))
        .multipart(form);
    if !token.trim().is_empty() {
        request = request.header("X-Api-Key", token.trim());
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("Docling 请求失败：{error}"))?;
    let response = ensure_remote_success(response, "Docling")?;
    let payload = response_json_limited(response, MAX_REMOTE_BODY_BYTES).await?;
    let status = payload
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    if status != "success" {
        let detail = payload
            .get("errors")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|item| {
                item.get("error")
                    .or_else(|| item.get("message"))
                    .and_then(Value::as_str)
            })
            .collect::<Vec<_>>()
            .join("；");
        return Err(format!(
            "Docling 转换失败：{}",
            if detail.is_empty() {
                format!("状态 {}", if status.is_empty() { "未知" } else { &status })
            } else {
                detail
            }
        ));
    }
    let text = payload
        .pointer("/document/md_content")
        .and_then(Value::as_str)
        .ok_or_else(|| "Docling 返回成功但没有 Markdown 内容。".to_string())?;
    Ok(ExtractedDocument {
        text: limit_text(text.to_string()),
        metadata: json!({"format": extension(filename), "parser": "docling-remote"}),
    })
}

async fn extract_docling_cli(
    state: &AppState,
    filename: &str,
    bytes: &[u8],
    config: &Value,
) -> Result<ExtractedDocument, String> {
    if native_supports(filename) && !native_is_ooxml(filename) {
        return documents::extract(filename, bytes);
    }
    let program = config
        .get("local_cli_path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|value| {
            let path = PathBuf::from(value);
            path.is_file().then_some(path)
        })
        .or_else(|| find_executable("docling"));
    let Some(program) = program else {
        return Err(
            "Docling 本地模式未找到可执行程序。请在设置中改用远程 Docling Serve，或配置本机 docling CLI。"
                .to_string(),
        );
    };
    let work = create_work_dir(state, "docling")?;
    let input = work.join(safe_local_filename(filename));
    let output = work.join("output");
    std::fs::create_dir_all(&output)
        .map_err(|error| format!("无法创建 Docling 输出目录：{error}"))?;
    std::fs::write(&input, bytes).map_err(|error| format!("无法写入 Docling 输入文件：{error}"))?;
    let mut command = Command::new(program);
    command
        .arg("--to")
        .arg("md")
        .arg("--output")
        .arg(&output)
        .arg(&input)
        .env("TMP", &state.paths.temp)
        .env("TEMP", &state.paths.temp)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    run_command_with_timeout(command, CLI_TIMEOUT, "Docling").await?;
    let markdown = find_markdown_output(&output)?;
    let _ = std::fs::remove_dir_all(work);
    Ok(ExtractedDocument {
        text: markdown,
        metadata: json!({"format": extension(filename), "parser": "docling-cli"}),
    })
}

async fn extract_tika(
    state: &AppState,
    filename: &str,
    bytes: &[u8],
    settings: &Value,
) -> Result<ExtractedDocument, String> {
    let base_url = settings
        .pointer("/engines/tika/server_url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .trim_end_matches('/');
    validate_service_url(base_url)?;
    let encoded_name =
        url::form_urlencoded::byte_serialize(filename.as_bytes()).collect::<String>();
    let client = restricted_client(Duration::from_secs(300))?;
    let response = client
        .put(format!("{base_url}/tika"))
        .header("Content-Type", "application/octet-stream")
        .header(
            "Content-Disposition",
            format!("attachment; filename*=UTF-8''{encoded_name}"),
        )
        .header("Accept", "text/markdown, text/plain;q=0.9, */*;q=0.1")
        .body(bytes.to_vec())
        .send()
        .await
        .map_err(|error| format!("Tika 请求失败：{error}"))?;
    let response = ensure_remote_success(response, "Tika")?;
    let text = response_text_limited(response, MAX_REMOTE_BODY_BYTES).await?;
    let _ = state;
    Ok(ExtractedDocument {
        text: limit_text(text),
        metadata: json!({"format": extension(filename), "parser": "tika-remote"}),
    })
}

async fn extract_mineru(
    state: &AppState,
    filename: &str,
    bytes: &[u8],
    settings: &Value,
) -> Result<ExtractedDocument, String> {
    let config = settings
        .pointer("/engines/mineru")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if config.get("mode").and_then(Value::as_str) == Some("cloud") {
        return extract_mineru_cloud(filename, bytes, &config).await;
    }
    extract_mineru_cli(state, filename, bytes, &config).await
}

async fn extract_mineru_cloud(
    filename: &str,
    bytes: &[u8],
    config: &Value,
) -> Result<ExtractedDocument, String> {
    let base_url = config
        .get("api_base_url")
        .and_then(Value::as_str)
        .unwrap_or("https://mineru.net")
        .trim()
        .trim_end_matches('/');
    validate_https_url(base_url, "MinerU API")?;
    let token = config
        .get("api_token")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if token.is_empty() {
        return Err("MinerU 云端模式未配置 API token。".to_string());
    }
    let client = restricted_client(Duration::from_secs(60))?;
    let mut body = json!({
        "files": [{
            "name": safe_remote_filename(filename),
            "is_ocr": config.get("is_ocr").and_then(Value::as_bool).unwrap_or(false)
        }],
        "model_version": config.get("model_version").and_then(Value::as_str).unwrap_or("pipeline"),
        "enable_formula": config.get("enable_formula").and_then(Value::as_bool).unwrap_or(true),
        "enable_table": config.get("enable_table").and_then(Value::as_bool).unwrap_or(true)
    });
    let language = config
        .get("language")
        .and_then(Value::as_str)
        .unwrap_or("auto");
    if !language.is_empty() && !language.eq_ignore_ascii_case("auto") {
        body["language"] = Value::String(language.to_string());
    }
    let response = client
        .post(format!("{base_url}/api/v4/file-urls/batch"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .map_err(|error| format!("MinerU 申请上传地址失败：{error}"))?;
    let payload = response_json_limited(
        ensure_remote_success(response, "MinerU")?,
        MAX_REMOTE_BODY_BYTES,
    )
    .await?;
    check_mineru_code(&payload)?;
    let batch_id = payload
        .pointer("/data/batch_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let upload_url = payload
        .pointer("/data/file_urls/0")
        .and_then(Value::as_str)
        .unwrap_or("");
    if batch_id.is_empty() || upload_url.is_empty() {
        return Err("MinerU 没有返回 batch_id 或上传地址。".to_string());
    }
    validate_https_url(upload_url, "MinerU 上传地址")?;
    let upload_client = restricted_client(Duration::from_secs(300))?;
    let response = upload_client
        .put(upload_url)
        .body(bytes.to_vec())
        .send()
        .await
        .map_err(|error| format!("上传文件到 MinerU 失败：{error}"))?;
    ensure_remote_success(response, "MinerU 上传")?;

    let poll_client = restricted_client(Duration::from_secs(60))?;
    let deadline = Instant::now() + MINERU_TIMEOUT;
    let remote_name = safe_remote_filename(filename);
    let archive_url = loop {
        if Instant::now() >= deadline {
            return Err("MinerU 解析超时（300 秒）。".to_string());
        }
        let response = poll_client
            .get(format!(
                "{base_url}/api/v4/extract-results/batch/{batch_id}"
            ))
            .bearer_auth(token)
            .send()
            .await
            .map_err(|error| format!("MinerU 查询任务失败：{error}"))?;
        let payload = response_json_limited(
            ensure_remote_success(response, "MinerU")?,
            MAX_REMOTE_BODY_BYTES,
        )
        .await?;
        check_mineru_code(&payload)?;
        let entries = payload
            .pointer("/data/extract_result")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let entry = entries
            .iter()
            .find(|entry| entry.get("file_name").and_then(Value::as_str) == Some(&remote_name))
            .or_else(|| entries.first());
        if let Some(entry) = entry {
            let state = entry
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_ascii_lowercase();
            if state == "done" {
                break entry
                    .get("full_zip_url")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| "MinerU 已完成但没有返回结果压缩包。".to_string())?
                    .to_string();
            }
            if state == "failed" {
                return Err(format!(
                    "MinerU 解析失败：{}",
                    entry
                        .get("err_msg")
                        .and_then(Value::as_str)
                        .unwrap_or("未知错误")
                ));
            }
        }
        tokio::time::sleep(MINERU_POLL_INTERVAL).await;
    };
    let archive = download_url_limited(&archive_url, MAX_MINERU_ARCHIVE_BYTES).await?;
    let markdown = markdown_from_mineru_zip(&archive)?;
    Ok(ExtractedDocument {
        text: limit_text(markdown),
        metadata: json!({"format": extension(filename), "parser": "mineru-cloud"}),
    })
}

async fn extract_mineru_cli(
    state: &AppState,
    filename: &str,
    bytes: &[u8],
    config: &Value,
) -> Result<ExtractedDocument, String> {
    if native_supports(filename) && !native_is_ooxml(filename) && extension(filename) != "pdf" {
        return documents::extract(filename, bytes);
    }
    let configured = config
        .get("local_cli_path")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let program = if !configured.is_empty() {
        let path = PathBuf::from(configured);
        path.is_file().then_some(path)
    } else {
        find_executable("mineru")
    };
    let Some(program) = program else {
        return Err("MinerU 本地模式未找到 CLI。请配置 MinerU CLI，或切换到云端 API。".to_string());
    };
    let work = create_work_dir(state, "mineru")?;
    let input = work.join(safe_local_filename(filename));
    let output = work.join("output");
    std::fs::create_dir_all(&output)
        .map_err(|error| format!("无法创建 MinerU 输出目录：{error}"))?;
    std::fs::write(&input, bytes).map_err(|error| format!("无法写入 MinerU 输入文件：{error}"))?;
    let mut command = Command::new(program);
    command
        .arg("-p")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .env("TMP", &state.paths.temp)
        .env("TEMP", &state.paths.temp)
        .env("MINERU_MODEL_SOURCE", "local")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    run_command_with_timeout(command, CLI_TIMEOUT, "MinerU").await?;
    let markdown = find_markdown_output(&output)?;
    let _ = std::fs::remove_dir_all(work);
    Ok(ExtractedDocument {
        text: markdown,
        metadata: json!({"format": extension(filename), "parser": "mineru-cli"}),
    })
}

fn parser_signature(engine: &str, settings: &Value) -> String {
    let config = match engine {
        "docling" => settings.pointer("/engines/docling").cloned(),
        "tika" => settings.pointer("/engines/tika").cloned(),
        "mineru" => settings.pointer("/engines/mineru").cloned(),
        _ => None,
    }
    .unwrap_or_else(|| json!({}));
    let mut config = config;
    if let Some(object) = config.as_object_mut() {
        object.remove("api_token");
    }
    let material = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "engine": engine,
        "config": config
    });
    let encoded = serde_json::to_vec(&material).unwrap_or_default();
    format!("{:x}", Sha256::digest(encoded))
}

fn load_cache(directory: &Path) -> Option<CachedParse> {
    let manifest = directory.join("manifest.json");
    if !manifest.is_file() {
        return None;
    }
    let bytes = std::fs::read(directory.join("result.json")).ok()?;
    let cached = serde_json::from_slice::<CachedParse>(&bytes).ok()?;
    if cached.text.trim().is_empty() || cached.text.chars().count() > MAX_PARSE_CHARS {
        let _ = std::fs::remove_file(manifest);
        let _ = std::fs::remove_file(directory.join("result.json"));
        return None;
    }
    Some(cached)
}

fn write_cache(directory: &Path, parsed: &CachedParse) -> Result<(), String> {
    std::fs::create_dir_all(directory)
        .map_err(|error| format!("无法创建解析缓存目录 {}：{error}", directory.display()))?;
    let result_temp = directory.join(format!(".result-{}.tmp", uuid::Uuid::new_v4()));
    let result_bytes =
        serde_json::to_vec(parsed).map_err(|error| format!("无法序列化解析缓存：{error}"))?;
    std::fs::write(&result_temp, result_bytes)
        .map_err(|error| format!("无法写入解析缓存：{error}"))?;
    replace_file(&result_temp, &directory.join("result.json"))?;

    let manifest_temp = directory.join(format!(".manifest-{}.tmp", uuid::Uuid::new_v4()));
    let manifest = json!({
        "created_at": crate::storage::now(),
        "format": "deep-tutor-parse-cache-v1",
        "chars": parsed.text.chars().count()
    });
    std::fs::write(
        &manifest_temp,
        serde_json::to_vec(&manifest).unwrap_or_default(),
    )
    .map_err(|error| format!("无法写入解析缓存清单：{error}"))?;
    replace_file(&manifest_temp, &directory.join("manifest.json"))
}

fn replace_file(source: &Path, target: &Path) -> Result<(), String> {
    if target.exists() {
        std::fs::remove_file(target)
            .map_err(|error| format!("无法替换解析缓存 {}：{error}", target.display()))?;
    }
    std::fs::rename(source, target)
        .map_err(|error| format!("无法提交解析缓存 {}：{error}", target.display()))
}

fn with_cache_metadata(
    mut metadata: Value,
    reused: bool,
    engine: &str,
    source_hash: &str,
) -> Value {
    if !metadata.is_object() {
        metadata = json!({});
    }
    metadata["parse_engine"] = Value::String(engine.to_string());
    metadata["parse_reused"] = Value::Bool(reused);
    metadata["source_hash"] = Value::String(source_hash.to_string());
    metadata
}

fn limited_client(timeout: Duration) -> Result<Client, String> {
    Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .user_agent(format!("DeepTutor/{}", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(20))
        .timeout(timeout)
        .build()
        .map_err(|error| format!("无法初始化解析网络客户端：{error}"))
}

fn restricted_client(timeout: Duration) -> Result<Client, String> {
    limited_client(timeout)
}

fn ensure_remote_success(response: Response, label: &str) -> Result<Response, String> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    Err(format!(
        "{label} 返回 HTTP {}（{}）。",
        status.as_u16(),
        status.canonical_reason().unwrap_or("请检查服务配置或凭据")
    ))
}

async fn response_json_limited(response: Response, limit: u64) -> Result<Value, String> {
    let bytes = response_bytes_limited(response, limit).await?;
    serde_json::from_slice(&bytes).map_err(|error| format!("远程解析服务返回了无效 JSON：{error}"))
}

async fn response_text_limited(response: Response, limit: u64) -> Result<String, String> {
    let bytes = response_bytes_limited(response, limit).await?;
    String::from_utf8(bytes).map_err(|error| format!("远程解析服务返回了无效 UTF-8：{error}"))
}

async fn response_bytes_limited(response: Response, limit: u64) -> Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|length| length > limit)
    {
        return Err(format!(
            "远程解析响应超过允许的 {} MB。",
            limit / 1024 / 1024
        ));
    }
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("读取远程解析响应失败：{error}"))?;
        if bytes.len().saturating_add(chunk.len()) > limit as usize {
            return Err(format!(
                "远程解析响应超过允许的 {} MB。",
                limit / 1024 / 1024
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn download_url_limited(url: &str, limit: u64) -> Result<Vec<u8>, String> {
    let mut current = url.to_string();
    for redirects in 0..=2 {
        validate_https_url(&current, "MinerU 下载地址")?;
        let client = restricted_client(Duration::from_secs(300))?;
        let response = client
            .get(&current)
            .send()
            .await
            .map_err(|error| format!("下载 MinerU 结果失败：{error}"))?;
        if response.status().is_redirection() {
            if redirects == 2 {
                return Err("MinerU 下载重定向次数过多。".to_string());
            }
            current = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|location| url::Url::parse(&current).ok()?.join(location).ok())
                .map(|url| url.to_string())
                .ok_or_else(|| "MinerU 下载重定向缺少有效地址。".to_string())?;
            continue;
        }
        return response_bytes_limited(ensure_remote_success(response, "MinerU 下载")?, limit)
            .await;
    }
    Err("MinerU 下载失败。".to_string())
}

fn check_mineru_code(payload: &Value) -> Result<(), String> {
    if payload.get("code").and_then(Value::as_i64).unwrap_or(0) == 0 {
        return Ok(());
    }
    Err(format!(
        "MinerU API 错误 {}：{}",
        payload
            .get("code")
            .map(Value::to_string)
            .unwrap_or_else(|| "未知".to_string()),
        payload
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("未知错误")
    ))
}

fn validate_service_url(value: &str) -> Result<(), String> {
    let url = url::Url::parse(value).map_err(|_| "解析服务地址不是有效 URL。".to_string())?;
    if matches!(url.scheme(), "http" | "https") && url.host_str().is_some() {
        Ok(())
    } else {
        Err("解析服务地址必须使用 HTTP 或 HTTPS，并包含主机名。".to_string())
    }
}

fn validate_https_url(value: &str, label: &str) -> Result<(), String> {
    let url = url::Url::parse(value).map_err(|_| format!("{label}不是有效 URL。"))?;
    if url.scheme() == "https" && url.host_str().is_some() {
        Ok(())
    } else {
        Err(format!("{label}必须使用 HTTPS。"))
    }
}

fn markdown_from_mineru_zip(bytes: &[u8]) -> Result<String, String> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|error| format!("MinerU 结果不是有效 ZIP：{error}"))?;
    if archive.len() > MAX_MINERU_ARCHIVE_ENTRIES {
        return Err(format!(
            "MinerU 结果压缩包文件数超过 {}。",
            MAX_MINERU_ARCHIVE_ENTRIES
        ));
    }
    let mut markdown_names = Vec::new();
    let mut content_list_names = Vec::new();
    let mut total_uncompressed = 0u64;
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|error| format!("无法读取 MinerU ZIP 条目：{error}"))?;
        if entry.is_dir() {
            continue;
        }
        total_uncompressed = total_uncompressed.saturating_add(entry.size());
        if total_uncompressed > MAX_MINERU_UNCOMPRESSED_BYTES {
            return Err("MinerU 解压后内容超过 500 MB 安全上限。".to_string());
        }
        let name = safe_archive_name(entry.name())?;
        if name.to_ascii_lowercase().ends_with(".md") {
            markdown_names.push(name);
        } else if name.to_ascii_lowercase().ends_with("_content_list.json") {
            content_list_names.push(name);
        }
    }
    markdown_names.sort_by_key(|name| (name.matches('/').count(), name.clone()));
    if let Some(name) = markdown_names.first() {
        let mut entry = archive
            .by_name(name)
            .map_err(|error| format!("无法读取 MinerU Markdown：{error}"))?;
        let mut text = String::new();
        entry
            .take(MAX_PARSE_CHARS as u64 * 4)
            .read_to_string(&mut text)
            .map_err(|error| format!("无法解码 MinerU Markdown：{error}"))?;
        return Ok(limit_text(text));
    }
    content_list_names.sort_by_key(|name| (name.matches('/').count(), name.clone()));
    if let Some(name) = content_list_names.first() {
        let mut entry = archive
            .by_name(name)
            .map_err(|error| format!("无法读取 MinerU 内容清单：{error}"))?;
        let mut text = String::new();
        entry
            .take(MAX_PARSE_CHARS as u64 * 4)
            .read_to_string(&mut text)
            .map_err(|error| format!("无法解码 MinerU 内容清单：{error}"))?;
        let blocks = serde_json::from_str::<Value>(&text).unwrap_or(Value::Null);
        return Ok(limit_text(content_blocks_text(&blocks)));
    }
    Err("MinerU 结果压缩包中没有 Markdown 或内容清单。".to_string())
}

fn content_blocks_text(blocks: &Value) -> String {
    let rows = blocks
        .as_array()
        .cloned()
        .unwrap_or_else(|| vec![blocks.clone()]);
    rows.iter()
        .filter_map(|block| {
            for key in [
                "text",
                "content",
                "table_body",
                "latex",
                "image_caption",
                "img_caption",
            ] {
                if let Some(value) = block.get(key) {
                    if let Some(text) = value.as_str().filter(|text| !text.trim().is_empty()) {
                        return Some(text.trim().to_string());
                    }
                    if let Some(items) = value.as_array() {
                        let joined = items
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(" ");
                        if !joined.trim().is_empty() {
                            return Some(joined);
                        }
                    }
                }
            }
            None
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn safe_archive_name(raw: &str) -> Result<String, String> {
    let normalized = raw.replace('\\', "/");
    let path = Path::new(&normalized);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!("MinerU ZIP 包含不安全路径：{raw}"));
    }
    Ok(normalized)
}

fn create_work_dir(state: &AppState, engine: &str) -> Result<PathBuf, String> {
    let path = state
        .paths
        .cache
        .join("parse-work")
        .join(engine)
        .join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir_all(&path)
        .map_err(|error| format!("无法创建解析工作目录 {}：{error}", path.display()))?;
    Ok(path)
}

async fn run_command_with_timeout(
    mut command: Command,
    timeout: Duration,
    label: &str,
) -> Result<(), String> {
    let mut child = command
        .spawn()
        .map_err(|error| format!("无法启动 {label} CLI：{error}"))?;
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(result) => result.map_err(|error| format!("等待 {label} CLI 失败：{error}"))?,
        Err(_) => {
            let _ = child.kill().await;
            return Err(format!("{label} CLI 超过 {} 秒未完成。", timeout.as_secs()));
        }
    };
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "{label} CLI 退出状态：{}。请检查命令行配置和工具日志。",
            status
        ))
    }
}

fn find_markdown_output(root: &Path) -> Result<String, String> {
    let mut candidates = WalkDir::new(root)
        .max_depth(8)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| {
            entry
                .path()
                .extension()
                .and_then(|value| value.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
        })
        .map(|entry| entry.into_path())
        .collect::<Vec<_>>();
    candidates.sort_by_key(|path| (path.components().count(), path.to_path_buf()));
    let path = candidates
        .first()
        .ok_or_else(|| "本地解析 CLI 没有生成 Markdown 文件。".to_string())?;
    let metadata =
        std::fs::metadata(path).map_err(|error| format!("无法读取本地解析输出信息：{error}"))?;
    if metadata.len() > MAX_PARSE_CHARS as u64 * 4 {
        return Err("本地解析生成的 Markdown 超过安全大小上限。".to_string());
    }
    let text =
        std::fs::read_to_string(path).map_err(|error| format!("无法读取本地解析输出：{error}"))?;
    Ok(limit_text(text))
}

fn native_is_ooxml(filename: &str) -> bool {
    matches!(
        extension(filename).as_str(),
        "docx" | "pptx" | "xlsx" | "xls" | "xlsb" | "ods" | "epub"
    )
}

fn find_executable(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let names = if cfg!(windows) {
        vec![
            format!("{name}.exe"),
            format!("{name}.cmd"),
            name.to_string(),
        ]
    } else {
        vec![name.to_string()]
    };
    for directory in std::env::split_paths(&path) {
        for candidate in &names {
            let path = directory.join(candidate);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

fn safe_remote_filename(filename: &str) -> String {
    safe_local_filename(filename)
}

fn safe_local_filename(filename: &str) -> String {
    let name = Path::new(filename)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("document");
    let sanitized = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.trim_matches('.').is_empty() {
        "document".to_string()
    } else {
        sanitized.chars().take(180).collect()
    }
}

fn limit_text(text: String) -> String {
    text.chars().take(MAX_PARSE_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_signature_ignores_api_tokens() {
        let first = json!({"engines": {"docling": {"mode": "remote", "api_token": "one"}}});
        let second = json!({"engines": {"docling": {"mode": "remote", "api_token": "two"}}});
        assert_eq!(
            parser_signature("docling", &first),
            parser_signature("docling", &second)
        );
    }

    #[test]
    fn archive_paths_reject_traversal() {
        assert!(safe_archive_name("../secret.md").is_err());
        assert!(safe_archive_name("C:/secret.md").is_err());
        assert!(safe_archive_name("auto/result.md").is_ok());
    }

    #[test]
    fn content_list_fallback_keeps_text_blocks() {
        let blocks = json!([
            {"type": "text", "text": "第一段"},
            {"type": "image", "image_caption": ["图一"]}
        ]);
        let text = content_blocks_text(&blocks);
        assert!(text.contains("第一段"));
        assert!(text.contains("图一"));
    }
}
