use chrono::Local;
use serde_yaml::Value;

const CHAT_ZH: &str = include_str!("../../../deeptutor/agents/chat/prompts/zh/agentic_chat.yaml");
const CHAT_EN: &str = include_str!("../../../deeptutor/agents/chat/prompts/en/agentic_chat.yaml");
const ASK_ZH: &str =
    include_str!("../../../deeptutor/capabilities/ask_questions/prompts/zh/system.md");
const ASK_EN: &str =
    include_str!("../../../deeptutor/capabilities/ask_questions/prompts/en/system.md");
const SOLVE_ZH: &str = include_str!("../../../deeptutor/capabilities/solve/prompts/zh/system.md");
const SOLVE_EN: &str = include_str!("../../../deeptutor/capabilities/solve/prompts/en/system.md");
const READING_ZH: &str =
    include_str!("../../../deeptutor/capabilities/reading/prompts/zh/reading.yaml");
const READING_EN: &str =
    include_str!("../../../deeptutor/capabilities/reading/prompts/en/reading.yaml");
const AUDIO_ZH: &str =
    include_str!("../../../deeptutor/capabilities/prompts/zh/audio_overview.yaml");
const AUDIO_EN: &str =
    include_str!("../../../deeptutor/capabilities/prompts/en/audio_overview.yaml");
const MASTERY_ZH: &str =
    include_str!("../../../deeptutor/capabilities/mastery/prompts/zh/mastery_loop.yaml");
const MASTERY_EN: &str =
    include_str!("../../../deeptutor/capabilities/mastery/prompts/en/mastery_loop.yaml");

/// 生成某一能力回合的系统提示词。
///
/// 基础对话策略直接复用项目内现有的中英文提示词资产，其他能力在其上叠加
/// 明确的结构化输出约束，从而让纯 Rust 后端保持与产品一致的教学语气。
pub fn system_prompt(capability: &str, language: &str) -> String {
    let chinese = language.to_ascii_lowercase().starts_with("zh");
    let chat = if chinese { CHAT_ZH } else { CHAT_EN };
    let general = yaml_text(chat, &["general"]).unwrap_or_default();
    let runtime_policy = yaml_text(chat, &["runtime_policy"]).unwrap_or_default();
    let loop_system = yaml_text(chat, &["loop", "system"]).unwrap_or_default();
    let datetime = Local::now().format("%Y-%m-%d %H:%M").to_string();
    let mut prompt = format!(
        "{general}\n\n{runtime_policy}\n\n当前时间：{datetime}。解析“今天、本周、今年、现在”等相对时间时必须以该时间为准。\n\n{loop_system}"
    );

    let extra = match capability {
        "ask_questions" => {
            let mut text = if chinese {
                ASK_ZH.to_string()
            } else {
                ASK_EN.to_string()
            };
            text.push_str(
                "\n\n本轮已强制启用结构化提问工具 `ask_user`。首轮必须调用该工具；用户回答后在同一回合继续完成原始请求。",
            );
            text
        }
        "deep_solve" => {
            let mut text = if chinese {
                SOLVE_ZH.to_string()
            } else {
                SOLVE_EN.to_string()
            };
            text.push_str(
                "\n\n可用工具包括 `solve_plan`、`solve_finish_step`、`solve_replan`、`web_search`、`paper_search`、`rag` 和 `geogebra_analysis`。严格按提示先计划再解题。",
            );
            text
        }
        "deep_question" => {
            if chinese {
                r#"
## 本轮任务：生成测验
根据用户主题与 `config` 参数生成可直接交互的题目。最后只输出一个 JSON 对象，不要代码围栏：
{
  "intro": "面向学习者的一句导语",
  "questions": [
    {
      "question_id": "q_1",
      "question_type": "choice|concept|fill_in_blank|short_answer|written|coding",
      "question": "题干，数学使用 LaTeX",
      "options": {"A":"...","B":"...","C":"...","D":"..."},
      "correct_answer": "A 或文字答案",
      "explanation": "清晰解释",
      "difficulty": "easy|medium|hard"
    }
  ]
}
`choice` 必须有 A-D 四个相近长度的选项；其他题型 options 可为空对象。题目数量、题型和难度严格遵守用户配置。不要编造与素材相矛盾的事实。
"#
                .to_string()
            } else {
                r#"
## Turn task: generate a quiz
Return exactly one JSON object with the shape:
{"intro":"...","questions":[{"question_id":"q_1","question_type":"choice|concept|fill_in_blank|short_answer|written|coding","question":"...","options":{"A":"...","B":"...","C":"...","D":"..."},"correct_answer":"...","explanation":"...","difficulty":"easy|medium|hard"}]}
For choice questions provide exactly four comparable A-D options. Follow the requested count, types and difficulty. No prose outside JSON.
"#
                .to_string()
            }
        }
        "deep_research" => {
            if chinese {
                r#"
## 本轮任务：深度研究
先澄清研究范围；信息已足够时不要为了提问而提问。随后使用 `web_search`、`paper_search`、`rag` 等工具收集相互独立的证据，最后写一份结构清晰、带引用链接和来源列表的中文研究报告。
阶段顺序：rephrasing（澄清）→ decomposing（拆解）→ researching（检索与核验）→ reporting（报告）。如果存在关键歧义，可调用 `ask_user`；否则直接完成报告。
不得虚构 URL、论文、作者或引文。对无法验证的内容明确标注不确定性。
"#
                    .to_string()
            } else {
                r#"
## Turn task: deep research
Clarify only genuinely blocking ambiguity, then use web_search, paper_search and rag to gather independent evidence and write a structured cited report. Stages: rephrasing, decomposing, researching, reporting. Never invent URLs, papers, authors or quotations.
"#
                .to_string()
            }
        }
        "visualize" => {
            if chinese {
                r#"
## 本轮任务：可视化
根据用户要求选择 svg、chartjs、mermaid、mindmap 或 html。最后只输出一个 JSON 对象，不要代码围栏：
{
  "render_type": "svg|chartjs|mermaid|mindmap|html",
  "response": "面向用户的简要说明",
  "title": "标题",
  "description": "可视化说明",
  "alt_text": "无障碍描述",
  "language": "svg|json|mermaid|html",
  "code": "完整自包含代码或数据",
  "analysis": {"description":"分析","visual_elements":["..."]}
}
SVG 必须是一个合法、自包含的 `<svg>`；Chart.js 必须是可 JSON.parse 的配置对象；Mermaid 必须输出合法 DSL；HTML 必须从 `<!DOCTYPE html>` 到 `</html>` 完整闭合，且不依赖构建工具。
"#
                    .to_string()
            } else {
                r#"
## Turn task: visualization
Choose svg, chartjs, mermaid, mindmap or html. Return exactly one JSON object with render_type, response, title, description, alt_text, language, code and analysis. The code must be complete and self-contained.
"#
                .to_string()
            }
        }
        "math_animator" => {
            if chinese {
                r#"
## 本轮任务：数学动画
设计一个清晰的分步数学动画，并使用纯 SVG + CSS/SMIL 动画表达，不依赖 Manim、Python 或外部程序。最后只输出一个 JSON 对象：
{"response":"讲解正文","title":"标题","svg":"完整合法的 SVG 文档，含 xmlns 和动画","summary":"一句话摘要","key_points":["..."]}
SVG 必须在浏览器中直接播放，所有标签闭合，所有 `&` 正确转义，不使用外部文件。
"#
                    .to_string()
            } else {
                r#"
## Turn task: math animation
Create a self-contained SVG animation with CSS or SMIL. Do not require Manim, Python or external programs. Return exactly one JSON object: {"response":"...","title":"...","svg":"complete SVG document","summary":"...","key_points":["..."]}.
"#
                .to_string()
            }
        }
        "mastery_path" => {
            let yaml = if chinese { MASTERY_ZH } else { MASTERY_EN };
            let general = yaml_text(yaml, &["general"]).unwrap_or_default();
            let runtime = yaml_text(yaml, &["runtime_policy"]).unwrap_or_default();
            let loop_system = yaml_text(yaml, &["loop", "system"]).unwrap_or_default();
            let playbook = yaml_text(yaml, &["playbook"]).unwrap_or_default();
            let mode = if chinese {
                "当前是掌握式学习。先诊断学习者已知内容，再以小步骤讲解、提问、反馈和复习。不要声称已掌握，除非有明确作答或评估证据。"
            } else {
                "You are in mastery learning mode. Diagnose prior knowledge, teach in small steps, question, give feedback and schedule review. Never claim mastery without evidence."
            };
            format!("{general}\n\n{runtime}\n\n{loop_system}\n\n{mode}\n\n{playbook}")
        }
        "immersive_reading" => {
            let playbook = yaml_text(if chinese { READING_ZH } else { READING_EN }, &["playbook"])
                .unwrap_or_default();
            format!(
                "{playbook}\n\n阅读材料由回合上下文提供。引用时写明页码或定位号；不能确认的内容必须明确说明超出材料范围。"
            )
        }
        "audio_overview" => {
            yaml_text(if chinese { AUDIO_ZH } else { AUDIO_EN }, &["system"]).unwrap_or_default()
        }
        "video_lesson" => {
            if chinese {
                r#"
## 本轮任务：视频课件
生成一个可直接在浏览器中播放的完整单文件 HTML 课件，用 CSS/JavaScript 实现分镜、字幕、旁白文本和播放控制。最后只输出一个 JSON 对象：
{"response":"面向学习者的说明","title":"课程标题","html":"从 <!DOCTYPE html> 到 </html> 的完整页面","duration_seconds":60,"narration":"逐段旁白"}
HTML 必须自包含，不依赖 Remotion、Node 或构建服务。需要旁白时优先使用浏览器 `speechSynthesis`，并允许静音。
"#
                    .to_string()
            } else {
                r#"
## Turn task: video lesson
Create a complete single-file HTML lesson with scenes, captions, narration and playback controls using CSS/JavaScript. Return exactly one JSON object: {"response":"...","title":"...","html":"complete document","duration_seconds":60,"narration":"..."}. Do not require Remotion or Node.
"#
                .to_string()
            }
        }
        _ => String::new(),
    };
    if !extra.trim().is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(extra.trim());
    }
    prompt.push_str(
        "\n\n## 工具真实性\n只调用实际提供的工具；不得虚构工具结果、文件、图片、链接或运行输出。无法完成的操作必须如实说明，并给出可执行的替代方案。",
    );
    prompt
}

fn yaml_text(raw: &str, keys: &[&str]) -> Option<String> {
    let value: Value = serde_yaml::from_str(raw).ok()?;
    let mut current = &value;
    for key in keys {
        current = current.get(*key)?;
    }
    current.as_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompts_are_embedded_for_every_capability() {
        for capability in [
            "chat",
            "ask_questions",
            "audio_overview",
            "deep_solve",
            "deep_question",
            "deep_research",
            "visualize",
            "math_animator",
            "mastery_path",
            "immersive_reading",
            "video_lesson",
        ] {
            let prompt = system_prompt(capability, "zh");
            assert!(prompt.contains("DeepTutor") || prompt.contains("学习"));
        }
    }
}
