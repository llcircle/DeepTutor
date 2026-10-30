use super::catalog::service_selection;
use super::llm::{self, LlmChunk};
use super::AppState;
use parking_lot::Mutex;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const HISTORY_TURNS: usize = 4;
const MAX_MESSAGE_CHARS: usize = 700;
const CACHE_LIMIT: usize = 256;
const CACHE_TTL_SECONDS: f64 = 30.0 * 60.0;
const LLM_TIMEOUT_SECONDS: u64 = 12;

#[derive(Clone)]
struct CachedHint {
    transcript_length: usize,
    generated_at: f64,
    hint: String,
}

/// 进程内、按会话尾部消息数失效的输入提示缓存。
pub struct HintCache {
    entries: Mutex<HashMap<String, CachedHint>>,
}

impl HintCache {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    fn get(&self, session_id: &str, transcript_length: usize) -> Option<CachedHint> {
        let now = crate::storage::now();
        let mut entries = self.entries.lock();
        if entries
            .get(session_id)
            .is_some_and(|entry| now - entry.generated_at >= CACHE_TTL_SECONDS)
        {
            entries.remove(session_id);
        }
        entries
            .get(session_id)
            .filter(|entry| entry.transcript_length == transcript_length)
            .cloned()
    }

    fn put(&self, session_id: String, hint: CachedHint) {
        let mut entries = self.entries.lock();
        if entries.len() >= CACHE_LIMIT && !entries.contains_key(&session_id) {
            let now = crate::storage::now();
            entries.retain(|_, entry| now - entry.generated_at < CACHE_TTL_SECONDS);
            if entries.len() >= CACHE_LIMIT {
                if let Some(oldest) = entries
                    .iter()
                    .min_by(|left, right| left.1.generated_at.total_cmp(&right.1.generated_at))
                    .map(|(key, _)| key.clone())
                {
                    entries.remove(&oldest);
                }
            }
        }
        entries.insert(session_id, hint);
    }
}

struct Material {
    transcript: Vec<(String, String)>,
    transcript_length: usize,
}

impl Material {
    fn last_user_message(&self) -> &str {
        self.transcript
            .iter()
            .rev()
            .find(|(role, _)| role == "user")
            .map(|(_, content)| content.as_str())
            .unwrap_or("")
    }
}

pub async fn get_ask_hint(state: &AppState, session_id: &str) -> Result<Value, String> {
    let generated_at = crate::storage::now();
    let material = collect_material(state, session_id)?;
    if material.transcript.is_empty() {
        return Ok(empty_hint(session_id, generated_at));
    }
    if let Some(cached) = state.hint_cache.get(session_id, material.transcript_length) {
        return Ok(json!({
            "hint": cached.hint,
            "session_id": session_id,
            "generated_at": cached.generated_at
        }));
    }

    let language = response_language(state, session_id);
    let chinese = is_chinese(&language);
    let settings = resolve_task_llm(state)?;
    let messages = vec![
        json!({
            "role": "system",
            "content": if chinese { SYSTEM_ZH } else { SYSTEM_EN }
        }),
        json!({
            "role": "user",
            "content": render_material(&material, chinese)
        }),
    ];
    let cancel = CancellationToken::new();
    let response = tokio::time::timeout(
        Duration::from_secs(LLM_TIMEOUT_SECONDS),
        llm::stream_completion(
            &state.client,
            &settings,
            &messages,
            &[],
            None,
            &cancel,
            |_chunk: LlmChunk| {},
        ),
    )
    .await;
    let raw = match response {
        Ok(Ok(response)) => response.content,
        _ => return Ok(empty_hint(session_id, generated_at)),
    };
    let hint = sanitize_hint(&raw, chinese, material.last_user_message());
    if hint.is_empty() {
        return Ok(empty_hint(session_id, generated_at));
    }
    let generated_at = crate::storage::now();
    state.hint_cache.put(
        session_id.to_string(),
        CachedHint {
            transcript_length: material.transcript_length,
            generated_at,
            hint: hint.clone(),
        },
    );
    Ok(json!({
        "hint": hint,
        "session_id": session_id,
        "generated_at": generated_at
    }))
}

fn collect_material(state: &AppState, session_id: &str) -> Result<Material, String> {
    if session_id.trim().is_empty() {
        return Ok(Material {
            transcript: Vec::new(),
            transcript_length: 0,
        });
    }
    // 会话不存在或数据库暂不可读时，输入提示应静默降级为空字符串。
    let Ok(Some(_session)) = state.database.get_session_meta(session_id) else {
        return Ok(Material {
            transcript: Vec::new(),
            transcript_length: 0,
        });
    };
    let messages = state.database.get_messages(session_id).unwrap_or_default();
    let filtered = messages
        .into_iter()
        .filter(|message| {
            matches!(
                message.get("role").and_then(Value::as_str),
                Some("user" | "assistant")
            )
        })
        .collect::<Vec<_>>();
    let transcript_length = filtered.len();
    let mut transcript = filtered
        .iter()
        .rev()
        .take(HISTORY_TURNS)
        .filter_map(|message| {
            let role = message.get("role")?.as_str()?.to_string();
            let content = message.get("content")?.as_str()?;
            let normalized = content.split_whitespace().collect::<Vec<_>>().join(" ");
            if normalized.is_empty() {
                return None;
            }
            Some((
                role,
                normalized
                    .chars()
                    .take(MAX_MESSAGE_CHARS)
                    .collect::<String>(),
            ))
        })
        .collect::<Vec<_>>();
    transcript.reverse();
    Ok(Material {
        transcript,
        transcript_length,
    })
}

fn resolve_task_llm(state: &AppState) -> Result<llm::LlmSettings, String> {
    let catalog = state
        .database
        .get_setting("catalog")?
        .unwrap_or_else(|| json!({}));
    if let Some((profile, Some(model))) = service_selection(&catalog, "task", None, None) {
        return super::provider_probe::llm_settings(&profile, &model);
    }
    super::engine::resolve_llm_settings(state, &json!({}))
}

fn response_language(state: &AppState, session_id: &str) -> String {
    if let Ok(Some(session)) = state.database.get_session_meta(session_id) {
        if let Some(language) = session
            .pointer("/preferences/reply_language_override")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty() && *value != "auto")
        {
            return language.to_string();
        }
    }
    state
        .database
        .get_setting("ui")
        .ok()
        .flatten()
        .and_then(|ui| {
            ui.get("response_language")
                .or_else(|| ui.get("language"))
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty() && *value != "auto")
                .map(str::to_string)
        })
        .unwrap_or_else(|| "en".to_string())
}

fn render_material(material: &Material, chinese: bool) -> String {
    let mut body = String::new();
    for (role, content) in &material.transcript {
        let speaker = match (chinese, role.as_str()) {
            (true, "user") => "用户",
            (true, "assistant") => "助手",
            (false, "user") => "User",
            _ => "Assistant",
        };
        body.push_str(&format!("[{speaker}] {content}\n"));
    }
    format!(
        "{}{}\n\n{}",
        if chinese {
            "# 对话结尾\n"
        } else {
            "# End of the conversation\n"
        },
        body.trim_end(),
        if chinese {
            "请写出用户接下来最可能说的那一句。"
        } else {
            "Write that single next line."
        }
    )
}

fn sanitize_hint(raw: &str, chinese: bool, last_user_message: &str) -> String {
    let lines = raw
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if lines.len() != 1 {
        return String::new();
    }
    let mut text = lines[0].trim().to_string();
    if text.starts_with("```") || text.ends_with("```") {
        return String::new();
    }
    text = text
        .trim_start_matches(['-', '•', '*', '#', ' '])
        .trim()
        .to_string();
    if text.chars().count() >= 2 {
        let first = text.chars().next().unwrap_or_default();
        let last = text.chars().last().unwrap_or_default();
        if matches!(
            (first, last),
            ('"', '"') | ('\'', '\'') | ('“', '”') | ('‘', '’') | ('「', '」')
        ) {
            text = text
                .chars()
                .skip(1)
                .take(text.chars().count().saturating_sub(2))
                .collect::<String>()
                .trim()
                .to_string();
        }
    }
    text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() || text.chars().count() > if chinese { 44 } else { 110 } {
        return String::new();
    }
    let meta = if chinese {
        static ZH: OnceLock<Regex> = OnceLock::new();
        ZH.get_or_init(|| {
            Regex::new(r"你可以|你能试着|你应该|不妨问|建议问|可以这样问")
                .expect("固定中文元话语正则")
        })
    } else {
        static EN: OnceLock<Regex> = OnceLock::new();
        EN.get_or_init(|| {
            Regex::new(
                r"(?i)(?:\byou (?:can|could|should)\b|\btry (?:asking|saying)\b|\bconsider asking\b|\bthe user (?:can|could|should)\b)",
            )
            .expect("固定英文元话语正则")
        })
    };
    let voice = if chinese {
        static ZH: OnceLock<Regex> = OnceLock::new();
        ZH.get_or_init(|| {
            Regex::new(r"^(?:好的|当然|没问题|作为(?:一个)?(?:人工智能|AI))")
                .expect("固定中文助手口吻正则")
        })
    } else {
        static EN: OnceLock<Regex> = OnceLock::new();
        EN.get_or_init(|| {
            Regex::new(
                r"(?i)^(?:sure|certainly|of course|here(?:'|’)s|here is|as an ai|i(?:'|’)d be happy)\b",
            )
            .expect("固定英文助手口吻正则")
        })
    };
    if meta.is_match(&text)
        || voice.is_match(&text)
        || echoes_last_message(&text, last_user_message)
    {
        return String::new();
    }
    text
}

fn echoes_last_message(hint: &str, message: &str) -> bool {
    if message.is_empty() {
        return false;
    }
    let hint = normal_form(hint);
    let message = normal_form(message);
    let hint_len = hint.chars().count();
    let message_len = message.chars().count();
    if hint_len < 6 {
        return false;
    }
    if hint.contains(&message) || message.contains(&hint) {
        return true;
    }
    // 中文输入提示常见“请解释傅里叶变换”与“请解释一下傅里叶变换”这类
    // 只多一个口语填充词的复述，不能只靠子串判断。
    let common = longest_common_subsequence(&hint, &message);
    common as f64 / hint_len.min(message_len).max(1) as f64 >= 0.8
}

fn normal_form(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn longest_common_subsequence(left: &str, right: &str) -> usize {
    let left = left.chars().collect::<Vec<_>>();
    let right = right.chars().collect::<Vec<_>>();
    if left.is_empty() || right.is_empty() {
        return 0;
    }
    let mut previous = vec![0usize; right.len() + 1];
    let mut current = vec![0usize; right.len() + 1];
    for left_char in left {
        for (index, right_char) in right.iter().enumerate() {
            current[index + 1] = if left_char == *right_char {
                previous[index] + 1
            } else {
                previous[index + 1].max(current[index])
            };
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}

fn is_chinese(language: &str) -> bool {
    let lower = language.to_ascii_lowercase();
    lower.starts_with("zh")
        || lower.contains("chinese")
        || language.contains("中文")
        || language.contains("简体")
}

fn empty_hint(session_id: &str, generated_at: f64) -> Value {
    json!({
        "hint": "",
        "session_id": session_id,
        "generated_at": generated_at
    })
}

const SYSTEM_EN: &str = "You predict ONE line the user is likely to type next into the chat box. Reply with that line alone: no quotes, prefix, markdown or commentary. Write in the user's first-person voice, under 14 words. Do not describe a question; write the actual line. Never answer as the assistant. Ground it in the assistant's latest message and do not introduce an unrelated subject.";

const SYSTEM_ZH: &str = "你要预测用户接下来最可能在聊天框里打出的一句话。只回复那一句话本身：不要引号、前缀、markdown 或补充说明。使用用户第一人称口吻，不超过 25 个字。不要描述一个问题，而要直接写出那句话。绝不能使用助手口吻回答。要接得上助手刚说的内容，不要引入无关话题。";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 输入提示只保留单行有效内容() {
        assert_eq!(
            sanitize_hint("“换一种更简洁的说法”", true, ""),
            "换一种更简洁的说法"
        );
        assert_eq!(sanitize_hint("你可以问问重试上限", true, ""), "");
        assert_eq!(sanitize_hint("当然可以。", true, ""), "");
        assert_eq!(sanitize_hint("第一行\n第二行", true, ""), "");
    }

    #[test]
    fn 输入提示不会回显用户上一句() {
        assert_eq!(
            sanitize_hint("请解释傅里叶变换", true, "请解释一下傅里叶变换"),
            ""
        );
    }
}
