use super::error::{ApiError, ApiResult};
use super::{files, knowledge, AppState, ScopedState};
use axum::body::Bytes;
use axum::extract::{Multipart, Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

const MAX_OFFICE_BYTES: usize = 25 * 1024 * 1024;
const MAX_PDF_BYTES: u64 = 60 * 1024 * 1024;
const CONVERSION_TIMEOUT: Duration = Duration::from_secs(45);
const OFFICE_SUFFIXES: &[&str] = &[
    "doc", "docm", "docx", "ppt", "pptm", "pptx", "xls", "xlsm", "xlsx",
];

#[derive(Debug, Deserialize)]
struct PreviewQuery {
    source: String,
}

pub fn router() -> Router<AppState> {
    Router::new().route(
        "/file-preview/pdf",
        get(preview_source).post(preview_upload),
    )
}

async fn preview_source(
    ScopedState(state): ScopedState,
    Query(query): Query<PreviewQuery>,
) -> ApiResult<Response> {
    let path = resolve_source_path(&state, &query.source)?;
    let filename = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| ApiError::bad_request("预览源文件缺少有效文件名。"))?
        .to_string();
    ensure_office_name(&filename)?;
    let data = tokio::fs::read(&path)
        .await
        .map_err(|error| ApiError::not_found(format!("无法读取预览源文件：{error}")))?;
    rendered_pdf_response(&state, &data, &filename).await
}

async fn preview_upload(
    ScopedState(state): ScopedState,
    mut multipart: Multipart,
) -> ApiResult<Response> {
    let mut filename = String::new();
    let mut data: Option<Bytes> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|error| ApiError::bad_request(format!("无法读取预览上传表单：{error}")))?
    {
        if field.name() != Some("file") {
            continue;
        }
        filename = field.file_name().unwrap_or("document.docx").to_string();
        let bytes = field
            .bytes()
            .await
            .map_err(|error| ApiError::bad_request(format!("无法读取上传文件：{error}")))?;
        if bytes.len() > MAX_OFFICE_BYTES {
            return Err(ApiError::bad_request("Office 文件超过 25 MiB，无法预览。"));
        }
        data = Some(bytes);
    }
    let data = data.ok_or_else(|| ApiError::bad_request("上传表单缺少 file 字段。"))?;
    ensure_office_name(&filename)?;
    rendered_pdf_response(&state, &data, &filename).await
}

fn ensure_office_name(filename: &str) -> ApiResult<()> {
    let suffix = Path::new(filename)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !OFFICE_SUFFIXES.contains(&suffix.as_str()) {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "只支持 Word、PowerPoint 与 Excel 文件的 PDF 预览。",
            false,
        ));
    }
    Ok(())
}

fn resolve_source_path(state: &AppState, source: &str) -> ApiResult<PathBuf> {
    if source.len() > 8192
        || !source.starts_with('/')
        || source.contains('#')
        || source.contains("://")
    {
        return Err(ApiError::bad_request("预览来源必须是本地文件 URL。"));
    }
    let path_part = source
        .split_once('?')
        .map(|(path, _)| path)
        .unwrap_or(source);
    let path = percent_decode(path_part)?;
    if let Some(relative) = path.strip_prefix("/files/") {
        return files::resolve_file(state, relative).map_err(|(status, message)| {
            ApiError::new(status, "preview_source_error", message, false)
        });
    }
    if let Some(rest) = path.strip_prefix("/api/knowledge-bases/") {
        let (kb_name, file) = rest
            .split_once("/files/")
            .ok_or_else(|| ApiError::bad_request("不支持的知识库预览 URL。"))?;
        if kb_name.is_empty() || file.is_empty() {
            return Err(ApiError::bad_request("知识库预览 URL 不完整。"));
        }
        return knowledge::raw_file_path(state, kb_name, file);
    }
    Err(ApiError::bad_request("不支持的预览来源 URL。"))
}

async fn rendered_pdf_response(
    state: &AppState,
    data: &[u8],
    filename: &str,
) -> ApiResult<Response> {
    if data.is_empty() || data.len() > MAX_OFFICE_BYTES {
        return Err(ApiError::bad_request("Office 文件为空或超过 25 MiB。"));
    }
    let suffix = Path::new(filename)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let key = {
        let mut hasher = Sha256::new();
        hasher.update(b"deeptutor-office-preview-v1\0");
        hasher.update(suffix.as_bytes());
        hasher.update(b"\0");
        hasher.update(data);
        hex::encode(hasher.finalize())
    };
    let cache = state.paths.cache.join("office-previews");
    let cached = cache.join(format!("{key}.pdf"));
    if let Ok(pdf) = tokio::fs::read(&cached).await {
        if !pdf.is_empty() && pdf.len() as u64 <= MAX_PDF_BYTES && pdf.starts_with(b"%PDF-") {
            return Ok(pdf_response(pdf, filename));
        }
    }

    let soffice = find_soffice(state).ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "office_preview_unavailable",
            "未找到 LibreOffice。请安装 LibreOffice，或把便携运行时放到 tools/libreoffice。",
            false,
        )
    })?;
    let data = data.to_vec();
    let temp_root = state.paths.temp.join("office-previews");
    let pdf = tokio::time::timeout(
        CONVERSION_TIMEOUT,
        tokio::task::spawn_blocking(move || {
            convert_with_libreoffice(&soffice, &temp_root, &data, &suffix)
        }),
    )
    .await
    .map_err(|_| {
        ApiError::new(
            StatusCode::GATEWAY_TIMEOUT,
            "office_preview_timeout",
            "Office 转 PDF 超时。",
            true,
        )
    })?
    .map_err(|error| ApiError::internal(format!("Office 转换任务失败：{error}")))??;

    if let Err(error) = tokio::fs::create_dir_all(&cache).await {
        tracing::warn!(%error, cache = %cache.display(), "无法创建 Office 预览缓存目录");
    } else {
        let temporary = cache.join(format!(".pending-{}.pdf", uuid::Uuid::new_v4()));
        if let Err(error) = tokio::fs::write(&temporary, &pdf).await {
            tracing::warn!(%error, "无法写入 Office 预览缓存");
        } else if let Err(error) = tokio::fs::rename(&temporary, &cached).await {
            tracing::warn!(%error, "无法提交 Office 预览缓存");
            let _ = tokio::fs::remove_file(&temporary).await;
        }
    }
    Ok(pdf_response(pdf, filename))
}

fn pdf_response(pdf: Vec<u8>, filename: &str) -> Response {
    let stem = Path::new(filename)
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("preview");
    let safe_stem = stem
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    let disposition = format!("inline; filename=\"{safe_stem}.pdf\"");
    (
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/pdf"),
            ),
            (
                header::CONTENT_DISPOSITION,
                HeaderValue::from_str(&disposition)
                    .unwrap_or_else(|_| HeaderValue::from_static("inline")),
            ),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("private, no-store"),
            ),
            (
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            ),
        ],
        pdf,
    )
        .into_response()
}

fn convert_with_libreoffice(
    soffice: &Path,
    temp_root: &Path,
    data: &[u8],
    suffix: &str,
) -> ApiResult<Vec<u8>> {
    let operation = temp_root.join(format!("conversion-{}", uuid::Uuid::new_v4()));
    let profile = operation.join("profile");
    let output = operation.join("output");
    std::fs::create_dir_all(&profile)
        .and_then(|_| std::fs::create_dir_all(&output))
        .map_err(|error| {
            let _ = std::fs::remove_dir_all(&operation);
            ApiError::internal(format!("无法创建 Office 预览临时目录：{error}"))
        })?;
    let source = operation.join(format!("source.{suffix}"));
    if let Err(error) = std::fs::write(&source, data) {
        let _ = std::fs::remove_dir_all(&operation);
        return Err(ApiError::internal(format!(
            "无法写入 Office 预览临时文件：{error}"
        )));
    }
    let profile_url = url::Url::from_directory_path(&profile)
        .map_err(|_| ApiError::internal("LibreOffice 临时配置路径无效。"))?;
    let mut child = std::process::Command::new(soffice)
        .arg(format!("-env:UserInstallation={}", profile_url.as_str()))
        .args([
            "--headless",
            "--nologo",
            "--nodefault",
            "--nofirststartwizard",
        ])
        .arg("--convert-to")
        .arg("pdf")
        .arg("--outdir")
        .arg(&output)
        .arg(&source)
        .current_dir(&operation)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            let _ = std::fs::remove_dir_all(&operation);
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "office_preview_conversion_failed",
                format!("无法启动 LibreOffice：{error}"),
                true,
            )
        })?;
    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < CONVERSION_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_dir_all(&operation);
                return Err(ApiError::new(
                    StatusCode::GATEWAY_TIMEOUT,
                    "office_preview_timeout",
                    "Office 转 PDF 超时。",
                    true,
                ));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_dir_all(&operation);
                return Err(ApiError::internal(format!(
                    "无法等待 LibreOffice 转换：{error}"
                )));
            }
        }
    };
    let rendered = output.join("source.pdf");
    let result = if status.success() && rendered.is_file() {
        let metadata = std::fs::metadata(&rendered)
            .map_err(|error| ApiError::internal(format!("无法读取转换后的 PDF：{error}")))?;
        if metadata.len() == 0 || metadata.len() > MAX_PDF_BYTES {
            Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "office_preview_invalid",
                "转换后的 PDF 为空或过大。",
                false,
            ))
        } else {
            let pdf = std::fs::read(&rendered)
                .map_err(|error| ApiError::internal(format!("无法读取转换后的 PDF：{error}")))?;
            if pdf.starts_with(b"%PDF-") {
                Ok(pdf)
            } else {
                Err(ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    "office_preview_conversion_failed",
                    "LibreOffice 返回了无效的 PDF。",
                    true,
                ))
            }
        }
    } else {
        Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "office_preview_conversion_failed",
            "LibreOffice 无法渲染这个文件。",
            true,
        ))
    };
    let _ = std::fs::remove_dir_all(&operation);
    result
}

fn find_soffice(state: &AppState) -> Option<PathBuf> {
    let executable = if cfg!(windows) {
        "soffice.exe"
    } else {
        "soffice"
    };
    let portable_candidates = [
        state
            .paths
            .root
            .join("tools/libreoffice/program")
            .join(executable),
        state
            .paths
            .root
            .join("resources/libreoffice/program")
            .join(executable),
    ];
    if let Some(path) = portable_candidates.into_iter().find(|path| path.is_file()) {
        return Some(path);
    }
    if let Some(path) = std::env::var_os("DEEPTUTOR_SOFFICE").map(PathBuf::from) {
        if path.is_file() {
            return Some(path);
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|directory| {
            let direct = directory.join(executable);
            if cfg!(windows) {
                vec![direct, directory.join("soffice.exe")]
            } else {
                vec![direct]
            }
        })
        .find(|candidate| candidate.is_file())
        .or_else(|| {
            if cfg!(windows) {
                ["C:\\Program Files\\LibreOffice\\program\\soffice.exe"]
                    .into_iter()
                    .map(PathBuf::from)
                    .find(|path| path.is_file())
            } else {
                ["/usr/bin/soffice", "/usr/local/bin/soffice"]
                    .into_iter()
                    .map(PathBuf::from)
                    .find(|path| path.is_file())
            }
        })
}

fn percent_decode(value: &str) -> ApiResult<String> {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err(ApiError::bad_request("预览 URL 的百分号编码不完整。"));
            }
            let high = hex_value(bytes[index + 1])
                .ok_or_else(|| ApiError::bad_request("预览 URL 的百分号编码无效。"))?;
            let low = hex_value(bytes[index + 2])
                .ok_or_else(|| ApiError::bad_request("预览 URL 的百分号编码无效。"))?;
            output.push((high << 4) | low);
            index += 3;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(output).map_err(|_| ApiError::bad_request("预览 URL 不是有效的 UTF-8 路径。"))
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 百分号解码支持中文与空格() {
        assert_eq!(
            percent_decode("/files/%E4%B8%AD%E6%96%87%20file.docx").unwrap(),
            "/files/中文 file.docx"
        );
        assert!(percent_decode("/files/%ZZ.docx").is_err());
    }

    #[test]
    fn 只接受受支持的办公文件() {
        assert!(ensure_office_name("讲义.DOCX").is_ok());
        assert!(ensure_office_name("讲义.pdf").is_err());
        assert!(ensure_office_name("没有扩展名").is_err());
    }
}
