use super::error::{ApiError, ApiResult};
use super::knowledge::{kb_raw_root, normalized_kb_name, raw_file_path};
use super::{AppState, ScopedState};
use axum::body::Body;
use axum::extract::{Path, Query};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::Response;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use image::{DynamicImage, ImageFormat, RgbaImage};
use pdf_render::pdf_interpret::InterpreterSettings;
use pdf_render::pdf_syntax::Pdf;
use pdf_render::{render, RenderSettings};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::io::{Cursor, Read};
use std::path::{Component, Path as FilePath, PathBuf};
use walkdir::WalkDir;

const VISUAL_ASSET_LIMIT: u64 = 5 * 1024 * 1024;
const PDF_SOURCE_LIMIT: u64 = 512 * 1024 * 1024;
const MAX_RENDER_PIXELS: f64 = 8_000_000.0;
const MAX_FULL_PAGE_PIXELS: f64 = 32_000_000.0;

#[derive(Debug, Deserialize)]
struct CoverageQuery {
    #[serde(default)]
    source_path: String,
    #[serde(default)]
    offset: usize,
    #[serde(default = "default_coverage_limit")]
    limit: usize,
}

#[derive(Debug, Deserialize)]
struct SourcePageQuery {
    source_path: String,
    page: usize,
    #[serde(default)]
    source_hash: String,
    #[serde(default)]
    region: String,
}

fn default_coverage_limit() -> usize {
    100
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/knowledge-bases/{kb_name}/visual-assets/{asset_id}",
            get(serve_visual_asset),
        )
        .route(
            "/knowledge-bases/{kb_name}/visual-coverage",
            get(visual_coverage),
        )
        .route(
            "/knowledge-bases/{kb_name}/source-page",
            get(render_source_page),
        )
        .route(
            "/knowledge-bases/{kb_name}/progress/clear",
            post(clear_progress),
        )
        .route(
            "/knowledge-bases/{kb_name}/indexing-policy",
            put(update_indexing_policy),
        )
}

async fn serve_visual_asset(
    ScopedState(state): ScopedState,
    Path((kb_name, asset_id)): Path<(String, String)>,
) -> ApiResult<Response> {
    validate_asset_id(&asset_id)?;
    let raw_root = kb_raw_root(&state, &kb_name)?;
    let kb_root = raw_root
        .parent()
        .ok_or_else(|| ApiError::internal("知识库目录结构无效。"))?
        .to_path_buf();
    let assets = load_visual_manifest(&kb_root)?;
    let record = assets
        .get(&asset_id)
        .and_then(Value::as_object)
        .ok_or_else(|| ApiError::not_found("视觉资源不存在。"))?;
    if record.get("asset_id").and_then(Value::as_str) != Some(asset_id.as_str()) {
        return Err(ApiError::not_found("视觉资源记录不匹配。"));
    }
    if matches!(
        visual_source_state(&state, &kb_name, record)?,
        "changed" | "missing"
    ) {
        return Err(ApiError::conflict(
            "引用的源文档已更改或丢失，请显式选择当前版本。",
        ));
    }
    let mime = record
        .get("mime_type")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::not_found("视觉资源缺少媒体类型。"))?;
    let extension =
        mime_extension(mime).ok_or_else(|| ApiError::not_found("视觉资源媒体类型不受支持。"))?;
    let asset_path = safe_join(
        &kb_root.join("visual_assets"),
        &format!("{asset_id}{extension}"),
    )?
    .ok_or_else(|| ApiError::not_found("视觉资源不存在。"))?;
    if asset_path
        .symlink_metadata()
        .is_ok_and(|meta| meta.file_type().is_symlink())
    {
        return Err(ApiError::forbidden("拒绝读取符号链接形式的视觉资源。"));
    }
    let bytes =
        std::fs::read(&asset_path).map_err(|_| ApiError::not_found("视觉资源文件不可用。"))?;
    if bytes.len() as u64 > VISUAL_ASSET_LIMIT
        || record.get("size").and_then(Value::as_u64) != Some(bytes.len() as u64)
        || record.get("image_sha256").and_then(Value::as_str)
            != Some(format!("{:x}", Sha256::digest(&bytes)).as_str())
        || sniff_image_mime(&bytes) != Some(mime)
    {
        return Err(ApiError::not_found("视觉资源校验失败。"));
    }
    binary_response(bytes, mime, None)
}

async fn visual_coverage(
    ScopedState(state): ScopedState,
    Path(kb_name): Path<String>,
    Query(query): Query<CoverageQuery>,
) -> ApiResult<Json<Value>> {
    let raw_root = kb_raw_root(&state, &kb_name)?;
    let kb_root = raw_root
        .parent()
        .ok_or_else(|| ApiError::internal("知识库目录结构无效。"))?
        .to_path_buf();
    let assets = load_visual_manifest(&kb_root)?;
    let limit = query.limit.clamp(1, 200);
    let source_filter = normalize_source_filter(&query.source_path)?;
    let files = collect_coverage_files(&raw_root, source_filter.as_deref())?;
    let mut documents = Vec::with_capacity(files.len());
    for file in files {
        let source_key = match file.strip_prefix(&raw_root) {
            Ok(relative) => relative.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        let digest = hash_file(&file)?;
        let report_file = coverage_report_path(&kb_root, &source_key)?;
        let mut report = read_coverage_report(&report_file)
            .filter(|value| {
                value.get("source_document_id").and_then(Value::as_str)
                    == Some(digest.as_str())
            })
            .unwrap_or_else(|| {
                json!({
                    "source_path": source_key,
                    "source_document_id": digest,
                    "whole_document_verified": false,
                    "issues": [{
                        "status": "unverified",
                        "reason": "extraction_coverage_unknown"
                    }],
                    "page_fallback": file.extension().is_some_and(|value| value.eq_ignore_ascii_case("pdf"))
                })
            });
        let retained = assets
            .values()
            .filter_map(Value::as_object)
            .filter(|record| {
                record.get("source_path").and_then(Value::as_str) == Some(source_key.as_str())
                    && record.get("source_document_id").and_then(Value::as_str)
                        == Some(digest.as_str())
            })
            .collect::<Vec<_>>();
        let locators = retained
            .iter()
            .filter_map(|record| record.get("source_locator").and_then(Value::as_str))
            .collect::<BTreeSet<_>>();
        report["retained_count"] = Value::from(locators.len());
        report["indexing_status"] = Value::String("not_verified_by_extraction_report".into());
        documents.push(report);
    }
    let mut selected_assets = assets
        .values()
        .filter_map(Value::as_object)
        .filter(|record| {
            source_filter.as_deref().is_none_or(|source| {
                record.get("source_path").and_then(Value::as_str) == Some(source)
            })
        })
        .cloned()
        .map(Value::Object)
        .collect::<Vec<_>>();
    selected_assets.sort_by(|left, right| {
        left.get("asset_id")
            .and_then(Value::as_str)
            .cmp(&right.get("asset_id").and_then(Value::as_str))
    });
    let total_assets = selected_assets.len();
    let page = selected_assets
        .into_iter()
        .skip(query.offset)
        .take(limit)
        .collect::<Vec<_>>();
    Ok(Json(json!({
        "documents": documents,
        "assets": page,
        "total_assets": total_assets,
        "offset": query.offset,
        "limit": limit,
        "model_image_limit": 2
    })))
}

async fn render_source_page(
    ScopedState(state): ScopedState,
    Path(kb_name): Path<String>,
    Query(query): Query<SourcePageQuery>,
) -> ApiResult<Response> {
    if query.page == 0 {
        return Err(ApiError::unprocessable("页码必须从 1 开始。"));
    }
    let region = parse_region(&query.region)?;
    let path = raw_file_path(&state, &kb_name, &query.source_path)?;
    if !path
        .extension()
        .is_some_and(|value| value.eq_ignore_ascii_case("pdf"))
    {
        return Err(ApiError::unprocessable("页面与区域预览仅支持 PDF 源文档。"));
    }
    let size = path
        .metadata()
        .map_err(|error| ApiError::unprocessable(format!("无法读取 PDF 信息：{error}")))?
        .len();
    if size > PDF_SOURCE_LIMIT {
        return Err(ApiError::unprocessable(
            "PDF 超过 512 MiB 页面预览上限，请打开原始文档。",
        ));
    }
    let page = query.page;
    let expected_hash = query.source_hash.trim().to_string();
    let rendered =
        tokio::task::spawn_blocking(move || render_pdf_page(&path, page, &expected_hash, region))
            .await
            .map_err(|error| ApiError::internal(format!("PDF 页面渲染任务失败：{error}")))??;
    let mut response = binary_response(rendered.bytes, &rendered.mime, None)?;
    response.headers_mut().insert(
        "x-source-document-id",
        HeaderValue::from_str(&rendered.source_document_id)
            .map_err(|_| ApiError::internal("无法生成源文档标识响应头。"))?,
    );
    Ok(response)
}

async fn clear_progress(
    ScopedState(state): ScopedState,
    Path(kb_name): Path<String>,
) -> ApiResult<Json<Value>> {
    let name = normalized_kb_name(&kb_name)?;
    let mut kb = state
        .database
        .get_resource("knowledge_base", &name)?
        .ok_or_else(|| ApiError::not_found("知识库不存在。"))?;
    if let Some(object) = kb.as_object_mut() {
        object.remove("progress");
    }
    state.database.put_resource("knowledge_base", &name, &kb)?;
    Ok(Json(json!({
        "status": "success",
        "message": format!("已清除知识库“{kb_name}”的进度状态。")
    })))
}

async fn update_indexing_policy(
    ScopedState(state): ScopedState,
    Path(kb_name): Path<String>,
    Json(_body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let name = normalized_kb_name(&kb_name)?;
    state
        .database
        .get_resource("knowledge_base", &name)?
        .ok_or_else(|| ApiError::not_found("知识库不存在。"))?;
    Err(ApiError::conflict(
        "空 LightRAG 知识库使用默认索引策略，请在设置中配置模型。",
    ))
}

struct RenderedPage {
    bytes: Vec<u8>,
    mime: String,
    source_document_id: String,
}

fn render_pdf_page(
    path: &FilePath,
    page_number: usize,
    expected_hash: &str,
    region: Option<[f64; 4]>,
) -> ApiResult<RenderedPage> {
    let digest = hash_file(path)?;
    if !expected_hash.is_empty() && expected_hash != digest {
        return Err(ApiError::unprocessable(
            "源文档在引用创建后已经更改，请显式选择当前页面。",
        ));
    }
    let bytes = std::fs::read(path)
        .map_err(|error| ApiError::unprocessable(format!("无法读取 PDF：{error}")))?;
    let pdf = Pdf::new(bytes)
        .map_err(|error| ApiError::unprocessable(format!("PDF 页面无法解码：{error:?}")))?;
    let pages = pdf.pages();
    if page_number == 0 || page_number > pages.len() {
        return Err(ApiError::unprocessable(format!(
            "页码必须位于 1 到 {} 之间。",
            pages.len()
        )));
    }
    let page = &pages[page_number - 1];
    let (page_width, page_height) = page.render_dimensions();
    if page_width <= 0.0 || page_height <= 0.0 {
        return Err(ApiError::unprocessable("PDF 页面尺寸无效。"));
    }
    let normalized_region = region.unwrap_or([0.0, 0.0, 1.0, 1.0]);
    let page_width = page_width as f64;
    let page_height = page_height as f64;
    let crop_width = (normalized_region[2] - normalized_region[0]) * page_width;
    let crop_height = (normalized_region[3] - normalized_region[1]) * page_height;
    let preferred_scale: f64 = if region.is_some() { 4.0 } else { 2.0 };
    let crop_budget_scale = (MAX_RENDER_PIXELS / (crop_width * crop_height)).sqrt();
    let full_budget_scale = (MAX_FULL_PAGE_PIXELS / (page_width * page_height)).sqrt();
    let dimension_scale = ((u16::MAX as f64 - 1.0) / page_width.max(page_height)).min(1.0e4);
    let scale = preferred_scale
        .min(crop_budget_scale)
        .min(full_budget_scale)
        .min(dimension_scale);
    if !scale.is_finite() || scale <= 0.0 {
        return Err(ApiError::unprocessable("PDF 页面缩放计算失败。"));
    }
    let pixmap = render(
        page,
        &InterpreterSettings::default(),
        &RenderSettings {
            x_scale: scale as f32,
            y_scale: scale as f32,
            bg_color: pdf_render::vello_cpu::color::palette::css::WHITE,
            ..Default::default()
        },
    );
    let width = pixmap.width() as u32;
    let height = pixmap.height() as u32;
    let pixels = pixmap.data_as_u8_slice().to_vec();
    let full = RgbaImage::from_raw(width, height, pixels)
        .ok_or_else(|| ApiError::internal("PDF 像素缓冲区尺寸不匹配。"))?;
    let x0 = (normalized_region[0] * width as f64).floor().max(0.0) as u32;
    let y0 = (normalized_region[1] * height as f64).floor().max(0.0) as u32;
    let x1 = (normalized_region[2] * width as f64)
        .ceil()
        .min(width as f64) as u32;
    let y1 = (normalized_region[3] * height as f64)
        .ceil()
        .min(height as f64) as u32;
    if x1 <= x0 || y1 <= y0 {
        return Err(ApiError::unprocessable("PDF 预览区域为空。"));
    }
    let cropped = image::imageops::crop_imm(&full, x0, y0, x1 - x0, y1 - y0).to_image();
    let (bytes, mime) = encode_bounded_image(cropped)?;
    if hash_file(path)? != digest {
        return Err(ApiError::unprocessable(
            "PDF 在页面渲染期间发生变化，请使用当前源文件重试。",
        ));
    }
    Ok(RenderedPage {
        bytes,
        mime,
        source_document_id: digest,
    })
}

fn encode_bounded_image(image: RgbaImage) -> ApiResult<(Vec<u8>, String)> {
    let mut png = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(image.clone())
        .write_to(&mut png, ImageFormat::Png)
        .map_err(|error| ApiError::internal(format!("无法编码 PDF 页面 PNG：{error}")))?;
    if png.get_ref().len() as u64 <= VISUAL_ASSET_LIMIT {
        return Ok((png.into_inner(), "image/png".to_string()));
    }
    let mut jpeg = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(image)
        .to_rgb8()
        .write_to(&mut jpeg, ImageFormat::Jpeg)
        .map_err(|error| ApiError::internal(format!("无法编码 PDF 页面 JPEG：{error}")))?;
    if jpeg.get_ref().len() as u64 > VISUAL_ASSET_LIMIT {
        return Err(ApiError::unprocessable(
            "页面图像超过模型字节预算，请选择更小的区域。",
        ));
    }
    Ok((jpeg.into_inner(), "image/jpeg".to_string()))
}

fn binary_response(bytes: Vec<u8>, mime: &str, source_id: Option<&str>) -> ApiResult<Response> {
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime)
        .header(header::CACHE_CONTROL, "private, no-store")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(header::CONTENT_LENGTH, bytes.len().to_string());
    if let Some(source_id) = source_id {
        builder = builder.header("x-source-document-id", source_id);
    }
    builder
        .body(Body::from(bytes))
        .map_err(|error| ApiError::internal(format!("无法生成二进制响应：{error}")))
}

fn load_visual_manifest(kb_root: &FilePath) -> ApiResult<Map<String, Value>> {
    let root = kb_root.join("visual_assets");
    if !root.exists() {
        return Ok(Map::new());
    }
    let root_meta = root
        .symlink_metadata()
        .map_err(|error| ApiError::internal(format!("无法读取视觉资源目录：{error}")))?;
    if !root_meta.is_dir() || root_meta.file_type().is_symlink() {
        return Err(ApiError::conflict("视觉资源目录不具有安全目录结构。"));
    }
    let manifest = root.join("manifest.json");
    if !manifest.exists() {
        return Ok(Map::new());
    }
    if manifest
        .symlink_metadata()
        .is_ok_and(|meta| meta.file_type().is_symlink())
    {
        return Err(ApiError::conflict("视觉资源清单不能是符号链接。"));
    }
    let mut text = String::new();
    std::fs::File::open(&manifest)
        .and_then(|mut file| file.read_to_string(&mut text))
        .map_err(|error| ApiError::conflict(format!("无法读取视觉资源清单：{error}")))?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|error| ApiError::conflict(format!("视觉资源清单格式无效：{error}")))?;
    let assets = value
        .get("assets")
        .and_then(Value::as_object)
        .ok_or_else(|| ApiError::conflict("视觉资源清单缺少 assets 对象。"))?;
    if assets.values().any(|value| !value.is_object()) {
        return Err(ApiError::conflict("视觉资源清单包含无效记录。"));
    }
    Ok(assets.clone())
}

fn visual_source_state(
    state: &AppState,
    kb_name: &str,
    record: &Map<String, Value>,
) -> ApiResult<&'static str> {
    if record.get("managed_source").and_then(Value::as_bool) != Some(true) {
        return Ok("unmanaged");
    }
    let Some(source_path) = record.get("source_path").and_then(Value::as_str) else {
        return Ok("missing");
    };
    let Ok(path) = raw_file_path(state, kb_name, source_path) else {
        return Ok("missing");
    };
    let Ok(digest) = hash_file(&path) else {
        return Ok("missing");
    };
    Ok(
        if record.get("source_document_id").and_then(Value::as_str) == Some(digest.as_str()) {
            "current"
        } else {
            "changed"
        },
    )
}

fn validate_asset_id(asset_id: &str) -> ApiResult<()> {
    if asset_id.len() != 64
        || !asset_id
            .bytes()
            .all(|value| value.is_ascii_digit() || (b'a'..=b'f').contains(&value))
    {
        return Err(ApiError::bad_request(
            "视觉资源 ID 必须是 64 位小写十六进制。",
        ));
    }
    Ok(())
}

fn mime_extension(mime: &str) -> Option<&'static str> {
    match mime {
        "image/png" => Some(".png"),
        "image/jpeg" => Some(".jpg"),
        "image/gif" => Some(".gif"),
        "image/webp" => Some(".webp"),
        _ => None,
    }
}

fn sniff_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else {
        None
    }
}

fn safe_join(root: &FilePath, relative: &str) -> ApiResult<Option<PathBuf>> {
    let relative_path = FilePath::new(relative);
    if relative_path.is_absolute()
        || relative_path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Ok(None);
    }
    let target = root.join(relative_path);
    let canonical_root = match root.canonicalize() {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let canonical_target = match target.canonicalize() {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    if !canonical_target.starts_with(&canonical_root) || !canonical_target.is_file() {
        return Ok(None);
    }
    Ok(Some(canonical_target))
}

fn normalize_source_filter(source_path: &str) -> ApiResult<Option<String>> {
    let source_path = source_path.trim().replace('\\', "/");
    if source_path.is_empty() {
        return Ok(None);
    }
    let path = FilePath::new(&source_path);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(ApiError::bad_request(
            "源文档路径必须是知识库内的相对路径。",
        ));
    }
    Ok(Some(source_path))
}

fn collect_coverage_files(root: &FilePath, source_filter: Option<&str>) -> ApiResult<Vec<PathBuf>> {
    if let Some(source_filter) = source_filter {
        let path = root.join(source_filter);
        let canonical_root = root
            .canonicalize()
            .map_err(|error| ApiError::conflict(format!("无法读取知识库目录：{error}")))?;
        let canonical_path = path
            .canonicalize()
            .map_err(|_| ApiError::conflict("指定的源文档不可用。"))?;
        if !canonical_path.starts_with(&canonical_root) || !canonical_path.is_file() {
            return Err(ApiError::conflict("指定的源文档不可用。"));
        }
        return Ok(vec![canonical_path]);
    }
    let mut files = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .collect::<Vec<_>>();
    files.sort();
    Ok(files)
}

fn coverage_report_path(kb_root: &FilePath, source_key: &str) -> ApiResult<PathBuf> {
    let key_hash = format!("{:x}", Sha256::digest(source_key.as_bytes()));
    let report_dir = kb_root.join("visual_assets").join("coverage");
    if report_dir
        .symlink_metadata()
        .is_ok_and(|meta| meta.file_type().is_symlink())
    {
        return Err(ApiError::conflict("视觉覆盖报告目录不能是符号链接。"));
    }
    Ok(report_dir.join(format!("{key_hash}.json")))
}

fn read_coverage_report(path: &FilePath) -> Option<Value> {
    let metadata = path.symlink_metadata().ok()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value.is_object().then_some(value)
}

fn parse_region(region: &str) -> ApiResult<Option<[f64; 4]>> {
    if region.trim().is_empty() {
        return Ok(None);
    }
    let values = region
        .split(',')
        .map(|value| {
            value
                .trim()
                .parse::<f64>()
                .map_err(|_| ApiError::unprocessable("region 必须包含四个有限的归一化坐标。"))
        })
        .collect::<ApiResult<Vec<_>>>()?;
    if values.len() != 4 || values.iter().any(|value| !value.is_finite()) {
        return Err(ApiError::unprocessable(
            "region 必须包含四个有限的归一化坐标。",
        ));
    }
    let region = [values[0], values[1], values[2], values[3]];
    if !(0.0 <= region[0]
        && region[0] < region[2]
        && region[2] <= 1.0
        && 0.0 <= region[1]
        && region[1] < region[3]
        && region[3] <= 1.0)
    {
        return Err(ApiError::unprocessable(
            "region 必须满足 0 <= x0 < x1 <= 1 且 0 <= y0 < y1 <= 1。",
        ));
    }
    Ok(Some(region))
}

fn hash_file(path: &FilePath) -> ApiResult<String> {
    let mut file = std::fs::File::open(path)
        .map_err(|error| ApiError::unprocessable(format!("无法读取源文档：{error}")))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| ApiError::unprocessable(format!("无法读取源文档：{error}")))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 区域坐标必须位于页面内() {
        assert!(parse_region("0,0,1,1").unwrap().is_some());
        assert!(parse_region("0.2,0.1,0.8,0.9").unwrap().is_some());
        assert!(parse_region("0.8,0,0.2,1").is_err());
        assert!(parse_region("0,0,1").is_err());
    }

    #[test]
    fn 视觉资源标识拒绝路径逃逸() {
        assert!(validate_asset_id(&"a".repeat(64)).is_ok());
        assert!(validate_asset_id("../asset").is_err());
        assert!(validate_asset_id(&"A".repeat(64)).is_err());
    }
}
