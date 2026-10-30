use super::AppState;
use super::ScopedState;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use std::path::{Path as FilePath, PathBuf};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

pub fn router() -> Router<AppState> {
    Router::new().route("/files/{*path}", get(serve_file))
}

async fn serve_file(
    Path(relative): Path<String>,
    headers: HeaderMap,
    ScopedState(state): ScopedState,
) -> Response {
    let path = match resolve_file(&state, &relative) {
        Ok(path) => path,
        Err((status, message)) => {
            return (status, Json(json!({"detail": message}))).into_response();
        }
    };

    serve_local_file(path, &headers).await
}

pub(super) fn resolve_file(
    state: &AppState,
    relative: &str,
) -> Result<PathBuf, (StatusCode, String)> {
    let normalized = relative.trim_start_matches('/').replace('\\', "/");
    if normalized.is_empty() {
        return Err((StatusCode::NOT_FOUND, "文件路径为空。".to_string()));
    }
    let segments = normalized.split('/').collect::<Vec<_>>();
    if segments.iter().any(|segment| {
        segment.is_empty()
            || *segment == "."
            || *segment == ".."
            || segment.contains(':')
            || segment.contains('\\')
    }) {
        return Err((
            StatusCode::BAD_REQUEST,
            "文件路径包含非法片段。".to_string(),
        ));
    }

    let (root, rest) = match segments.first().copied() {
        Some("outputs") => (&state.paths.outputs, &segments[1..]),
        Some("attachments") => (
            &state.paths.data.join("uploads").join("attachments"),
            &segments[1..],
        ),
        Some("library") => (
            &state.paths.data.join("uploads").join("library"),
            &segments[1..],
        ),
        Some("workspace-items") => (
            &state.paths.data.join("uploads").join("workspace-items"),
            &segments[1..],
        ),
        _ => return Err((StatusCode::NOT_FOUND, "不支持的文件区域。".to_string())),
    };
    if rest.is_empty() {
        return Err((StatusCode::NOT_FOUND, "缺少文件名。".to_string()));
    }

    let root =
        ensure_directory(root).map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))?;
    let requested = rest
        .iter()
        .fold(root.clone(), |path, segment| path.join(segment));
    let canonical = requested
        .canonicalize()
        .map_err(|_| (StatusCode::NOT_FOUND, "文件不存在。".to_string()))?;
    if !canonical.starts_with(&root) {
        return Err((
            StatusCode::FORBIDDEN,
            "拒绝访问便携数据目录之外的文件。".to_string(),
        ));
    }
    if !canonical.is_file() {
        return Err((StatusCode::NOT_FOUND, "请求路径不是文件。".to_string()));
    }
    Ok(canonical)
}

fn ensure_directory(path: &FilePath) -> Result<PathBuf, String> {
    std::fs::create_dir_all(path)
        .map_err(|error| format!("无法创建文件目录 {}：{error}", path.display()))?;
    path.canonicalize()
        .map_err(|error| format!("无法解析文件目录 {}：{error}", path.display()))
}

pub async fn serve_path(path: PathBuf, request_headers: &HeaderMap) -> Response {
    serve_local_file(path, request_headers).await
}

async fn serve_local_file(path: PathBuf, request_headers: &HeaderMap) -> Response {
    let mut file = match File::open(&path).await {
        Ok(file) => file,
        Err(error) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"detail": format!("无法打开文件：{error}")})),
            )
                .into_response()
        }
    };
    let metadata = match file.metadata().await {
        Ok(metadata) => metadata,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"detail": format!("无法读取文件信息：{error}")})),
            )
                .into_response()
        }
    };
    let length = metadata.len();
    let mime = mime_guess::from_path(&path).first_or_octet_stream();
    let range = request_headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok());

    let mut response = if let Some(range) = range {
        let (start, end) = match parse_range(range, length) {
            Ok(range) => range,
            Err(()) => {
                return Response::builder()
                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                    .header(header::CONTENT_RANGE, format!("bytes */{length}"))
                    .header(header::ACCEPT_RANGES, "bytes")
                    .body(Body::empty())
                    .unwrap_or_else(|_| StatusCode::RANGE_NOT_SATISFIABLE.into_response());
            }
        };
        let byte_count = end.saturating_sub(start).saturating_add(1);
        if file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"detail": "无法定位文件读取位置。"})),
            )
                .into_response();
        }
        let stream = ReaderStream::new(file.take(byte_count));
        Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header(header::CONTENT_TYPE, mime.as_ref())
            .header(header::CONTENT_LENGTH, byte_count)
            .header(
                header::CONTENT_RANGE,
                format!("bytes {start}-{end}/{length}"),
            )
            .header(header::ACCEPT_RANGES, "bytes")
            .body(Body::from_stream(stream))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    } else {
        let stream = ReaderStream::new(file);
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime.as_ref())
            .header(header::CONTENT_LENGTH, length)
            .header(header::ACCEPT_RANGES, "bytes")
            .body(Body::from_stream(stream))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    };

    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=3600"),
    );
    if let Some(filename) = path.file_name().and_then(|name| name.to_str()) {
        if let Ok(value) = HeaderValue::from_str(&format!("inline; filename=\"{filename}\"")) {
            response
                .headers_mut()
                .insert(header::CONTENT_DISPOSITION, value);
        }
    }
    response
}

fn parse_range(value: &str, length: u64) -> Result<(u64, u64), ()> {
    if length == 0 {
        return Err(());
    }
    let range = value.strip_prefix("bytes=").ok_or(())?;
    if range.contains(',') {
        return Err(());
    }
    let (start, end) = range.split_once('-').ok_or(())?;
    if start.is_empty() {
        let suffix = end.parse::<u64>().map_err(|_| ())?;
        if suffix == 0 {
            return Err(());
        }
        let start = length.saturating_sub(suffix);
        return Ok((start, length - 1));
    }
    let start = start.parse::<u64>().map_err(|_| ())?;
    if start >= length {
        return Err(());
    }
    let end = if end.is_empty() {
        length - 1
    } else {
        end.parse::<u64>().map_err(|_| ())?.min(length - 1)
    };
    if end < start {
        return Err(());
    }
    Ok((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 范围解析支持常见浏览器请求() {
        assert_eq!(parse_range("bytes=0-99", 1000), Ok((0, 99)));
        assert_eq!(parse_range("bytes=900-", 1000), Ok((900, 999)));
        assert_eq!(parse_range("bytes=-100", 1000), Ok((900, 999)));
        assert!(parse_range("bytes=1000-", 1000).is_err());
        assert!(parse_range("bytes=0-1,3-4", 1000).is_err());
    }
}
