use calamine::{open_workbook_auto_from_rs, Data, Reader};
use encoding_rs::{GB18030, UTF_8};
use regex::Regex;
use serde_json::{json, Value};
use std::io::{Cursor, Read};
use std::path::Path;
use zip::ZipArchive;

const MAX_EXTRACTED_CHARS: usize = 8_000_000;

#[derive(Debug, Clone)]
pub struct ExtractedDocument {
    pub text: String,
    pub metadata: Value,
}

/// 原生解析 DeepTutor 支持和常见文本格式。解析失败的格式会返回错误，
/// 由调用方决定显示“无法解析”还是仅保存原始文件。
pub fn extract(filename: &str, bytes: &[u8]) -> Result<ExtractedDocument, String> {
    let extension = Path::new(filename)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let extracted = match extension.as_str() {
        "pdf" => super::pdf_text::extract(bytes)?,
        "docx" => extract_docx(bytes)?,
        "pptx" => extract_pptx(bytes)?,
        "epub" => extract_epub(bytes)?,
        "xlsx" | "xls" | "xlsb" | "ods" => extract_spreadsheet(filename, bytes)?,
        "html" | "htm" | "xhtml" => ExtractedDocument {
            text: normalize_text(&markup_to_text(&decode_text(bytes))),
            metadata: json!({"format": "html", "parser": "native-markup"}),
        },
        "xml" | "svg" | "json" | "jsonc" | "json5" | "yaml" | "yml" | "toml" | "md"
        | "markdown" | "txt" | "text" | "log" | "csv" | "tsv" | "rst" | "tex" | "latex" | "bib"
        | "css" | "scss" | "sass" | "less" | "js" | "mjs" | "cjs" | "ts" | "mts" | "cts"
        | "jsx" | "tsx" | "vue" | "svelte" | "py" | "java" | "kt" | "kts" | "scala" | "groovy"
        | "gradle" | "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" | "cs" | "go"
        | "rs" | "zig" | "nim" | "swift" | "m" | "mm" | "rb" | "php" | "pl" | "pm" | "lua"
        | "r" | "jl" | "dart" | "hs" | "clj" | "cljs" | "cljc" | "ex" | "exs" | "erl" | "ml"
        | "mli" | "fs" | "fsx" | "lisp" | "lsp" | "scm" | "rkt" | "sol" | "sh" | "bash" | "zsh"
        | "fish" | "ps1" | "vim" | "sql" | "graphql" | "gql" | "proto" | "cmake" | "mk" | "tf"
        | "hcl" | "nginxconf" | "dockerfile" | "ini" | "cfg" | "conf" | "env" | "properties" => {
            ExtractedDocument {
                text: normalize_text(&decode_text(bytes)),
                metadata: json!({"format": extension, "parser": "native-text"}),
            }
        }
        _ => {
            return Err(format!(
                "原生解析器暂不支持 .{extension} 文件；文件仍会保存到便携数据目录。"
            ));
        }
    };
    if extracted.text.trim().is_empty() {
        return Err(extracted
            .metadata
            .get("ocr_warning")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or("没有从文档中提取到可索引文本。")
            .to_string());
    }
    Ok(extracted)
}

pub fn extract_path(path: &Path) -> Result<ExtractedDocument, String> {
    let bytes =
        std::fs::read(path).map_err(|error| format!("无法读取文档 {}：{error}", path.display()))?;
    let filename = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("document");
    extract(filename, &bytes)
}

/// 按 Unicode 字符边界切块，避免中文、Emoji 被截断。
pub fn chunk_text(text: &str, chunk_size: usize, overlap: usize) -> Vec<String> {
    let characters = text.chars().collect::<Vec<_>>();
    if characters.is_empty() {
        return Vec::new();
    }
    let chunk_size = chunk_size.max(200);
    let overlap = overlap.min(chunk_size.saturating_sub(1));
    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < characters.len() {
        let end = (start + chunk_size).min(characters.len());
        let chunk = characters[start..end]
            .iter()
            .collect::<String>()
            .trim()
            .to_string();
        if !chunk.is_empty() {
            chunks.push(chunk);
        }
        if end == characters.len() {
            break;
        }
        start = end.saturating_sub(overlap);
    }
    chunks
}

pub fn decode_text(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return UTF_8.decode(&bytes[3..]).0.into_owned();
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        let utf16 = bytes[2..]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        return String::from_utf16_lossy(&utf16);
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        let utf16 = bytes[2..]
            .chunks_exact(2)
            .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        return String::from_utf16_lossy(&utf16);
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_string();
    }
    let (text, _, _) = GB18030.decode(bytes);
    text.into_owned()
}

fn extract_docx(bytes: &[u8]) -> Result<ExtractedDocument, String> {
    let xml = read_zip_entry(bytes, "word/document.xml")?;
    let text = normalize_text(&wordprocessing_markup_to_text(&decode_text(&xml)));
    Ok(ExtractedDocument {
        text,
        metadata: json!({"format": "docx", "parser": "native-ooxml"}),
    })
}

fn extract_pptx(bytes: &[u8]) -> Result<ExtractedDocument, String> {
    let mut archive = open_archive(bytes)?;
    let mut slide_names = (0..archive.len())
        .filter_map(|index| {
            archive
                .by_index(index)
                .ok()
                .map(|entry| entry.name().to_string())
        })
        .filter(|name| name.starts_with("ppt/slides/slide") && name.ends_with(".xml"))
        .collect::<Vec<_>>();
    slide_names.sort_by_key(|name| slide_number(name));
    let mut pages = Vec::new();
    for name in slide_names {
        let mut entry = archive
            .by_name(&name)
            .map_err(|error| format!("无法读取 PPTX 幻灯片 {name}：{error}"))?;
        let mut xml = String::new();
        entry
            .read_to_string(&mut xml)
            .map_err(|error| format!("无法解码 PPTX 幻灯片 {name}：{error}"))?;
        pages.push(normalize_text(&wordprocessing_markup_to_text(&xml)));
    }
    let text = pages
        .into_iter()
        .enumerate()
        .filter(|(_, page)| !page.is_empty())
        .map(|(index, page)| format!("## 幻灯片 {}\n{page}", index + 1))
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok(ExtractedDocument {
        text: limit_text(text),
        metadata: json!({"format": "pptx", "parser": "native-ooxml"}),
    })
}

fn extract_epub(bytes: &[u8]) -> Result<ExtractedDocument, String> {
    let mut archive = open_archive(bytes)?;
    let mut chapters = Vec::new();
    for index in 0..archive.len() {
        let Ok(mut entry) = archive.by_index(index) else {
            continue;
        };
        let name = entry.name().to_string();
        let lower = name.to_ascii_lowercase();
        if !(lower.ends_with(".xhtml") || lower.ends_with(".html") || lower.ends_with(".htm")) {
            continue;
        }
        let mut content = Vec::new();
        if entry.read_to_end(&mut content).is_ok() {
            let text = normalize_text(&markup_to_text(&decode_text(&content)));
            if !text.is_empty() {
                chapters.push((name, text));
            }
        }
    }
    chapters.sort_by(|left, right| natural_cmp(&left.0, &right.0));
    let text = chapters
        .into_iter()
        .map(|(name, chapter)| format!("## {name}\n{chapter}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok(ExtractedDocument {
        text: limit_text(text),
        metadata: json!({"format": "epub", "parser": "native-epub"}),
    })
}

fn extract_spreadsheet(filename: &str, bytes: &[u8]) -> Result<ExtractedDocument, String> {
    let extension = Path::new(filename)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let cursor = Cursor::new(bytes.to_vec());
    let mut workbook = open_workbook_auto_from_rs(cursor)
        .map_err(|error| format!("无法打开 {filename} 工作簿：{error}"))?;
    let mut sheets = Vec::new();
    for sheet_name in workbook.sheet_names() {
        let range = workbook
            .worksheet_range(&sheet_name)
            .map_err(|error| format!("无法读取工作表 {sheet_name}：{error}"))?;
        let rows = range
            .rows()
            .map(|row| {
                row.iter()
                    .map(cell_text)
                    .collect::<Vec<_>>()
                    .join("\t")
                    .trim_end()
                    .to_string()
            })
            .filter(|row| !row.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        if !rows.is_empty() {
            sheets.push(format!("## {sheet_name}\n{rows}"));
        }
    }
    Ok(ExtractedDocument {
        text: limit_text(sheets.join("\n\n")),
        metadata: json!({"format": extension, "parser": "calamine"}),
    })
}

fn cell_text(cell: &Data) -> String {
    match cell {
        Data::Empty => String::new(),
        Data::String(value) => value.clone(),
        Data::Float(value) => value.to_string(),
        Data::Int(value) => value.to_string(),
        Data::Bool(value) => value.to_string(),
        Data::DateTime(value) => value.to_string(),
        Data::DateTimeIso(value) => value.clone(),
        Data::DurationIso(value) => value.clone(),
        Data::Error(value) => format!("#错误:{value:?}"),
    }
}

fn open_archive(bytes: &[u8]) -> Result<ZipArchive<Cursor<&[u8]>>, String> {
    ZipArchive::new(Cursor::new(bytes)).map_err(|error| format!("无法读取 ZIP 文档：{error}"))
}

fn read_zip_entry(bytes: &[u8], name: &str) -> Result<Vec<u8>, String> {
    let mut archive = open_archive(bytes)?;
    let mut entry = archive
        .by_name(name)
        .map_err(|error| format!("文档缺少 {name}：{error}"))?;
    let mut content = Vec::new();
    entry
        .read_to_end(&mut content)
        .map_err(|error| format!("无法读取 {name}：{error}"))?;
    Ok(content)
}

fn wordprocessing_markup_to_text(xml: &str) -> String {
    let with_breaks = Regex::new(r"(?i)</(?:w:p|a:p|w:tr|w:table)>")
        .expect("静态正则")
        .replace_all(xml, "\n");
    let with_tabs = Regex::new(r"(?i)</(?:w:tc|a:tc)>")
        .expect("静态正则")
        .replace_all(&with_breaks, "\t");
    markup_to_text(&with_tabs)
}

fn markup_to_text(markup: &str) -> String {
    let mut without_scripts = markup.to_string();
    for tag in ["script", "style", "noscript"] {
        let pattern = format!(r"(?is)<{tag}\b[^>]*>.*?</{tag}\s*>");
        let expression = Regex::new(&pattern).expect("静态正则");
        without_scripts = expression.replace_all(&without_scripts, " ").into_owned();
    }
    let blocks = Regex::new(
        r"(?i)</?(?:p|div|section|article|header|footer|li|tr|h[1-6]|blockquote|pre|br)\b[^>]*>",
    )
    .expect("静态正则")
    .replace_all(&without_scripts, "\n");
    let tags = Regex::new(r"(?s)<[^>]+>")
        .expect("静态正则")
        .replace_all(&blocks, " ");
    decode_entities(&tags)
}

fn decode_entities(text: &str) -> String {
    text.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn normalize_text(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut previous_blank = false;
    for line in text.replace('\r', "\n").lines() {
        let normalized = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if normalized.is_empty() {
            if !previous_blank && !output.is_empty() {
                output.push('\n');
            }
            previous_blank = true;
            continue;
        }
        output.push_str(normalized.trim());
        output.push('\n');
        previous_blank = false;
    }
    limit_text(output.trim().to_string())
}

fn limit_text(text: String) -> String {
    text.chars().take(MAX_EXTRACTED_CHARS).collect()
}

fn slide_number(name: &str) -> u32 {
    name.trim_end_matches(".xml")
        .rsplit('/')
        .next()
        .and_then(|value| value.trim_start_matches("slide").parse().ok())
        .unwrap_or(u32::MAX)
}

fn natural_cmp(left: &str, right: &str) -> std::cmp::Ordering {
    let left_key = split_numeric(left);
    let right_key = split_numeric(right);
    left_key.cmp(&right_key)
}

fn split_numeric(value: &str) -> Vec<(u8, u64, String)> {
    let mut result = Vec::new();
    let mut current = String::new();
    let mut numeric = None;
    for character in value.chars() {
        let is_numeric = character.is_ascii_digit();
        if numeric.is_some_and(|state| state != is_numeric) && !current.is_empty() {
            push_segment(&mut result, numeric.unwrap_or(false), &mut current);
        }
        numeric = Some(is_numeric);
        current.push(character);
    }
    if !current.is_empty() {
        push_segment(&mut result, numeric.unwrap_or(false), &mut current);
    }
    result
}

fn push_segment(output: &mut Vec<(u8, u64, String)>, numeric: bool, current: &mut String) {
    let value = std::mem::take(current);
    if numeric {
        output.push((1, value.parse().unwrap_or(u64::MAX), String::new()));
    } else {
        output.push((0, 0, value.to_ascii_lowercase()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 文本切块不会破坏中文字符() {
        let text = "数".repeat(1000);
        let chunks = chunk_text(&text, 300, 50);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|chunk| chunk.chars().count() <= 300));
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.chars().count())
                .sum::<usize>(),
            1000 + (chunks.len() - 1) * 50
        );
    }

    #[test]
    fn html解析会丢弃脚本并保留正文() {
        let text = markup_to_text(
            "<html><style>bad</style><body><h1>标题</h1><p>第一段 &amp; 内容</p></body></html>",
        );
        assert!(text.contains("标题"));
        assert!(text.contains("第一段 & 内容"));
        assert!(!text.contains("bad"));
    }
}
