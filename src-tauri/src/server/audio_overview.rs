use super::catalog::{resolve_catalog_connections, service_selection};
use super::llm::{self, LlmSettings, LlmUsage};
use super::media_services;
use super::tools;
use super::AppState;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::Path;
use tokio_util::sync::CancellationToken;

const MAX_QUERY_CHARS: usize = 1_000;
const MAX_CONTEXT_CHARS: usize = 16_000;
const MAX_SNIPPET_CHARS: usize = 2_000;
const MAX_SEGMENT_CHARS: usize = 4_000;
const MAX_TOTAL_AUDIO_BYTES: usize = 512 * 1024 * 1024;

const PROMPTS_ZH: &str =
    include_str!("../../../deeptutor/capabilities/prompts/zh/audio_overview.yaml");
const PROMPTS_EN: &str =
    include_str!("../../../deeptutor/capabilities/prompts/en/audio_overview.yaml");

#[derive(Clone, Debug)]
pub struct RequestConfig {
    pub topic: String,
    pub target_minutes: u64,
    pub host_voice: Option<String>,
    pub expert_voice: Option<String>,
    pub max_context_chunks: usize,
}

impl RequestConfig {
    pub fn from_request(request: &Value) -> Result<Self, String> {
        let config = request
            .get("config")
            .filter(|value| value.is_object())
            .unwrap_or(&Value::Null);
        let topic = config
            .get("topic")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .chars()
            .take(500)
            .collect::<String>();
        let target_minutes = config.get("target_minutes").and_then(json_u64).unwrap_or(5);
        if !(1..=15).contains(&target_minutes) {
            return Err("音频概览时长必须为 1 到 15 分钟。".to_string());
        }
        let max_context_chunks = config
            .get("max_context_chunks")
            .and_then(json_u64)
            .unwrap_or(6);
        if !(1..=12).contains(&max_context_chunks) {
            return Err("音频概览上下文片段数必须为 1 到 12。".to_string());
        }
        Ok(Self {
            topic,
            target_minutes,
            host_voice: optional_voice(config.get("host_voice"), "主持人")?,
            expert_voice: optional_voice(config.get("expert_voice"), "专家")?,
            max_context_chunks: max_context_chunks as usize,
        })
    }
}

#[derive(Clone, Debug)]
pub struct Citation {
    pub source_id: String,
    pub title: String,
    pub locator: String,
    pub snippet: String,
}

impl Citation {
    pub fn reference_json(&self) -> Value {
        json!({
            "source_id": self.source_id,
            "title": self.title,
            "locator": self.locator
        })
    }
}

#[derive(Clone, Debug)]
pub struct RetrievedContext {
    pub answer: String,
    pub citations: Vec<Citation>,
    pub kb_name: String,
}

#[derive(Clone, Debug)]
pub struct ScriptSegment {
    pub speaker: String,
    pub text: String,
    pub source_ids: Vec<String>,
}

impl ScriptSegment {
    pub fn json(&self) -> Value {
        json!({
            "speaker": self.speaker,
            "text": self.text,
            "source_ids": self.source_ids
        })
    }
}

#[derive(Clone, Debug)]
pub struct AudioScript {
    pub title: String,
    pub segments: Vec<ScriptSegment>,
}

#[derive(Clone, Debug)]
pub struct PublishedAudio {
    pub title: String,
    pub transcript: String,
    pub audio_relative: String,
    pub transcript_relative: String,
    pub audio_filename: String,
    pub transcript_filename: String,
    pub audio_bytes: u64,
    pub transcript_bytes: u64,
}

pub async fn retrieve(
    state: &AppState,
    request: &Value,
    config: &RequestConfig,
) -> Result<RetrievedContext, String> {
    let topic = if config.topic.is_empty() {
        request
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("知识库概览")
            .trim()
    } else {
        config.topic.as_str()
    };
    let note_id = request
        .get("note_document_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if !note_id.is_empty() {
        let note = load_note(state, note_id)?
            .ok_or_else(|| "笔记页面不存在，无法生成音频概览。".to_string())?;
        let text = resource_text(&note);
        if text.trim().is_empty() {
            return Err("笔记页面内容为空，无法生成音频概览。".to_string());
        }
        let title = note
            .get("title")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("笔记文档")
            .chars()
            .take(200)
            .collect::<String>();
        return Ok(RetrievedContext {
            answer: truncate_chars(&text, MAX_CONTEXT_CHARS),
            citations: vec![Citation {
                source_id: "S1".to_string(),
                title,
                locator: format!("/co-writer/{note_id}"),
                snippet: truncate_chars(&text, MAX_SNIPPET_CHARS),
            }],
            kb_name: String::new(),
        });
    }

    let kb_name = request
        .get("knowledge_bases")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .find(|value| !value.is_empty())
        .ok_or_else(|| "请先挂载知识库，再生成音频概览。".to_string())?
        .to_string();
    let query = truncate_chars(&format!("{topic} 音频概览"), MAX_QUERY_CHARS);
    let arguments = json!({
        "query": query,
        "kb_name": kb_name,
        "max_results": config.max_context_chunks
    });
    let outcome = tools::execute("rag", &arguments, state, Some(request)).await;
    if !outcome.success {
        return Err(format!("知识库检索失败：{}", outcome.content));
    }
    let rows = outcome
        .metadata
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if rows.is_empty() {
        return Err("知识库没有返回可用于音频概览的内容。".to_string());
    }
    let citations = rows
        .iter()
        .enumerate()
        .map(|(index, row)| {
            let source = first_text(row, &["source", "title", "filename", "document", "path"]);
            let title = Path::new(&source)
                .file_name()
                .and_then(|value| value.to_str())
                .filter(|value| !value.is_empty())
                .unwrap_or(&source);
            let title = if title.is_empty() {
                format!("来源 {}", index + 1)
            } else {
                truncate_chars(title, 200)
            };
            let locator = row.get("locator").map(scalar_text).unwrap_or_default();
            Citation {
                source_id: format!("S{}", index + 1),
                title,
                locator: truncate_chars(&locator, 200),
                snippet: truncate_chars(
                    &first_text(row, &["content", "snippet", "text", "preview"]),
                    MAX_SNIPPET_CHARS,
                ),
            }
        })
        .collect::<Vec<_>>();
    Ok(RetrievedContext {
        answer: truncate_chars(&outcome.content, MAX_CONTEXT_CHARS),
        citations,
        kb_name,
    })
}

pub async fn generate_script(
    state: &AppState,
    llm_settings: &LlmSettings,
    request: &Value,
    context: &RetrievedContext,
    config: &RequestConfig,
    language: &str,
    cancel: &CancellationToken,
) -> Result<(AudioScript, LlmUsage), String> {
    let chinese = language.to_ascii_lowercase().starts_with("zh");
    let (system_prompt, instruction_prompt) =
        load_prompts(if chinese { PROMPTS_ZH } else { PROMPTS_EN })?;
    let topic = if config.topic.is_empty() {
        request
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("知识库概览")
            .trim()
    } else {
        config.topic.as_str()
    };
    let language_label = if chinese { "简体中文" } else { "English" };
    let citation_block = if context.citations.is_empty() {
        "没有可用的来源元数据，只能依据检索摘要作答。".to_string()
    } else {
        context
            .citations
            .iter()
            .map(|citation| {
                format!(
                    "- [{}] {}{}\n  {}",
                    citation.source_id,
                    citation.title,
                    if citation.locator.is_empty() {
                        String::new()
                    } else {
                        format!("（{}）", citation.locator)
                    },
                    citation.snippet
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let prompt = format!(
        "{instruction_prompt}\n\n语言：{language_label}\n主题：{topic}\n目标时长：约 {} 分钟\n\
         有效引用 ID：\n{citation_block}\n\n检索上下文：\n{}",
        config.target_minutes,
        if context.answer.is_empty() {
            "（没有合成答案）"
        } else {
            context.answer.as_str()
        }
    );
    let messages = vec![
        json!({"role": "system", "content": system_prompt}),
        json!({"role": "user", "content": prompt}),
    ];
    let response = llm::stream_completion(
        &state.client,
        llm_settings,
        &messages,
        &[],
        None,
        cancel,
        |_| {},
    )
    .await
    .map_err(|error| format!("音频概览脚本生成失败：{error}"))?;
    let citation_ids = context
        .citations
        .iter()
        .map(|citation| citation.source_id.as_str())
        .collect::<BTreeSet<_>>();
    let script = parse_script(&response.content, &citation_ids)
        .map_err(|error| format!("音频概览脚本校验失败：{error}"))?;
    Ok((script, response.usage))
}

pub async fn publish(
    state: &AppState,
    context: &RetrievedContext,
    script: &AudioScript,
    config: &RequestConfig,
    turn_id: &str,
    language: &str,
    cancel: &CancellationToken,
) -> Result<PublishedAudio, String> {
    let catalog = state
        .database
        .get_setting("catalog")
        .map_err(|error| format!("无法读取语音设置：{error}"))?
        .unwrap_or_else(|| json!({}));
    let catalog = resolve_catalog_connections(&catalog);
    let service = catalog
        .pointer("/services/tts")
        .ok_or_else(|| "尚未配置语音合成服务。请先在设置中配置 TTS。".to_string())?;
    let profile_id = service
        .get("active_profile_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "尚未选择语音合成配置。".to_string())?;
    let model_id = service
        .get("active_model_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "尚未选择语音合成模型。".to_string())?;
    let Some((_profile, Some(model))) =
        service_selection(&catalog, "tts", Some(profile_id), Some(model_id))
    else {
        return Err("当前语音合成配置已失效，请重新选择。".to_string());
    };
    let model_voice = model
        .get("voice")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let mut host_voice = config
        .host_voice
        .clone()
        .or_else(|| model_voice.map(str::to_string));
    let mut expert_voice = config.expert_voice.clone();
    if host_voice.is_none() || expert_voice.is_none() {
        let discovered = media_services::discover_voices(state, &catalog, profile_id, model_id)
            .await
            .ok()
            .map(|value| {
                value
                    .get("voices")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|voice| voice.get("id").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if host_voice.is_none() {
            host_voice = discovered.first().cloned();
        }
        if expert_voice.is_none() {
            expert_voice = discovered
                .iter()
                .find(|voice| Some(voice.as_str()) != host_voice.as_deref())
                .cloned();
        }
    }
    let host_voice =
        host_voice.ok_or_else(|| "请配置主持人语音，或为该 TTS 模型启用语音列表。".to_string())?;
    let expert_voice = expert_voice.ok_or_else(|| "请配置与主持人不同的专家语音。".to_string())?;
    if host_voice == expert_voice {
        return Err("主持人与专家必须使用不同语音。".to_string());
    }

    let mut parts = Vec::with_capacity(script.segments.len());
    let mut total_bytes = 0usize;
    for segment in &script.segments {
        if cancel.is_cancelled() {
            return Err("音频概览生成已取消。".to_string());
        }
        let voice = if segment.speaker == "host" {
            host_voice.as_str()
        } else {
            expert_voice.as_str()
        };
        let (audio, content_type) = media_services::synthesize_active_tts(
            state,
            &catalog,
            profile_id,
            model_id,
            &segment.text,
            Some(voice),
            None,
        )
        .await
        .map_err(|error| format!("语音合成失败：{error}"))?;
        if audio.is_empty() {
            return Err("语音服务返回了空音频。".to_string());
        }
        total_bytes = total_bytes.saturating_add(audio.len());
        if total_bytes > MAX_TOTAL_AUDIO_BYTES {
            return Err("音频概览超过 512 MiB 限制，请缩短时长。".to_string());
        }
        parts.push((audio, content_type));
    }

    let output_dir = state.paths.outputs.join("audio-overviews");
    tokio::fs::create_dir_all(&output_dir)
        .await
        .map_err(|error| format!("无法创建音频概览目录：{error}"))?;
    let stem = safe_stem(&format!("{turn_id}-audio-overview"), "audio-overview");
    let transcript = build_transcript(script, &context.citations, language);
    let transcript_path = output_dir.join(format!("{stem}.md"));
    let transcript_bytes = transcript.as_bytes().to_vec();
    tokio::fs::write(&transcript_path, &transcript_bytes)
        .await
        .map_err(|error| format!("无法写入音频概览文稿：{error}"))?;

    let audio = tokio::task::spawn_blocking(move || media_services::join_audio_parts(&parts))
        .await
        .map_err(|error| format!("音频拼接任务异常：{error}"))?
        .map_err(|error| format!("音频拼接失败：{error}"))?;
    if audio.is_empty() {
        return Err("拼接后的音频为空。".to_string());
    }
    let audio_path = output_dir.join(format!("{stem}.wav"));
    tokio::fs::write(&audio_path, &audio)
        .await
        .map_err(|error| format!("无法写入音频概览：{error}"))?;

    let audio_filename = audio_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let transcript_filename = transcript_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    Ok(PublishedAudio {
        title: script.title.clone(),
        transcript,
        audio_relative: format!("audio-overviews/{audio_filename}"),
        transcript_relative: format!("audio-overviews/{transcript_filename}"),
        audio_filename,
        transcript_filename,
        audio_bytes: audio.len() as u64,
        transcript_bytes: transcript_bytes.len() as u64,
    })
}

pub fn parse_script(raw: &str, citation_ids: &BTreeSet<&str>) -> Result<AudioScript, String> {
    let payload =
        parse_json_object(raw).ok_or_else(|| "模型没有返回合法的 JSON 对象。".to_string())?;
    let title = payload
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if title.is_empty() || title.chars().count() > 200 {
        return Err("标题长度必须为 1 到 200 个字符。".to_string());
    }
    let rows = payload
        .get("segments")
        .and_then(Value::as_array)
        .ok_or_else(|| "缺少 segments 数组。".to_string())?;
    if !(2..=80).contains(&rows.len()) {
        return Err("对白段数必须为 2 到 80 段。".to_string());
    }
    let mut segments = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        let expected = if index % 2 == 0 { "host" } else { "expert" };
        let speaker = row
            .get("speaker")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if speaker != expected {
            return Err(format!(
                "第 {} 段必须由 {expected} 发言，实际为 {}。",
                index + 1,
                if speaker.is_empty() {
                    "未知角色"
                } else {
                    speaker
                }
            ));
        }
        let text = row
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if text.is_empty() || text.chars().count() > MAX_SEGMENT_CHARS {
            return Err(format!(
                "第 {} 段文本长度必须为 1 到 {} 个字符。",
                index + 1,
                MAX_SEGMENT_CHARS
            ));
        }
        let mut seen = BTreeSet::new();
        let mut source_ids = Vec::new();
        for source_id in row
            .get("source_ids")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            if !citation_ids.contains(source_id) {
                return Err(format!(
                    "第 {} 段引用了不存在的来源 ID：{source_id}。",
                    index + 1
                ));
            }
            if seen.insert(source_id.to_string()) {
                source_ids.push(source_id.to_string());
            }
        }
        segments.push(ScriptSegment {
            speaker: speaker.to_string(),
            text,
            source_ids,
        });
    }
    Ok(AudioScript { title, segments })
}

pub fn build_transcript(script: &AudioScript, citations: &[Citation], language: &str) -> String {
    let chinese = language.to_ascii_lowercase().starts_with("zh");
    let labels = if chinese {
        ("主持人", "专家", "来源", "逐字稿")
    } else {
        ("Host", "Expert", "Sources", "Transcript")
    };
    let mut lines = vec![format!("# {}", script.title), String::new()];
    if !citations.is_empty() {
        lines.push(format!("## {}", labels.2));
        lines.push(String::new());
        for citation in citations {
            lines.push(format!(
                "- [{}] {}{}",
                citation.source_id,
                citation.title,
                if citation.locator.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", citation.locator)
                }
            ));
        }
        lines.push(String::new());
    }
    lines.push(format!("## {}", labels.3));
    lines.push(String::new());
    for segment in &script.segments {
        let label = if segment.speaker == "host" {
            labels.0
        } else {
            labels.1
        };
        lines.push(format!("### {label}"));
        lines.push(String::new());
        let references = segment
            .source_ids
            .iter()
            .map(|source_id| format!("[{source_id}]"))
            .collect::<Vec<_>>()
            .join(" ");
        lines.push(if references.is_empty() {
            segment.text.clone()
        } else {
            format!("{} {references}", segment.text)
        });
        lines.push(String::new());
    }
    lines.join("\n").trim_end().to_string() + "\n"
}

fn load_prompts(raw: &str) -> Result<(String, String), String> {
    let value: serde_yaml::Value =
        serde_yaml::from_str(raw).map_err(|error| format!("音频概览提示词无效：{error}"))?;
    let system = value
        .get("system")
        .and_then(serde_yaml::Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let instructions = value
        .get("instructions")
        .and_then(serde_yaml::Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if system.is_empty() || instructions.is_empty() {
        return Err("音频概览提示词缺少 system 或 instructions。".to_string());
    }
    Ok((system, instructions))
}

fn load_note(state: &AppState, note_id: &str) -> Result<Option<Value>, String> {
    for domain in ["document", "note", "notebook_record"] {
        if let Some(value) = state.database.get_resource(domain, note_id)? {
            return Ok(Some(value));
        }
    }
    Ok(None)
}

fn resource_text(value: &Value) -> String {
    for pointer in [
        "/content",
        "/text",
        "/markdown",
        "/body",
        "/description",
        "/summary",
    ] {
        if let Some(text) = value.pointer(pointer).and_then(Value::as_str) {
            if !text.trim().is_empty() {
                return text.to_string();
            }
        }
    }
    value.to_string()
}

fn first_text(row: &Value, fields: &[&str]) -> String {
    for field in fields {
        if let Some(value) = row.get(*field) {
            let text = scalar_text(value);
            if !text.is_empty() {
                return text;
            }
        }
    }
    String::new()
}

fn scalar_text(value: &Value) -> String {
    match value {
        Value::String(value) => value.trim().to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn optional_voice(value: Option<&Value>, label: &str) -> Result<Option<String>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let voice = value
        .as_str()
        .ok_or_else(|| format!("{label}语音必须是字符串。"))?
        .trim();
    if voice.is_empty() {
        return Ok(None);
    }
    if voice.chars().count() > 64 {
        return Err(format!("{label}语音标识不能超过 64 个字符。"));
    }
    Ok(Some(voice.to_string()))
}

fn json_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn safe_stem(value: &str, fallback: &str) -> String {
    let cleaned = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    let cleaned = cleaned.trim_matches(['-', '.']);
    if cleaned.is_empty() {
        fallback.to_string()
    } else {
        cleaned.chars().take(100).collect()
    }
}

fn parse_json_object(raw: &str) -> Option<Value> {
    let trimmed = raw.trim();
    let without_fence = if trimmed.starts_with("```") {
        trimmed
            .trim_start_matches("```json")
            .trim_start_matches("```JSON")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim()
    } else {
        trimmed
    };
    if let Ok(value) = serde_json::from_str::<Value>(without_fence) {
        if value.is_object() {
            return Some(value);
        }
    }
    let start = without_fence.find('{')?;
    let end = without_fence.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str::<Value>(&without_fence[start..=end])
        .ok()
        .filter(Value::is_object)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_requires_alternating_speakers_and_valid_citations() {
        let ids = ["S1"].into_iter().collect::<BTreeSet<_>>();
        let valid = json!({
            "title": "概览",
            "segments": [
                {"speaker": "host", "text": "欢迎", "source_ids": []},
                {"speaker": "expert", "text": "解释", "source_ids": ["S1", "S1"]}
            ]
        });
        let script = parse_script(&valid.to_string(), &ids).expect("脚本有效");
        assert_eq!(script.segments[1].source_ids, vec!["S1"]);

        let invalid_role = json!({
            "title": "概览",
            "segments": [
                {"speaker": "expert", "text": "错误", "source_ids": []},
                {"speaker": "host", "text": "错误", "source_ids": []}
            ]
        });
        assert!(parse_script(&invalid_role.to_string(), &ids).is_err());

        let invalid_citation = json!({
            "title": "概览",
            "segments": [
                {"speaker": "host", "text": "欢迎", "source_ids": []},
                {"speaker": "expert", "text": "解释", "source_ids": ["S9"]}
            ]
        });
        assert!(parse_script(&invalid_citation.to_string(), &ids).is_err());
    }

    #[test]
    fn transcript_preserves_source_references() {
        let script = AudioScript {
            title: "测试概览".into(),
            segments: vec![
                ScriptSegment {
                    speaker: "host".into(),
                    text: "开场".into(),
                    source_ids: vec![],
                },
                ScriptSegment {
                    speaker: "expert".into(),
                    text: "结论".into(),
                    source_ids: vec!["S1".into()],
                },
            ],
        };
        let citations = vec![Citation {
            source_id: "S1".into(),
            title: "教材.pdf".into(),
            locator: "第 2 页".into(),
            snippet: "原文".into(),
        }];
        let transcript = build_transcript(&script, &citations, "zh");
        assert!(transcript.contains("[S1] 教材.pdf"));
        assert!(transcript.contains("结论 [S1]"));
    }

    #[test]
    fn file_stem_cannot_escape_output_directory() {
        assert_eq!(safe_stem("../../turn-1", "fallback"), "turn-1");
        assert_eq!(safe_stem("../..", "fallback"), "fallback");
    }
}
