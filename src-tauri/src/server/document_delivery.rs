use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

static DECLINED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(不要|不用|不需要|无需|先不|别).{0,3}(写|生成|新建|创建|整理成|形成|保存)|(不需要|无需).{0,3}(文档|笔记|报告|总结)|只(在聊天中)?(回答|输出)|(don't|do not)\s+(write|create|generate|save|produce)")
        .expect("文档创建否定规则")
});

/// 只检查本轮用户的直接要求，引用、代码和一般功能咨询不构成创建授权。
pub(super) fn requested(request: &Value, capability: &str) -> bool {
    if !matches!(capability, "chat" | "immersive_reading" | "deep_research") {
        return false;
    }
    let mut fenced = false;
    request["content"]
        .as_str()
        .unwrap_or("")
        .lines()
        .fold(false, |requested, line| {
            let line = line.trim();
            if line.starts_with("```") || line.starts_with("~~~") {
                fenced = !fenced;
                return requested;
            }
            if fenced
                || line.starts_with('>')
                || line.starts_with('#')
                || line.starts_with(['“', '「', '"'])
            {
                return requested;
            }
            let line = line.to_lowercase();
            if DECLINED.is_match(&line) {
                return false;
            }
            if [
                "能不能",
                "是否",
                "支持",
                "如何",
                "怎么",
                "可以吗",
                "吗？",
                "吗?",
                "这句话",
                "can you",
                "how to",
                "is it possible",
            ]
            .iter()
            .any(|term| line.contains(term))
            {
                return requested;
            }
            let action = [
                "写",
                "生成",
                "新建",
                "创建",
                "整理成",
                "形成",
                "保存为",
                "输出一",
                "create ",
                "write ",
                "generate ",
                "save as ",
                "produce ",
            ]
            .iter()
            .any(|term| line.contains(term));
            let target = [
                "文档",
                "复习资料",
                "复习笔记",
                "学习资料",
                "总结",
                "笔记",
                "报告",
                "document",
                "study guide",
                "revision notes",
                "report",
                "summary",
            ]
            .iter()
            .any(|term| line.contains(term));
            requested || (action && target)
        })
}

pub(super) fn title(content: &str) -> String {
    content
        .lines()
        .find_map(|line| {
            let line = line.trim();
            let heading = line.trim_start_matches('#').trim();
            (line.starts_with("# ") && !heading.is_empty())
                .then(|| heading.chars().take(160).collect())
        })
        .unwrap_or_else(|| "阅读整理文档".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn 仅直接创作要求触发文档交付() {
        for text in [
            "请根据这些章节写一个总结文档",
            "# 复习提纲\n\n严格按照这个提纲来补全内容，形成一个复习资料",
            "整理成一篇复习笔记",
            "Write a study guide using these sources.",
            "请写一份总结文档，不要遗漏来源",
            "先不要生成文档\n现在请写一份总结文档",
            "Write a document, do not omit the sources.",
        ] {
            assert!(
                requested(&json!({"content": text}), "immersive_reading"),
                "{text}"
            );
        }
        for text in [
            "解释这一页",
            "现在支持新建文档吗？",
            "如何写总结文档",
            "不要生成文档，只回答问题",
            "> 请生成文档\n解释这段话",
            "```text\n请写总结文档\n```\n解释这段代码",
            "# 请生成文档\n这是引用标题",
            "Can you create a document?",
            "分析“生成文档”这句话的意思",
            "请写一份总结文档\n先不要生成文档，只回答问题",
        ] {
            assert!(
                !requested(&json!({"content": text}), "immersive_reading"),
                "{text}"
            );
        }
        assert!(!requested(&json!({"content":"生成总结文档"}), "visualize"));
    }
}
