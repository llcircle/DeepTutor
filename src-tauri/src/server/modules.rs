use axum::Json;
use serde_json::{json, Value};

// 桌面发行版把官方模块作为同一 Rust 二进制的内置功能交付。
// 编译期嵌入 v1.7.0 清单，仅复用模块标识和导航契约，不依赖 Python 包。
const MODULE_MANIFESTS: [&str; 25] = [
    include_str!("../../../deeptutor/modules/manifests/audio-overview.json"),
    include_str!("../../../deeptutor/modules/manifests/books.json"),
    include_str!("../../../deeptutor/modules/manifests/calendar.json"),
    include_str!("../../../deeptutor/modules/manifests/co-writer.json"),
    include_str!("../../../deeptutor/modules/manifests/core.json"),
    include_str!("../../../deeptutor/modules/manifests/knowledge.json"),
    include_str!("../../../deeptutor/modules/manifests/mastery.json"),
    include_str!("../../../deeptutor/modules/manifests/memory.json"),
    include_str!("../../../deeptutor/modules/manifests/notebook.json"),
    include_str!("../../../deeptutor/modules/manifests/parser-docling.json"),
    include_str!("../../../deeptutor/modules/manifests/parser-liteparse.json"),
    include_str!("../../../deeptutor/modules/manifests/parser-markitdown.json"),
    include_str!("../../../deeptutor/modules/manifests/parser-pymupdf4llm.json"),
    include_str!("../../../deeptutor/modules/manifests/parsing.json"),
    include_str!("../../../deeptutor/modules/manifests/partners.json"),
    include_str!("../../../deeptutor/modules/manifests/providers.json"),
    include_str!("../../../deeptutor/modules/manifests/questions.json"),
    include_str!("../../../deeptutor/modules/manifests/reading.json"),
    include_str!("../../../deeptutor/modules/manifests/renderer-manim.json"),
    include_str!("../../../deeptutor/modules/manifests/research.json"),
    include_str!("../../../deeptutor/modules/manifests/server.json"),
    include_str!("../../../deeptutor/modules/manifests/solve.json"),
    include_str!("../../../deeptutor/modules/manifests/video-lesson.json"),
    include_str!("../../../deeptutor/modules/manifests/visualize.json"),
    include_str!("../../../deeptutor/modules/manifests/web.json"),
];

fn modules_payload() -> Value {
    let mut rows = MODULE_MANIFESTS
        .iter()
        .map(|raw| {
            let manifest: Value = serde_json::from_str(raw).expect("内置模块清单必须是有效 JSON");
            let capabilities = manifest
                .get("capabilities")
                .and_then(Value::as_object)
                .map(|value| value.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            json!({
                "id": manifest["id"],
                "distribution": manifest["distribution"],
                "installed": true,
                "enabled": true,
                "ready": true,
                "issues": [],
                "requires": manifest["requires"],
                "navigation": manifest["navigation"],
                "capabilities": capabilities,
                "source": true
            })
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| left["id"].as_str().cmp(&right["id"].as_str()));
    json!({ "modules": rows })
}

pub async fn list_modules() -> Json<Value> {
    Json(modules_payload())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_inventory_reports_every_integrated_module_as_ready() {
        let payload = modules_payload();
        let modules = payload["modules"].as_array().expect("模块清单必须是数组");
        assert_eq!(modules.len(), MODULE_MANIFESTS.len());
        assert!(modules.iter().all(|row| {
            row["installed"] == true && row["enabled"] == true && row["ready"] == true
        }));

        let reading = modules
            .iter()
            .find(|row| row["id"] == "reading")
            .expect("reading 模块必须存在");
        assert_eq!(
            reading["navigation"],
            json!(["/reading", "/learning/reading", "/learning/watching"])
        );
    }
}
