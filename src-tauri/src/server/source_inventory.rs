use super::AppState;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

const MANIFEST_PREVIEW_CHARS: usize = 2_000;
const MAX_TEXT_BASE64_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug)]
struct SourceEntry {
    sid: String,
    kind: String,
    name: String,
    full_text: String,
    fresh: bool,
    first_seen_turn: usize,
    availability_note: String,
}

#[derive(Default)]
struct SourceInventory {
    entries: Vec<SourceEntry>,
    positions: HashMap<String, usize>,
}

impl SourceInventory {
    fn add(&mut self, entry: SourceEntry) {
        if entry.sid.is_empty()
            || (entry.full_text.trim().is_empty() && entry.availability_note.is_empty())
        {
            return;
        }
        if let Some(position) = self.positions.get(&entry.sid).copied() {
            if entry.fresh && !self.entries[position].fresh {
                self.entries[position] = entry;
            }
            return;
        }
        self.positions.insert(entry.sid.clone(), self.entries.len());
        self.entries.push(entry);
    }

    fn contains(&self, sid: &str) -> bool {
        self.positions.contains_key(sid)
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// 构建当前分支可见的来源清单，并返回供 `read_source` 使用的完整文本索引。
pub fn build(
    state: &AppState,
    request: &Value,
    session_id: &str,
    leaf_message_id: Option<i64>,
) -> Result<(String, HashMap<String, String>), String> {
    let lineage = branch_lineage(state, session_id, leaf_message_id)?;
    let current_turn = lineage
        .iter()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        .count()
        + 1;
    let mut inventory = SourceInventory::default();
    add_fresh_sources(&mut inventory, state, request, current_turn)?;
    add_historical_sources(&mut inventory, state, &lineage)?;
    Ok(render(&inventory))
}

fn branch_lineage(
    state: &AppState,
    session_id: &str,
    leaf_message_id: Option<i64>,
) -> Result<Vec<Value>, String> {
    let messages = if session_id.is_empty() {
        Vec::new()
    } else {
        state.database.get_messages(session_id)?
    };
    let Some(leaf) = leaf_message_id else {
        return Ok(messages);
    };
    let by_id = messages
        .iter()
        .filter_map(|message| {
            message
                .get("id")
                .and_then(Value::as_i64)
                .map(|id| (id, message.clone()))
        })
        .collect::<HashMap<_, _>>();
    let mut current = Some(leaf);
    let mut chain = Vec::new();
    let mut seen = HashSet::new();
    while let Some(message_id) = current {
        if !seen.insert(message_id) {
            break;
        }
        let Some(message) = by_id.get(&message_id) else {
            break;
        };
        chain.push(message.clone());
        current = message.get("parent_message_id").and_then(Value::as_i64);
    }
    chain.reverse();
    Ok(chain)
}

fn add_fresh_sources(
    inventory: &mut SourceInventory,
    state: &AppState,
    request: &Value,
    turn: usize,
) -> Result<(), String> {
    add_attachments(
        inventory,
        request.get("attachments").and_then(Value::as_array),
        turn,
        true,
    );
    add_reading_references(
        inventory,
        state,
        request.get("reading_references").and_then(Value::as_array),
        turn,
        true,
    )?;
    add_book_references(
        inventory,
        state,
        request.get("book_references").and_then(Value::as_array),
        turn,
        true,
    )?;
    add_notebook_references(
        inventory,
        state,
        request.get("notebook_references").and_then(Value::as_array),
        turn,
        true,
    )?;
    add_history_references(
        inventory,
        state,
        request.get("history_references").and_then(Value::as_array),
        turn,
        true,
    )?;
    add_question_references(
        inventory,
        state,
        request
            .get("question_notebook_references")
            .or_else(|| request.get("question_entry_ids"))
            .and_then(Value::as_array),
        turn,
        true,
    )?;
    Ok(())
}

fn add_historical_sources(
    inventory: &mut SourceInventory,
    state: &AppState,
    lineage: &[Value],
) -> Result<(), String> {
    let mut turn = 0usize;
    for message in lineage {
        if message.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        turn += 1;
        add_attachments(
            inventory,
            message.get("attachments").and_then(Value::as_array),
            turn,
            false,
        );
        let Some(snapshot) = message
            .pointer("/metadata/request_snapshot")
            .filter(|value| value.is_object())
        else {
            continue;
        };
        add_reading_references(
            inventory,
            state,
            array_field(snapshot, &["reading_references", "readingReferences"]),
            turn,
            false,
        )?;
        add_book_references(
            inventory,
            state,
            array_field(snapshot, &["book_references", "bookReferences"]),
            turn,
            false,
        )?;
        add_notebook_references(
            inventory,
            state,
            array_field(snapshot, &["notebook_references", "notebookReferences"]),
            turn,
            false,
        )?;
        add_history_references(
            inventory,
            state,
            array_field(snapshot, &["history_references", "historyReferences"]),
            turn,
            false,
        )?;
        add_question_references(
            inventory,
            state,
            array_field(
                snapshot,
                &[
                    "question_notebook_references",
                    "questionNotebookReferences",
                    "question_entry_ids",
                ],
            ),
            turn,
            false,
        )?;
    }
    Ok(())
}

/// 只持久化来源定位信息，避免把附件正文、密钥或 base64 重复写进消息元数据。
pub fn request_snapshot(request: &Value) -> Value {
    let mut snapshot = serde_json::Map::new();
    for key in [
        "reading_references",
        "book_references",
        "notebook_references",
        "history_references",
        "question_notebook_references",
        "question_entry_ids",
        "partner_group_references",
    ] {
        if let Some(value) = request.get(key).filter(|value| value.is_array()) {
            snapshot.insert(key.to_string(), value.clone());
        }
    }
    Value::Object(snapshot)
}

fn add_attachments(
    inventory: &mut SourceInventory,
    attachments: Option<&Vec<Value>>,
    turn: usize,
    fresh: bool,
) {
    for attachment in attachments.into_iter().flatten() {
        let mime = attachment
            .get("mime_type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase();
        if attachment.get("type").and_then(Value::as_str) == Some("image")
            || mime.starts_with("image/")
        {
            continue;
        }
        let Some(id) = attachment
            .get("id")
            .or_else(|| attachment.get("attachment_id"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        let mut text = attachment
            .get("extracted_text")
            .or_else(|| attachment.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if text.trim().is_empty() {
            if let Some(base64) = attachment.get("base64").and_then(Value::as_str) {
                if mime.starts_with("text/") || mime == "application/json" {
                    use base64::Engine;
                    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(base64) {
                        if bytes.len() <= MAX_TEXT_BASE64_BYTES {
                            text = String::from_utf8_lossy(&bytes).into_owned();
                        }
                    }
                }
            }
        }
        let name = attachment
            .get("filename")
            .and_then(Value::as_str)
            .unwrap_or("附件");
        let availability_note = if text.trim().is_empty() {
            "未能从该文件提取文本（可能是扫描件或暂不支持的格式），因此不能直接引用。".to_string()
        } else {
            String::new()
        };
        inventory.add(SourceEntry {
            sid: format!("at-{id}"),
            kind: "attachment".to_string(),
            name: name.to_string(),
            full_text: text,
            fresh,
            first_seen_turn: turn,
            availability_note,
        });
    }
}

fn add_reading_references(
    inventory: &mut SourceInventory,
    state: &AppState,
    references: Option<&Vec<Value>>,
    turn: usize,
    fresh: bool,
) -> Result<(), String> {
    for reference in references.into_iter().flatten() {
        let Some(id) = reference
            .get("material_id")
            .or_else(|| reference.get("reading_material_id"))
            .or_else(|| reference.get("id"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        let sid = format!("rd-{id}");
        if inventory.contains(&sid) {
            continue;
        }
        if let Some(resource) =
            find_resource(state, &["reading_material", "reading", "reading_unit"], id)?
        {
            let text = extract_resource_text(&resource);
            let name = reference_name(reference, &resource, "阅读材料");
            inventory.add(SourceEntry {
                sid,
                kind: "reading".to_string(),
                name,
                full_text: text,
                fresh,
                first_seen_turn: turn,
                availability_note: String::new(),
            });
        }
    }
    Ok(())
}

fn add_book_references(
    inventory: &mut SourceInventory,
    state: &AppState,
    references: Option<&Vec<Value>>,
    turn: usize,
    fresh: bool,
) -> Result<(), String> {
    for reference in references.into_iter().flatten() {
        let Some(id) = reference
            .get("book_id")
            .or_else(|| reference.get("id"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        let sid = format!("bk-{id}");
        if inventory.contains(&sid) {
            continue;
        }
        if let Some(resource) = find_resource(state, &["book", "book_page"], id)? {
            let title = resource
                .get("title")
                .or_else(|| resource.get("name"))
                .and_then(Value::as_str)
                .unwrap_or(id);
            let mut text = extract_resource_text(&resource);
            if let Some(pages) = reference.get("page_ids").and_then(Value::as_array) {
                text.push_str(&format!(
                    "\n\n引用页面：{}",
                    pages
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("、")
                ));
            }
            inventory.add(SourceEntry {
                sid,
                kind: "book".to_string(),
                name: title.to_string(),
                full_text: text,
                fresh,
                first_seen_turn: turn,
                availability_note: String::new(),
            });
        }
    }
    Ok(())
}

fn add_notebook_references(
    inventory: &mut SourceInventory,
    state: &AppState,
    references: Option<&Vec<Value>>,
    turn: usize,
    fresh: bool,
) -> Result<(), String> {
    for reference in references.into_iter().flatten() {
        let Some(notebook_id) = reference
            .get("notebook_id")
            .or_else(|| reference.get("id"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        let Some(notebook) = state.database.get_resource("notebook", notebook_id)? else {
            continue;
        };
        let requested = reference
            .get("record_ids")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<HashSet<_>>()
            });
        let records = notebook
            .get("records")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut added = false;
        for record in &records {
            let record_id = record.get("id").and_then(Value::as_str).unwrap_or("");
            if requested
                .as_ref()
                .is_some_and(|requested| !requested.contains(record_id))
            {
                continue;
            }
            if record_id.is_empty() {
                continue;
            }
            let sid = format!("nb-{record_id}");
            if inventory.contains(&sid) {
                continue;
            }
            let text = extract_resource_text(record);
            if text.trim().is_empty() {
                continue;
            }
            let name = record
                .get("title")
                .or_else(|| record.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("收藏记录");
            inventory.add(SourceEntry {
                sid,
                kind: "notebook".to_string(),
                name: name.to_string(),
                full_text: text,
                fresh,
                first_seen_turn: turn,
                availability_note: String::new(),
            });
            added = true;
        }
        if !added && requested.is_none() {
            let sid = format!("nb-{notebook_id}");
            let text = extract_resource_text(&notebook);
            if !text.trim().is_empty() {
                let name = notebook
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("收藏笔记本");
                inventory.add(SourceEntry {
                    sid,
                    kind: "notebook".to_string(),
                    name: name.to_string(),
                    full_text: text,
                    fresh,
                    first_seen_turn: turn,
                    availability_note: String::new(),
                });
            }
        }
    }
    Ok(())
}

fn add_history_references(
    inventory: &mut SourceInventory,
    state: &AppState,
    references: Option<&Vec<Value>>,
    turn: usize,
    fresh: bool,
) -> Result<(), String> {
    for reference in references.into_iter().flatten() {
        let Some(session_id) = reference
            .as_str()
            .or_else(|| reference.get("session_id").and_then(Value::as_str))
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        let sid = format!("hs-{session_id}");
        if inventory.contains(&sid) {
            continue;
        }
        let messages = match state.database.get_messages(session_id) {
            Ok(messages) => messages,
            Err(_) => continue,
        };
        let text = messages
            .iter()
            .filter_map(|message| {
                let role = message.get("role").and_then(Value::as_str)?;
                let content = message.get("content").and_then(Value::as_str)?;
                (!content.trim().is_empty()).then(|| format!("{role}: {content}"))
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        if text.is_empty() {
            continue;
        }
        let name = state
            .database
            .get_session_meta(session_id)?
            .and_then(|session| {
                session
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| format!("历史会话 {session_id}"));
        inventory.add(SourceEntry {
            sid,
            kind: "history".to_string(),
            name,
            full_text: text,
            fresh,
            first_seen_turn: turn,
            availability_note: String::new(),
        });
    }
    Ok(())
}

fn add_question_references(
    inventory: &mut SourceInventory,
    state: &AppState,
    references: Option<&Vec<Value>>,
    turn: usize,
    fresh: bool,
) -> Result<(), String> {
    for reference in references.into_iter().flatten() {
        let Some(entry_id) = reference
            .as_i64()
            .or_else(|| reference.as_str().and_then(|value| value.parse().ok()))
            .or_else(|| reference.get("entry_id").and_then(Value::as_i64))
            .or_else(|| reference.get("id").and_then(Value::as_i64))
        else {
            continue;
        };
        let sid = format!("qb-{entry_id}");
        if inventory.contains(&sid) {
            continue;
        }
        if let Some(block) = super::question_bank::source_block(state, entry_id)? {
            let name = block
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("题库题目")
                .to_string();
            let text = block
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            inventory.add(SourceEntry {
                sid,
                kind: "question".to_string(),
                name,
                full_text: text,
                fresh,
                first_seen_turn: turn,
                availability_note: String::new(),
            });
        }
    }
    Ok(())
}

fn find_resource(state: &AppState, domains: &[&str], id: &str) -> Result<Option<Value>, String> {
    for domain in domains {
        if let Some(resource) = state.database.get_resource(domain, id)? {
            return Ok(Some(resource));
        }
    }
    Ok(None)
}

fn extract_resource_text(value: &Value) -> String {
    for pointer in [
        "/content",
        "/text",
        "/markdown",
        "/body",
        "/description",
        "/summary",
        "/output",
        "/payload/render/content",
    ] {
        if let Some(text) = value.pointer(pointer).and_then(Value::as_str) {
            return text.to_string();
        }
    }
    serde_json::to_string_pretty(value).unwrap_or_default()
}

fn reference_name(reference: &Value, resource: &Value, fallback: &str) -> String {
    reference
        .get("name")
        .or_else(|| reference.get("title"))
        .or_else(|| resource.get("name"))
        .or_else(|| resource.get("title"))
        .and_then(Value::as_str)
        .unwrap_or(fallback)
        .to_string()
}

fn array_field<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Vec<Value>> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_array))
}

fn render(inventory: &SourceInventory) -> (String, HashMap<String, String>) {
    if inventory.is_empty() {
        return (String::new(), HashMap::new());
    }
    let mut source_index = HashMap::new();
    let mut rows = Vec::new();
    for entry in &inventory.entries {
        if !entry.full_text.trim().is_empty() {
            source_index.insert(entry.sid.clone(), entry.full_text.clone());
        }
        rows.push(render_row(entry));
    }
    let header = "[Attached Sources]\n这是本会话当前分支可见的来源索引。标记为本回合附件的行带有 preview；较早回合附件的行只显示身份与大小。需要完整原文时调用 read_source，source_id 必须逐字复制；不要虚构 id，也不要用其他文件工具绕过来源索引。";
    (format!("{header}\n\n{}", rows.join("\n\n")), source_index)
}

fn render_row(entry: &SourceEntry) -> String {
    let identity = format!(
        "- id={}  type={}  name={:?}",
        entry.sid, entry.kind, entry.name
    );
    if !entry.availability_note.is_empty() {
        let scope = if entry.fresh {
            String::new()
        } else {
            format!(
                "  source: previously attached (turn {})",
                entry.first_seen_turn
            )
        };
        return format!("{identity}{scope}\n  note: {}", entry.availability_note);
    }
    if entry.fresh {
        let preview = clip_preview(&entry.full_text);
        format!("{identity}\n  preview: {preview:?}")
    } else {
        format!(
            "{identity}  size={}  source: previously attached (turn {})",
            format_size(entry.full_text.chars().count()),
            entry.first_seen_turn
        )
    }
}

fn clip_preview(text: &str) -> String {
    let cleaned = text.trim();
    if cleaned.chars().count() <= MANIFEST_PREVIEW_CHARS {
        return cleaned.to_string();
    }
    let mut preview = cleaned
        .chars()
        .take(MANIFEST_PREVIEW_CHARS)
        .collect::<String>();
    preview.push('…');
    preview
}

fn format_size(chars: usize) -> String {
    if chars >= 1024 {
        format!("~{} KB", chars.div_ceil(1024))
    } else {
        format!("~{chars} chars")
    }
}

pub fn metadata_for_read(sid: &str, text: &str) -> Value {
    json!({
        "kind": "source_read",
        "source_id": sid,
        "char_count": text.chars().count(),
        "private_result": true
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_renders_cumulative_rows_without_truncating_index() {
        let mut inventory = SourceInventory::default();
        let long = "甲".repeat(2_200);
        inventory.add(SourceEntry {
            sid: "at-a".to_string(),
            kind: "attachment".to_string(),
            name: "notes.txt".to_string(),
            full_text: long.clone(),
            fresh: true,
            first_seen_turn: 1,
            availability_note: String::new(),
        });
        inventory.add(SourceEntry {
            sid: "at-a".to_string(),
            kind: "attachment".to_string(),
            name: "old.txt".to_string(),
            full_text: "old".to_string(),
            fresh: false,
            first_seen_turn: 1,
            availability_note: String::new(),
        });
        let (manifest, index) = render(&inventory);
        assert!(manifest.contains("preview:"));
        assert_eq!(index.get("at-a"), Some(&long));
    }
}
