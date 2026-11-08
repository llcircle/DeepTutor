use serde_json::Value;

fn string(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_string()
}

fn safe_link(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    url.is_empty()
        || lower.starts_with("https://")
        || lower.starts_with("http://")
        || lower.starts_with("mailto:")
        || (url.starts_with('/') && !url.starts_with("//"))
        || url.starts_with('#')
}

fn inline_markdown(content: &Value, depth: usize) -> String {
    if depth > 16 {
        return String::new();
    }
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    let Some(items) = content.as_array() else {
        return String::new();
    };
    items
        .iter()
        .map(|item| {
            let props = &item["props"];
            let mut text = match item["type"].as_str().unwrap_or_default() {
                "math" | "inlineMath" => format!("${}$", string(props, "formula")),
                "pageLink" => format!(
                    "[{}](/co-writer/{})",
                    string(props, "title"),
                    string(props, "pageId")
                ),
                "noteDate" => string(props, "date"),
                _ => item["text"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| inline_markdown(&item["content"], depth + 1)),
            };
            for (key, marker) in [
                ("code", "`"),
                ("bold", "**"),
                ("italic", "*"),
                ("strike", "~~"),
            ] {
                if item["styles"][key].as_bool().unwrap_or(false) {
                    text = format!("{marker}{text}{marker}");
                }
            }
            let url = string(item, "href");
            if item["type"] == "link" && safe_link(&url) {
                text = format!("[{text}]({url})");
            }
            text
        })
        .collect()
}

fn code_block(source: &str, language: &str) -> String {
    let fence = "`".repeat(
        source
            .split(|character| character != '`')
            .map(str::len)
            .max()
            .unwrap_or(0)
            .saturating_add(1)
            .max(3),
    );
    format!("{fence}{language}\n{source}\n{fence}")
}

fn learning_markdown(props: &Value) -> String {
    let mut text = ["markdown", "prompt", "title"]
        .iter()
        .map(|key| string(props, key))
        .find(|text| !text.is_empty())
        .unwrap_or_default();
    let data = props["data"]
        .as_str()
        .and_then(|value| serde_json::from_str::<Value>(value).ok())
        .unwrap_or(Value::Null);
    let (kind, key) = match props["kind"].as_str().unwrap_or_default() {
        "flashcards" => ("flashcards", "cards"),
        "quiz" => ("quiz", "questions"),
        "timeline" => ("timeline", "events"),
        _ => ("", ""),
    };
    if let Some(items) = data[key].as_array().filter(|items| !items.is_empty()) {
        text = items
            .iter()
            .map(|item| match kind {
                "flashcards" => {
                    format!("**{}**\n\n{}", string(item, "front"), string(item, "back"))
                }
                "timeline" => format!(
                    "{} — {}\n{}",
                    string(item, "date"),
                    string(item, "title"),
                    string(item, "description")
                ),
                _ => {
                    let options = item["options"]
                        .as_object()
                        .map(|options| {
                            options
                                .iter()
                                .map(|(key, value)| {
                                    format!("- {key}: {}", value.as_str().unwrap_or_default())
                                })
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                        .unwrap_or_default();
                    format!(
                        "{}\n\n{options}\n\n答案：{}\n\n{}",
                        string(item, "question"),
                        string(item, "correct_answer"),
                        string(item, "explanation")
                    )
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n");
    }
    if let Some(items) = data["workspace_items"]
        .as_array()
        .filter(|items| !items.is_empty())
        .or_else(|| data["artifacts"].as_array())
    {
        for item in items {
            let url = string(item, "url");
            if !url.is_empty() && safe_link(&url) {
                let title = item["filename"]
                    .as_str()
                    .or_else(|| item["title"].as_str())
                    .unwrap_or("附件");
                text.push_str(&format!("\n\n[{title}]({url})"));
            }
        }
    }
    text
}

// 与 Python 版一样，以块树为正文来源；旧式纯 Markdown 页面仍保留原文。
pub(super) fn blocks_to_markdown(blocks: &[Value]) -> String {
    project_blocks(blocks, 0, 0)
}

fn project_blocks(blocks: &[Value], depth: usize, indent: usize) -> String {
    if depth > 16 {
        return String::new();
    }
    let mut parts = Vec::new();
    for block in blocks {
        let kind = block["type"].as_str().unwrap_or_default();
        let props = &block["props"];
        let inline = inline_markdown(&block["content"], 0);
        let text = match kind {
            "heading" | "toggleHeading" => format!(
                "{} {inline}",
                "#".repeat(props["level"].as_u64().unwrap_or(1).clamp(1, 6) as usize)
            ),
            "bulletListItem" | "toggleListItem" => format!("- {inline}"),
            "numberedListItem" => format!("1. {inline}"),
            "checkListItem" => format!(
                "- [{}] {inline}",
                if props["checked"].as_bool().unwrap_or(false) {
                    "x"
                } else {
                    " "
                }
            ),
            "quote" | "callout" => inline
                .lines()
                .map(|line| format!("> {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
            "codeBlock" => code_block(&inline, &string(props, "language")),
            "htmlEmbed" => code_block(&string(props, "source"), "html"),
            "divider" => "---".to_string(),
            "math" => format!("$$\n{}\n$$", string(props, "formula")),
            "taskView" => "[工作区任务](/kanban)".to_string(),
            "tableOfContents" | "noteColumn" | "noteColumns" => String::new(),
            "image" | "video" | "audio" | "file" => {
                let title = props["caption"]
                    .as_str()
                    .filter(|text| !text.is_empty())
                    .or_else(|| props["name"].as_str())
                    .unwrap_or(kind);
                let url = string(props, "url");
                format!(
                    "{}[{title}]({})",
                    if kind == "image" { "!" } else { "" },
                    if safe_link(&url) { &url } else { "" }
                )
            }
            "table" => {
                let mut lines = Vec::new();
                if let Some(rows) = block["content"]["rows"].as_array() {
                    for (index, row) in rows.iter().enumerate() {
                        let cells = row["cells"].as_array().map(Vec::as_slice).unwrap_or(&[]);
                        let line = cells
                            .iter()
                            .map(|cell| {
                                inline_markdown(cell.get("content").unwrap_or(cell), 0)
                                    .replace('|', "\\|")
                                    .replace('\n', "<br>")
                            })
                            .collect::<Vec<_>>()
                            .join(" | ");
                        lines.push(format!("| {line} |"));
                        if index == 0 {
                            lines.push(format!("| {} |", vec!["---"; cells.len()].join(" | ")));
                        }
                    }
                }
                lines.join("\n")
            }
            "webCard" => {
                let url = string(props, "url");
                format!(
                    "[{}]({})\n\n{}",
                    props["title"].as_str().unwrap_or(&url),
                    if safe_link(&url) { &url } else { "" },
                    string(props, "description")
                )
            }
            "learning" => learning_markdown(props),
            _ if inline.is_empty() => ["markdown", "prompt", "title"]
                .iter()
                .map(|key| string(props, key))
                .find(|text| !text.is_empty())
                .unwrap_or_default(),
            _ => inline,
        };
        let source = string(props, "sourceUrl");
        let text =
            if matches!(kind, "clip" | "learning") && !source.is_empty() && safe_link(&source) {
                format!("{text}\n\n[来源]({source})")
            } else {
                text
            };
        if !text.trim().is_empty() {
            let prefix = "  ".repeat(indent);
            parts.push(
                text.lines()
                    .map(|line| format!("{prefix}{line}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        }
        if let Some(children) = block["children"].as_array() {
            let next_indent = indent + usize::from(!matches!(kind, "noteColumn" | "noteColumns"));
            let child_text = project_blocks(children, depth + 1, next_indent);
            if !child_text.is_empty() {
                parts.push(child_text);
            }
        }
    }
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn 富文本与嵌套块保留标题链接公式和列表() {
        let blocks = vec![
            json!({"type":"heading","props":{"level":2},"content":[{"type":"text","text":"误差理论"}]}),
            json!({"type":"paragraph","content":[
                {"type":"text","text":"重要","styles":{"bold":true}},
                {"type":"link","href":"https://example.test","content":[{"type":"text","text":"来源"}]},
                {"type":"inlineMath","props":{"formula":"x^2"}}
            ],"children":[{"type":"checkListItem","props":{"checked":true},"content":[{"type":"text","text":"复核"}]}]}),
        ];
        assert_eq!(
            blocks_to_markdown(&blocks),
            "## 误差理论\n\n**重要**[来源](https://example.test)$x^2$\n\n  - [x] 复核"
        );
    }

    #[test]
    fn 表格代码和原始块正文均可投影() {
        let blocks = vec![
            json!({"type":"table","content":{"rows":[{"cells":[[{"type":"text","text":"A|B"}],{"content":[{"type":"text","text":"值"}]}]}]}}),
            json!({"type":"codeBlock","props":{"language":"rust"},"content":[{"type":"text","text":"```"}]}),
            json!({"content":"原始正文"}),
        ];
        let markdown = blocks_to_markdown(&blocks);
        assert!(markdown.contains("| A\\|B | 值 |\n| --- | --- |"));
        assert!(markdown.contains("````rust\n```\n````"));
        assert!(markdown.ends_with("原始正文"));
    }

    #[test]
    fn 学习卡片与来源保留且危险链接不导出() {
        let blocks = vec![
            json!({"type":"learning","props":{"kind":"flashcards","data":"{\"cards\":[{\"front\":\"问题\",\"back\":\"答案\"}]}","sourceUrl":"/reading/source"}}),
            json!({"type":"paragraph","content":[{"type":"link","href":"javascript:alert(1)","content":[{"type":"text","text":"普通文字"}]}]}),
        ];
        let markdown = blocks_to_markdown(&blocks);
        assert!(markdown.contains("**问题**\n\n答案\n\n[来源](/reading/source)"));
        assert!(markdown.ends_with("普通文字"));
        assert!(!markdown.contains("javascript:"));
        assert!(blocks_to_markdown(&[]).is_empty());
    }
}
