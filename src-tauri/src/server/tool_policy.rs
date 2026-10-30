use super::{deferred, mastery, memory, notebook, plugins, question_bank, skills, AppState};
use regex::Regex;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::OnceLock;

const USER_TOGGLEABLE: &[&str] = &[
    "brainstorm",
    "web_search",
    "paper_search",
    "zotero_search",
    "reason",
    "geogebra_analysis",
    "imagegen",
    "videogen",
];

const WORKSPACE_BASELINE: &[&str] = &[
    "workspace_list",
    "workspace_read",
    "workspace_search",
    "workspace_present",
    "write_note",
    "search_notes",
];

#[derive(Clone, Debug)]
pub struct ToolSurface {
    pub names: Vec<String>,
    pub deferred_definitions: Vec<Value>,
}

/// 复刻 Python 版工具组合规则：用户开关、上下文自动挂载和能力自带工具分层合并。
pub fn compose(
    state: &AppState,
    request: &Value,
    capability: &str,
    user_enabled: &[String],
    has_sources: bool,
) -> Result<ToolSurface, String> {
    let mut names = WORKSPACE_BASELINE
        .iter()
        .map(|name| (*name).to_string())
        .collect::<Vec<_>>();
    let whitelist = USER_TOGGLEABLE.iter().copied().collect::<HashSet<_>>();
    for name in user_enabled {
        if whitelist.contains(name.as_str()) {
            names.push(name.clone());
        }
    }

    let has_kb = request
        .get("knowledge_bases")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .any(|item| item.as_str().is_some_and(|v| !v.trim().is_empty()))
        });
    let has_memory = memory::has_content(state);
    let has_notebooks = notebook::has_notebooks(state)?;
    let has_question_bank = question_bank::has_entries(state)?;
    let has_skills = skills::has_skills(state)?;
    let has_deferred = deferred::has_available(state)?;
    let has_mastery = mastery::has_topics(state)?;
    let scientific = capability == "deep_research"
        || user_enabled.iter().any(|name| name == "paper_search")
        || scientific_request(request);

    if scientific {
        names.extend(
            ["preprint", "research_audit", "research_lit"]
                .into_iter()
                .map(str::to_string),
        );
    }
    if has_kb {
        names.extend(
            ["rag", "kb_files", "knowledge_frontier"]
                .into_iter()
                .map(str::to_string),
        );
    }
    if has_sources {
        names.push("read_source".to_string());
    }
    if has_memory {
        names.push("read_memory".to_string());
    }
    if has_notebooks {
        names.extend(
            ["list_favorites", "save_favorite"]
                .into_iter()
                .map(str::to_string),
        );
    }
    if has_question_bank
        || request
            .get("partner_group")
            .and_then(Value::as_object)
            .is_some()
    {
        names.push("question_bank".to_string());
    }
    if has_skills {
        names.push("read_skill".to_string());
    }
    if has_deferred {
        names.push("load_tools".to_string());
    }
    if exec_enabled(state, request) {
        names.push("exec".to_string());
    }
    if has_mastery {
        if capability != "mastery_path" {
            names.push("mastery_topics".to_string());
        }
        names.extend(
            [
                "mastery_sessions",
                "mastery_open_session",
                "mastery_new_session",
            ]
            .into_iter()
            .map(str::to_string),
        );
    }

    if capability == "deep_solve" {
        names.extend(
            ["solve_plan", "solve_finish_step"]
                .into_iter()
                .map(str::to_string),
        );
    }
    if capability == "deep_research" {
        names.extend(
            ["web_search", "paper_search"]
                .into_iter()
                .map(str::to_string),
        );
    }
    names.extend(
        [
            "write_memory",
            "learning_status",
            "learning_update",
            "web_fetch",
            "github",
            "ask_user",
        ]
        .into_iter()
        .map(str::to_string),
    );
    if !request
        .get("cron_trigger")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        names.push("cron".to_string());
    }
    if request
        .pointer("/partner_group/allow_invoke_other")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        names.push("invoke_other".to_string());
    }

    // 已批准的插件 Tool 与当前会话装入的延迟工具共同进入模型工具表。
    let plugin_tools = plugins::plugin_tool_definitions(state)?;
    let session_tools = deferred::loaded_definitions(state, request_session_id(request))?;
    for definition in plugin_tools.iter().chain(&session_tools) {
        if let Some(name) = definition.pointer("/function/name").and_then(Value::as_str) {
            names.push(name.to_string());
        }
    }
    let mut deferred_definitions = plugin_tools;
    deferred_definitions.extend(session_tools);

    Ok(ToolSurface {
        names: ordered_unique(names),
        deferred_definitions,
    })
}

pub fn available_deferred(state: &AppState) -> Result<Vec<Value>, String> {
    Ok(state
        .database
        .list_resources("mcp_tool_schema")?
        .into_iter()
        .chain(state.database.list_resources("prompt_tool_schema")?)
        .collect())
}

fn exec_enabled(state: &AppState, request: &Value) -> bool {
    if let Some(value) = request.get("exec_enabled").and_then(Value::as_bool) {
        return value;
    }
    state
        .database
        .get_setting("sandbox_allow_subprocess")
        .ok()
        .flatten()
        .and_then(|value| value.as_bool())
        // 便携客户端默认不提供执行面，用户必须显式开启。
        .unwrap_or(false)
}

fn scientific_request(request: &Value) -> bool {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    let pattern = PATTERN.get_or_init(|| {
        Regex::new(
            r"(?i)arxiv|alphaxiv|literature review|paper.{0,40}(audit|code|reproduc)|论文|文献综述|复现",
        )
        .expect("科研上下文正则有效")
    });
    let message = request.get("content").and_then(Value::as_str).unwrap_or("");
    pattern.is_match(message)
}

fn request_session_id(request: &Value) -> &str {
    request
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or("")
}

fn ordered_unique(names: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    names
        .into_iter()
        .filter(|name| !name.trim().is_empty() && seen.insert(name.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordered_unique_preserves_canonical_order() {
        let values = ordered_unique(vec![
            "ask_user".into(),
            "workspace_read".into(),
            "ask_user".into(),
        ]);
        assert_eq!(values, vec!["ask_user", "workspace_read"]);
    }

    #[test]
    fn scientific_regex_matches_chinese_queries() {
        assert!(scientific_request(&serde_json::json!({
            "content": "请帮我做这篇论文的复现审计"
        })));
    }
}
