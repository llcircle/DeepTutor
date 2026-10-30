use super::AppState;
use serde_json::{json, Value};
use std::collections::HashSet;

const MCP_TOOL_DOMAIN: &str = "mcp_tool_schema";
const PROMPT_TOOL_DOMAIN: &str = "prompt_tool_schema";
const SESSION_DOMAIN: &str = "deferred_tool_session";

pub fn has_available(state: &AppState) -> Result<bool, String> {
    Ok(!catalog(state)?.is_empty())
}

/// 返回已经由 `load_tools` 装入当前会话的扩展工具定义。
pub fn loaded_definitions(state: &AppState, session_id: &str) -> Result<Vec<Value>, String> {
    let loaded = loaded_names(state, session_id)?;
    if loaded.is_empty() {
        return Ok(Vec::new());
    }
    Ok(catalog(state)?
        .into_iter()
        .filter(|entry| {
            entry
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| loaded.contains(name))
        })
        .filter_map(|entry| entry.get("definition").cloned())
        .collect())
}

pub fn load(state: &AppState, session_id: &str, names: &[String]) -> Result<Value, String> {
    if names.is_empty() {
        return Err("load_tools 需要非空 names 数组。".to_string());
    }
    let catalog = catalog(state)?;
    let mut available = HashSet::new();
    for entry in &catalog {
        if let Some(name) = entry.get("name").and_then(Value::as_str) {
            available.insert(name.to_string());
        }
    }
    let mut already_loaded = loaded_names(state, session_id)?;
    let mut loaded = Vec::new();
    let mut unknown = Vec::new();
    let mut already = Vec::new();
    for name in names {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        if !available.contains(name) {
            unknown.push(name.to_string());
            continue;
        }
        if !already_loaded.insert(name.to_string()) {
            already.push(name.to_string());
            continue;
        }
        state.database.put_resource(
            SESSION_DOMAIN,
            &session_key(session_id, name),
            &json!({
                "session_id": session_id,
                "tool_name": name,
                "loaded_at": crate::storage::now()
            }),
        )?;
        loaded.push(name.to_string());
    }
    Ok(json!({
        "loaded": loaded,
        "already_loaded": already,
        "unknown": unknown
    }))
}

pub fn find_definition(state: &AppState, name: &str) -> Result<Option<Value>, String> {
    Ok(catalog(state)?.into_iter().find_map(|entry| {
        (entry.get("name").and_then(Value::as_str) == Some(name))
            .then(|| entry.get("definition").cloned())
            .flatten()
    }))
}

pub fn provider_entry(state: &AppState, name: &str) -> Result<Option<Value>, String> {
    Ok(catalog(state)?
        .into_iter()
        .find(|entry| entry.get("name").and_then(Value::as_str) == Some(name)))
}

pub fn store_mcp_tools(state: &AppState, server: &str, tools: &[Value]) -> Result<(), String> {
    for tool in tools {
        let raw_name = tool.get("name").and_then(Value::as_str).unwrap_or("");
        if raw_name.is_empty() {
            continue;
        }
        let qualified = format!("mcp_{}_{}", sanitize(server), sanitize(raw_name));
        let description = tool
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("MCP 扩展工具");
        let schema = tool
            .get("inputSchema")
            .or_else(|| tool.get("input_schema"))
            .or_else(|| tool.get("parameters"))
            .cloned()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
        state.database.put_resource(
            MCP_TOOL_DOMAIN,
            &format!("{server}:{raw_name}"),
            &json!({
                "server": server,
                "server_tool": raw_name,
                "name": qualified,
                "description": description,
                "input_schema": schema,
                "definition": {
                    "type": "function",
                    "function": {
                        "name": qualified,
                        "description": format!("[MCP {server}] {description}"),
                        "parameters": schema
                    }
                }
            }),
        )?;
    }
    Ok(())
}

pub fn remove_mcp_server_tools(state: &AppState, server: &str) -> Result<(), String> {
    for entry in state.database.list_resources(MCP_TOOL_DOMAIN)? {
        if entry.get("server").and_then(Value::as_str) == Some(server) {
            if let Some(id) = entry.get("id").and_then(Value::as_str) {
                state.database.delete_resource(MCP_TOOL_DOMAIN, id)?;
            }
        }
    }
    Ok(())
}

pub fn store_prompt_tool(
    state: &AppState,
    name: &str,
    provider_kind: &str,
    provider_id: &str,
    definition: &Value,
) -> Result<(), String> {
    state.database.put_resource(
        PROMPT_TOOL_DOMAIN,
        name,
        &json!({
            "name": name,
            "provider_kind": provider_kind,
            "provider_id": provider_id,
            "definition": definition
        }),
    )?;
    Ok(())
}

fn catalog(state: &AppState) -> Result<Vec<Value>, String> {
    let mut entries = state.database.list_resources(MCP_TOOL_DOMAIN)?;
    entries.extend(state.database.list_resources(PROMPT_TOOL_DOMAIN)?);
    entries.sort_by(|left, right| {
        left.get("name")
            .and_then(Value::as_str)
            .cmp(&right.get("name").and_then(Value::as_str))
    });
    Ok(entries)
}

fn loaded_names(state: &AppState, session_id: &str) -> Result<HashSet<String>, String> {
    Ok(state
        .database
        .list_resources(SESSION_DOMAIN)?
        .into_iter()
        .filter(|entry| entry.get("session_id").and_then(Value::as_str) == Some(session_id))
        .filter_map(|entry| {
            entry
                .get("tool_name")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect())
}

fn session_key(session_id: &str, name: &str) -> String {
    format!("{session_id}:{name}")
}

fn sanitize(value: &str) -> String {
    let mut output = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    output = output.trim_matches('_').to_string();
    if output.is_empty() {
        "tool".to_string()
    } else {
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualified_mcp_names_are_stable_and_safe() {
        assert_eq!(sanitize("alpha-xiv"), "alpha_xiv");
        assert_eq!(sanitize("工具"), "tool");
    }
}
