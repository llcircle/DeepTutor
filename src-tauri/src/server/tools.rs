use super::llm::{self, LlmChunk, LlmSettings};
use super::{catalog, files, media_services, plugins, AppState};
use base64::Engine;
use regex::Regex;
use reqwest::redirect::Policy;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use walkdir::WalkDir;

pub struct ToolRuntime<'a> {
    pub state: &'a AppState,
    pub request: Option<&'a Value>,
    pub llm: Option<&'a LlmSettings>,
    pub cancel: Option<&'a CancellationToken>,
    pub turn_id: &'a str,
    pub session_id: &'a str,
    pub language: &'a str,
    pub source_index: Option<&'a HashMap<String, String>>,
}

#[derive(Clone, Debug)]
pub struct ToolOutcome {
    pub content: String,
    pub metadata: Value,
    pub success: bool,
    pub terminate_turn: bool,
}

impl ToolOutcome {
    pub(super) fn success(content: impl Into<String>, metadata: Value) -> Self {
        Self {
            content: content.into(),
            metadata,
            success: true,
            terminate_turn: false,
        }
    }

    pub(super) fn failure(message: impl Into<String>) -> Self {
        Self {
            content: message.into(),
            metadata: json!({"kind": "tool_error"}),
            success: false,
            terminate_turn: false,
        }
    }
}

pub fn definitions(
    _capability: &str,
    enabled_tools: &[String],
    request: Option<&Value>,
    deferred_definitions: &[Value],
) -> Vec<Value> {
    let enabled = |name: &str| enabled_tools.iter().any(|tool| tool == name);
    let candidates = [
        ("ask_user", ask_user_definition as fn() -> Value),
        ("solve_plan", solve_plan_definition),
        ("solve_finish_step", solve_step_definition),
        ("web_search", web_search_definition),
        ("paper_search", paper_search_definition),
        ("zotero_search", zotero_search_definition),
        ("brainstorm", brainstorm_definition),
        ("reason", reason_definition),
        ("geogebra_analysis", geogebra_definition),
        ("imagegen", imagegen_definition),
        ("videogen", videogen_definition),
        ("rag", rag_definition),
        ("read_source", read_source_definition),
        ("read_memory", read_memory_definition),
        ("write_memory", write_memory_definition),
        ("learning_status", learning_status_definition),
        ("learning_update", learning_update_definition),
        ("workspace_list", workspace_list_definition),
        ("workspace_read", workspace_read_definition),
        ("workspace_search", workspace_search_definition),
        ("workspace_present", workspace_present_definition),
        ("write_note", write_note_definition),
        ("search_notes", search_notes_definition),
        ("list_notebook", list_notebook_definition),
        ("list_favorites", list_notebook_definition),
        ("save_favorite", save_favorite_definition),
        ("web_fetch", web_fetch_definition),
        ("github", github_definition),
        ("read_skill", read_skill_definition),
        ("question_bank", question_bank_definition),
        ("load_tools", load_tools_definition),
        ("exec", exec_definition),
        ("cron", cron_definition),
        ("preprint", preprint_definition),
        ("research_audit", research_audit_definition),
        ("research_lit", research_lit_definition),
        ("kb_files", kb_files_definition),
        ("knowledge_frontier", knowledge_frontier_definition),
        ("mastery_topics", mastery_topics_definition),
        ("mastery_sessions", mastery_sessions_definition),
        ("mastery_open_session", mastery_open_session_definition),
        ("mastery_new_session", mastery_new_session_definition),
        ("invoke_other", invoke_other_definition),
    ];
    let mut tools = Vec::new();
    let mut names = std::collections::HashSet::<String>::new();
    for (name, factory) in candidates {
        if enabled(name) && names.insert(name.to_string()) {
            let mut definition = factory();
            definition["function"]["name"] = Value::String(name.to_string());
            tools.push(definition);
        }
    }
    if request.is_some_and(super::reading::is_reading_turn) {
        for definition in super::reading::tool_definitions() {
            let Some(name) = definition
                .pointer("/function/name")
                .and_then(Value::as_str)
                .map(str::to_string)
            else {
                continue;
            };
            if enabled(&name) && names.insert(name) {
                tools.push(definition);
            }
        }
    }
    for definition in deferred_definitions {
        let Some(name) = definition
            .pointer("/function/name")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            continue;
        };
        if enabled(&name) && names.insert(name) {
            tools.push(definition.clone());
        }
    }
    tools
}

pub async fn execute(
    name: &str,
    arguments: &Value,
    state: &AppState,
    request: Option<&Value>,
) -> ToolOutcome {
    execute_with_runtime(
        name,
        arguments,
        ToolRuntime {
            state,
            request,
            llm: None,
            cancel: None,
            turn_id: "",
            session_id: "",
            language: "zh",
            source_index: None,
        },
    )
    .await
}

pub async fn execute_with_runtime(
    name: &str,
    arguments: &Value,
    runtime: ToolRuntime<'_>,
) -> ToolOutcome {
    let state = runtime.state;
    if let Some(outcome) = super::reading::execute_tool(name, arguments, &runtime).await {
        return outcome;
    }
    match name {
        "web_search" => web_search(arguments, state).await,
        "paper_search" => paper_search(arguments, state).await,
        "rag" => rag_search(arguments, state),
        "read_source" => read_source(arguments, &runtime),
        "read_file" => read_file(arguments, state),
        "memory_search" => memory_search(arguments, state),
        "read_memory" => read_memory(&runtime),
        "write_memory" => write_memory(arguments, &runtime),
        "learning_status" => learning_status(&runtime),
        "learning_update" => learning_update(arguments, &runtime),
        "zotero_search" => zotero_search(arguments, state).await,
        "brainstorm" => brainstorm(arguments, &runtime).await,
        "reason" => reason(arguments, &runtime).await,
        "geogebra_analysis" => geogebra_analysis(arguments, &runtime).await,
        "imagegen" => imagegen(arguments, &runtime).await,
        "videogen" => videogen(arguments, &runtime).await,
        "workspace_list" => workspace_list(arguments, &runtime),
        "workspace_read" => workspace_read(arguments, &runtime),
        "workspace_search" => workspace_search(arguments, &runtime),
        "workspace_present" => workspace_present(arguments, &runtime),
        "write_note" => write_note(arguments, &runtime),
        "search_notes" => search_notes(arguments, &runtime),
        "list_notebook" | "list_favorites" => list_notebook(arguments, &runtime),
        "save_favorite" => save_favorite(arguments, &runtime),
        "web_fetch" => web_fetch(arguments, state).await,
        "github" => github_query(arguments, state).await,
        "read_skill" => read_skill(arguments, state),
        "question_bank" => question_bank(arguments, &runtime),
        "load_tools" => load_tools(arguments, &runtime),
        "exec" => exec(arguments, &runtime).await,
        "cron" => cron(arguments, &runtime).await,
        "preprint" => preprint(arguments, &runtime).await,
        "research_audit" => research_audit(arguments, &runtime).await,
        "research_lit" => research_lit(arguments, &runtime).await,
        "kb_files" => kb_files(arguments, &runtime),
        "knowledge_frontier" => knowledge_frontier(arguments, &runtime).await,
        "mastery_topics" => mastery_topics(arguments, &runtime),
        "mastery_sessions" => mastery_sessions(arguments, &runtime),
        "mastery_open_session" => mastery_open_session(arguments, &runtime),
        "mastery_new_session" => mastery_new_session(arguments, &runtime),
        "solve_plan" => ToolOutcome::success(
            "解题计划已登记。请按步骤继续，在每一步完成后调用 solve_finish_step。",
            json!({"kind": "solve_plan", "arguments": arguments}),
        ),
        "solve_finish_step" => ToolOutcome::success(
            "步骤已完成。请继续下一步；全部完成后给出最终答案。",
            json!({"kind": "solve_step_finished", "arguments": arguments}),
        ),
        "invoke_other" => invoke_other(arguments, runtime.request),
        _ if name.starts_with("mcp_") => call_mcp_tool(name, arguments, &runtime).await,
        _ if name.starts_with("plugin_") => {
            match plugins::execute_plugin_tool(
                state,
                name,
                arguments,
                runtime.turn_id,
                runtime.session_id,
                runtime.language,
            )
            .await
            {
                Ok(Some(result)) => ToolOutcome {
                    content: result.content,
                    metadata: result.metadata,
                    success: result.success,
                    terminate_turn: result.terminate_turn,
                },
                Ok(None) => ToolOutcome::failure("插件工具未注册。"),
                Err(error) => ToolOutcome::failure(format!("插件工具执行失败：{error}")),
            }
        }
        _ => ToolOutcome::failure(format!("工具 `{name}` 在当前 Rust 回合中不可用。")),
    }
}

async fn exec(arguments: &Value, runtime: &ToolRuntime<'_>) -> ToolOutcome {
    match super::exec_tool::execute(runtime.state, runtime.turn_id, arguments).await {
        Ok(report) => {
            if report.success {
                ToolOutcome::success(report.content, report.metadata)
            } else {
                let mut outcome = ToolOutcome::failure(report.content);
                outcome.metadata = report.metadata;
                outcome
            }
        }
        Err(error) => ToolOutcome::failure(format!("Exec 未执行：{error}")),
    }
}

async fn cron(arguments: &Value, runtime: &ToolRuntime<'_>) -> ToolOutcome {
    match super::cron::handle(
        runtime.state,
        runtime.session_id,
        runtime.language,
        arguments,
    )
    .await
    {
        Ok((content, metadata)) => ToolOutcome::success(content, metadata),
        Err(error) => ToolOutcome::failure(format!("Cron 操作失败：{error}")),
    }
}

fn read_source(arguments: &Value, runtime: &ToolRuntime<'_>) -> ToolOutcome {
    const HISTORY_PAGE_CHARS: usize = 16_000;
    let Some(index) = runtime.source_index.filter(|index| !index.is_empty()) else {
        return ToolOutcome::failure("本轮没有可读取的附加来源。");
    };
    let mut source_id = string_arg(arguments, "source_id").trim().to_string();
    if source_id.is_empty() {
        if index.len() == 1 {
            source_id = index.keys().next().cloned().unwrap_or_default();
        } else {
            return ToolOutcome::failure("存在多个来源时必须提供来源清单中的 source_id。");
        }
    }
    let Some(full_text) = index.get(&source_id) else {
        let mut available = index.keys().cloned().collect::<Vec<_>>();
        available.sort();
        return ToolOutcome::failure(format!(
            "未知 source_id `{source_id}`。本轮有效来源：{}",
            available.join(", ")
        ));
    };
    let total_chars = full_text.chars().count();
    if !source_id.starts_with("hs-") {
        return ToolOutcome::success(
            full_text.clone(),
            json!({
                "kind": "source_read",
                "source_id": source_id,
                "char_count": total_chars
            }),
        );
    }
    let offset = arguments.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
    if offset > total_chars || (offset == total_chars && total_chars > 0) {
        return ToolOutcome::failure("offset 必须是来源文本范围内的非负字符偏移。");
    }
    let end = (offset + HISTORY_PAGE_CHARS).min(total_chars);
    let content = full_text
        .chars()
        .skip(offset)
        .take(end.saturating_sub(offset))
        .collect::<String>();
    let next_offset = (end < total_chars).then_some(end);
    ToolOutcome::success(
        serde_json::to_string(&json!({
            "archive_id": source_id,
            "offset": offset,
            "next_offset": next_offset,
            "total_chars": total_chars,
            "content": content
        }))
        .unwrap_or(content),
        json!({
            "kind": "source_read",
            "source_id": source_id,
            "char_count": total_chars,
            "offset": offset,
            "next_offset": next_offset
        }),
    )
}

fn read_memory(runtime: &ToolRuntime<'_>) -> ToolOutcome {
    match super::memory::l3_concat(runtime.state) {
        Ok(text) => {
            let char_count = text.chars().count();
            ToolOutcome::success(
                text,
                json!({"kind": "memory_read", "char_count": char_count}),
            )
        }
        Err(error) => ToolOutcome::failure(format!("读取长期记忆失败：{error}")),
    }
}

fn write_memory(arguments: &Value, runtime: &ToolRuntime<'_>) -> ToolOutcome {
    let op = string_arg(arguments, "op");
    let text = string_arg(arguments, "text");
    let target_id = optional_string_arg(arguments, "target_id");
    let reason = optional_string_arg(arguments, "reason");
    match super::memory::write_preference(
        runtime.state,
        &op,
        &text,
        target_id.as_deref(),
        reason.as_deref(),
    ) {
        Ok(report) => {
            let message = report
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("偏好已保存。")
                .to_string();
            ToolOutcome::success(
                message,
                json!({
                    "kind": "memory_write",
                    "op": op,
                    "entry_id": report.get("entry_id"),
                    "deduplicated": report.get("deduplicated").and_then(Value::as_bool).unwrap_or(false)
                }),
            )
        }
        Err(error) => ToolOutcome::failure(format!("写入偏好失败：{error}")),
    }
}

fn learning_status(runtime: &ToolRuntime<'_>) -> ToolOutcome {
    match super::learning_journal::status_markdown(runtime.state) {
        Ok(text) => {
            let char_count = text.chars().count();
            ToolOutcome::success(
                text,
                json!({"kind": "learning_status", "char_count": char_count}),
            )
        }
        Err(error) => ToolOutcome::failure(format!("学习日志无法读取，原数据已保留：{error}")),
    }
}

fn learning_update(arguments: &Value, runtime: &ToolRuntime<'_>) -> ToolOutcome {
    let op = string_arg(arguments, "op").trim().to_lowercase();
    let result = match op.as_str() {
        "set_mission" => super::learning_journal::set_mission(
            runtime.state,
            &string_arg(arguments, "topic"),
            &string_arg(arguments, "why"),
            &string_arg(arguments, "level"),
        ),
        "note_session" => super::learning_journal::note_session(
            runtime.state,
            &string_arg(arguments, "summary"),
            &string_arg(arguments, "next_focus"),
        ),
        "add_record" => super::learning_journal::add_record(
            runtime.state,
            &string_arg(arguments, "title"),
            &string_arg(arguments, "insight"),
        ),
        _ => return ToolOutcome::failure("op 必须是 set_mission、note_session 或 add_record。"),
    };
    match result {
        Ok(result) => {
            let metadata = json!({
                "kind": "learning_update",
                "op": op,
                "record_id": result.record_id,
                "deduplicated": result.deduplicated
            });
            if result.accepted {
                ToolOutcome::success(result.message, metadata)
            } else {
                let mut outcome = ToolOutcome::failure(result.message);
                outcome.metadata = metadata;
                outcome
            }
        }
        Err(error) => ToolOutcome::failure(format!("学习日志更新失败，原数据已保留：{error}")),
    }
}

fn load_tools(arguments: &Value, runtime: &ToolRuntime<'_>) -> ToolOutcome {
    if runtime.session_id.is_empty() {
        return ToolOutcome::failure("当前上下文没有可装载扩展工具的会话。");
    }
    let names = arguments
        .get("names")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect::<Vec<_>>();
    match super::deferred::load(runtime.state, runtime.session_id, &names) {
        Ok(outcome) => {
            let loaded = outcome
                .get("loaded")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let already = outcome
                .get("already_loaded")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let unknown = outcome
                .get("unknown")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let mut lines = Vec::new();
            if !loaded.is_empty() {
                lines.push(format!("已装载：{}", join_string_values(&loaded)));
            }
            if !already.is_empty() {
                lines.push(format!("此前已装载：{}", join_string_values(&already)));
            }
            if !unknown.is_empty() {
                lines.push(format!(
                    "未知工具：{}（请使用扩展工具清单中的精确名称）",
                    join_string_values(&unknown)
                ));
            }
            let success = !loaded.is_empty() || unknown.is_empty();
            let mut result = if success {
                ToolOutcome::success(
                    if lines.is_empty() {
                        "没有需要装载的工具。".to_string()
                    } else {
                        lines.join("\n")
                    },
                    json!({"kind": "load_tools", "result": outcome}),
                )
            } else {
                ToolOutcome::failure(lines.join("\n"))
            };
            result.metadata["result"] = outcome;
            result
        }
        Err(error) => ToolOutcome::failure(format!("装载扩展工具失败：{error}")),
    }
}

fn mastery_topics(arguments: &Value, runtime: &ToolRuntime<'_>) -> ToolOutcome {
    match super::mastery::navigation_topics(runtime.state, &string_arg(arguments, "query")) {
        Ok(payload) => ToolOutcome::success(
            serde_json::to_string(&payload).unwrap_or_else(|_| "掌握主题列表已生成。".to_string()),
            json!({"kind": "mastery_topics", "mastery_topics": payload}),
        ),
        Err(error) => ToolOutcome::failure(format!("读取掌握主题失败：{error}")),
    }
}

fn mastery_sessions(arguments: &Value, runtime: &ToolRuntime<'_>) -> ToolOutcome {
    let path_id = string_arg(arguments, "path_id");
    if path_id.trim().is_empty() {
        return ToolOutcome::failure("mastery_sessions 需要 path_id；请先调用 mastery_topics。");
    }
    match super::mastery::navigation_sessions(runtime.state, path_id.trim()) {
        Ok(payload) => ToolOutcome::success(
            serde_json::to_string(&payload)
                .unwrap_or_else(|_| "掌握学习会话列表已生成。".to_string()),
            json!({"kind": "mastery_sessions", "mastery_sessions": payload}),
        ),
        Err(error) => ToolOutcome::failure(format!("读取掌握学习会话失败：{error}")),
    }
}

fn mastery_open_session(arguments: &Value, runtime: &ToolRuntime<'_>) -> ToolOutcome {
    mastery_handoff("open", arguments, runtime)
}

fn mastery_new_session(arguments: &Value, runtime: &ToolRuntime<'_>) -> ToolOutcome {
    mastery_handoff("new", arguments, runtime)
}

fn mastery_handoff(kind: &str, arguments: &Value, runtime: &ToolRuntime<'_>) -> ToolOutcome {
    let result = super::mastery::navigation_handoff(
        runtime.state,
        kind,
        &string_arg(arguments, "path_id"),
        &string_arg(arguments, "session_id"),
        &string_arg(arguments, "opening_message"),
        &string_arg(arguments, "module"),
        &string_arg(arguments, "reason"),
    );
    match result {
        Ok(payload) => ToolOutcome::success(
            serde_json::to_string(&payload)
                .unwrap_or_else(|_| "掌握学习交接卡片已生成。".to_string()),
            json!({
                "kind": "mastery_navigation",
                "mastery_handoff": payload
            }),
        ),
        Err(error) => ToolOutcome::failure(format!("生成掌握学习交接失败：{error}")),
    }
}

fn tool(name: &str, description: &str, parameters: Value) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": parameters
        }
    })
}

fn ask_user_definition() -> Value {
    tool(
        "ask_user",
        "当且仅当缺少只有用户才能提供的关键信息时，暂停当前回合并一次性提出 1-4 个结构化问题。",
        json!({
            "type": "object",
            "properties": {
                "intro": {"type": "string", "description": "问题卡片上方的一句引导语"},
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 4,
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {"type": "string"},
                            "prompt": {"type": "string"},
                            "header": {"type": "string"},
                            "multi_select": {"type": "boolean"},
                            "allow_free_text": {"type": "boolean"},
                            "placeholder": {"type": "string"},
                            "options": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": {"type": "string"},
                                        "description": {"type": "string"}
                                    },
                                    "required": ["label"]
                                }
                            }
                        },
                        "required": ["prompt"]
                    }
                }
            },
            "required": ["questions"]
        }),
    )
}

fn web_search_definition() -> Value {
    tool(
        "web_search",
        "搜索公开网页并返回标题、URL 与摘要。用于实时事实、新闻和外部资料核验。",
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "max_results": {"type": "integer", "minimum": 1, "maximum": 10}
            },
            "required": ["query"]
        }),
    )
}

fn paper_search_definition() -> Value {
    tool(
        "paper_search",
        "搜索 arXiv 论文并返回标题、作者、摘要和链接。",
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "max_results": {"type": "integer", "minimum": 1, "maximum": 10}
            },
            "required": ["query"]
        }),
    )
}

fn zotero_search_definition() -> Value {
    tool(
        "zotero_search",
        "搜索用户配置的 Zotero 资料库。",
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "max_results": {"type": "integer", "minimum": 1, "maximum": 25}
            },
            "required": ["query"]
        }),
    )
}

fn brainstorm_definition() -> Value {
    tool(
        "brainstorm",
        "围绕主题进行广度优先发散，给出多个不同方向及简短理由。",
        json!({
            "type": "object",
            "properties": {
                "topic": {"type": "string", "description": "需要发散思考的主题、目标或问题"},
                "context": {"type": "string", "description": "可选背景、限制或补充信息"}
            },
            "required": ["topic"]
        }),
    )
}

fn reason_definition() -> Value {
    tool(
        "reason",
        "针对一个明确问题执行单次、无外部工具的深入推理与逐步论证。",
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "需要深入分析或推导的具体焦点"},
                "context": {"type": "string", "description": "可选的原问题、计划或已知信息"}
            },
            "required": ["query"]
        }),
    )
}

fn rag_definition() -> Value {
    tool(
        "rag",
        "检索安装目录内已建立索引的知识库。必须逐字使用用户提供的知识库名称。",
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "kb_name": {"type": "string"},
                "max_results": {"type": "integer", "minimum": 1, "maximum": 12}
            },
            "required": ["query"]
        }),
    )
}

fn read_file_definition() -> Value {
    tool(
        "read_file",
        "读取安装目录内的文本文件。传入相对于 data 目录的路径。",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "max_chars": {"type": "integer", "minimum": 100, "maximum": 120000}
            },
            "required": ["path"]
        }),
    )
}

fn memory_search_definition() -> Value {
    tool(
        "memory_search",
        "检索学习者的本地长期记忆。",
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "max_results": {"type": "integer", "minimum": 1, "maximum": 20}
            },
            "required": ["query"]
        }),
    )
}

fn geogebra_definition() -> Value {
    tool(
        "geogebra_analysis",
        "分析数学题图片并生成可执行的 GeoGebra 命令。必须存在图片附件或 image_path。",
        json!({
            "type": "object",
            "properties": {
                "question": {"type": "string", "description": "需要分析的数学题文字"},
                "image_path": {"type": "string", "description": "可选的 data 目录内图片路径；通常由服务器自动注入附件"}
            },
            "required": ["question"]
        }),
    )
}

fn imagegen_definition() -> Value {
    tool(
        "imagegen",
        "使用设置中已配置的图像生成模型，根据文字描述生成图片并保存到便携输出目录。",
        json!({
            "type": "object",
            "properties": {
                "prompt": {"type": "string", "description": "自包含、具体的画面描述"},
                "size": {"type": "string", "description": "可选尺寸，例如 1024x1024"},
                "n": {"type": "integer", "minimum": 1, "maximum": 4, "description": "生成数量，默认 1"}
            },
            "required": ["prompt"]
        }),
    )
}

fn videogen_definition() -> Value {
    tool(
        "videogen",
        "使用设置中已配置的视频生成模型，根据文字描述生成短视频并保存到便携输出目录。",
        json!({
            "type": "object",
            "properties": {
                "prompt": {"type": "string", "description": "包含场景、动作和风格的自包含描述"},
                "aspect_ratio": {"type": "string", "description": "可选画幅，例如 16:9 或 9:16"},
                "duration": {"type": "string", "description": "可选时长，单位为秒"}
            },
            "required": ["prompt"]
        }),
    )
}

fn workspace_list_definition() -> Value {
    tool(
        "workspace_list",
        "列出当前工作区内的文件和文件夹。路径均相对于工作区根目录。",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "工作区相对目录，默认根目录"},
                "depth": {"type": "integer", "minimum": 1, "maximum": 5},
                "limit": {"type": "integer", "minimum": 1, "maximum": 1000}
            }
        }),
    )
}

fn workspace_read_definition() -> Value {
    tool(
        "workspace_read",
        "读取当前工作区内的 UTF-8 文本文件，支持字符偏移与分页。",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "工作区相对文件路径"},
                "offset": {"type": "integer", "minimum": 0},
                "limit": {"type": "integer", "minimum": 1, "maximum": 100000}
            },
            "required": ["path"]
        }),
    )
}

fn workspace_search_definition() -> Value {
    tool(
        "workspace_search",
        "在当前工作区内搜索文件名和 UTF-8 文本内容。",
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "path": {"type": "string", "description": "可选的相对搜索目录"},
                "limit": {"type": "integer", "minimum": 1, "maximum": 200}
            },
            "required": ["query"]
        }),
    )
}

fn workspace_present_definition() -> Value {
    tool(
        "workspace_present",
        "把一个或多个工作区文件作为可打开卡片展示给用户。最终 Markdown 必须使用返回的精确相对路径。",
        json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 20,
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string"},
                            "title": {"type": "string"},
                            "caption": {"type": "string"}
                        },
                        "required": ["path"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["items"],
            "additionalProperties": false
        }),
    )
}

fn write_note_definition() -> Value {
    tool(
        "write_note",
        "新建一篇作者笔记页面并保存到便携数据库。已有页面应通过写作界面修改。",
        json!({
            "type": "object",
            "properties": {
                "title": {"type": "string", "description": "页面标题"},
                "content": {"type": "string", "description": "完整 Markdown 正文，保留引用和来源链接"}
            },
            "required": ["title", "content"]
        }),
    )
}

fn search_notes_definition() -> Value {
    tool(
        "search_notes",
        "按标题或全文查找作者笔记页面；空查询列出最近页面。提供 page_id 可读取整页。",
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "page_id": {"type": "string"}
            }
        }),
    )
}

fn list_notebook_definition() -> Value {
    tool(
        "list_notebook",
        "列出收藏笔记本；提供 notebook_id 时列出该笔记本中的记录。",
        json!({
            "type": "object",
            "properties": {
                "notebook_id": {"type": "string"}
            }
        }),
    )
}

fn save_favorite_definition() -> Value {
    tool(
        "save_favorite",
        "新建或编辑收藏记录。append 创建记录，edit 按 record_id 更新标题或摘要。",
        json!({
            "type": "object",
            "properties": {
                "mode": {"type": "string", "enum": ["append", "edit"]},
                "notebook_id": {"type": "string"},
                "record_id": {"type": "string"},
                "title": {"type": "string"},
                "content": {"type": "string", "description": "可选的作者 Markdown 正文"},
                "turns_to_include": {"type": "integer", "minimum": 1, "maximum": 50},
                "note": {"type": "string"}
            },
            "required": ["mode", "notebook_id"]
        }),
    )
}

fn web_fetch_definition() -> Value {
    tool(
        "web_fetch",
        "读取用户提供的具体网页链接，并提取可读正文。一般主题检索请使用 web_search。",
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "完整的 http:// 或 https:// URL"},
                "max_chars": {"type": "integer", "minimum": 1000, "maximum": 200000}
            },
            "required": ["url"]
        }),
    )
}

fn github_definition() -> Value {
    tool(
        "github",
        "通过 GitHub 只读 API 查询仓库、Issue、Pull Request、Actions 运行或 REST API 路径。不会执行任何写操作。",
        json!({
            "type": "object",
            "properties": {
                "query_type": {"type": "string", "enum": ["pr", "issue", "run", "repo", "api"]},
                "target": {"type": "string", "description": "owner/repo[#编号]、完整 URL 或 API 相对路径"}
            },
            "required": ["query_type", "target"]
        }),
    )
}

fn read_skill_definition() -> Value {
    tool(
        "read_skill",
        "读取已安装技能包的 SKILL.md 或包内参考文件。应先读取完整说明再执行匹配的任务。",
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "技能目录中的精确名称"},
                "file": {"type": "string", "description": "技能包内相对路径，默认 SKILL.md"}
            },
            "required": ["name"]
        }),
    )
}

fn question_bank_definition() -> Value {
    tool(
        "question_bank",
        "读取、整理或记录学习者题库中的错题与测验题目。",
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["overview", "list", "record", "organize", "unfile", "bookmark"]},
                "question": {"type": "string"},
                "user_answer": {"type": "string"},
                "correct_answer": {"type": "string"},
                "explanation": {"type": "string"},
                "is_correct": {"type": "boolean"},
                "filter": {"type": "string", "enum": ["all", "wrong", "uncategorized", "bookmarked"]},
                "category": {"type": "string"},
                "search": {"type": "string"},
                "entry_ids": {"type": "array", "items": {"type": "integer"}},
                "bookmarked": {"type": "boolean"},
                "limit": {"type": "integer", "minimum": 1, "maximum": 100}
            },
            "required": ["action"]
        }),
    )
}

fn read_source_definition() -> Value {
    tool(
        "read_source",
        "按来源清单中的精确 id 读取本轮附加来源全文。仅当清单预览不足以回答或需要引用原文时调用；历史会话 hs-tutor-* 使用 offset 分页。",
        json!({
            "type": "object",
            "properties": {
                "source_id": {
                    "type": "string",
                    "description": "来源清单中的精确 id，例如 nb-、bk-、rd-、hs-、qb- 或 at- 开头"
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "历史会话分页字符偏移，默认 0"
                }
            },
            "required": ["source_id"]
        }),
    )
}

fn read_memory_definition() -> Value {
    tool(
        "read_memory",
        "读取用户跨会话的 L3 长期记忆：近期摘要、用户画像、知识范围与明确偏好。用于个性化表达和难度，不要每回合盲目调用。",
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
    )
}

fn write_memory_definition() -> Value {
    tool(
        "write_memory",
        "仅当用户明确陈述长期偏好时写入 preferences 记忆。不要猜测偏好。",
        json!({
            "type": "object",
            "properties": {
                "op": {
                    "type": "string",
                    "enum": ["add", "edit"],
                    "description": "add 新增偏好，edit 修改已有偏好"
                },
                "text": {
                    "type": "string",
                    "description": "用用户原话表达的偏好，最多 240 个字符"
                },
                "target_id": {
                    "type": "string",
                    "description": "edit 时必填，形式为 m_xxx"
                },
                "reason": {
                    "type": "string",
                    "description": "可选的一行原因"
                }
            },
            "required": ["op", "text"],
            "additionalProperties": false
        }),
    )
}

fn learning_status_definition() -> Value {
    tool(
        "learning_status",
        "读取自由学习日志中的使命、上次会话交接和最近学习记录。需要历史记录或用户要求继续学习时调用。",
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
    )
}

fn learning_update_definition() -> Value {
    tool(
        "learning_update",
        "更新自由学习日志：set_mission 固定主题，note_session 记录本次与下次重点，add_record 保存一条长期洞见。只记录学习者确认的内容。",
        json!({
            "type": "object",
            "properties": {
                "op": {
                    "type": "string",
                    "enum": ["set_mission", "note_session", "add_record"]
                },
                "topic": {"type": "string"},
                "why": {"type": "string"},
                "level": {"type": "string"},
                "summary": {"type": "string"},
                "next_focus": {"type": "string"},
                "title": {"type": "string"},
                "insight": {"type": "string"}
            },
            "required": ["op"],
            "additionalProperties": false
        }),
    )
}

fn load_tools_definition() -> Value {
    tool(
        "load_tools",
        "把扩展工具清单中的一项或多项工具 schema 装入当前会话，使其后续可调用。必须使用清单中的精确名称。",
        json!({
            "type": "object",
            "properties": {
                "names": {
                    "type": "array",
                    "minItems": 1,
                    "items": {"type": "string"}
                }
            },
            "required": ["names"],
            "additionalProperties": false
        }),
    )
}

fn exec_definition() -> Value {
    tool(
        "exec",
        "在当前回合的便携隔离目录运行完整源码或显式 shell 脚本。默认关闭；只调用用户自行安装的 Python、C/C++ 工具链，绝不回退到随包运行时。",
        json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "完整源码；language=shell 时是 shell 脚本"
                },
                "language": {
                    "type": "string",
                    "enum": ["python", "c", "cpp", "shell"],
                    "default": "python"
                },
                "stdin": {"type": "string"},
                "timeout": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 300,
                    "description": "超时秒数，默认 30"
                }
            },
            "required": ["code"],
            "additionalProperties": false
        }),
    )
}

fn cron_definition() -> Value {
    tool(
        "cron",
        "为当前会话安排定时任务、列出或取消任务。到点后任务消息作为同一会话的新回合运行。at、every_seconds、cron_expr 必须三选一。",
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["schedule", "list", "cancel"]
                },
                "message": {
                    "type": "string",
                    "description": "schedule：到点执行的完整、自包含指令"
                },
                "name": {"type": "string"},
                "at": {
                    "type": "string",
                    "description": "schedule：ISO 8601 一次性时间"
                },
                "every_seconds": {
                    "type": "integer",
                    "minimum": 30,
                    "description": "schedule：重复间隔秒数"
                },
                "cron_expr": {
                    "type": "string",
                    "description": "schedule：五段 cron 表达式"
                },
                "tz": {
                    "type": "string",
                    "description": "cron_expr 使用的 IANA 时区"
                },
                "delete_after_run": {
                    "type": "boolean",
                    "description": "运行一次后删除；at 默认 true"
                },
                "job_id": {"type": "string"}
            },
            "required": ["action"],
            "additionalProperties": false
        }),
    )
}

fn preprint_definition() -> Value {
    tool(
        "preprint",
        "读取带版本号的 arXiv 论文原文文本和来源页链接；也可查看社区页面或已授权的科研 MCP 工具。社区摘要不得冒充论文原文。",
        json!({
            "type": "object",
            "properties": {
                "paper": {
                    "type": "string",
                    "description": "arXiv ID、带版本 ID 或规范 URL"
                },
                "action": {
                    "type": "string",
                    "enum": ["read", "figures", "discussion", "mcp"],
                    "default": "read"
                },
                "pages": {
                    "type": "array",
                    "maxItems": 20,
                    "items": {"type": "integer", "minimum": 1}
                },
                "mcp_tool": {"type": "string"},
                "arguments": {"type": "object"}
            },
            "additionalProperties": false
        }),
    )
}

fn research_audit_definition() -> Value {
    tool(
        "research_audit",
        "基于论文原文和不可变 GitHub 提交准备静态复现审计。只读取代码，不执行仓库代码；缺少证据时明确标记未验证。",
        json!({
            "type": "object",
            "properties": {
                "paper": {"type": "string"},
                "repository": {"type": "string"},
                "ref": {"type": "string", "default": "HEAD"},
                "files": {
                    "type": "array",
                    "maxItems": 8,
                    "items": {"type": "string"}
                },
                "pages": {
                    "type": "array",
                    "maxItems": 20,
                    "items": {"type": "integer", "minimum": 1}
                },
                "checks": {
                    "type": "array",
                    "maxItems": 12,
                    "items": {"type": "object"}
                }
            },
            "required": ["paper", "repository"],
            "additionalProperties": false
        }),
    )
}

fn research_lit_definition() -> Value {
    tool(
        "research_lit",
        "围绕主题执行有界的一手论文发现，可有最多三轮后续检索，并输出比较证据矩阵。仅凭摘要不能证明共识或争议。",
        json!({
            "type": "object",
            "properties": {
                "topic": {"type": "string"},
                "followups": {
                    "type": "array",
                    "maxItems": 3,
                    "items": {"type": "string"}
                }
            },
            "required": ["topic"],
            "additionalProperties": false
        }),
    )
}

fn kb_files_definition() -> Value {
    tool(
        "kb_files",
        "列出本轮可访问知识库中的文档、总数和匹配结果。用于回答知识库有多少文件、文件名称或某文件是否存在；不要用 rag 代替。",
        json!({
            "type": "object",
            "properties": {
                "kb_name": {"type": "string"},
                "pattern": {
                    "type": "string",
                    "description": "可选 glob 或大小写不敏感子串"
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 500
                }
            },
            "required": ["kb_name"],
            "additionalProperties": false
        }),
    )
}

fn knowledge_frontier_definition() -> Value {
    tool(
        "knowledge_frontier",
        "总结已附加知识库的主题与空白，再检索近年 arXiv 工作作为延伸建议。只推荐，不自动导入来源。",
        json!({
            "type": "object",
            "properties": {
                "kb_name": {"type": "string"},
                "focus": {"type": "string"},
                "max_papers": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 10,
                    "default": 5
                },
                "years_limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 10,
                    "default": 3
                }
            },
            "required": ["kb_name"],
            "additionalProperties": false
        }),
    )
}

fn mastery_topics_definition() -> Value {
    tool(
        "mastery_topics",
        "列出学习者的掌握主题、目标完成度、复习与学习会话摘要。先解析真实 path_id，再调用会话导航工具。",
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "可选名称、目标或课程模块过滤"
                }
            },
            "additionalProperties": false
        }),
    )
}

fn mastery_sessions_definition() -> Value {
    tool(
        "mastery_sessions",
        "列出一个掌握主题下的学习会话；优先恢复正在等待回答或与当前问题最相关的会话。",
        json!({
            "type": "object",
            "properties": {
                "path_id": {"type": "string"}
            },
            "required": ["path_id"],
            "additionalProperties": false
        }),
    )
}

fn mastery_open_session_definition() -> Value {
    tool(
        "mastery_open_session",
        "生成一个打开既有掌握学习会话的界面交接卡片。不会自行导航或修改进度。",
        json!({
            "type": "object",
            "properties": {
                "path_id": {"type": "string"},
                "session_id": {"type": "string"},
                "opening_message": {"type": "string"},
                "module": {"type": "string"},
                "reason": {"type": "string"}
            },
            "required": ["path_id", "session_id"],
            "additionalProperties": false
        }),
    )
}

fn mastery_new_session_definition() -> Value {
    tool(
        "mastery_new_session",
        "生成一个新建掌握学习会话的界面交接卡片。既按模块和学习者口吻撰写开场消息。",
        json!({
            "type": "object",
            "properties": {
                "path_id": {"type": "string"},
                "opening_message": {"type": "string"},
                "module": {"type": "string"},
                "reason": {"type": "string"}
            },
            "required": ["path_id"],
            "additionalProperties": false
        }),
    )
}

fn invoke_other_definition() -> Value {
    tool(
        "invoke_other",
        "在已写出完整正式回答后，提议向另一位伙伴提出一个聚焦的追问。该操作只创建待用户批准的建议，不会直接联系对方；每回合最多调用一次。",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "target_partner_id": {
                    "type": "string",
                    "description": "一名合格伙伴的精确 @标识"
                },
                "question": {
                    "type": "string",
                    "description": "可独立理解、具体且不替对方作答的问题"
                }
            },
            "required": ["target_partner_id", "question"]
        }),
    )
}

fn solve_plan_definition() -> Value {
    tool(
        "solve_plan",
        "在开始解题前登记简短分析和有序步骤。",
        json!({
            "type": "object",
            "properties": {
                "analysis": {"type": "string"},
                "steps": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {"type": "string"},
                            "title": {"type": "string"}
                        },
                        "required": ["id", "title"]
                    }
                }
            },
            "required": ["analysis", "steps"]
        }),
    )
}

fn solve_step_definition() -> Value {
    tool(
        "solve_finish_step",
        "完成当前解题步骤并记录简短结论。",
        json!({
            "type": "object",
            "properties": {
                "step_id": {"type": "string"},
                "summary": {"type": "string"}
            },
            "required": ["step_id", "summary"]
        }),
    )
}

fn invoke_other(arguments: &Value, request: Option<&Value>) -> ToolOutcome {
    let Some(group) = request
        .and_then(|value| value.get("partner_group"))
        .filter(|value| value.is_object())
    else {
        return ToolOutcome::failure("invoke_other 只能在伙伴群组回合中使用。");
    };
    if !group
        .get("allow_invoke_other")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return ToolOutcome::failure("当前伙伴回合不允许继续发起协作提问。");
    }
    let self_id = group
        .get("self_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let target_id = string_arg(arguments, "target_partner_id")
        .trim()
        .trim_start_matches('@')
        .to_string();
    let question = string_arg(arguments, "question").trim().to_string();
    let member = group
        .get("members")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|member| {
            member.get("partner_id").and_then(Value::as_str) == Some(target_id.as_str())
        });
    let Some(member) = member else {
        return ToolOutcome::failure("未登记提议：目标不是当前群组的合格伙伴。");
    };
    if target_id == self_id {
        return ToolOutcome::failure("未登记提议：不能向自己发起追问。");
    }
    if question.is_empty() {
        return ToolOutcome::failure("未登记提议：追问内容不能为空。");
    }
    if question.chars().count() > 2_000 {
        return ToolOutcome::failure("未登记提议：追问内容不能超过 2000 个字符。");
    }
    let target_name = member
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(&target_id);
    let mut outcome = ToolOutcome::success(
        serde_json::to_string(&json!({
            "status": "pending_user_approval",
            "target_partner_id": target_id,
            "target_partner_name": target_name,
            "message": "建议已登记，请结束当前回合，等待用户批准。"
        }))
        .unwrap_or_else(|_| "建议已登记，等待用户批准。".to_string()),
        json!({
            "kind": "partner_invocation",
            "partner_invocation": {
                "target_partner_id": target_id,
                "target_partner_name": target_name,
                "question": question
            }
        }),
    );
    outcome.terminate_turn = true;
    outcome
}

async fn web_search(arguments: &Value, state: &AppState) -> ToolOutcome {
    let query = string_arg(arguments, "query");
    if query.trim().is_empty() {
        return ToolOutcome::failure("web_search 需要非空 query。");
    }
    let limit = integer_arg(arguments, "max_results", 5).clamp(1, 10) as usize;
    match search_provider(&query, limit, state).await {
        Ok(results) if !results.is_empty() => {
            let content = results
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    format!(
                        "[{}] {}\n{}\n{}",
                        index + 1,
                        item["title"].as_str().unwrap_or("未命名结果"),
                        item["url"].as_str().unwrap_or(""),
                        item["snippet"].as_str().unwrap_or("")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n\n");
            ToolOutcome::success(
                content,
                json!({
                    "provider": "web",
                    "query": query,
                    "results": results
                }),
            )
        }
        Ok(_) => ToolOutcome::failure("网络搜索没有返回结果。请调整查询词或改用其他来源。"),
        Err(error) => ToolOutcome::failure(error),
    }
}

async fn search_provider(
    query: &str,
    limit: usize,
    state: &AppState,
) -> Result<Vec<Value>, String> {
    let catalog = state
        .database
        .get_setting("catalog")
        .map_err(|error| format!("无法读取搜索设置：{error}"))?
        .unwrap_or_else(|| json!({}));
    search_with_catalog(query, limit, state, &catalog).await
}

pub(super) async fn search_with_catalog(
    query: &str,
    limit: usize,
    state: &AppState,
    catalog: &Value,
) -> Result<Vec<Value>, String> {
    let service = catalog.pointer("/services/search");
    let active_profile_id = service
        .and_then(|service| service.get("active_profile_id"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let profile = service
        .and_then(|service| service.get("profiles"))
        .and_then(Value::as_array)
        .and_then(|profiles| {
            profiles.iter().find(|profile| {
                profile.get("id").and_then(Value::as_str) == Some(active_profile_id)
            })
        });
    if let Some(profile) = profile {
        let profile = catalog::resolve_profile_provider(catalog, "search", profile, None)?;
        let provider = profile
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        let api_key = profile
            .get("api_key")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let base_url = profile
            .get("base_url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim_end_matches('/');
        if provider.contains("tavily") {
            return tavily_search(query, limit, api_key, base_url, state).await;
        }
        if provider.contains("serper") {
            return serper_search(query, limit, api_key, base_url, state).await;
        }
        if provider.contains("brave") {
            return brave_search(query, limit, api_key, base_url, state).await;
        }
        if provider.contains("searx") || !base_url.is_empty() {
            return searx_search(query, limit, base_url, state).await;
        }
    }
    duckduckgo_search(query, limit, state).await
}

/// 只测试用户明确选择的搜索服务，不启用普通搜索的 DuckDuckGo 回退。
pub(super) async fn search_with_catalog_strict(
    query: &str,
    limit: usize,
    state: &AppState,
    catalog: &Value,
) -> Result<Vec<Value>, String> {
    let service = catalog
        .pointer("/services/search")
        .ok_or_else(|| "尚未配置搜索服务。".to_string())?;
    let active_profile_id = service
        .get("active_profile_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "尚未选择搜索配置。".to_string())?;
    let profile = service
        .get("profiles")
        .and_then(Value::as_array)
        .and_then(|profiles| {
            profiles.iter().find(|profile| {
                profile.get("id").and_then(Value::as_str) == Some(active_profile_id)
            })
        })
        .ok_or_else(|| "当前搜索配置已不存在。".to_string())?;
    let profile = catalog::resolve_profile_provider(catalog, "search", profile, None)?;
    let provider = profile
        .get("provider")
        .or_else(|| profile.get("binding"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let api_key = profile
        .get("api_key")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let base_url = profile
        .get("base_url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim_end_matches('/');
    let rows = if provider.contains("tavily") {
        tavily_search(query, limit, api_key, base_url, state).await?
    } else if provider.contains("serper") {
        serper_search(query, limit, api_key, base_url, state).await?
    } else if provider.contains("brave") {
        brave_search(query, limit, api_key, base_url, state).await?
    } else if provider.contains("searx") {
        if base_url.is_empty() {
            return Err("SearXNG 搜索缺少服务地址。".to_string());
        }
        searx_search(query, limit, base_url, state).await?
    } else if provider == "duckduckgo" || provider == "ddg" {
        duckduckgo_search(query, limit, state).await?
    } else if !base_url.is_empty() {
        searx_search(query, limit, base_url, state).await?
    } else {
        return Err(format!("不支持的搜索提供方：{provider}"));
    };
    if rows.is_empty() {
        return Err("搜索服务没有返回结果。".to_string());
    }
    Ok(rows)
}

async fn tavily_search(
    query: &str,
    limit: usize,
    api_key: &str,
    base_url: &str,
    state: &AppState,
) -> Result<Vec<Value>, String> {
    let endpoint = if base_url.is_empty() {
        "https://api.tavily.com/search"
    } else {
        base_url
    };
    let response = state
        .client
        .post(endpoint)
        .json(&json!({
            "api_key": api_key,
            "query": query,
            "max_results": limit,
            "include_answer": false,
            "search_depth": "advanced"
        }))
        .send()
        .await
        .map_err(|error| format!("Tavily 搜索失败：{error}"))?;
    let value = json_response(response, "Tavily").await?;
    Ok(value
        .get("results")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| {
                    json!({
                        "title": item.get("title").and_then(Value::as_str).unwrap_or_default(),
                        "url": item.get("url").and_then(Value::as_str).unwrap_or_default(),
                        "snippet": item.get("content").and_then(Value::as_str).unwrap_or_default(),
                        "provider": "tavily"
                    })
                })
                .collect()
        })
        .unwrap_or_default())
}

async fn serper_search(
    query: &str,
    limit: usize,
    api_key: &str,
    base_url: &str,
    state: &AppState,
) -> Result<Vec<Value>, String> {
    let endpoint = if base_url.is_empty() {
        "https://google.serper.dev/search"
    } else {
        base_url
    };
    let response = state
        .client
        .post(endpoint)
        .header("X-API-KEY", api_key)
        .json(&json!({"q": query, "num": limit}))
        .send()
        .await
        .map_err(|error| format!("Serper 搜索失败：{error}"))?;
    let value = json_response(response, "Serper").await?;
    Ok(value
        .get("organic")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .take(limit)
                .map(|item| {
                    json!({
                        "title": item.get("title").and_then(Value::as_str).unwrap_or_default(),
                        "url": item.get("link").and_then(Value::as_str).unwrap_or_default(),
                        "snippet": item.get("snippet").and_then(Value::as_str).unwrap_or_default(),
                        "provider": "serper"
                    })
                })
                .collect()
        })
        .unwrap_or_default())
}

async fn brave_search(
    query: &str,
    limit: usize,
    api_key: &str,
    base_url: &str,
    state: &AppState,
) -> Result<Vec<Value>, String> {
    let endpoint = if base_url.is_empty() {
        "https://api.search.brave.com/res/v1/web/search"
    } else {
        base_url
    };
    let response = state
        .client
        .get(endpoint)
        .query(&[("q", query), ("count", &limit.to_string())])
        .header("X-Subscription-Token", api_key)
        .send()
        .await
        .map_err(|error| format!("Brave 搜索失败：{error}"))?;
    let value = json_response(response, "Brave").await?;
    Ok(value
        .pointer("/web/results")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .take(limit)
                .map(|item| {
                    json!({
                        "title": item.get("title").and_then(Value::as_str).unwrap_or_default(),
                        "url": item.get("url").and_then(Value::as_str).unwrap_or_default(),
                        "snippet": item.get("description").and_then(Value::as_str).unwrap_or_default(),
                        "provider": "brave"
                    })
                })
                .collect()
        })
        .unwrap_or_default())
}

async fn searx_search(
    query: &str,
    limit: usize,
    base_url: &str,
    state: &AppState,
) -> Result<Vec<Value>, String> {
    let base = if base_url.is_empty() {
        "https://searx.be"
    } else {
        base_url
    };
    let endpoint = format!("{}/search", base.trim_end_matches('/'));
    let response = state
        .client
        .get(endpoint)
        .query(&[("q", query), ("format", "json"), ("language", "zh-CN")])
        .send()
        .await
        .map_err(|error| format!("SearXNG 搜索失败：{error}"))?;
    let value = json_response(response, "SearXNG").await?;
    Ok(value
        .get("results")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .take(limit)
                .map(|item| {
                    json!({
                        "title": item.get("title").and_then(Value::as_str).unwrap_or_default(),
                        "url": item.get("url").and_then(Value::as_str).unwrap_or_default(),
                        "snippet": item.get("content").and_then(Value::as_str).unwrap_or_default(),
                        "provider": "searxng"
                    })
                })
                .collect()
        })
        .unwrap_or_default())
}

async fn duckduckgo_search(
    query: &str,
    limit: usize,
    state: &AppState,
) -> Result<Vec<Value>, String> {
    let response = state
        .client
        .get("https://html.duckduckgo.com/html/")
        .query(&[("q", query)])
        .header("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8")
        .send()
        .await
        .map_err(|error| format!("DuckDuckGo 搜索失败：{error}"))?;
    if !response.status().is_success() {
        return Err(format!("DuckDuckGo 返回 HTTP {}", response.status()));
    }
    let html = response
        .text()
        .await
        .map_err(|error| format!("读取 DuckDuckGo 结果失败：{error}"))?;
    let link_regex =
        Regex::new(r#"(?s)<a[^>]*class="[^"]*result__a[^"]*"[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#)
            .expect("固定正则");
    let snippet_regex =
        Regex::new(r#"(?s)<a[^>]*class="[^"]*result__snippet[^"]*"[^>]*>(.*?)</a>"#)
            .expect("固定正则");
    let snippets = snippet_regex
        .captures_iter(&html)
        .map(|capture| clean_html(&capture[1]))
        .collect::<Vec<_>>();
    Ok(link_regex
        .captures_iter(&html)
        .take(limit)
        .enumerate()
        .map(|(index, capture)| {
            json!({
                "title": clean_html(&capture[2]),
                "url": decode_redirect_url(&capture[1]),
                "snippet": snippets.get(index).cloned().unwrap_or_default(),
                "provider": "duckduckgo"
            })
        })
        .collect())
}

async fn paper_search(arguments: &Value, state: &AppState) -> ToolOutcome {
    let query = string_arg(arguments, "query");
    if query.trim().is_empty() {
        return ToolOutcome::failure("paper_search 需要非空 query。");
    }
    let limit = integer_arg(arguments, "max_results", 5).clamp(1, 10);
    let response = match state
        .client
        .get("https://export.arxiv.org/api/query")
        .query(&[
            ("search_query", format!("all:{}", query.trim())),
            ("start", "0".to_string()),
            ("max_results", limit.to_string()),
            ("sortBy", "relevance".to_string()),
        ])
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => return ToolOutcome::failure(format!("arXiv 搜索失败：{error}")),
    };
    if !response.status().is_success() {
        return ToolOutcome::failure(format!("arXiv 返回 HTTP {}", response.status()));
    }
    let xml = match response.text().await {
        Ok(text) => text,
        Err(error) => return ToolOutcome::failure(format!("读取 arXiv 结果失败：{error}")),
    };
    let entry_regex = Regex::new(r"(?s)<entry>(.*?)</entry>").expect("固定正则");
    let title_regex = Regex::new(r"(?s)<title>(.*?)</title>").expect("固定正则");
    let id_regex = Regex::new(r"(?s)<id>(.*?)</id>").expect("固定正则");
    let summary_regex = Regex::new(r"(?s)<summary>(.*?)</summary>").expect("固定正则");
    let author_regex = Regex::new(r"(?s)<name>(.*?)</name>").expect("固定正则");
    let results = entry_regex
        .captures_iter(&xml)
        .map(|entry| {
            let entry = &entry[1];
            let authors = author_regex
                .captures_iter(entry)
                .map(|capture| clean_html(&capture[1]))
                .collect::<Vec<_>>();
            json!({
                "title": capture_text(&title_regex, entry),
                "url": capture_text(&id_regex, entry),
                "snippet": capture_text(&summary_regex, entry),
                "authors": authors,
                "provider": "arxiv"
            })
        })
        .collect::<Vec<_>>();
    if results.is_empty() {
        return ToolOutcome::failure("arXiv 未返回匹配论文。");
    }
    let content = results
        .iter()
        .enumerate()
        .map(|(index, item)| {
            format!(
                "[{}] {}\n作者：{}\n{}\n{}",
                index + 1,
                item["title"].as_str().unwrap_or_default(),
                item["authors"]
                    .as_array()
                    .map(|authors| authors
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", "))
                    .unwrap_or_default(),
                item["url"].as_str().unwrap_or_default(),
                item["snippet"].as_str().unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    ToolOutcome::success(
        content,
        json!({"provider": "arxiv", "query": query, "results": results}),
    )
}

fn kb_files(arguments: &Value, runtime: &ToolRuntime<'_>) -> ToolOutcome {
    let kb_name = string_arg(arguments, "kb_name").trim().to_string();
    if kb_name.is_empty() {
        return ToolOutcome::failure("kb_files 需要显式 kb_name。");
    }
    if let Some(attached) = runtime
        .request
        .and_then(|request| request.get("knowledge_bases"))
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty())
    {
        let attached = attached
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();
        if !attached.iter().any(|name| *name == kb_name) {
            return ToolOutcome::failure(format!(
                "知识库 `{kb_name}` 未附加到本轮。可访问：{}",
                attached.join(", ")
            ));
        }
    }
    let root = match super::knowledge::kb_raw_root(runtime.state, &kb_name) {
        Ok(root) => root,
        Err(error) => return ToolOutcome::failure(error.to_string()),
    };
    let pattern = string_arg(arguments, "pattern").trim().to_lowercase();
    let limit = integer_arg(arguments, "limit", 200).clamp(1, 500) as usize;
    let mut files = Vec::new();
    if root.is_dir() {
        let mut entries = WalkDir::new(&root)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| {
                let relative = entry
                    .path()
                    .strip_prefix(&root)
                    .unwrap_or(entry.path())
                    .to_string_lossy()
                    .replace('\\', "/");
                let size = entry.metadata().map(|metadata| metadata.len()).unwrap_or(0);
                (relative, size)
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        files = entries;
    }
    let total = files.len();
    let matched = files
        .iter()
        .filter(|(name, _)| pattern.is_empty() || wildcard_match(&pattern, &name.to_lowercase()))
        .cloned()
        .collect::<Vec<_>>();
    let listed = matched
        .iter()
        .take(limit)
        .map(|(name, size)| json!({"name": name, "size": size}))
        .collect::<Vec<_>>();
    let omitted = matched.len().saturating_sub(listed.len());
    let content = format!(
        "# 知识库文件清单\n\n- 知识库：{kb_name}\n- 文件总数：{total}\n- 匹配数：{}\n- 本次列出：{}\n- 未列出：{omitted}\n\n{}",
        matched.len(),
        listed.len(),
        listed
            .iter()
            .map(|item| format!(
                "- {}（{} 字节）",
                item.get("name").and_then(Value::as_str).unwrap_or(""),
                item.get("size").and_then(Value::as_u64).unwrap_or(0)
            ))
            .collect::<Vec<_>>()
            .join("\n")
    );
    ToolOutcome::success(
        content,
        json!({
            "kind": "kb_files",
            "kb_name": kb_name,
            "total": total,
            "matched": matched.len(),
            "listed": listed,
            "omitted": omitted,
            "pattern": pattern
        }),
    )
}

async fn knowledge_frontier(arguments: &Value, runtime: &ToolRuntime<'_>) -> ToolOutcome {
    let kb_name = string_arg(arguments, "kb_name").trim().to_string();
    if kb_name.is_empty() {
        return ToolOutcome::failure("knowledge_frontier 需要显式 kb_name。");
    }
    let root = match super::knowledge::kb_raw_root(runtime.state, &kb_name) {
        Ok(root) => root,
        Err(error) => return ToolOutcome::failure(error.to_string()),
    };
    let mut file_names = WalkDir::new(&root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter_map(|entry| {
            entry
                .path()
                .file_stem()
                .and_then(|value| value.to_str())
                .map(str::to_string)
        })
        .take(12)
        .collect::<Vec<_>>();
    file_names.sort();
    let focus = string_arg(arguments, "focus");
    let years = integer_arg(arguments, "years_limit", 3).clamp(1, 10);
    let max_papers = integer_arg(arguments, "max_papers", 5).clamp(1, 10) as usize;
    let query = if focus.trim().is_empty() {
        if file_names.is_empty() {
            kb_name.clone()
        } else {
            file_names.join(" ")
        }
    } else {
        focus.trim().to_string()
    };
    let papers = match arxiv_results(
        runtime.state,
        &query,
        max_papers.saturating_mul(2),
        "submittedDate",
    )
    .await
    {
        Ok(papers) => papers,
        Err(error) => return ToolOutcome::failure(format!("科研前沿检索失败：{error}")),
    };
    let cutoff = chrono::Utc::now() - chrono::Duration::days(years * 366);
    let papers = papers
        .into_iter()
        .filter(|paper| {
            paper
                .get("published")
                .and_then(Value::as_str)
                .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                .is_some_and(|value| value.with_timezone(&chrono::Utc) >= cutoff)
        })
        .take(max_papers)
        .collect::<Vec<_>>();
    let themes = if focus.trim().is_empty() {
        file_names.join("、")
    } else {
        focus.trim().to_string()
    };
    let content = format!(
        "# 知识库科研前沿\n\n知识库：{kb_name}\n\n## 当前主题线索\n{themes}\n\n## 近期论文建议\n\n{}\n\n> 仅推荐，未自动导入知识库。摘要不能单独证明共识或争议。",
        papers
            .iter()
            .enumerate()
            .map(|(index, paper)| format!(
                "{}. **{}**\n   - {}\n   - {}\n   - {}",
                index + 1,
                paper.get("title").and_then(Value::as_str).unwrap_or(""),
                paper.get("url").and_then(Value::as_str).unwrap_or(""),
                paper.get("published").and_then(Value::as_str).unwrap_or(""),
                paper.get("snippet").and_then(Value::as_str).unwrap_or("")
            ))
            .collect::<Vec<_>>()
            .join("\n\n")
    );
    ToolOutcome::success(
        content,
        json!({
            "kind": "knowledge_frontier",
            "kb_name": kb_name,
            "focus": focus,
            "themes": themes,
            "papers": papers,
            "notice": "只推荐近年 arXiv 工作，不自动导入；摘要不构成全文证据。"
        }),
    )
}

async fn arxiv_results(
    state: &AppState,
    query: &str,
    limit: usize,
    sort_by: &str,
) -> Result<Vec<Value>, String> {
    let response = state
        .client
        .get("https://export.arxiv.org/api/query")
        .query(&[
            ("search_query", format!("all:{}", query.trim())),
            ("start", "0".to_string()),
            ("max_results", limit.clamp(1, 30).to_string()),
            ("sortBy", sort_by.to_string()),
            ("sortOrder", "descending".to_string()),
        ])
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|error| format!("arXiv 请求失败：{error}"))?;
    if !response.status().is_success() {
        return Err(format!("arXiv 返回 HTTP {}", response.status()));
    }
    let xml = response
        .text()
        .await
        .map_err(|error| format!("读取 arXiv 响应失败：{error}"))?;
    let entry_regex = Regex::new(r"(?s)<entry>(.*?)</entry>").expect("固定正则");
    let title_regex = Regex::new(r"(?s)<title>(.*?)</title>").expect("固定正则");
    let id_regex = Regex::new(r"(?s)<id>(.*?)</id>").expect("固定正则");
    let summary_regex = Regex::new(r"(?s)<summary>(.*?)</summary>").expect("固定正则");
    let published_regex = Regex::new(r"(?s)<published>(.*?)</published>").expect("固定正则");
    let author_regex = Regex::new(r"(?s)<name>(.*?)</name>").expect("固定正则");
    Ok(entry_regex
        .captures_iter(&xml)
        .map(|entry| {
            let entry = &entry[1];
            json!({
                "title": capture_text(&title_regex, entry),
                "url": capture_text(&id_regex, entry),
                "snippet": capture_text(&summary_regex, entry),
                "published": capture_text(&published_regex, entry),
                "authors": author_regex
                    .captures_iter(entry)
                    .map(|capture| clean_html(&capture[1]))
                    .collect::<Vec<_>>(),
                "provider": "arxiv"
            })
        })
        .collect())
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let mut previous = vec![false; value.len() + 1];
    previous[0] = true;
    for token in pattern {
        let mut current = vec![false; value.len() + 1];
        if *token == b'*' {
            current[0] = previous[0];
            for index in 1..=value.len() {
                current[index] = previous[index] || current[index - 1];
            }
        } else {
            for index in 1..=value.len() {
                current[index] =
                    previous[index - 1] && (*token == b'?' || *token == value[index - 1]);
            }
        }
        previous = current;
    }
    previous[value.len()]
}

async fn preprint(arguments: &Value, runtime: &ToolRuntime<'_>) -> ToolOutcome {
    let action = string_arg(arguments, "action");
    let action = if action.is_empty() {
        "read".to_string()
    } else {
        action
    };
    if action == "mcp" {
        return research_mcp(arguments, runtime).await;
    }
    let identifier = match normalize_arxiv_id(&string_arg(arguments, "paper")) {
        Ok(identifier) => identifier,
        Err(error) => return ToolOutcome::failure(error),
    };
    if action == "discussion" {
        let url = format!("https://www.alphaxiv.org/abs/{identifier}");
        let parsed = match safe_public_url(&url) {
            Ok(url) => url,
            Err(error) => return ToolOutcome::failure(error),
        };
        let (response, final_url) = match fetch_public_response(parsed).await {
            Ok(value) => value,
            Err(error) => return ToolOutcome::failure(format!("读取社区页面失败：{error}")),
        };
        if !response.status().is_success() {
            return ToolOutcome::failure(format!("社区页面返回 HTTP {}", response.status()));
        }
        let body = match read_response_bytes(response, 2 * 1024 * 1024).await {
            Ok(body) => body,
            Err(error) => return ToolOutcome::failure(error),
        };
        let raw = super::documents::decode_text(&body);
        let content = html_to_readable(&raw)
            .chars()
            .take(12_000)
            .collect::<String>();
        return ToolOutcome::success(
            content,
            json!({
                "kind": "preprint_community",
                "paper": identifier,
                "url": final_url.as_str(),
                "layer": "community_page",
                "notice": "社区页面可能含生成式摘要或未加载的讨论，不能冒充论文原文或声称已取回全部注释。",
                "sources": [{"type": "community", "url": final_url.as_str(), "title": format!("alphaXiv {identifier}")}]
            }),
        );
    }
    if !matches!(action.as_str(), "read" | "figures") {
        return ToolOutcome::failure("preprint action 必须是 read、figures、discussion 或 mcp。");
    }
    let metadata = match arxiv_metadata(runtime.state, &identifier).await {
        Ok(metadata) => metadata,
        Err(error) => return ToolOutcome::failure(error),
    };
    let pdf_url =
        url::Url::parse(&format!("https://arxiv.org/pdf/{identifier}")).expect("固定 arXiv URL");
    let (response, final_url) = match fetch_public_response(pdf_url.clone()).await {
        Ok(value) => value,
        Err(error) => return ToolOutcome::failure(format!("下载论文 PDF 失败：{error}")),
    };
    if !response.status().is_success() {
        return ToolOutcome::failure(format!("论文 PDF 返回 HTTP {}", response.status()));
    }
    let bytes = match read_response_bytes(response, 80 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(error) => return ToolOutcome::failure(error),
    };
    let parsed = match super::parsing::extract(runtime.state, &format!("{identifier}.pdf"), &bytes)
        .await
    {
        Ok(document) => document,
        Err(error) => return ToolOutcome::failure(format!("PDF 已下载但无法提取文本：{error}")),
    };
    let all_pages = parsed
        .text
        .split('\u{000c}')
        .map(str::trim)
        .filter(|page| !page.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    let pages_available = all_pages.len().max(1);
    let requested_pages = arguments
        .get("pages")
        .and_then(Value::as_array)
        .map(|pages| {
            pages
                .iter()
                .filter_map(Value::as_u64)
                .filter_map(|page| usize::try_from(page).ok())
                .filter(|page| (1..=pages_available).contains(page))
                .take(20)
                .collect::<Vec<_>>()
        })
        .filter(|pages| !pages.is_empty())
        .unwrap_or_else(|| (1..=pages_available.min(20)).collect());
    let mut page_rows = Vec::new();
    for page in requested_pages {
        let text = if all_pages.len() > 1 {
            all_pages.get(page - 1).cloned().unwrap_or_default()
        } else if page == 1 {
            parsed.text.clone()
        } else {
            String::new()
        };
        page_rows.push(json!({
            "page": page,
            "url": format!("{pdf_url}#page={page}"),
            "text": text
        }));
    }
    let combined = page_rows
        .iter()
        .map(|page| {
            format!(
                "\n\n--- 第 {} 页 ---\n{}",
                page.get("page").and_then(Value::as_u64).unwrap_or(0),
                page.get("text").and_then(Value::as_str).unwrap_or("")
            )
        })
        .collect::<String>();
    let truncated = combined.chars().count() > 180_000;
    let text = combined.chars().take(180_000).collect::<String>();
    let payload = json!({
        "paper": metadata,
        "pdf_url": pdf_url.as_str(),
        "final_pdf_url": final_url.as_str(),
        "pages": page_rows,
        "page_count_detected": if all_pages.len() > 1 { Value::from(all_pages.len()) } else { Value::Null },
        "text": text,
        "truncated": truncated,
        "action": action,
        "layer": "primary_pdf_text",
        "visual_inspection": if action == "figures" {
            "PDF 已下载；如需检查图形，请结合页面链接逐页核对，不要仅凭文本声称已查看像素。"
        } else {
            "文本来自论文 PDF；图形与公式排版需用页面链接核验。"
        },
        "sources": [{
            "type": "paper",
            "provider": "arxiv",
            "url": format!("https://arxiv.org/abs/{identifier}"),
            "pdf_url": pdf_url.as_str(),
            "title": metadata.get("title")
        }]
    });
    ToolOutcome::success(
        serde_json::to_string_pretty(&payload).unwrap_or_else(|_| text.clone()),
        json!({
            "kind": "preprint",
            "paper": identifier,
            "pages": payload["pages"],
            "pdf_url": pdf_url.as_str(),
            "sources": payload["sources"]
        }),
    )
}

async fn research_audit(arguments: &Value, runtime: &ToolRuntime<'_>) -> ToolOutcome {
    let identifier = match normalize_arxiv_id(&string_arg(arguments, "paper")) {
        Ok(identifier) => identifier,
        Err(error) => return ToolOutcome::failure(error),
    };
    let paper = match preprint(
        &json!({
            "paper": identifier,
            "action": "read",
            "pages": arguments.get("pages").cloned().unwrap_or_else(|| json!([]))
        }),
        runtime,
    )
    .await
    {
        outcome if outcome.success => outcome,
        outcome => return outcome,
    };
    let repository = string_arg(arguments, "repository");
    let reference = {
        let value = string_arg(arguments, "ref");
        if value.is_empty() {
            "HEAD".to_string()
        } else {
            value
        }
    };
    let requested_files = arguments
        .get("files")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .take(8)
        .map(str::to_string)
        .collect::<Vec<_>>();
    let code = match github_snapshot(runtime.state, &repository, &reference, &requested_files).await
    {
        Ok(code) => code,
        Err(error) => return ToolOutcome::failure(format!("读取 GitHub 快照失败：{error}")),
    };
    let paper_text = paper
        .metadata
        .pointer("/paper/text")
        .and_then(Value::as_str)
        .unwrap_or("");
    let code_text = code
        .get("files")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|file| file.get("content").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    let checks = arguments
        .get("checks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(12)
        .map(|check| {
            let paper_quote = check
                .get("paper_quote")
                .and_then(Value::as_str)
                .unwrap_or("");
            let code_quote = check
                .get("code_quote")
                .and_then(Value::as_str)
                .unwrap_or("");
            let expected = check.get("expected").cloned().unwrap_or(Value::Null);
            let implemented = check.get("implemented").cloned().unwrap_or(Value::Null);
            let quotes_verified = (paper_quote.is_empty() || paper_text.contains(paper_quote))
                && (code_quote.is_empty() || code_text.contains(code_quote));
            let status = if !quotes_verified {
                "unverified_quote"
            } else if !expected.is_null() && !implemented.is_null() && expected != implemented {
                "candidate_discrepancy"
            } else {
                "verified_quote"
            };
            json!({
                "claim": check.get("claim").cloned().unwrap_or(Value::Null),
                "paper_quote": paper_quote,
                "file": check.get("file").cloned().unwrap_or(Value::Null),
                "code_quote": code_quote,
                "expected": expected,
                "implemented": implemented,
                "status": status,
                "quote_verified": quotes_verified
            })
        })
        .collect::<Vec<_>>();
    let payload = json!({
        "paper": paper.metadata.get("paper").cloned().unwrap_or(Value::Null),
        "implementation": code,
        "checks": checks,
        "scope": "仅静态检查主论文 PDF 与不可变 GitHub 提交，未执行仓库代码",
        "report_contract": "按“主张 → 论文页/引文 → 提交/文件/行号证据 → 候选差异或未验证 → 所需运行检查”输出；不能把未执行的仓库称为可复现。",
        "sources": [
            paper.metadata.get("sources").cloned().unwrap_or_else(|| json!([])),
            code.get("sources").cloned().unwrap_or_else(|| json!([]))
        ]
    });
    ToolOutcome::success(
        serde_json::to_string_pretty(&payload).unwrap_or_default(),
        json!({
            "kind": "research_audit",
            "paper": identifier,
            "repository": repository,
            "resolved_commit": code.get("commit"),
            "checks": payload["checks"],
            "sources": payload["sources"]
        }),
    )
}

async fn research_lit(arguments: &Value, runtime: &ToolRuntime<'_>) -> ToolOutcome {
    let topic = string_arg(arguments, "topic");
    if topic.trim().is_empty() {
        return ToolOutcome::failure("research_lit 需要非空 topic。");
    }
    let mut queries = vec![topic.clone()];
    queries.extend(
        arguments
            .get("followups")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .take(3)
            .map(str::to_string),
    );
    let mut papers = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (index, query) in queries.iter().enumerate() {
        let limit = if index == 0 { 8 } else { 5 };
        let results = match arxiv_results(runtime.state, query, limit, "relevance").await {
            Ok(results) => results,
            Err(error) => {
                return ToolOutcome::failure(format!(
                    "文献检索 `{query}` 失败：{error}。不得把不可用证据当作空搜索结果。"
                ))
            }
        };
        for mut paper in results {
            let key = paper
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if key.is_empty() || !seen.insert(key) {
                continue;
            }
            paper["query"] = Value::String(query.clone());
            paper["evidence_layer"] = Value::String("title_and_abstract_only".to_string());
            papers.push(paper);
        }
    }
    let payload = json!({
        "topic": topic,
        "queries": queries,
        "papers": papers,
        "evidence_matrix": papers.iter().map(|paper| json!({
            "title": paper.get("title"),
            "url": paper.get("url"),
            "query": paper.get("query"),
            "evidence": "仅标题与摘要；不能据此证明共识、争议或复现性",
            "next_step": "需要全文时调用 preprint 读取主 PDF"
        })).collect::<Vec<_>>(),
        "sources": papers.iter().map(|paper| json!({
            "type": "paper",
            "provider": "arxiv",
            "title": paper.get("title"),
            "url": paper.get("url")
        })).collect::<Vec<_>>()
    });
    ToolOutcome::success(
        serde_json::to_string_pretty(&payload).unwrap_or_default(),
        json!({
            "kind": "research_lit",
            "topic": payload["topic"],
            "papers": payload["papers"],
            "sources": payload["sources"]
        }),
    )
}

async fn research_mcp(arguments: &Value, runtime: &ToolRuntime<'_>) -> ToolOutcome {
    let name = string_arg(arguments, "mcp_tool");
    if name.is_empty() {
        return ToolOutcome::failure("action=mcp 时必须提供已授权的 mcp_tool。");
    }
    let args = arguments
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    call_mcp_tool(&name, &args, runtime).await
}

async fn call_mcp_tool(name: &str, arguments: &Value, runtime: &ToolRuntime<'_>) -> ToolOutcome {
    match super::mcp_cli::call_tool(runtime.state, name, arguments).await {
        Ok(result) => {
            let text = result
                .get("content")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .filter(|text| !text.is_empty())
                .unwrap_or_else(|| serde_json::to_string_pretty(&result).unwrap_or_default());
            ToolOutcome::success(
                text,
                json!({
                    "kind": "mcp_tool",
                    "tool": name,
                    "result": result
                }),
            )
        }
        Err(error) => ToolOutcome::failure(format!("MCP 工具 {name} 调用失败：{error}")),
    }
}

async fn arxiv_metadata(state: &AppState, identifier: &str) -> Result<Value, String> {
    let response = state
        .client
        .get("https://export.arxiv.org/api/query")
        .query(&[
            ("id_list", identifier.to_string()),
            ("max_results", "1".to_string()),
        ])
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|error| format!("arXiv 元数据请求失败：{error}"))?;
    if !response.status().is_success() {
        return Err(format!("arXiv 元数据返回 HTTP {}", response.status()));
    }
    let xml = response
        .text()
        .await
        .map_err(|error| format!("读取 arXiv 元数据失败：{error}"))?;
    let title_regex = Regex::new(r"(?s)<entry>.*?<title>(.*?)</title>").expect("固定正则");
    let summary_regex = Regex::new(r"(?s)<summary>(.*?)</summary>").expect("固定正则");
    let published_regex = Regex::new(r"(?s)<published>(.*?)</published>").expect("固定正则");
    let author_regex = Regex::new(r"(?s)<name>(.*?)</name>").expect("固定正则");
    Ok(json!({
        "arxiv_id": identifier,
        "title": capture_text(&title_regex, &xml),
        "abstract": capture_text(&summary_regex, &xml),
        "published": capture_text(&published_regex, &xml),
        "authors": author_regex
            .captures_iter(&xml)
            .map(|capture| clean_html(&capture[1]))
            .collect::<Vec<_>>(),
        "abs_url": format!("https://arxiv.org/abs/{identifier}")
    }))
}

fn normalize_arxiv_id(raw: &str) -> Result<String, String> {
    let value = raw
        .trim()
        .trim_end_matches('/')
        .trim_start_matches("https://arxiv.org/abs/")
        .trim_start_matches("http://arxiv.org/abs/")
        .trim_start_matches("https://arxiv.org/pdf/")
        .trim_start_matches("http://arxiv.org/pdf/")
        .trim_start_matches("https://www.alphaxiv.org/abs/")
        .trim_end_matches(".pdf");
    let valid = !value.is_empty()
        && value.len() <= 80
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '/' | 'v')
        })
        && !value.starts_with('/')
        && !value.ends_with('/')
        && !value.contains("//")
        && !value.contains("..");
    if valid {
        Ok(value.to_string())
    } else {
        Err("paper 必须是有效的 arXiv ID 或规范 URL。".to_string())
    }
}

async fn read_response_bytes(response: reqwest::Response, limit: usize) -> Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(format!("远程内容超过 {} 字节上限。", limit));
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = futures_util::StreamExt::next(&mut stream).await {
        let chunk = chunk.map_err(|error| format!("读取远程内容失败：{error}"))?;
        let remaining = limit.saturating_sub(body.len());
        if remaining == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
    Ok(body)
}

async fn github_snapshot(
    state: &AppState,
    repository: &str,
    reference: &str,
    requested_files: &[String],
) -> Result<Value, String> {
    let repository = repository
        .trim()
        .trim_start_matches("https://github.com/")
        .trim_start_matches("http://github.com/")
        .trim_end_matches('/')
        .trim_end_matches(".git");
    if repository.split('/').count() != 2
        || !repository.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | '/')
        })
    {
        return Err("repository 必须是 https://github.com/owner/repository。".to_string());
    }
    if !reference
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.'))
    {
        return Err("GitHub ref 只能包含字母、数字、点、下划线和连字符。".to_string());
    }
    let commit_url = format!("https://api.github.com/repos/{repository}/commits/{reference}");
    let commit = github_json(state, &commit_url).await?;
    let sha = commit
        .get("sha")
        .and_then(Value::as_str)
        .ok_or_else(|| "GitHub 提交响应缺少 SHA。".to_string())?
        .to_string();
    let tree_url = format!("https://api.github.com/repos/{repository}/git/trees/{sha}?recursive=1");
    let tree = github_json(state, &tree_url).await?;
    let all = tree
        .get("tree")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut candidates = Vec::new();
    if requested_files.is_empty() {
        for entry in all {
            if entry.get("type").and_then(Value::as_str) != Some("blob") {
                continue;
            }
            let path = entry.get("path").and_then(Value::as_str).unwrap_or("");
            let relevant = path.starts_with("src/")
                || path.starts_with("lib/")
                || path.starts_with("scripts/")
                || matches!(
                    path.rsplit('.')
                        .next()
                        .unwrap_or("")
                        .to_ascii_lowercase()
                        .as_str(),
                    "py" | "rs"
                        | "js"
                        | "ts"
                        | "tsx"
                        | "jsx"
                        | "go"
                        | "java"
                        | "cpp"
                        | "c"
                        | "h"
                        | "yaml"
                        | "yml"
                        | "toml"
                        | "json"
                        | "md"
                );
            if relevant {
                candidates.push(entry);
            }
            if candidates.len() >= 8 {
                break;
            }
        }
    } else {
        for requested in requested_files {
            if let Some(entry) = all
                .iter()
                .find(|entry| entry.get("path").and_then(Value::as_str) == Some(requested.as_str()))
            {
                candidates.push(entry.clone());
            }
        }
    }
    let mut files = Vec::new();
    let mut total_chars = 0usize;
    for entry in candidates {
        if total_chars >= 140_000 {
            break;
        }
        let path = entry.get("path").and_then(Value::as_str).unwrap_or("");
        let blob_sha = entry.get("sha").and_then(Value::as_str).unwrap_or("");
        if path.is_empty() || blob_sha.is_empty() {
            continue;
        }
        let blob = github_json(
            state,
            &format!("https://api.github.com/repos/{repository}/git/blobs/{blob_sha}"),
        )
        .await?;
        let encoded = blob.get("content").and_then(Value::as_str).unwrap_or("");
        let cleaned = encoded.replace(['\n', '\r'], "");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(cleaned)
            .map_err(|error| format!("GitHub blob 解码失败：{error}"))?;
        if bytes.len() > 1_000_000 {
            continue;
        }
        let content = String::from_utf8_lossy(&bytes)
            .chars()
            .take(40_000)
            .collect::<String>();
        total_chars += content.chars().count();
        files.push(json!({
            "path": path,
            "sha": blob_sha,
            "content": content
        }));
    }
    Ok(json!({
        "repository": repository,
        "ref": reference,
        "commit": sha,
        "files": files,
        "sources": [{
            "type": "github",
            "url": format!("https://github.com/{repository}/tree/{sha}"),
            "title": format!("{repository}@{sha}")
        }]
    }))
}

async fn github_json(state: &AppState, endpoint: &str) -> Result<Value, String> {
    let mut request = state
        .client
        .get(endpoint)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28");
    if let Some(token) = github_token(state) {
        request = request.bearer_auth(token);
    }
    let response = request
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|error| format!("GitHub 请求失败：{error}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let detail = response.text().await.unwrap_or_default();
        return Err(format!(
            "GitHub 返回 HTTP {status}：{}",
            detail.chars().take(800).collect::<String>()
        ));
    }
    response
        .json::<Value>()
        .await
        .map_err(|error| format!("GitHub 返回无效 JSON：{error}"))
}

fn rag_search(arguments: &Value, state: &AppState) -> ToolOutcome {
    let query = string_arg(arguments, "query");
    let kb_name = string_arg(arguments, "kb_name");
    let limit = integer_arg(arguments, "max_results", 6).clamp(1, 12) as usize;
    if query.trim().is_empty() {
        return ToolOutcome::failure("rag 需要非空 query。");
    }
    let rows = match state.database.list_resources("kb_chunk") {
        Ok(rows) => rows,
        Err(error) => return ToolOutcome::failure(error),
    };
    let mut scored = rows
        .into_iter()
        .filter(|row| {
            kb_name.trim().is_empty()
                || row.get("kb_name").and_then(Value::as_str) == Some(kb_name.trim())
        })
        .filter_map(|row| {
            let text = row
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let score = text_match_score(&query, text);
            (score > 0).then_some((score, row))
        })
        .collect::<Vec<_>>();
    scored.sort_by(|left, right| right.0.cmp(&left.0));
    let matches = scored
        .into_iter()
        .take(limit)
        .map(|(score, row)| {
            json!({
                "score": score,
                "kb_name": row.get("kb_name").and_then(Value::as_str).unwrap_or_default(),
                "source": row.get("source").and_then(Value::as_str).unwrap_or_default(),
                "locator": row.get("locator").cloned().unwrap_or(Value::Null),
                "content": row.get("content").and_then(Value::as_str).unwrap_or_default()
            })
        })
        .collect::<Vec<_>>();
    if matches.is_empty() {
        return ToolOutcome::failure("知识库中没有找到相关片段。请调整查询词或确认文件已索引。");
    }
    let content = matches
        .iter()
        .enumerate()
        .map(|(index, item)| {
            format!(
                "[{}] {} {}（相关度 {}）\n{}",
                index + 1,
                item["kb_name"].as_str().unwrap_or_default(),
                item["source"].as_str().unwrap_or_default(),
                item["score"].as_u64().unwrap_or_default(),
                item["content"].as_str().unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    ToolOutcome::success(content, json!({"provider": "local-kb", "results": matches}))
}

fn read_file(arguments: &Value, state: &AppState) -> ToolOutcome {
    let relative = string_arg(arguments, "path");
    if relative.trim().is_empty() {
        return ToolOutcome::failure("read_file 需要 path。");
    }
    let requested = PathBuf::from(relative.trim());
    let absolute = if requested.is_absolute() {
        requested
    } else {
        state.paths.data.join(requested)
    };
    let canonical = match absolute.canonicalize() {
        Ok(path) => path,
        Err(error) => return ToolOutcome::failure(format!("文件不可访问：{error}")),
    };
    let data_root = match state.paths.data.canonicalize() {
        Ok(path) => path,
        Err(_) => state.paths.data.clone(),
    };
    if !canonical.starts_with(&data_root) {
        return ToolOutcome::failure("拒绝读取安装目录 data 之外的文件。");
    }
    let max_chars = integer_arg(arguments, "max_chars", 40_000).clamp(100, 120_000) as usize;
    match std::fs::read_to_string(&canonical) {
        Ok(text) => ToolOutcome::success(
            text.chars().take(max_chars).collect::<String>(),
            json!({
                "provider": "local-file",
                "path": canonical.to_string_lossy(),
                "truncated": text.chars().count() > max_chars
            }),
        ),
        Err(error) => ToolOutcome::failure(format!("无法按 UTF-8 读取文件：{error}")),
    }
}

fn memory_search(arguments: &Value, state: &AppState) -> ToolOutcome {
    let query = string_arg(arguments, "query");
    let limit = integer_arg(arguments, "max_results", 8).clamp(1, 20) as usize;
    let rows = match state.database.list_resources("memory") {
        Ok(rows) => rows,
        Err(error) => return ToolOutcome::failure(error),
    };
    let mut scored = rows
        .into_iter()
        .filter_map(|row| {
            let text = row
                .get("content")
                .or_else(|| row.get("text"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let score = text_match_score(&query, text);
            (score > 0).then_some((score, row))
        })
        .collect::<Vec<_>>();
    scored.sort_by(|left, right| right.0.cmp(&left.0));
    let results = scored
        .into_iter()
        .take(limit)
        .map(|(_, row)| row)
        .collect::<Vec<_>>();
    ToolOutcome::success(
        if results.is_empty() {
            "没有找到相关长期记忆。".to_string()
        } else {
            serde_json::to_string_pretty(&results).unwrap_or_default()
        },
        json!({"provider": "local-memory", "results": results}),
    )
}

async fn zotero_search(arguments: &Value, state: &AppState) -> ToolOutcome {
    let query = string_arg(arguments, "query");
    let limit = integer_arg(arguments, "max_results", 10).clamp(1, 25);
    let settings = state
        .database
        .get_setting("zotero")
        .ok()
        .flatten()
        .unwrap_or_else(|| json!({}));
    let user_id = settings
        .get("user_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let api_key = settings
        .get("api_key")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if user_id.is_empty() || api_key.is_empty() {
        return ToolOutcome::failure("尚未配置 Zotero。请在设置中填写 user_id 与 api_key 后重试。");
    }
    let response = state
        .client
        .get(format!(
            "https://api.zotero.org/users/{}/items",
            url::form_urlencoded::byte_serialize(user_id.as_bytes()).collect::<String>()
        ))
        .query(&[
            ("q", query),
            ("limit", limit.to_string()),
            ("format", "json".to_string()),
        ])
        .header("Zotero-API-Key", api_key)
        .send()
        .await;
    let response = match response {
        Ok(response) => response,
        Err(error) => return ToolOutcome::failure(format!("Zotero 请求失败：{error}")),
    };
    let value = match json_response(response, "Zotero").await {
        Ok(value) => value,
        Err(error) => return ToolOutcome::failure(error),
    };
    ToolOutcome::success(
        serde_json::to_string_pretty(&value).unwrap_or_default(),
        json!({"provider": "zotero", "results": value}),
    )
}

async fn brainstorm(arguments: &Value, runtime: &ToolRuntime<'_>) -> ToolOutcome {
    let topic = string_arg(arguments, "topic");
    if topic.is_empty() {
        return ToolOutcome::failure("brainstorm 需要非空 topic。");
    }
    let context = string_arg(arguments, "context");
    let system = r#"你是一个广度优先的发散思考引擎。针对主题给出 5 到 8 个来自不同角度的具体方向；每个方向都必须说明可执行想法和简短理由。优先覆盖方法、框架、应用、风险、实验和产品方向，不要把不确定事实写成已验证结论。使用简洁 Markdown，每个方向包含“方向”和“理由”两行。"#;
    let mut user = format!("## 主题\n{topic}");
    if !context.is_empty() {
        user.push_str(&format!("\n\n## 背景与限制\n{context}"));
    }
    match call_text_llm(runtime, system, &user).await {
        Ok(answer) if !answer.trim().is_empty() => ToolOutcome::success(
            answer.clone(),
            json!({
                "provider": "llm",
                "kind": "brainstorm",
                "topic": topic,
                "answer": answer
            }),
        ),
        Ok(_) => ToolOutcome::failure("思维发散没有返回内容。"),
        Err(error) => ToolOutcome::failure(error),
    }
}

async fn reason(arguments: &Value, runtime: &ToolRuntime<'_>) -> ToolOutcome {
    let query = string_arg(arguments, "query");
    if query.is_empty() {
        return ToolOutcome::failure("reason 需要非空 query。");
    }
    let context = string_arg(arguments, "context");
    let system = r#"你是一个严谨的深度推理引擎。围绕给定焦点逐步分析并得出清晰结论。展示必要的推导过程，数学推导要写出关键代数步骤，逻辑推断要明确前提与结论。不得虚构来源；已有背景只作为待分析材料。最后用“结论”一节简洁收束。"#;
    let mut user = String::new();
    if !context.is_empty() {
        user.push_str(&format!("## 背景\n{context}\n\n"));
    }
    user.push_str(&format!("## 推理焦点\n{query}"));
    match call_text_llm(runtime, system, &user).await {
        Ok(answer) if !answer.trim().is_empty() => ToolOutcome::success(
            answer.clone(),
            json!({
                "provider": "llm",
                "kind": "reason",
                "query": query,
                "answer": answer
            }),
        ),
        Ok(_) => ToolOutcome::failure("深度推理没有返回内容。"),
        Err(error) => ToolOutcome::failure(error),
    }
}

async fn geogebra_analysis(arguments: &Value, runtime: &ToolRuntime<'_>) -> ToolOutcome {
    let question = string_arg(arguments, "question");
    if question.is_empty() {
        return ToolOutcome::failure("geogebra_analysis 需要非空 question。");
    }
    let image = match image_input(arguments, runtime) {
        Some(image) => image,
        None => {
            return ToolOutcome::failure(
                "没有可分析的图片。请先附上数学题图片，或提供 data 目录内的 image_path。",
            )
        }
    };
    let Some(llm_settings) = runtime.llm else {
        return ToolOutcome::failure("GeoGebra 图形分析需要可用的语言模型配置。");
    };
    let fallback_cancel = CancellationToken::new();
    let cancel = runtime.cancel.unwrap_or(&fallback_cancel);
    let system = r#"你是数学图形与 GeoGebra 专家。分析图片中的几何元素、约束和关系，并生成可直接粘贴到 GeoGebra 输入栏的命令。只输出一个 JSON 对象，不要 Markdown 代码围栏：
{"analysis":{"summary":"...","constraints":["..."],"geometric_relations":["..."]},"commands":["A = (0, 0)","..."]}
命令必须使用图片中真实可见的信息；无法确定的坐标使用参数或构造关系，不要编造比例。至少给出一个有效命令。"#;
    let mime = image.1;
    let encoded = base64::engine::general_purpose::STANDARD.encode(image.0);
    let messages = vec![
        json!({"role": "system", "content": system}),
        json!({
            "role": "user",
            "content": [
                {"type": "text", "text": format!("## 题目\n{question}\n\n请只输出约定的 JSON。")},
                {"type": "image_url", "image_url": {"url": format!("data:{mime};base64,{encoded}")}}
            ]
        }),
    ];
    let response = llm::stream_completion(
        &runtime.state.client,
        llm_settings,
        &messages,
        &[],
        None,
        cancel,
        |_chunk: LlmChunk| {},
    )
    .await;
    let answer = match response {
        Ok(response) => response.content.trim().to_string(),
        Err(error) => return ToolOutcome::failure(format!("GeoGebra 视觉分析失败：{error}")),
    };
    if answer.is_empty() {
        return ToolOutcome::failure("GeoGebra 视觉分析没有返回内容。");
    }
    let parsed = parse_json_object_text(&answer);
    let commands = parsed
        .as_ref()
        .and_then(|value| value.get("commands"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if commands.is_empty() {
        return ToolOutcome::success(
            format!("视觉模型返回了分析，但没有结构化 GeoGebra 命令：\n\n{answer}"),
            json!({
                "provider": "llm-vision",
                "has_image": true,
                "commands_count": 0,
                "final_ggb_commands": [],
                "raw_analysis": answer
            }),
        );
    }
    let mut content = String::from("GeoGebra 命令：\n\n```ggb\n");
    content.push_str(&commands.join("\n"));
    content.push_str("\n```");
    if let Some(summary) = parsed
        .as_ref()
        .and_then(|value| value.pointer("/analysis/summary"))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        content = format!("{}\n\n{}", summary.trim(), content);
    }
    ToolOutcome::success(
        content,
        json!({
            "provider": "llm-vision",
            "has_image": true,
            "commands_count": commands.len(),
            "final_ggb_commands": commands,
            "analysis": parsed.as_ref().and_then(|value| value.get("analysis")).cloned()
                .unwrap_or(Value::Null)
        }),
    )
}

async fn imagegen(arguments: &Value, runtime: &ToolRuntime<'_>) -> ToolOutcome {
    let prompt = string_arg(arguments, "prompt");
    if prompt.is_empty() {
        return ToolOutcome::failure("imagegen 需要非空 prompt。");
    }
    let count = integer_arg(arguments, "n", 1).clamp(1, 4) as usize;
    let mut overrides = serde_json::Map::new();
    let size = string_arg(arguments, "size");
    if !size.is_empty() {
        overrides.insert("size".to_string(), Value::String(size));
    }
    let (generation_catalog, profile_id, model_id) =
        match generation_catalog(runtime.state, "imagegen", overrides) {
            Ok(value) => value,
            Err(error) => return ToolOutcome::failure(error),
        };
    let mut artifacts = Vec::new();
    let mut failures = Vec::new();
    for index in 0..count {
        match media_services::generate_image(
            runtime.state,
            &generation_catalog,
            &profile_id,
            &model_id,
            &prompt,
        )
        .await
        {
            Ok((bytes, content_type)) => match save_generated_media(
                runtime,
                "imagegen",
                &prompt,
                index,
                &bytes,
                &content_type,
                "image",
            ) {
                Ok(item) => artifacts.push(item),
                Err(error) => failures.push(error),
            },
            Err(error) => failures.push(error.to_string()),
        }
    }
    media_tool_result("图片", artifacts, failures, &prompt, "image")
}

async fn videogen(arguments: &Value, runtime: &ToolRuntime<'_>) -> ToolOutcome {
    let prompt = string_arg(arguments, "prompt");
    if prompt.is_empty() {
        return ToolOutcome::failure("videogen 需要非空 prompt。");
    }
    let mut overrides = serde_json::Map::new();
    for (argument, field) in [("aspect_ratio", "aspect_ratio"), ("duration", "duration")] {
        let value = string_arg(arguments, argument);
        if !value.is_empty() {
            overrides.insert(field.to_string(), Value::String(value));
        }
    }
    let (generation_catalog, profile_id, model_id) =
        match generation_catalog(runtime.state, "videogen", overrides) {
            Ok(value) => value,
            Err(error) => return ToolOutcome::failure(error),
        };
    let result = media_services::generate_video(
        runtime.state,
        &generation_catalog,
        &profile_id,
        &model_id,
        &prompt,
        None,
    )
    .await;
    match result {
        Ok((bytes, content_type)) => match save_generated_media(
            runtime,
            "videogen",
            &prompt,
            0,
            &bytes,
            &content_type,
            "video",
        ) {
            Ok(item) => media_tool_result("视频", vec![item], Vec::new(), &prompt, "video"),
            Err(error) => ToolOutcome::failure(error),
        },
        Err(error) => ToolOutcome::failure(format!("视频生成失败：{error}")),
    }
}

async fn call_text_llm(
    runtime: &ToolRuntime<'_>,
    system: &str,
    user: &str,
) -> Result<String, String> {
    let settings = runtime
        .llm
        .ok_or_else(|| "当前工具需要可用的语言模型配置。".to_string())?;
    let fallback_cancel = CancellationToken::new();
    let cancel = runtime.cancel.unwrap_or(&fallback_cancel);
    let messages = vec![
        json!({"role": "system", "content": system}),
        json!({"role": "user", "content": user}),
    ];
    llm::stream_completion(
        &runtime.state.client,
        settings,
        &messages,
        &[],
        None,
        cancel,
        |_chunk: LlmChunk| {},
    )
    .await
    .map(|response| response.content.trim().to_string())
}

fn generation_catalog(
    state: &AppState,
    service_name: &str,
    overrides: serde_json::Map<String, Value>,
) -> Result<(Value, String, String), String> {
    let raw = state
        .database
        .get_setting("catalog")?
        .unwrap_or_else(|| json!({}));
    let mut resolved = catalog::resolve_catalog_connections(&raw);
    let (profile, model) = catalog::service_selection(&resolved, service_name, None, None)
        .ok_or_else(|| {
            format!(
                "尚未配置{}模型。请先在设置中启用服务并选择模型。",
                if service_name == "imagegen" {
                    "图像生成"
                } else {
                    "视频生成"
                }
            )
        })?;
    let profile_id = profile
        .get("id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "当前媒体服务配置缺少 ID。".to_string())?
        .to_string();
    let model = model.ok_or_else(|| "当前媒体服务没有选择模型。".to_string())?;
    let model_id = model
        .get("id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "当前媒体模型缺少 ID。".to_string())?
        .to_string();
    let profile = resolved
        .pointer_mut(&format!("/services/{service_name}/profiles"))
        .and_then(Value::as_array_mut)
        .and_then(|profiles| {
            profiles.iter_mut().find(|profile| {
                profile.get("id").and_then(Value::as_str) == Some(profile_id.as_str())
            })
        })
        .ok_or_else(|| "无法读取当前媒体服务配置。".to_string())?;
    let model = profile
        .get_mut("models")
        .and_then(Value::as_array_mut)
        .and_then(|models| {
            models
                .iter_mut()
                .find(|model| model.get("id").and_then(Value::as_str) == Some(model_id.as_str()))
        })
        .ok_or_else(|| "无法读取当前媒体模型配置。".to_string())?;
    if let Some(model) = model.as_object_mut() {
        model.extend(overrides);
    }
    Ok((resolved, profile_id, model_id))
}

fn save_generated_media(
    runtime: &ToolRuntime<'_>,
    kind: &str,
    prompt: &str,
    index: usize,
    bytes: &[u8],
    content_type: &str,
    attachment_type: &str,
) -> Result<Value, String> {
    if bytes.is_empty() {
        return Err("媒体服务返回了空文件。".to_string());
    }
    let extension = media_extension(content_type, kind);
    let slug = media_slug(prompt, kind);
    let suffix = if index == 0 {
        String::new()
    } else {
        format!("-{}", index + 1)
    };
    let turn = if runtime.turn_id.is_empty() {
        uuid::Uuid::new_v4().simple().to_string()
    } else {
        runtime
            .turn_id
            .chars()
            .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
            .take(80)
            .collect::<String>()
    };
    let filename = format!("{slug}{suffix}-{turn}.{extension}");
    let relative = format!("tools/{kind}/{filename}");
    let directory = runtime.state.paths.outputs.join("tools").join(kind);
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("无法创建媒体输出目录：{error}"))?;
    let path = directory.join(&filename);
    std::fs::write(&path, bytes)
        .map_err(|error| format!("无法保存生成的媒体文件 {}：{error}", path.display()))?;
    Ok(json!({
        "type": attachment_type,
        "url": format!("/files/outputs/{relative}"),
        "path": relative,
        "relative_path": relative,
        "filename": filename,
        "mime_type": content_type,
        "size_bytes": bytes.len(),
        "generated": true
    }))
}

fn media_tool_result(
    label: &str,
    artifacts: Vec<Value>,
    failures: Vec<String>,
    prompt: &str,
    kind: &str,
) -> ToolOutcome {
    if artifacts.is_empty() {
        return ToolOutcome::failure(format!(
            "{label}生成失败：{}",
            failures
                .into_iter()
                .next()
                .unwrap_or_else(|| "服务没有返回文件。".to_string())
        ));
    }
    let paths = artifacts
        .iter()
        .filter_map(|item| item.get("relative_path").and_then(Value::as_str))
        .map(|path| format!("- `{path}`"))
        .collect::<Vec<_>>()
        .join("\n");
    ToolOutcome::success(
        format!(
            "已生成 {} 个{label}文件：\n{paths}\n\n最终回答中请使用工具返回的精确相对路径引用文件。",
            artifacts.len()
        ),
        json!({
            "provider": match kind {
                "video" => "videogen",
                _ => "imagegen",
            },
            "kind": kind,
            "prompt": prompt,
            "artifacts": artifacts,
            "workspace_items": artifacts.clone(),
            "failures": failures
        }),
    )
}

fn media_extension(content_type: &str, fallback: &str) -> &'static str {
    match content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/quicktime" => "mov",
        _ if fallback == "video" => "mp4",
        _ => "png",
    }
}

fn media_slug(prompt: &str, fallback: &str) -> String {
    let mut words = Vec::new();
    let mut current = String::new();
    for character in prompt.chars() {
        if character.is_ascii_alphanumeric() {
            current.push(character.to_ascii_lowercase());
        } else if !current.is_empty() {
            words.push(std::mem::take(&mut current));
            if words.len() == 6 {
                break;
            }
        }
    }
    if !current.is_empty() && words.len() < 6 {
        words.push(current);
    }
    let value = words.join("_");
    let value = value.chars().take(40).collect::<String>();
    if value.is_empty() {
        fallback.to_string()
    } else {
        value
    }
}

fn image_input(arguments: &Value, runtime: &ToolRuntime<'_>) -> Option<(Vec<u8>, String)> {
    let explicit = string_arg(arguments, "image_path");
    if !explicit.is_empty() {
        if let Some(image) = read_image_path(runtime.state, &explicit) {
            return Some(image);
        }
    }
    for attachment in runtime
        .request
        .and_then(|request| request.get("attachments"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let mime = attachment
            .get("mime_type")
            .and_then(Value::as_str)
            .unwrap_or("image/png")
            .to_string();
        let is_image = attachment.get("type").and_then(Value::as_str) == Some("image")
            || mime.starts_with("image/");
        if !is_image {
            continue;
        }
        if let Some(encoded) = attachment.get("base64").and_then(Value::as_str) {
            let encoded = encoded
                .split_once(',')
                .filter(|(header, _)| header.starts_with("data:"))
                .map(|(_, payload)| payload)
                .unwrap_or(encoded);
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(encoded) {
                if !bytes.is_empty() && bytes.len() <= 32 * 1024 * 1024 {
                    return Some((bytes, mime));
                }
            }
        }
        for key in ["relative_path", "path", "url"] {
            if let Some(path) = attachment.get(key).and_then(Value::as_str) {
                if let Some(image) = read_image_path(runtime.state, path) {
                    return Some((image.0, mime.clone()));
                }
            }
        }
    }
    None
}

fn read_image_path(state: &AppState, raw: &str) -> Option<(Vec<u8>, String)> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let path = if raw.starts_with("/files/") {
        files::resolve_file(state, raw.trim_start_matches("/files/")).ok()?
    } else {
        let requested = PathBuf::from(raw);
        let path = if requested.is_absolute() {
            requested
        } else if matches!(
            raw.split('/').next().unwrap_or_default(),
            "outputs" | "attachments" | "library" | "workspace-items"
        ) {
            files::resolve_file(state, raw).ok()?
        } else {
            state.paths.data.join(requested)
        };
        path.canonicalize().ok()?
    };
    let root = state.paths.data.canonicalize().ok()?;
    if !path.starts_with(&root) || !path.is_file() {
        return None;
    }
    let metadata = std::fs::metadata(&path).ok()?;
    if metadata.len() == 0 || metadata.len() > 32 * 1024 * 1024 {
        return None;
    }
    let mime = mime_guess::from_path(&path)
        .first_or_octet_stream()
        .essence_str()
        .to_string();
    if !mime.starts_with("image/") {
        return None;
    }
    std::fs::read(path).ok().map(|bytes| (bytes, mime))
}

fn parse_json_object_text(raw: &str) -> Option<Value> {
    let trimmed = raw.trim();
    let unfenced = if trimmed.starts_with("```") {
        let content = trimmed
            .trim_start_matches("```json")
            .trim_start_matches("```JSON")
            .trim_start_matches("```");
        content.trim_end_matches("```").trim()
    } else {
        trimmed
    };
    if let Ok(value) = serde_json::from_str::<Value>(unfenced) {
        if value.is_object() {
            return Some(value);
        }
    }
    let start = unfenced.find('{')?;
    let end = unfenced.rfind('}')?;
    serde_json::from_str::<Value>(&unfenced[start..=end])
        .ok()
        .filter(Value::is_object)
}

fn workspace_list(arguments: &Value, runtime: &ToolRuntime<'_>) -> ToolOutcome {
    let root = match workspace_root(runtime) {
        Ok(root) => root,
        Err(error) => return ToolOutcome::failure(error),
    };
    let requested = string_arg(arguments, "path");
    let requested = if requested.is_empty() {
        "."
    } else {
        &requested
    };
    let directory = match resolve_workspace_path(&root, requested, false) {
        Ok(path) if path.is_dir() => path,
        Ok(_) => return ToolOutcome::failure("workspace_list 的 path 不是目录。"),
        Err(error) => return ToolOutcome::failure(error),
    };
    let depth = integer_arg(arguments, "depth", 1).clamp(1, 5) as usize;
    let limit = integer_arg(arguments, "limit", 200).clamp(1, 1000) as usize;
    let mut entries = Vec::new();
    for entry in WalkDir::new(&directory)
        .min_depth(1)
        .max_depth(depth)
        .follow_links(false)
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => return ToolOutcome::failure(format!("无法遍历工作区：{error}")),
        };
        if entry.path_is_symlink() {
            continue;
        }
        if entries.len() >= limit {
            break;
        }
        let relative = entry
            .path()
            .strip_prefix(&root)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .replace('\\', "/");
        let metadata = entry.metadata().ok();
        entries.push(json!({
            "path": relative,
            "name": entry.file_name().to_string_lossy(),
            "kind": if entry.file_type().is_dir() { "directory" } else { "file" },
            "size_bytes": metadata.as_ref().map(|value| value.len()).unwrap_or(0)
        }));
    }
    entries.sort_by(|left, right| {
        let left_key = (
            left.get("kind").and_then(Value::as_str) != Some("directory"),
            left.get("path").and_then(Value::as_str).unwrap_or(""),
        );
        let right_key = (
            right.get("kind").and_then(Value::as_str) != Some("directory"),
            right.get("path").and_then(Value::as_str).unwrap_or(""),
        );
        left_key.cmp(&right_key)
    });
    ToolOutcome::success(
        serde_json::to_string_pretty(&entries).unwrap_or_else(|_| "[]".to_string()),
        json!({
            "provider": "workspace",
            "path": requested,
            "entries": entries,
            "truncated": entries.len() >= limit
        }),
    )
}

fn workspace_read(arguments: &Value, runtime: &ToolRuntime<'_>) -> ToolOutcome {
    let relative = string_arg(arguments, "path");
    if relative.is_empty() {
        return ToolOutcome::failure("workspace_read 需要 path。");
    }
    let root = match workspace_root(runtime) {
        Ok(root) => root,
        Err(error) => return ToolOutcome::failure(error),
    };
    let path = match resolve_workspace_path(&root, &relative, false) {
        Ok(path) if path.is_file() => path,
        Ok(_) => return ToolOutcome::failure("workspace_read 的 path 不是文件。"),
        Err(error) => return ToolOutcome::failure(error),
    };
    let metadata = match std::fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) => return ToolOutcome::failure(format!("无法读取文件信息：{error}")),
    };
    if metadata.len() > 64 * 1024 * 1024 {
        return ToolOutcome::failure("文件超过 64 MB，无法作为文本读取。");
    }
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => return ToolOutcome::failure(format!("无法读取文件：{error}")),
    };
    let text = super::documents::decode_text(&bytes);
    let offset = integer_arg(arguments, "offset", 0).max(0) as usize;
    let limit = integer_arg(arguments, "limit", 20_000).clamp(1, 100_000) as usize;
    let total_chars = text.chars().count();
    let content = text.chars().skip(offset).take(limit).collect::<String>();
    ToolOutcome::success(
        content,
        json!({
            "provider": "workspace",
            "path": relative,
            "offset": offset,
            "limit": limit,
            "total_chars": total_chars,
            "truncated": offset.saturating_add(limit) < total_chars
        }),
    )
}

fn workspace_search(arguments: &Value, runtime: &ToolRuntime<'_>) -> ToolOutcome {
    let query = string_arg(arguments, "query");
    if query.is_empty() {
        return ToolOutcome::failure("workspace_search 需要非空 query。");
    }
    let root = match workspace_root(runtime) {
        Ok(root) => root,
        Err(error) => return ToolOutcome::failure(error),
    };
    let requested = string_arg(arguments, "path");
    let base = match resolve_workspace_path(
        &root,
        if requested.is_empty() {
            "."
        } else {
            &requested
        },
        false,
    ) {
        Ok(path) => path,
        Err(error) => return ToolOutcome::failure(error),
    };
    let limit = integer_arg(arguments, "limit", 50).clamp(1, 200) as usize;
    let query_lower = query.to_lowercase();
    let mut matches = Vec::new();
    let walker = if base.is_file() {
        WalkDir::new(&base).max_depth(0).into_iter()
    } else {
        WalkDir::new(&base).follow_links(false).into_iter()
    };
    for entry in walker {
        if matches.len() >= limit {
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if entry.path_is_symlink() || !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(&root)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .replace('\\', "/");
        let name_match = entry
            .file_name()
            .to_string_lossy()
            .to_lowercase()
            .contains(&query_lower);
        let metadata = entry.metadata().ok();
        if metadata.as_ref().map(|value| value.len()).unwrap_or(0) > 8 * 1024 * 1024 {
            if name_match {
                matches.push(json!({
                    "path": relative,
                    "match": "filename",
                    "excerpt": entry.file_name().to_string_lossy()
                }));
            }
            continue;
        }
        let bytes = match std::fs::read(entry.path()) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        let text = super::documents::decode_text(&bytes);
        if let Some(position) = text.to_lowercase().find(&query_lower) {
            matches.push(json!({
                "path": relative,
                "match": "content",
                "excerpt": excerpt_around(&text, position, 120, 500)
            }));
        } else if name_match {
            matches.push(json!({
                "path": relative,
                "match": "filename",
                "excerpt": entry.file_name().to_string_lossy()
            }));
        }
    }
    ToolOutcome::success(
        serde_json::to_string_pretty(&matches).unwrap_or_else(|_| "[]".to_string()),
        json!({
            "provider": "workspace",
            "query": query,
            "matches": matches,
            "truncated": matches.len() >= limit
        }),
    )
}

fn workspace_present(arguments: &Value, runtime: &ToolRuntime<'_>) -> ToolOutcome {
    let Some(items) = arguments.get("items").and_then(Value::as_array) else {
        return ToolOutcome::failure("workspace_present 需要 items 数组。");
    };
    if items.is_empty() || items.len() > 20 {
        return ToolOutcome::failure("workspace_present 一次需要展示 1 到 20 个文件。");
    }
    let root = match workspace_root(runtime) {
        Ok(root) => root,
        Err(error) => return ToolOutcome::failure(error),
    };
    let mut presented = Vec::new();
    for item in items {
        let relative = item
            .get("path")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        if relative.is_empty() {
            return ToolOutcome::failure("workspace_present 的每个 item 都需要 path。");
        }
        let source = match resolve_workspace_path(&root, relative, false) {
            Ok(path) if path.is_file() => path,
            Ok(_) => return ToolOutcome::failure(format!("工作区路径不是文件：{relative}")),
            Err(error) => return ToolOutcome::failure(error),
        };
        let metadata = match std::fs::metadata(&source) {
            Ok(metadata) => metadata,
            Err(error) => return ToolOutcome::failure(format!("无法读取文件信息：{error}")),
        };
        if metadata.len() > 256 * 1024 * 1024 {
            return ToolOutcome::failure(format!("文件超过 256 MB，无法展示：{relative}"));
        }
        let bytes = match std::fs::read(&source) {
            Ok(bytes) => bytes,
            Err(error) => return ToolOutcome::failure(format!("无法读取文件：{error}")),
        };
        let id = uuid::Uuid::new_v4().to_string();
        let filename = source
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("file.bin")
            .to_string();
        let target_dir = runtime
            .state
            .paths
            .uploads
            .join("workspace-items")
            .join(&id);
        if let Err(error) = std::fs::create_dir_all(&target_dir) {
            return ToolOutcome::failure(format!("无法创建展示目录：{error}"));
        }
        let target = target_dir.join(&filename);
        if let Err(error) = std::fs::write(&target, &bytes) {
            return ToolOutcome::failure(format!("无法保存展示文件：{error}"));
        }
        let mime = mime_guess::from_path(&source)
            .first_or_octet_stream()
            .essence_str()
            .to_string();
        presented.push(json!({
            "type": "workspace_item",
            "workspace_item_id": id,
            "path": relative,
            "relative_path": relative,
            "url": format!("/files/workspace-items/{id}/{filename}"),
            "filename": filename,
            "title": item.get("title").and_then(Value::as_str).unwrap_or(&filename),
            "caption": item.get("caption").and_then(Value::as_str).unwrap_or(""),
            "mime_type": mime,
            "size_bytes": bytes.len(),
            "sha256": hex::encode(Sha256::digest(&bytes)),
            "generated": false
        }));
    }
    let content = presented
        .iter()
        .filter_map(|item| item.get("relative_path").and_then(Value::as_str))
        .map(|path| format!("已展示 `{path}`。Markdown 中请使用这个精确路径。"))
        .collect::<Vec<_>>()
        .join("\n");
    ToolOutcome::success(
        content,
        json!({
            "provider": "workspace",
            "workspace_items": presented,
            "artifacts": presented
        }),
    )
}

fn write_note(arguments: &Value, runtime: &ToolRuntime<'_>) -> ToolOutcome {
    let title = string_arg(arguments, "title");
    let content = string_arg(arguments, "content");
    if title.is_empty() || content.is_empty() {
        return ToolOutcome::failure("write_note 需要非空 title 和 content。");
    }
    if content.chars().count() > 200_000 {
        return ToolOutcome::failure("笔记正文不能超过 200000 个字符。");
    }
    let id = uuid::Uuid::new_v4().simple().to_string();
    let now = crate::storage::now();
    let document = json!({
        "id": id,
        "doc_id": id,
        "title": title.chars().take(160).collect::<String>(),
        "content": content,
        "blocks": null,
        "revision": 1,
        "parent_id": null,
        "icon": "",
        "cover": "",
        "pinned": false,
        "created_at": now,
        "updated_at": now
    });
    if let Err(error) = runtime
        .state
        .database
        .put_resource("document", &id, &document)
    {
        return ToolOutcome::failure(format!("无法保存笔记：{error}"));
    }
    ToolOutcome::success(
        serde_json::to_string_pretty(&json!({
            "id": id,
            "title": document["title"],
            "url": format!("/co-writer/{id}")
        }))
        .unwrap_or_default(),
        json!({"kind": "note", "page": document}),
    )
}

fn search_notes(arguments: &Value, runtime: &ToolRuntime<'_>) -> ToolOutcome {
    let page_id = string_arg(arguments, "page_id");
    if !page_id.is_empty() {
        let document = match runtime.state.database.get_resource("document", &page_id) {
            Ok(Some(document)) => document,
            Ok(None) => return ToolOutcome::failure("笔记页面不存在。"),
            Err(error) => return ToolOutcome::failure(error),
        };
        return ToolOutcome::success(
            serde_json::to_string_pretty(&json!({
                "id": page_id,
                "title": document.get("title").cloned().unwrap_or(Value::Null),
                "content": document.get("content").cloned().unwrap_or(Value::Null),
                "url": format!("/co-writer/{page_id}")
            }))
            .unwrap_or_default(),
            json!({"kind": "note", "page": document}),
        );
    }
    let query = string_arg(arguments, "query").to_lowercase();
    let mut documents = match runtime.state.database.list_resources("document") {
        Ok(documents) => documents,
        Err(error) => return ToolOutcome::failure(error),
    };
    documents.sort_by(|left, right| {
        let left = left
            .get("updated_at")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let right = right
            .get("updated_at")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        right.total_cmp(&left)
    });
    let mut results = Vec::new();
    for document in documents {
        let id = document
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let title = document
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("未命名笔记");
        let content = document
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if query.is_empty()
            || title.to_lowercase().contains(&query)
            || content.to_lowercase().contains(&query)
        {
            let position = if query.is_empty() {
                0
            } else {
                (title.to_lowercase().find(&query))
                    .or_else(|| content.to_lowercase().find(&query))
                    .unwrap_or(0)
            };
            results.push(json!({
                "id": id,
                "title": title,
                "excerpt": excerpt_around(content, position, 80, 400),
                "updated_at": document.get("updated_at").cloned().unwrap_or(Value::Null),
                "url": format!("/co-writer/{id}")
            }));
        }
        if results.len() >= 20 {
            break;
        }
    }
    ToolOutcome::success(
        serde_json::to_string_pretty(&results).unwrap_or_else(|_| "[]".to_string()),
        json!({"kind": "notes", "query": query, "results": results}),
    )
}

fn list_notebook(arguments: &Value, runtime: &ToolRuntime<'_>) -> ToolOutcome {
    let notebook_id = string_arg(arguments, "notebook_id");
    if !notebook_id.is_empty() {
        let notebook = match runtime
            .state
            .database
            .get_resource("notebook", &notebook_id)
        {
            Ok(Some(notebook)) => notebook,
            Ok(None) => return ToolOutcome::failure("笔记本不存在。"),
            Err(error) => return ToolOutcome::failure(error),
        };
        let records = notebook
            .get("records")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|record| {
                json!({
                    "record_id": record.get("id").cloned().unwrap_or(Value::Null),
                    "type": record.get("type").cloned().unwrap_or(Value::Null),
                    "title": record.get("title").cloned().unwrap_or(Value::Null),
                    "summary": record.get("summary").cloned().unwrap_or(Value::Null),
                    "created_at": record.get("created_at").cloned().unwrap_or(Value::Null)
                })
            })
            .collect::<Vec<_>>();
        return ToolOutcome::success(
            serde_json::to_string_pretty(&json!({
                "notebook_id": notebook_id,
                "name": notebook.get("name").cloned().unwrap_or(Value::Null),
                "records": records
            }))
            .unwrap_or_default(),
            json!({"kind": "notebook", "notebook_id": notebook_id, "records": records}),
        );
    }
    let mut notebooks = match runtime.state.database.list_resources("notebook") {
        Ok(notebooks) => notebooks,
        Err(error) => return ToolOutcome::failure(error),
    };
    notebooks.sort_by(|left, right| {
        let left = left
            .get("updated_at")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let right = right
            .get("updated_at")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        right.total_cmp(&left)
    });
    let rows = notebooks
        .into_iter()
        .map(|notebook| {
            json!({
                "notebook_id": notebook.get("id").cloned().unwrap_or(Value::Null),
                "name": notebook.get("name").cloned().unwrap_or(Value::Null),
                "description": notebook.get("description").cloned().unwrap_or(Value::Null),
                "record_count": notebook.get("records").and_then(Value::as_array).map(Vec::len).unwrap_or(0),
                "updated_at": notebook.get("updated_at").cloned().unwrap_or(Value::Null)
            })
        })
        .collect::<Vec<_>>();
    ToolOutcome::success(
        serde_json::to_string_pretty(&rows).unwrap_or_else(|_| "[]".to_string()),
        json!({"kind": "notebooks", "notebooks": rows}),
    )
}

fn save_favorite(arguments: &Value, runtime: &ToolRuntime<'_>) -> ToolOutcome {
    let mode = string_arg(arguments, "mode");
    let notebook_id = string_arg(arguments, "notebook_id");
    if !matches!(mode.as_str(), "append" | "edit") {
        return ToolOutcome::failure("save_favorite 的 mode 必须是 append 或 edit。");
    }
    if notebook_id.is_empty() {
        return ToolOutcome::failure("save_favorite 需要 notebook_id。");
    }
    let mut notebook = match runtime
        .state
        .database
        .get_resource("notebook", &notebook_id)
    {
        Ok(Some(notebook)) => notebook,
        Ok(None) => return ToolOutcome::failure("笔记本不存在。"),
        Err(error) => return ToolOutcome::failure(error),
    };
    let notebook_name = notebook
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("笔记本")
        .to_string();
    let now = crate::storage::now();
    if mode == "edit" {
        let record_id = string_arg(arguments, "record_id");
        if record_id.is_empty() {
            return ToolOutcome::failure("编辑收藏记录需要 record_id。");
        }
        let Some(records) = notebook.get_mut("records").and_then(Value::as_array_mut) else {
            return ToolOutcome::failure("笔记本中没有记录。");
        };
        let Some(record) = records
            .iter_mut()
            .find(|record| record.get("id").and_then(Value::as_str) == Some(record_id.as_str()))
        else {
            return ToolOutcome::failure("收藏记录不存在。");
        };
        let title = string_arg(arguments, "title");
        if !title.is_empty() {
            record["title"] = Value::String(title);
        }
        let note = string_arg(arguments, "note");
        if !note.is_empty() {
            record["summary"] = Value::String(note);
        }
        record["updated_at"] = Value::from(now);
    } else {
        let title = string_arg(arguments, "title");
        if title.is_empty() {
            return ToolOutcome::failure("新建收藏记录需要 title。");
        }
        let mut content = string_arg(arguments, "content");
        if content.is_empty() {
            content = favorite_transcript(
                runtime.state,
                runtime.session_id,
                integer_arg(arguments, "turns_to_include", 3).clamp(1, 50) as usize,
            );
        }
        let note = string_arg(arguments, "note");
        if !note.is_empty() {
            content = format!("> {note}\n\n{content}");
        }
        let summary = note
            .chars()
            .take(280)
            .collect::<String>()
            .trim()
            .to_string();
        let record_id = uuid::Uuid::new_v4().simple().to_string();
        let record = json!({
            "id": record_id,
            "type": "chat",
            "title": title.chars().take(200).collect::<String>(),
            "summary": summary,
            "user_query": "",
            "output": content,
            "metadata": {"source": "save_favorite_tool"},
            "created_at": now,
            "updated_at": now
        });
        if !notebook.get("records").is_some_and(Value::is_array) {
            notebook["records"] = Value::Array(Vec::new());
        }
        notebook["records"]
            .as_array_mut()
            .expect("刚补齐 records 数组")
            .push(record.clone());
        notebook["updated_at"] = Value::from(now);
    }
    if let Err(error) = runtime
        .state
        .database
        .put_resource("notebook", &notebook_id, &notebook)
    {
        return ToolOutcome::failure(format!("无法保存收藏记录：{error}"));
    }
    ToolOutcome::success(
        format!("已保存到收藏笔记本“{notebook_name}”。"),
        json!({
            "kind": "favorite",
            "mode": mode,
            "notebook_id": notebook_id,
            "notebook_name": notebook_name
        }),
    )
}

async fn web_fetch(arguments: &Value, state: &AppState) -> ToolOutcome {
    let raw_url = string_arg(arguments, "url");
    let url = match safe_public_url(&raw_url) {
        Ok(url) => url,
        Err(error) => return ToolOutcome::failure(error),
    };
    let max_chars = integer_arg(arguments, "max_chars", 50_000).clamp(1_000, 200_000) as usize;
    let (response, final_url) = match fetch_public_response(url).await {
        Ok(value) => value,
        Err(error) => return ToolOutcome::failure(format!("读取网页失败：{error}")),
    };
    if !response.status().is_success() {
        return ToolOutcome::failure(format!("网页返回 HTTP {}", response.status()));
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !content_type.is_empty()
        && !content_type.contains("text/")
        && !content_type.contains("json")
        && !content_type.contains("xml")
        && !content_type.contains("javascript")
    {
        return ToolOutcome::failure(format!("该链接不是可读文本页面：{content_type}"));
    }
    let byte_limit = max_chars.saturating_mul(4).saturating_add(64 * 1024);
    if response
        .content_length()
        .is_some_and(|length| length > byte_limit as u64)
    {
        return ToolOutcome::failure(format!(
            "网页正文超过允许的大小上限（{} 字节）。",
            byte_limit
        ));
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = futures_util::StreamExt::next(&mut stream).await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => return ToolOutcome::failure(format!("读取网页正文失败：{error}")),
        };
        let remaining = byte_limit.saturating_sub(body.len());
        if remaining == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if body.len() >= byte_limit {
            break;
        }
    }
    let raw = super::documents::decode_text(&body);
    let title = Regex::new(r"(?is)<title[^>]*>(.*?)</title>")
        .expect("固定标题正则")
        .captures(&raw)
        .and_then(|capture| capture.get(1))
        .map(|capture| clean_html(capture.as_str()))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| final_url.host_str().unwrap_or("网页").to_string());
    let text = if content_type.contains("html") || raw.trim_start().starts_with('<') {
        html_to_readable(&raw)
    } else {
        raw
    };
    let truncated = text.chars().count() > max_chars;
    let markdown = text.chars().take(max_chars).collect::<String>();
    ToolOutcome::success(
        markdown.clone(),
        json!({
            "provider": "web",
            "url": final_url.as_str(),
            "title": title,
            "char_count": markdown.chars().count(),
            "truncated": truncated,
            "sources": [{
                "type": "web",
                "url": final_url.as_str(),
                "title": title
            }]
        }),
    )
}

async fn fetch_public_response(
    mut current: url::Url,
) -> Result<(reqwest::Response, url::Url), String> {
    const MAX_REDIRECTS: usize = 5;
    for redirect_count in 0..=MAX_REDIRECTS {
        current = safe_public_url(current.as_str())?;
        let host = current
            .host_str()
            .ok_or_else(|| "URL 缺少主机名。".to_string())?
            .trim_end_matches('.')
            .to_string();
        let port = current
            .port_or_known_default()
            .ok_or_else(|| "URL 缺少有效端口。".to_string())?;
        let addresses = tokio::net::lookup_host((host.as_str(), port))
            .await
            .map_err(|error| format!("无法解析主机名 {host}：{error}"))?
            .collect::<Vec<_>>();
        if addresses.is_empty() {
            return Err(format!("主机名 {host} 没有可用地址。"));
        }
        if addresses.iter().any(|address| is_blocked_ip(address.ip())) {
            return Err("拒绝访问解析到本机或内部网络的地址。".to_string());
        }
        let address = addresses[0];
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .user_agent(format!("DeepTutor/{}", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .resolve(&host, address)
            .build()
            .map_err(|error| format!("无法初始化网页请求客户端：{error}"))?;
        let response = client
            .get(current.clone())
            .send()
            .await
            .map_err(|error| format!("网络请求失败：{error}"))?;
        if matches!(
            response.status(),
            reqwest::StatusCode::MOVED_PERMANENTLY
                | reqwest::StatusCode::FOUND
                | reqwest::StatusCode::SEE_OTHER
                | reqwest::StatusCode::TEMPORARY_REDIRECT
                | reqwest::StatusCode::PERMANENT_REDIRECT
        ) {
            if redirect_count == MAX_REDIRECTS {
                return Err(format!("网页重定向超过 {MAX_REDIRECTS} 次。"));
            }
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| "网页返回重定向但缺少 Location。".to_string())?;
            current = current
                .join(location)
                .map_err(|error| format!("网页重定向地址无效：{error}"))?;
            continue;
        }
        return Ok((response, current));
    }
    Err("网页重定向次数过多。".to_string())
}

async fn github_query(arguments: &Value, state: &AppState) -> ToolOutcome {
    let query_type = string_arg(arguments, "query_type");
    let target = string_arg(arguments, "target");
    if target.is_empty() {
        return ToolOutcome::failure("github 需要非空 target。");
    }
    let endpoint = match github_endpoint(&query_type, &target) {
        Ok(endpoint) => endpoint,
        Err(error) => return ToolOutcome::failure(error),
    };
    let mut request = state
        .client
        .get(&endpoint)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28");
    if let Some(token) = github_token(state) {
        request = request.bearer_auth(token);
    }
    let response = match request.timeout(Duration::from_secs(30)).send().await {
        Ok(response) => response,
        Err(error) => return ToolOutcome::failure(format!("GitHub 请求失败：{error}")),
    };
    let status = response.status();
    if !status.is_success() {
        let detail = response.text().await.unwrap_or_default();
        return ToolOutcome::failure(format!(
            "GitHub 返回 HTTP {status}：{}",
            detail.chars().take(800).collect::<String>()
        ));
    }
    let value = match response.json::<Value>().await {
        Ok(value) => value,
        Err(error) => return ToolOutcome::failure(format!("GitHub 返回了无效 JSON：{error}")),
    };
    let rendered = serde_json::to_string_pretty(&value).unwrap_or_default();
    let rendered = rendered.chars().take(120_000).collect::<String>();
    ToolOutcome::success(
        rendered,
        json!({
            "provider": "github",
            "query_type": query_type,
            "target": target,
            "api_url": endpoint,
            "sources": [{
                "type": "github",
                "query_type": query_type,
                "target": target,
                "url": endpoint
            }]
        }),
    )
}

fn read_skill(arguments: &Value, state: &AppState) -> ToolOutcome {
    let name = string_arg(arguments, "name").to_ascii_lowercase();
    if name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_alphanumeric())
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
    {
        return ToolOutcome::failure("技能名称只能包含小写字母、数字和连字符。");
    }
    let relative_file = {
        let value = string_arg(arguments, "file");
        if value.is_empty() {
            "SKILL.md".to_string()
        } else {
            value
        }
    };
    let root = state.paths.data.join("skills");
    let skill_root = root.join(&name);
    if !skill_root.is_dir() {
        return ToolOutcome::failure("技能不存在或未安装。");
    }
    let path = match resolve_workspace_path(&skill_root, &relative_file, false) {
        Ok(path) => path,
        Err(error) => return ToolOutcome::failure(error),
    };
    if !path.is_file() {
        return ToolOutcome::failure("技能文件不存在。");
    }
    let metadata = match std::fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) => return ToolOutcome::failure(format!("无法读取技能文件信息：{error}")),
    };
    if metadata.len() > 2 * 1024 * 1024 {
        return ToolOutcome::failure("技能文件超过 2 MB。");
    }
    match std::fs::read(&path) {
        Ok(bytes) => ToolOutcome::success(
            super::documents::decode_text(&bytes),
            json!({
                "provider": "skill",
                "name": name,
                "file": relative_file
            }),
        ),
        Err(error) => ToolOutcome::failure(format!("无法读取技能文件：{error}")),
    }
}

fn question_bank(arguments: &Value, runtime: &ToolRuntime<'_>) -> ToolOutcome {
    let action = string_arg(arguments, "action");
    if action.is_empty() {
        return ToolOutcome::failure("question_bank 需要 action。");
    }
    let result = match action.as_str() {
        "overview" => question_bank_overview(runtime.state),
        "list" => question_bank_list(arguments, runtime.state),
        "record" => question_bank_record(arguments, runtime),
        "organize" | "unfile" => {
            question_bank_organize(arguments, runtime.state, action == "unfile")
        }
        "bookmark" => question_bank_bookmark(arguments, runtime.state),
        _ => Err("不支持的 question_bank action。".to_string()),
    };
    match result {
        Ok((content, metadata)) => ToolOutcome::success(content, metadata),
        Err(error) => ToolOutcome::failure(error),
    }
}

fn question_bank_overview(state: &AppState) -> Result<(String, Value), String> {
    let (counts, categories, entries) = state.database.with_connection(|connection| {
        let (total, wrong, unresolved, bookmarked): (i64, i64, i64, i64) = connection
            .query_row(
                "SELECT COUNT(*), \
                        COALESCE(SUM(CASE WHEN is_correct=0 THEN 1 ELSE 0 END),0), \
                        COALESCE(SUM(CASE WHEN resolved=0 THEN 1 ELSE 0 END),0), \
                        COALESCE(SUM(bookmarked),0) \
                 FROM qn_entries",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .map_err(|error| format!("无法统计题库：{error}"))?;
        let mut statement = connection
            .prepare(
                "SELECT c.id, c.name, COUNT(ec.entry_id) \
                 FROM qn_categories c \
                 LEFT JOIN qn_entry_categories ec ON ec.category_id=c.id \
                 GROUP BY c.id, c.name ORDER BY c.name COLLATE NOCASE",
            )
            .map_err(|error| format!("无法读取题库分类：{error}"))?;
        let rows = statement
            .query_map([], |row| {
                Ok(json!({
                    "id": row.get::<_, i64>(0)?,
                    "name": row.get::<_, String>(1)?,
                    "entry_count": row.get::<_, i64>(2)?
                }))
            })
            .map_err(|error| format!("无法读取题库分类：{error}"))?;
        let categories = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("无法解析题库分类：{error}"))?;
        let mut statement = connection
            .prepare(
                "SELECT id, question, is_correct, resolved, bookmarked, created_at \
                 FROM qn_entries ORDER BY created_at DESC LIMIT 20",
            )
            .map_err(|error| format!("无法读取最近题目：{error}"))?;
        let rows = statement
            .query_map([], |row| {
                Ok(json!({
                    "id": row.get::<_, i64>(0)?,
                    "question": row.get::<_, String>(1)?,
                    "is_correct": row.get::<_, i64>(2)? != 0,
                    "resolved": row.get::<_, i64>(3)? != 0,
                    "bookmarked": row.get::<_, i64>(4)? != 0,
                    "created_at": row.get::<_, f64>(5)?
                }))
            })
            .map_err(|error| format!("无法读取最近题目：{error}"))?;
        let entries = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("无法解析最近题目：{error}"))?;
        let counts = json!({
            "total": total,
            "wrong": wrong,
            "unresolved": unresolved,
            "bookmarked": bookmarked
        });
        Ok((counts, categories, entries))
    })?;
    let content = serde_json::to_string_pretty(&json!({
        "counts": counts,
        "categories": categories,
        "recent_entries": entries
    }))
    .unwrap_or_default();
    Ok((
        content,
        json!({
            "kind": "question_bank",
            "action": "overview",
            "counts": counts,
            "categories": categories,
            "recent_entries": entries
        }),
    ))
}

fn question_bank_list(arguments: &Value, state: &AppState) -> Result<(String, Value), String> {
    let filter = string_arg(arguments, "filter");
    let filter = if filter.is_empty() { "all" } else { &filter };
    let category = string_arg(arguments, "category").to_lowercase();
    let search = string_arg(arguments, "search").to_lowercase();
    let limit = integer_arg(arguments, "limit", 20).clamp(1, 100) as usize;
    let mut entries = Vec::new();
    state.database.with_connection(|connection| {
        let mut statement = connection
            .prepare(
                "SELECT id, question, user_answer, correct_answer, explanation, \
                        is_correct, resolved, bookmarked, created_at \
                 FROM qn_entries ORDER BY created_at DESC, id DESC LIMIT 1000",
            )
            .map_err(|error| format!("无法读取题库：{error}"))?;
        let rows = statement
            .query_map([], |row| {
                Ok(json!({
                    "id": row.get::<_, i64>(0)?,
                    "question": row.get::<_, String>(1)?,
                    "user_answer": row.get::<_, String>(2)?,
                    "correct_answer": row.get::<_, String>(3)?,
                    "explanation": row.get::<_, String>(4)?,
                    "is_correct": row.get::<_, i64>(5)? != 0,
                    "resolved": row.get::<_, i64>(6)? != 0,
                    "bookmarked": row.get::<_, i64>(7)? != 0,
                    "created_at": row.get::<_, f64>(8)?,
                    "categories": []
                }))
            })
            .map_err(|error| format!("无法读取题库：{error}"))?;
        entries = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("无法解析题库：{error}"))?;
        let mut statement = connection
            .prepare(
                "SELECT ec.entry_id, c.name FROM qn_entry_categories ec \
                 JOIN qn_categories c ON c.id=ec.category_id ORDER BY c.name COLLATE NOCASE",
            )
            .map_err(|error| format!("无法读取题目分类：{error}"))?;
        let links = statement
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| format!("无法读取题目分类：{error}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("无法解析题目分类：{error}"))?;
        for entry in &mut entries {
            let id = entry.get("id").and_then(Value::as_i64).unwrap_or_default();
            let names = links
                .iter()
                .filter(|(entry_id, _)| *entry_id == id)
                .map(|(_, name)| Value::String(name.clone()))
                .collect::<Vec<_>>();
            entry["categories"] = Value::Array(names);
        }
        Ok(())
    })?;
    entries.retain(|entry| {
        let categories = entry
            .get("categories")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let filter_match = match filter {
            "wrong" => entry.get("is_correct").and_then(Value::as_bool) == Some(false),
            "uncategorized" => categories.is_empty(),
            "bookmarked" => entry.get("bookmarked").and_then(Value::as_bool) == Some(true),
            _ => true,
        };
        let category_match = category.is_empty()
            || categories
                .iter()
                .filter_map(Value::as_str)
                .any(|value| value.to_lowercase() == category);
        let search_match = search.is_empty()
            || ["question", "user_answer", "correct_answer", "explanation"]
                .iter()
                .any(|field| {
                    entry
                        .get(*field)
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_lowercase()
                        .contains(&search)
                });
        filter_match && category_match && search_match
    });
    entries.truncate(limit);
    let content = if entries.is_empty() {
        "题库中没有符合条件的题目。".to_string()
    } else {
        serde_json::to_string_pretty(&entries).unwrap_or_default()
    };
    Ok((
        content,
        json!({
            "kind": "question_bank",
            "action": "list",
            "entries": entries,
            "filter": filter,
            "category": category,
            "search": search
        }),
    ))
}

fn question_bank_record(
    arguments: &Value,
    runtime: &ToolRuntime<'_>,
) -> Result<(String, Value), String> {
    let question = string_arg(arguments, "question");
    if question.is_empty() {
        return Err("record 操作需要 question。".to_string());
    }
    let user_answer = string_arg(arguments, "user_answer");
    let correct_answer = string_arg(arguments, "correct_answer");
    let explanation = string_arg(arguments, "explanation");
    let is_correct = arguments
        .get("is_correct")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let category = string_arg(arguments, "category");
    let question_id = format!(
        "qb-{}",
        &hex::encode(Sha256::digest(question.as_bytes()))[..20]
    );
    let now = crate::storage::now();
    let entry = runtime.state.database.with_connection(|connection| {
        connection
            .execute(
                "INSERT INTO qn_entries( \
                    session_id, origin_type, origin_ref, turn_id, question_id, question, \
                    question_type, user_answer, correct_answer, explanation, is_correct, \
                    resolved, bookmarked, created_at, updated_at \
                 ) VALUES(?1, 'conversation', ?2, ?3, ?4, ?5, 'text', ?6, ?7, ?8, ?9, 0, 0, ?10, ?10) \
                 ON CONFLICT(origin_type, origin_ref, question_id, turn_id) DO UPDATE SET \
                    question=excluded.question, user_answer=excluded.user_answer, \
                    correct_answer=excluded.correct_answer, explanation=excluded.explanation, \
                    is_correct=excluded.is_correct, updated_at=excluded.updated_at",
                rusqlite::params![
                    runtime.session_id,
                    runtime.session_id,
                    runtime.turn_id,
                    question_id,
                    question,
                    user_answer,
                    correct_answer,
                    explanation,
                    is_correct as i64,
                    now
                ],
            )
            .map_err(|error| format!("无法记录题库条目：{error}"))?;
        let entry_id: i64 = connection
            .query_row(
                "SELECT id FROM qn_entries \
                 WHERE origin_type='conversation' AND origin_ref=?1 AND question_id=?2 AND turn_id=?3",
                rusqlite::params![runtime.session_id, question_id, runtime.turn_id],
                |row| row.get(0),
            )
            .map_err(|error| format!("无法读取刚保存的题目：{error}"))?;
        if !category.is_empty() {
            let category_id = ensure_question_category(connection, &category)?;
            connection
                .execute(
                    "INSERT OR IGNORE INTO qn_entry_categories(entry_id, category_id) VALUES(?1, ?2)",
                    rusqlite::params![entry_id, category_id],
                )
                .map_err(|error| format!("无法归类题目：{error}"))?;
        }
        Ok(entry_id)
    })?;
    let metadata = json!({
        "kind": "question_bank",
        "action": "record",
        "entry_id": entry,
        "question": question,
        "category": category
    });
    Ok((
        serde_json::to_string_pretty(&metadata).unwrap_or_default(),
        metadata,
    ))
}

fn question_bank_organize(
    arguments: &Value,
    state: &AppState,
    unfile: bool,
) -> Result<(String, Value), String> {
    let category = string_arg(arguments, "category");
    if category.is_empty() {
        return Err("organize/unfile 操作需要 category。".to_string());
    }
    let entry_ids = question_entry_ids(arguments)?;
    let changed = state.database.with_connection(|connection| {
        let category_id = if unfile {
            connection
                .query_row(
                    "SELECT id FROM qn_categories WHERE name=?1 COLLATE NOCASE",
                    rusqlite::params![category],
                    |row| row.get::<_, i64>(0),
                )
                .ok()
        } else {
            Some(ensure_question_category(connection, &category)?)
        };
        let Some(category_id) = category_id else {
            return Ok(0usize);
        };
        let mut changed = 0usize;
        for entry_id in &entry_ids {
            changed += if unfile {
                connection
                    .execute(
                        "DELETE FROM qn_entry_categories WHERE entry_id=?1 AND category_id=?2",
                        rusqlite::params![entry_id, category_id],
                    )
                    .map_err(|error| format!("无法移除题目分类：{error}"))?
            } else {
                connection
                    .execute(
                        "INSERT OR IGNORE INTO qn_entry_categories(entry_id, category_id) \
                         SELECT id, ?2 FROM qn_entries WHERE id=?1",
                        rusqlite::params![entry_id, category_id],
                    )
                    .map_err(|error| format!("无法归类题目：{error}"))?
            };
        }
        Ok(changed)
    })?;
    let metadata = json!({
        "kind": "question_bank",
        "action": if unfile { "unfile" } else { "organize" },
        "category": category,
        "entry_ids": entry_ids,
        "changed": changed
    });
    Ok((
        serde_json::to_string_pretty(&metadata).unwrap_or_default(),
        metadata,
    ))
}

fn question_bank_bookmark(arguments: &Value, state: &AppState) -> Result<(String, Value), String> {
    let entry_ids = question_entry_ids(arguments)?;
    let bookmarked = arguments
        .get("bookmarked")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let changed = state.database.with_connection(|connection| {
        let mut changed = 0usize;
        for entry_id in &entry_ids {
            changed += connection
                .execute(
                    "UPDATE qn_entries SET bookmarked=?2, updated_at=?3 WHERE id=?1",
                    rusqlite::params![entry_id, bookmarked as i64, crate::storage::now()],
                )
                .map_err(|error| format!("无法更新题目收藏：{error}"))?;
        }
        Ok(changed)
    })?;
    let metadata = json!({
        "kind": "question_bank",
        "action": "bookmark",
        "bookmarked": bookmarked,
        "entry_ids": entry_ids,
        "changed": changed
    });
    Ok((
        serde_json::to_string_pretty(&metadata).unwrap_or_default(),
        metadata,
    ))
}

fn question_entry_ids(arguments: &Value) -> Result<Vec<i64>, String> {
    let mut ids = arguments
        .get("entry_ids")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_i64)
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() || ids.len() > 500 {
        return Err("entry_ids 必须包含 1 到 500 个题目编号。".to_string());
    }
    Ok(ids)
}

fn ensure_question_category(connection: &rusqlite::Connection, name: &str) -> Result<i64, String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 80 {
        return Err("题库分类名称必须为 1 到 80 个字符。".to_string());
    }
    connection
        .execute(
            "INSERT INTO qn_categories(name, created_at) VALUES(?1, ?2) \
             ON CONFLICT(name) DO NOTHING",
            rusqlite::params![name, crate::storage::now()],
        )
        .map_err(|error| format!("无法创建题库分类：{error}"))?;
    connection
        .query_row(
            "SELECT id FROM qn_categories WHERE name=?1 COLLATE NOCASE",
            rusqlite::params![name],
            |row| row.get(0),
        )
        .map_err(|error| format!("无法读取题库分类：{error}"))
}

fn workspace_root(runtime: &ToolRuntime<'_>) -> Result<PathBuf, String> {
    let mut workspace_id = runtime
        .request
        .and_then(|request| request.get("workspace_id"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if workspace_id.is_empty() && !runtime.session_id.is_empty() {
        if let Ok(Some(session)) = runtime.state.database.get_session_meta(runtime.session_id) {
            workspace_id = session
                .get("content_workspace_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if workspace_id.is_empty() {
                workspace_id = session
                    .pointer("/preferences/workspace_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
            }
        }
    }
    super::workspace_data::workspace_root_for_tools(runtime.state, &workspace_id)
}

fn resolve_workspace_path(
    root: &Path,
    relative: &str,
    allow_missing: bool,
) -> Result<PathBuf, String> {
    let relative = relative.trim().replace('\\', "/");
    if relative.contains('\0') {
        return Err("路径包含空字符。".to_string());
    }
    let requested = PathBuf::from(relative.trim_start_matches('/'));
    if requested.components().any(|component| {
        !matches!(
            component,
            std::path::Component::Normal(_) | std::path::Component::CurDir
        )
    }) {
        return Err("拒绝访问工作区之外的路径。".to_string());
    }
    let root = root
        .canonicalize()
        .map_err(|error| format!("无法解析工作区目录：{error}"))?;
    let candidate = root.join(requested);
    if candidate.exists() {
        let canonical = candidate
            .canonicalize()
            .map_err(|error| format!("无法解析路径：{error}"))?;
        if !canonical.starts_with(&root) {
            return Err("拒绝访问工作区之外的路径。".to_string());
        }
        return Ok(canonical);
    }
    if !allow_missing {
        return Err("工作区路径不存在。".to_string());
    }
    let parent = candidate
        .parent()
        .ok_or_else(|| "目标路径没有父目录。".to_string())?
        .canonicalize()
        .map_err(|error| format!("无法解析目标目录：{error}"))?;
    if !parent.starts_with(&root) {
        return Err("拒绝访问工作区之外的路径。".to_string());
    }
    Ok(candidate)
}

fn excerpt_around(text: &str, byte_position: usize, before: usize, after: usize) -> String {
    let characters = text.chars().collect::<Vec<_>>();
    let byte_position = byte_position.min(text.len());
    let char_position = text[..byte_position].chars().count();
    let start = char_position.saturating_sub(before);
    let end = (char_position + after).min(characters.len());
    let mut excerpt = characters[start..end].iter().collect::<String>();
    if start > 0 {
        excerpt.insert_str(0, "…");
    }
    if end < characters.len() {
        excerpt.push('…');
    }
    excerpt
}

fn favorite_transcript(state: &AppState, session_id: &str, turns: usize) -> String {
    if session_id.is_empty() {
        return "（当前会话没有可保存的历史记录。）".to_string();
    }
    let mut messages = state
        .database
        .get_messages(session_id)
        .unwrap_or_default()
        .into_iter()
        .filter(|message| {
            matches!(
                message.get("role").and_then(Value::as_str),
                Some("user" | "assistant")
            )
        })
        .collect::<Vec<_>>();
    let keep = turns.saturating_mul(2);
    if messages.len() > keep {
        messages = messages.split_off(messages.len() - keep);
    }
    messages
        .into_iter()
        .filter_map(|message| {
            let role = match message.get("role").and_then(Value::as_str) {
                Some("user") => "用户",
                Some("assistant") => "DeepTutor",
                _ => return None,
            };
            let content = message
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            (!content.is_empty()).then(|| format!("### {role}\n\n{content}"))
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn safe_public_url(raw: &str) -> Result<url::Url, String> {
    let url = url::Url::parse(raw).map_err(|error| format!("URL 无效：{error}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("只允许 http:// 或 https:// URL。".to_string());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URL 不能包含内嵌凭据。".to_string());
    }
    let host = url
        .host_str()
        .ok_or_else(|| "URL 缺少主机名。".to_string())?
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".internal")
    {
        return Err("拒绝访问本机或内部网络地址。".to_string());
    }
    if let Ok(address) = host.parse::<std::net::IpAddr>() {
        if is_blocked_ip(address) {
            return Err("拒绝访问本机或内部网络地址。".to_string());
        }
    }
    Ok(url)
}

fn is_blocked_ip(address: std::net::IpAddr) -> bool {
    match address {
        std::net::IpAddr::V4(address) => {
            let octets = address.octets();
            address.is_private()
                || address.is_loopback()
                || address.is_link_local()
                || address.is_broadcast()
                || address.is_unspecified()
                || address.is_multicast()
                || octets[0] == 0
                || octets[0] >= 224
                || (octets[0] == 100 && (64..=127).contains(&octets[1]))
                || (octets[0] == 198 && matches!(octets[1], 18 | 19))
                || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
                || (octets[0] == 192 && octets[1] == 0 && octets[2] == 2)
                || (octets[0] == 198 && octets[1] == 51 && octets[2] == 100)
                || (octets[0] == 203 && octets[1] == 0 && octets[2] == 113)
        }
        std::net::IpAddr::V6(address) => {
            let segments = address.segments();
            address.is_loopback()
                || address.is_unspecified()
                || address.is_multicast()
                || address.is_unique_local()
                || address.is_unicast_link_local()
                || address
                    .to_ipv4_mapped()
                    .is_some_and(|mapped| is_blocked_ip(std::net::IpAddr::V4(mapped)))
                || (segments[0] == 0x2001 && segments[1] == 0x0db8)
                || (segments[0] == 0x2001 && segments[1] == 0)
        }
    }
}

fn html_to_readable(html: &str) -> String {
    let without_active = Regex::new(r"(?is)<(script|style|noscript|svg|template)[^>]*>.*?</\1>")
        .expect("固定脚本正则")
        .replace_all(html, " ");
    let block_break = Regex::new(
        r"(?i)</?(?:p|div|section|article|header|footer|li|tr|h[1-6]|blockquote|pre)[^>]*>",
    )
    .expect("固定块级正则");
    let with_breaks = block_break.replace_all(&without_active, "\n");
    clean_html(&with_breaks)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn github_endpoint(query_type: &str, target: &str) -> Result<String, String> {
    const API: &str = "https://api.github.com";
    let normalized = target
        .trim()
        .trim_end_matches('/')
        .trim_start_matches("https://github.com/")
        .trim_start_matches("http://github.com/");
    let endpoint = match query_type {
        "repo" => format!("{API}/repos/{normalized}"),
        "pr" | "issue" => {
            let (repository, number) = normalized
                .split_once('#')
                .or_else(|| {
                    let mut parts = normalized.rsplitn(2, '/');
                    let number = parts.next()?;
                    let repository = parts.next()?;
                    number.parse::<u64>().ok().map(|_| (repository, number))
                })
                .ok_or_else(|| "PR/Issue target 应包含 owner/repo#编号。".to_string())?;
            let number = number
                .parse::<u64>()
                .map_err(|_| "PR/Issue 编号必须是整数。".to_string())?;
            if query_type == "pr" {
                format!("{API}/repos/{repository}/pulls/{number}")
            } else {
                format!("{API}/repos/{repository}/issues/{number}")
            }
        }
        "run" => format!("{API}/repos/{normalized}/actions/runs?per_page=10"),
        "api" => {
            let path = normalized.trim_start_matches('/');
            if path.is_empty()
                || path.contains("://")
                || path
                    .split('/')
                    .any(|segment| segment == ".." || segment == ".")
            {
                return Err("GitHub API 路径无效。".to_string());
            }
            format!("{API}/{path}")
        }
        _ => return Err("query_type 必须是 pr、issue、run、repo 或 api。".to_string()),
    };
    Ok(endpoint)
}

fn github_token(state: &AppState) -> Option<String> {
    state
        .database
        .get_setting("github")
        .ok()
        .flatten()
        .and_then(|value| {
            value
                .get("token")
                .or_else(|| value.get("api_key"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        })
        .or_else(|| {
            std::env::var("GITHUB_TOKEN")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
}

async fn json_response(response: reqwest::Response, provider: &str) -> Result<Value, String> {
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(format!(
            "{provider} 返回 HTTP {status}：{}",
            text.chars().take(800).collect::<String>()
        ));
    }
    response
        .json()
        .await
        .map_err(|error| format!("{provider} 返回了无效 JSON：{error}"))
}

fn string_arg(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn optional_string_arg(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn join_string_values(values: &[Value]) -> String {
    values
        .iter()
        .filter_map(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .collect::<Vec<_>>()
        .join("、")
}

fn integer_arg(value: &Value, key: &str, default: i64) -> i64 {
    value
        .get(key)
        .and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
        })
        .unwrap_or(default)
}

fn text_match_score(query: &str, text: &str) -> u64 {
    let query = query.to_lowercase();
    let text_lower = text.to_lowercase();
    let mut score = 0u64;
    for token in query
        .split(|character: char| character.is_whitespace() || character.is_ascii_punctuation())
        .filter(|token| token.chars().count() >= 2)
    {
        score += text_lower.matches(token).count() as u64 * 10;
    }
    if text_lower.contains(&query) {
        score += 100;
    }
    score
}

fn capture_text(regex: &Regex, text: &str) -> String {
    regex
        .captures(text)
        .and_then(|capture| capture.get(1))
        .map(|capture| clean_html(capture.as_str()))
        .unwrap_or_default()
}

fn clean_html(html: &str) -> String {
    let tags = Regex::new(r"(?s)<[^>]+>").expect("固定正则");
    let text = tags
        .replace_all(html, "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    decode_entities(&text)
}

fn decode_entities(text: &str) -> String {
    text.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
}

fn decode_redirect_url(raw: &str) -> String {
    let raw = decode_entities(raw);
    let Some(query) = raw.split_once('?').map(|(_, query)| query) else {
        return raw;
    };
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key == "uddg" {
            return url::form_urlencoded::parse(value.as_bytes())
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join("&")
                .split_once('=')
                .map(|(_, value)| value.to_string())
                .unwrap_or_else(|| value.to_string());
        }
    }
    raw
}

pub fn ensure_inside(path: &Path, root: &Path) -> bool {
    path.canonicalize()
        .map(|path| path.starts_with(root.canonicalize().unwrap_or_else(|_| root.to_path_buf())))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 沉浸式阅读回合会向模型注册全部阅读工具() {
        let request = json!({
            "workspace_mode": "immersive_reading",
            "reading_workspace_id": "workspace-test",
            "reading_material_id": "material-test"
        });
        let enabled = super::super::reading::tool_names()
            .iter()
            .map(|name| (*name).to_string())
            .collect::<Vec<_>>();

        let definitions = definitions("chat", &enabled, Some(&request), &[]);
        let names = definitions
            .iter()
            .filter_map(|definition| definition.pointer("/function/name").and_then(Value::as_str))
            .collect::<Vec<_>>();

        assert_eq!(names.len(), 8);
        for name in super::super::reading::tool_names() {
            assert!(names.contains(name), "缺少阅读工具：{name}");
        }
    }

    #[test]
    fn 普通聊天不会注册沉浸式阅读工具() {
        let enabled = super::super::reading::tool_names()
            .iter()
            .map(|name| (*name).to_string())
            .collect::<Vec<_>>();
        let definitions = definitions("chat", &enabled, Some(&json!({})), &[]);

        assert!(definitions.is_empty());
    }

    #[test]
    fn html_text_is_cleaned() {
        assert_eq!(clean_html("<b>Hello</b>&amp; world"), "Hello& world");
    }

    #[test]
    fn local_relevance_prefers_exact_match() {
        assert!(
            text_match_score("傅里叶变换", "傅里叶变换把时域映射到频域")
                > text_match_score("傅里叶变换", "矩阵乘法")
        );
    }
}
