use crate::server::AppState;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const JOURNAL_VERSION: u64 = 1;
const MAX_RECORDS: usize = 50;
const MAX_TOPIC_CHARS: usize = 200;
const MAX_WHY_CHARS: usize = 800;
const MAX_LEVEL_CHARS: usize = 40;
const MAX_SUMMARY_CHARS: usize = 800;
const MAX_NEXT_FOCUS_CHARS: usize = 400;
const MAX_TITLE_CHARS: usize = 120;
const MAX_INSIGHT_CHARS: usize = 800;

static WRITE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub fn journal_path(state: &AppState) -> PathBuf {
    state
        .paths
        .data
        .join("learning_journal")
        .join("journal.json")
}

pub fn empty_journal() -> Value {
    json!({
        "version": JOURNAL_VERSION,
        "updated_at": "",
        "mission": {
            "topic": "",
            "why": "",
            "level": "",
            "updated_at": ""
        },
        "last_session": {
            "summary": "",
            "next_focus": "",
            "updated_at": ""
        },
        "records": []
    })
}

pub fn load(state: &AppState) -> Result<Value, String> {
    load_path(&journal_path(state))
}

fn load_path(path: &Path) -> Result<Value, String> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(empty_journal()),
        Err(error) => return Err(format!("无法读取学习日志：{error}")),
    };
    let value = serde_json::from_str::<Value>(&raw)
        .map_err(|error| format!("学习日志不是有效 JSON，原文件已保留：{error}"))?;
    validate(&value)?;
    Ok(value)
}

fn validate(value: &Value) -> Result<(), String> {
    if value.get("version").and_then(Value::as_u64) != Some(JOURNAL_VERSION) {
        return Err("学习日志版本不受支持，原文件已保留。".to_string());
    }
    for pointer in ["/mission", "/last_session"] {
        let object = value
            .pointer(pointer)
            .and_then(Value::as_object)
            .ok_or_else(|| format!("学习日志字段 {pointer} 无效，原文件已保留。"))?;
        if object.values().any(|item| !item.is_string()) {
            return Err(format!(
                "学习日志字段 {pointer} 含非字符串值，原文件已保留。"
            ));
        }
    }
    let records = value
        .get("records")
        .and_then(Value::as_array)
        .ok_or_else(|| "学习日志 records 必须是数组，原文件已保留。".to_string())?;
    for record in records {
        let object = record
            .as_object()
            .ok_or_else(|| "学习日志记录无效，原文件已保留。".to_string())?;
        if !string_field(object.get("id"))
            || !(string_field(object.get("title")) || string_field(object.get("insight")))
        {
            return Err("学习日志记录缺少有效 id/title/insight，原文件已保留。".to_string());
        }
    }
    Ok(())
}

fn string_field(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|text| !text.trim().is_empty())
}

pub fn overview(state: &AppState) -> Result<Value, String> {
    let journal = load(state)?;
    let mut snapshot = journal.clone();
    if let Some(records) = journal.get("records").and_then(Value::as_array) {
        snapshot["records"] = Value::Array(records.iter().rev().cloned().collect());
    }
    snapshot["injected_context"] = Value::String(injection_markdown(&journal));
    Ok(snapshot)
}

pub fn injection_markdown(journal: &Value) -> String {
    let mut parts = Vec::<String>::new();
    let mission_lines = mission_lines(journal);
    if !mission_lines.is_empty() {
        parts.push(format!("## Mission\n{}", mission_lines.join("\n")));
    }
    let session_lines = session_lines(journal);
    if !session_lines.is_empty() {
        parts.push(format!("## Last session\n{}", session_lines.join("\n")));
    }
    if parts.is_empty() {
        return String::new();
    }
    format!(
        "# Learning journal (carried over from earlier sessions)\n\n{}\n\n_Pick up from where this leaves off, and record what this session changes with `learning_update`._\n",
        parts.join("\n\n")
    )
}

pub fn status_markdown(state: &AppState) -> Result<String, String> {
    let journal = load(state)?;
    if journal_is_empty(&journal) {
        return Ok(
            "(No learning journal yet. When the user wants to learn a topic across sessions, call learning_update with op=set_mission, then note_session / add_record as you teach. Prefer this soft journal for free-form multi-session study; use Mastery Path tools for hard curriculum gates.)\n"
                .to_string(),
        );
    }

    let mut lines = vec!["# Learning journal".to_string()];
    let mission = mission_lines(&journal);
    if !mission.is_empty() {
        lines.extend(["".to_string(), "## Mission".to_string(), mission.join("\n")]);
    }
    let session = session_lines(&journal);
    if !session.is_empty() {
        lines.extend([
            "".to_string(),
            "## Last session".to_string(),
            session.join("\n"),
        ]);
    }
    if let Some(records) = journal
        .get("records")
        .and_then(Value::as_array)
        .filter(|records| !records.is_empty())
    {
        lines.extend(["".to_string(), "## Recent learning records".to_string()]);
        for record in records.iter().rev().take(8) {
            let id = record.get("id").and_then(Value::as_str).unwrap_or("");
            let title = record
                .get("title")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .unwrap_or("(untitled)");
            let insight = record.get("insight").and_then(Value::as_str).unwrap_or("");
            lines.push(format!("- `{id}` **{title}**: {insight}"));
        }
    }
    lines.extend([
        "".to_string(),
        "_Use Mastery Path (`mastery_status` / Guided Learning) for scored curriculum progress; this journal is the soft mission + insight layer._".to_string(),
    ]);
    Ok(format!("{}\n", lines.join("\n")))
}

#[derive(Debug)]
pub struct UpdateResult {
    pub accepted: bool,
    pub message: String,
    pub record_id: Option<String>,
    pub deduplicated: bool,
}

pub fn set_mission(
    state: &AppState,
    topic: &str,
    why: &str,
    level: &str,
) -> Result<UpdateResult, String> {
    let topic = clip(topic, MAX_TOPIC_CHARS);
    if topic.is_empty() {
        return Ok(rejected("set_mission requires a non-empty topic."));
    }
    let why = clip(why, MAX_WHY_CHARS);
    let level = clip(level, MAX_LEVEL_CHARS);
    mutate(state, move |journal, timestamp| {
        journal["mission"] = json!({
            "topic": topic,
            "why": why,
            "level": level,
            "updated_at": timestamp
        });
        Ok(accepted(format!(
            "mission set for topic={:?}.",
            journal["mission"]["topic"].as_str().unwrap_or("")
        )))
    })
}

pub fn note_session(
    state: &AppState,
    summary: &str,
    next_focus: &str,
) -> Result<UpdateResult, String> {
    let summary = clip(summary, MAX_SUMMARY_CHARS);
    let next_focus = clip(next_focus, MAX_NEXT_FOCUS_CHARS);
    if summary.is_empty() && next_focus.is_empty() {
        return Ok(rejected("note_session requires summary and/or next_focus."));
    }
    mutate(state, move |journal, timestamp| {
        journal["last_session"] = json!({
            "summary": summary,
            "next_focus": next_focus,
            "updated_at": timestamp
        });
        Ok(accepted("session note saved."))
    })
}

pub fn add_record(state: &AppState, title: &str, insight: &str) -> Result<UpdateResult, String> {
    let mut title = clip(title, MAX_TITLE_CHARS);
    let insight = clip(insight, MAX_INSIGHT_CHARS);
    if insight.is_empty() {
        return Ok(rejected("add_record requires a non-empty insight."));
    }
    if title.is_empty() {
        title = clip(&insight, 60);
    }
    mutate(state, move |journal, timestamp| {
        let key = normalize_key(&title, &insight);
        let records = journal["records"]
            .as_array_mut()
            .ok_or_else(|| "学习日志 records 结构无效".to_string())?;
        if let Some(existing) = records.iter().find(|record| {
            normalize_key(
                record.get("title").and_then(Value::as_str).unwrap_or(""),
                record.get("insight").and_then(Value::as_str).unwrap_or(""),
            ) == key
        }) {
            let id = existing
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            return Ok(UpdateResult {
                accepted: true,
                message: format!("record already saved (id={id}); skipped duplicate."),
                record_id: Some(id),
                deduplicated: true,
            });
        }

        let next = records
            .iter()
            .filter_map(|record| {
                record
                    .get("id")
                    .and_then(Value::as_str)
                    .and_then(|id| id.strip_prefix("lr_"))
                    .and_then(|suffix| suffix.parse::<u64>().ok())
            })
            .max()
            .unwrap_or(0)
            + 1;
        let record_id = format!("lr_{next:04}");
        records.push(json!({
            "id": record_id,
            "title": title,
            "insight": insight,
            "created_at": timestamp
        }));
        if records.len() > MAX_RECORDS {
            records.drain(0..records.len() - MAX_RECORDS);
        }
        Ok(UpdateResult {
            accepted: true,
            message: format!("record added (id={record_id})."),
            record_id: Some(record_id),
            deduplicated: false,
        })
    })
}

fn mutate<F>(state: &AppState, mutator: F) -> Result<UpdateResult, String>
where
    F: FnOnce(&mut Value, &str) -> Result<UpdateResult, String>,
{
    let lock = WRITE_LOCK.get_or_init(|| Mutex::new(()));
    let _guard = lock.lock();
    let path = journal_path(state);
    let mut journal = load_path(&path)?;
    let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    journal["updated_at"] = Value::String(timestamp.clone());
    let result = mutator(&mut journal, &timestamp)?;
    if result.accepted && !result.deduplicated {
        atomic_write_json(&path, &journal)?;
    }
    Ok(result)
}

fn atomic_write_json(path: &Path, value: &Value) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "学习日志路径缺少父目录".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("无法创建学习日志目录 {}：{error}", parent.display()))?;
    let temporary = parent.join(format!(".journal-{}.tmp", uuid::Uuid::new_v4()));
    let bytes =
        serde_json::to_vec_pretty(value).map_err(|error| format!("无法序列化学习日志：{error}"))?;
    fs::write(&temporary, bytes).map_err(|error| format!("无法写入学习日志临时文件：{error}"))?;
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(format!("无法原子替换学习日志：{error}"));
    }
    Ok(())
}

fn mission_lines(journal: &Value) -> Vec<String> {
    let mission = &journal["mission"];
    let mut lines = Vec::new();
    for (key, label) in [
        ("topic", "Topic"),
        ("why", "Why"),
        ("level", "Level"),
        ("updated_at", "Updated"),
    ] {
        if let Some(value) = mission
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            lines.push(format!("- {label}: {value}"));
        }
    }
    lines
}

fn session_lines(journal: &Value) -> Vec<String> {
    let session = &journal["last_session"];
    let mut lines = Vec::new();
    for (key, label) in [
        ("summary", "Summary"),
        ("next_focus", "Next focus"),
        ("updated_at", "Updated"),
    ] {
        if let Some(value) = session
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            lines.push(format!("- {label}: {value}"));
        }
    }
    lines
}

fn journal_is_empty(journal: &Value) -> bool {
    mission_lines(journal).is_empty()
        && session_lines(journal).is_empty()
        && journal
            .get("records")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
}

fn clip(value: &str, limit: usize) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= limit {
        return normalized;
    }
    let mut clipped = normalized
        .chars()
        .take(limit.saturating_sub(1))
        .collect::<String>();
    clipped.push('…');
    clipped
}

fn normalize_key(title: &str, insight: &str) -> String {
    format!(
        "{} {}",
        title.split_whitespace().collect::<Vec<_>>().join(" "),
        insight.split_whitespace().collect::<Vec<_>>().join(" ")
    )
    .to_lowercase()
}

fn accepted(message: impl Into<String>) -> UpdateResult {
    UpdateResult {
        accepted: true,
        message: message.into(),
        record_id: None,
        deduplicated: false,
    }
}

fn rejected(message: impl Into<String>) -> UpdateResult {
    UpdateResult {
        accepted: false,
        message: message.into(),
        record_id: None,
        deduplicated: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::portable::PortablePaths;
    use crate::storage::Database;

    fn test_state(root: &Path) -> AppState {
        let paths = PortablePaths {
            home: root.join("home"),
            data: root.join("data"),
            cache: root.join("cache"),
            temp: root.join("temp"),
            logs: root.join("logs"),
            webview: root.join("cache/webview2"),
            uploads: root.join("data/uploads"),
            outputs: root.join("data/outputs"),
            database: root.join("data/deeptutor.sqlite3"),
            root: root.to_path_buf(),
        };
        paths.prepare().expect("便携目录可创建");
        AppState::new(Database::new(&paths.database), paths).expect("状态可创建")
    }

    #[test]
    fn journal_updates_are_atomic_and_deduplicated() {
        let root =
            std::env::temp_dir().join(format!("deeptutor-journal-test-{}", uuid::Uuid::new_v4()));
        let state = test_state(&root);
        set_mission(&state, "  线性代数 ", " 理解矩阵 ", " beginner ").expect("任务可写入");
        let first = add_record(&state, "特征值", "特征向量方向不变").expect("记录可写入");
        let duplicate = add_record(&state, "特征值", "特征向量方向不变").expect("重复记录可检查");
        assert!(first.accepted);
        assert!(duplicate.deduplicated);
        let snapshot = overview(&state).expect("日志可读取");
        assert_eq!(snapshot["mission"]["topic"], "线性代数");
        assert_eq!(
            snapshot.pointer("/records/0/id").and_then(Value::as_str),
            Some("lr_0001")
        );
        assert!(snapshot["injected_context"]
            .as_str()
            .is_some_and(|text| text.contains("Last session") || text.contains("Mission")));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn corrupt_journal_is_preserved() {
        let root = std::env::temp_dir().join(format!(
            "deeptutor-journal-corrupt-test-{}",
            uuid::Uuid::new_v4()
        ));
        let state = test_state(&root);
        let path = journal_path(&state);
        std::fs::create_dir_all(path.parent().expect("父目录")).expect("目录可创建");
        std::fs::write(&path, b"{not-json").expect("损坏文件可写入");
        assert!(load(&state).is_err());
        assert_eq!(std::fs::read(&path).expect("原文件仍存在"), b"{not-json");
        let _ = std::fs::remove_dir_all(root);
    }
}
