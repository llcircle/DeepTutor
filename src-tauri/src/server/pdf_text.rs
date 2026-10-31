use super::documents::ExtractedDocument;
use pdf_render::pdf_interpret::InterpreterSettings;
use pdf_render::pdf_syntax::Pdf;
use pdf_render::{render, RenderSettings};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};

const MAX_TEXT_CHARS: usize = 8_000_000;
const MAX_MEDIA_PAGES: usize = 120;
const MAX_MEDIA_BYTES: usize = 60 * 1024 * 1024;
static PDF_WORKERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

pub(super) async fn extract_reading(
    bytes: Vec<u8>,
    media_directory: PathBuf,
) -> Result<ExtractedDocument, String> {
    let permit = PDF_WORKERS
        .acquire()
        .await
        .map_err(|_| "PDF 处理服务已关闭。".to_string())?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        catch_unwind(AssertUnwindSafe(|| {
            extract_inner(&bytes, Some(&media_directory), true)
        }))
        .map_err(|_| "PDF 解析失败，文件可能已损坏。".to_string())?
    })
    .await
    .map_err(|error| format!("PDF 处理任务失败：{error}"))?
}

/// 保留每一页的定位，优先读取文本层，仅对无文本页面进行本地 OCR。
pub(super) fn extract(bytes: &[u8]) -> Result<ExtractedDocument, String> {
    extract_inner(bytes, None, true)
}

fn extract_inner(
    bytes: &[u8],
    media_directory: Option<&Path>,
    enable_ocr: bool,
) -> Result<ExtractedDocument, String> {
    let pdf = Pdf::new(bytes.to_vec())
        .map_err(|error| format!("无法打开 PDF，文件可能已损坏或需要密码：{error:?}"))?;
    let page_count = pdf.pages().len();
    if page_count == 0 {
        return Err("PDF 没有可读取的页面。".to_string());
    }
    if page_count > 2_000 {
        return Err("PDF 超过 2000 页处理上限，请拆分后导入。".to_string());
    }
    let native = catch_unwind(AssertUnwindSafe(|| {
        pdf_extract::extract_text_from_mem_by_pages(bytes)
    }));
    let mut pages = match native {
        Ok(Ok(pages)) => pages,
        _ => vec![String::new(); page_count],
    };
    pages.resize(page_count, String::new());
    pages.truncate(page_count);
    for text in &mut pages {
        *text = text.replace(['\u{c}', '\0'], "").trim().to_string();
    }

    let mut recognized = 0usize;
    let mut warning = String::new();
    let mut media_warning = String::new();
    let mut media = HashMap::<String, usize>::new();
    let mut media_bytes = 0usize;
    let mut remaining = MAX_TEXT_CHARS;
    let mut ocr = if enable_ocr && pages.iter().any(|page| page.is_empty()) {
        match NativeOcr::new() {
            Ok(engine) => Some(engine),
            Err(error) => {
                warning = error;
                None
            }
        }
    } else {
        None
    };
    for (index, text) in pages.iter_mut().enumerate() {
        let needs_ocr = text.is_empty() && remaining > 0 && ocr.is_some();
        let needs_media =
            media_directory.is_some() && index < MAX_MEDIA_PAGES && media_bytes < MAX_MEDIA_BYTES;
        if needs_ocr || needs_media {
            let result = catch_unwind(AssertUnwindSafe(|| {
                let page = &pdf.pages()[index];
                let (width, height) = page.render_dimensions();
                if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
                    return Err("PDF 页面尺寸无效。".to_string());
                }
                let edge = if needs_ocr { 2200.0 } else { 1600.0 };
                let scale = (edge / width.max(height)).min(3.0);
                let pixmap = render(
                    page,
                    &InterpreterSettings::default(),
                    &RenderSettings {
                        x_scale: scale,
                        y_scale: scale,
                        bg_color: pdf_render::vello_cpu::color::palette::css::WHITE,
                        ..Default::default()
                    },
                );
                if needs_media {
                    let name = format!("page-{:04}.jpg", index + 1);
                    let saved = save_page_image(
                        media_directory.expect("已检查媒体目录"),
                        &name,
                        pixmap.width() as u32,
                        pixmap.height() as u32,
                        pixmap.data_as_u8_slice(),
                        MAX_MEDIA_BYTES.saturating_sub(media_bytes),
                    );
                    match saved {
                        Ok(size) => {
                            media_bytes += size;
                            media.insert(name, index + 1);
                        }
                        Err(error) if media_warning.is_empty() => media_warning = error,
                        _ => {}
                    }
                }
                if needs_ocr {
                    ocr.as_mut().expect("已检查 OCR 引擎").recognize(
                        pixmap.width() as u32,
                        pixmap.height() as u32,
                        pixmap.data_as_u8_slice(),
                    )
                } else {
                    Ok(String::new())
                }
            }));
            match result {
                Ok(Ok(value)) if !value.trim().is_empty() => {
                    *text = value.trim().to_string();
                    recognized += 1;
                }
                Ok(Err(error)) if warning.is_empty() => warning = error,
                Err(_) if warning.is_empty() => {
                    warning = "部分 PDF 页面无法完成本地文字识别，原始页面仍可阅读。".to_string();
                }
                _ => {}
            }
        }
        *text = text.chars().take(remaining).collect();
        remaining = remaining.saturating_sub(text.chars().count());
    }
    let empty_pages = pages.iter().filter(|text| text.is_empty()).count();
    let text = pages.join("\n\n");
    if text.trim().is_empty() && warning.is_empty() {
        warning = "此 PDF 没有可识别文字，已保留原始页面供阅读；可对文件进行 OCR 后重新导入。"
            .to_string();
    }
    let status = if text.trim().is_empty() {
        "unavailable"
    } else if empty_pages > 0 {
        "partial"
    } else if recognized > 0 {
        "ocr"
    } else {
        "embedded"
    };
    if empty_pages > 0 && !text.trim().is_empty() && warning.is_empty() {
        warning = format!("有 {empty_pages} 页未识别到文字，原始页面仍可阅读。");
    }
    if media_directory.is_some() && page_count > MAX_MEDIA_PAGES && media_warning.is_empty() {
        media_warning = format!("前 {MAX_MEDIA_PAGES} 页已生成图片预览，其余页面可在原文中阅读。");
    }
    let (outline, title) = pdf_structure(bytes, page_count);
    Ok(ExtractedDocument {
        text,
        metadata: json!({
            "format": "pdf",
            "parser": if recognized > 0 { "windows-ocr" } else { "pdf-extract" },
            "page_count": page_count,
            "page_texts": pages,
            "ocr_page_count": recognized,
            "empty_page_count": empty_pages,
            "text_status": status,
            "ocr_warning": warning,
            "media": media,
            "media_warning": media_warning,
            "outline": outline,
            "title": title
        }),
    })
}

fn save_page_image(
    directory: &Path,
    name: &str,
    width: u32,
    height: u32,
    rgba: &[u8],
    budget: usize,
) -> Result<usize, String> {
    let image = image::RgbaImage::from_raw(width, height, rgba.to_vec())
        .ok_or_else(|| "PDF 页面像素数据无效。".to_string())?;
    let image = image::DynamicImage::ImageRgba8(image);
    let image = if image.width() > 1600 || image.height() > 1600 {
        image.thumbnail(1600, 1600)
    } else {
        image
    };
    let image = image.to_rgb8();
    let mut encoded = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut encoded, 85)
        .encode_image(&image)
        .map_err(|error| format!("无法生成 PDF 页面图片：{error}"))?;
    if encoded.len() > budget {
        return Err("PDF 页面图片达到 60 MB 存储上限，原文仍可阅读。".to_string());
    }
    std::fs::create_dir_all(directory)
        .and_then(|_| std::fs::write(directory.join(name), &encoded))
        .map_err(|error| format!("无法保存 PDF 页面图片：{error}"))?;
    Ok(encoded.len())
}

/// 只使用原生书签；遍历限制和去重可避免损坏的书签链形成死循环。
fn pdf_structure(bytes: &[u8], page_count: usize) -> (Vec<Value>, String) {
    use lopdf::{Document, Object};
    fn resolve<'a>(document: &'a Document, object: &'a Object) -> Option<&'a Object> {
        document.dereference(object).ok().map(|(_, value)| value)
    }
    let Ok(document) = Document::load_mem(bytes) else {
        return (Vec::new(), String::new());
    };
    let title = document
        .trailer
        .get(b"Info")
        .ok()
        .and_then(|value| resolve(&document, value))
        .and_then(|value| value.as_dict().ok())
        .and_then(|info| info.get(b"Title").ok())
        .and_then(|value| value.as_str().ok())
        .map(decode_pdf_string)
        .unwrap_or_default();
    let Some(catalog) = document.catalog().ok() else {
        return (Vec::new(), title);
    };
    let pages = document
        .get_pages()
        .into_iter()
        .map(|(number, id)| (id, number as usize))
        .collect::<HashMap<_, _>>();
    let mut destinations = HashMap::<Vec<u8>, Object>::new();
    if let Some(dict) = catalog
        .get(b"Dests")
        .ok()
        .and_then(|value| resolve(&document, value))
        .and_then(|value| value.as_dict().ok())
    {
        for (name, value) in dict.iter() {
            destinations.insert(name.clone(), value.clone());
        }
    }
    let mut names = catalog
        .get(b"Names")
        .ok()
        .and_then(|value| resolve(&document, value))
        .and_then(|value| value.as_dict().ok())
        .and_then(|dict| dict.get(b"Dests").ok())
        .cloned()
        .into_iter()
        .collect::<Vec<_>>();
    for _ in 0..4_000 {
        let Some(node) = names.pop() else { break };
        let Some(dict) = resolve(&document, &node).and_then(|value| value.as_dict().ok()) else {
            continue;
        };
        if let Ok(values) = dict.get(b"Names").and_then(Object::as_array) {
            for pair in values.chunks_exact(2) {
                if let Ok(name) = pair[0].as_str() {
                    destinations.insert(name.to_vec(), pair[1].clone());
                }
            }
        }
        if let Ok(kids) = dict.get(b"Kids").and_then(Object::as_array) {
            names.extend(kids.iter().take(4_000).cloned());
        }
    }
    let first = catalog
        .get(b"Outlines")
        .ok()
        .and_then(|value| resolve(&document, value))
        .and_then(|value| value.as_dict().ok())
        .and_then(|dict| dict.get(b"First").ok());
    let mut pending = first
        .cloned()
        .map(|node| vec![(node, 1usize)])
        .unwrap_or_default();
    let mut seen = HashSet::new();
    let mut outline = Vec::new();
    for _ in 0..4_000 {
        let Some((node, level)) = pending.pop() else {
            break;
        };
        if let Ok(id) = node.as_reference() {
            if !seen.insert(id) {
                continue;
            }
        }
        let Some(dict) = resolve(&document, &node).and_then(|value| value.as_dict().ok()) else {
            continue;
        };
        if let Ok(next) = dict.get(b"Next") {
            pending.push((next.clone(), level));
        }
        if level < 32 {
            if let Ok(child) = dict.get(b"First") {
                pending.push((child.clone(), level + 1));
            }
        }
        let title = dict
            .get(b"Title")
            .ok()
            .and_then(|value| resolve(&document, value))
            .and_then(|value| value.as_str().ok())
            .map(decode_pdf_string)
            .unwrap_or_default();
        let action = dict
            .get(b"A")
            .ok()
            .and_then(|value| resolve(&document, value))
            .and_then(|value| value.as_dict().ok())
            .filter(|action| action.get(b"S").and_then(Object::as_name).ok() == Some(b"GoTo"));
        let dest = dict
            .get(b"Dest")
            .ok()
            .or_else(|| action.and_then(|action| action.get(b"D").ok()));
        let Some(mut dest) = dest.and_then(|value| resolve(&document, value)) else {
            continue;
        };
        for _ in 0..8 {
            match dest {
                Object::Name(name) | Object::String(name, _) => {
                    let Some(value) = destinations
                        .get(name)
                        .and_then(|value| resolve(&document, value))
                    else {
                        break;
                    };
                    dest = value;
                }
                Object::Dictionary(dict) => {
                    let Some(value) = dict
                        .get(b"D")
                        .ok()
                        .and_then(|value| resolve(&document, value))
                    else {
                        break;
                    };
                    dest = value;
                }
                _ => break,
            }
        }
        let locator = dest
            .as_array()
            .ok()
            .and_then(|values| values.first())
            .and_then(|value| value.as_reference().ok())
            .and_then(|id| pages.get(&id))
            .copied();
        if let Some(locator) = locator.filter(|page| *page > 0 && *page <= page_count) {
            if !title.trim().is_empty() {
                outline.push(json!({"locator": locator, "title": title, "level": level,
                    "synthesised": false, "source_href": "", "source_anchor": ""}));
            }
        }
    }
    (outline, title)
}

fn decode_pdf_string(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xfe, 0xff]) || bytes.starts_with(&[0xff, 0xfe]) {
        let big_endian = bytes[0] == 0xfe;
        let words = bytes[2..]
            .chunks_exact(2)
            .map(|pair| {
                if big_endian {
                    u16::from_be_bytes([pair[0], pair[1]])
                } else {
                    u16::from_le_bytes([pair[0], pair[1]])
                }
            })
            .collect::<Vec<_>>();
        String::from_utf16_lossy(&words)
    } else {
        String::from_utf8_lossy(bytes).to_string()
    }
}

#[cfg(windows)]
struct NativeOcr {
    engine: Option<windows::Media::Ocr::OcrEngine>,
}

#[cfg(windows)]
impl NativeOcr {
    fn new() -> Result<Self, String> {
        use windows::Media::Ocr::OcrEngine;
        use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
        unsafe { RoInitialize(RO_INIT_MULTITHREADED) }
            .map_err(|error| format!("无法初始化本地文字识别：{error}"))?;
        match OcrEngine::TryCreateFromUserProfileLanguages() {
            Ok(engine) => Ok(Self {
                engine: Some(engine),
            }),
            Err(error) => {
                unsafe { windows::Win32::System::WinRT::RoUninitialize() };
                Err(format!(
                    "本地 OCR 不可用：{error}。已保留 PDF 原始页面；可安装 Windows OCR 语言包，或对文件进行 OCR 后重新导入。"
                ))
            }
        }
    }

    fn recognize(&mut self, width: u32, height: u32, rgba: &[u8]) -> Result<String, String> {
        use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
        use windows::Storage::Streams::DataWriter;
        let recognize = || -> windows::core::Result<String> {
            let mut bgra = rgba.to_vec();
            for pixel in bgra.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
            let writer = DataWriter::new()?;
            writer.WriteBytes(&bgra)?;
            let bitmap = SoftwareBitmap::CreateCopyFromBuffer(
                &writer.DetachBuffer()?,
                BitmapPixelFormat::Bgra8,
                width as i32,
                height as i32,
            )?;
            let result = self
                .engine
                .as_ref()
                .expect("OCR 引擎已初始化")
                .RecognizeAsync(&bitmap)?
                .join();
            let _ = bitmap.Close();
            let result = result?;
            let mut lines = Vec::new();
            for line in result.Lines()? {
                lines.push(line.Text()?.to_string());
            }
            Ok(lines.join("\n"))
        };
        recognize().map_err(|error| format!("PDF 页面本地文字识别失败：{error}"))
    }
}

#[cfg(windows)]
impl Drop for NativeOcr {
    fn drop(&mut self) {
        self.engine.take();
        unsafe { windows::Win32::System::WinRT::RoUninitialize() };
    }
}

#[cfg(not(windows))]
struct NativeOcr;

#[cfg(not(windows))]
impl NativeOcr {
    fn new() -> Result<Self, String> {
        Err("此系统未提供内置 OCR，已保留 PDF 原始页面；可对文件进行 OCR 后重新导入。".to_string())
    }

    fn recognize(&mut self, _width: u32, _height: u32, _rgba: &[u8]) -> Result<String, String> {
        Err("本地 OCR 不可用。".to_string())
    }
}

#[cfg(test)]
#[path = "pdf_text_tests.rs"]
pub(super) mod tests;
