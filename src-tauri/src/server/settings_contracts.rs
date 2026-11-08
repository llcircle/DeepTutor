use super::{ApiError, ApiResult, AppState};
use serde_json::{json, Value};

pub fn defaults(key: &str) -> Value {
    match key {
        "chat_starters" => json!({"trace_count": 20}),
        "chat_attachments" => json!({
            "max_file_mb": 20, "max_total_mb": 25,
            "max_chars_per_doc": 200_000, "max_chars_total": 150_000
        }),
        "capability_settings" => json!({
            "chat": {"temperature": 0.2, "max_rounds": 32,
                "stage_budgets": {"exploring": 1600, "responding": 8000}},
            "solve": {"temperature": 0.3, "max_tokens": 8192, "max_rounds": 12, "max_replans": 2},
            "research": {"temperature": 0.5, "max_tokens": 16834, "researching": {
                "note_agent_mode": "auto", "tool_timeout": 240, "tool_max_retries": 0,
                "paper_search_years_limit": 5}},
            "question": {"temperature": 0.7, "max_tokens": 4096, "exploring": {
                "max_iterations": 7, "tool_summarizer": {"enabled": true, "max_tokens": 1024}}},
            "co_writer": {"temperature": 0.6, "max_tokens": 4096},
            "vision_solver": {"temperature": 0.3, "max_tokens": 12000},
            "math_animator": {"temperature": 0.2, "max_tokens": 16834},
            "visualize": {"temperature": 0.15, "max_tokens": 16000}
        }),
        _ => json!({}),
    }
}

fn bounds(name: &str) -> (f64, f64) {
    match name {
        "trace_count" => (3.0, 100.0),
        "temperature" => (0.0, 2.0),
        "max_rounds" | "max_iterations" | "paper_search_years_limit" => (1.0, 50.0),
        "max_replans" | "tool_max_retries" => (0.0, 10.0),
        "tool_timeout" => (1.0, 600.0),
        "max_file_mb" => (1.0, 200.0),
        "max_total_mb" => (1.0, 350.0),
        "max_chars_per_doc" | "max_chars_total" => (1_000.0, 8_000_000.0),
        _ => (1.0, 200_000.0),
    }
}

// 旧库中的缺失字段、null 和错误类型回退到默认值；新写入则严格校验。
fn overlay(base: &mut Value, patch: &Value, strict: bool, name: &str) -> ApiResult<()> {
    let field = name.rsplit('.').next().unwrap_or(name);
    if let Some(object) = base.as_object_mut() {
        let Some(patch) = patch.as_object() else {
            return if strict {
                Err(ApiError::bad_request(format!("设置 {name} 必须是对象。")))
            } else {
                Ok(())
            };
        };
        for (key, value) in patch {
            if let Some(target) = object.get_mut(key) {
                overlay(target, value, strict, &format!("{name}.{key}"))?;
            } else if strict {
                return Err(ApiError::bad_request(format!(
                    "未知设置字段：{name}.{key}。"
                )));
            }
        }
        return Ok(());
    }
    let valid = if base.is_number() {
        let (lo, hi) = bounds(field);
        patch.as_f64().is_some_and(|v| {
            v.is_finite() && v >= lo && v <= hi && (field == "temperature" || v.fract() == 0.0)
        })
    } else if base.is_boolean() {
        patch.is_boolean()
    } else if field == "note_agent_mode" {
        patch
            .as_str()
            .is_some_and(|v| ["auto", "on", "off", "always", "never"].contains(&v))
    } else {
        patch.is_string()
    };
    if valid {
        *base = if base.is_number() && field != "temperature" {
            json!(patch.as_f64().unwrap_or_default() as u64)
        } else {
            patch.clone()
        };
    } else if strict {
        let detail = if base.is_number() {
            let (lo, hi) = bounds(field);
            format!(
                "必须为 {lo}–{hi} 之间的{}，当前值为 {patch}",
                if field == "temperature" {
                    "数值"
                } else {
                    "整数"
                }
            )
        } else {
            "类型或取值无效".to_string()
        };
        return Err(ApiError::bad_request(format!("设置 {name} {detail}。")));
    }
    Ok(())
}

pub fn read(state: &AppState, key: &str) -> ApiResult<Value> {
    let mut value = defaults(key);
    if let Some(stored) = state.database.get_setting(key)? {
        // 兼容误存整个响应 envelope 的旧配置。
        let stored = stored.get("settings").unwrap_or(&stored);
        overlay(&mut value, stored, false, key)?;
    }
    Ok(value)
}

pub fn write(state: &AppState, key: &str, patch: &Value) -> ApiResult<Value> {
    let mut value = read(state, key)?;
    overlay(&mut value, patch, true, key)?;
    state.database.set_setting(key, &value)?;
    Ok(value)
}

pub fn starters(state: &AppState) -> ApiResult<Value> {
    Ok(json!({"settings": read(state, "chat_starters")?, "bounds": {"trace_count": [3, 100]}}))
}

pub fn attachments(state: &AppState) -> ApiResult<Value> {
    let settings = read(state, "chat_attachments")?;
    Ok(json!({
        "effective": {
            "max_file_bytes": settings["max_file_mb"].as_u64().unwrap_or(20) * 1024 * 1024,
            "max_total_bytes": settings["max_total_mb"].as_u64().unwrap_or(25) * 1024 * 1024,
            "max_chars_per_doc": settings["max_chars_per_doc"],
            "max_chars_total": settings["max_chars_total"],
            "ws_max_size": super::MAX_UPLOAD_FRAME_BYTES
        },
        "settings": settings,
        "bounds": {"max_file_mb": [1, 200], "max_total_mb": [1, 350], "chars": [1000, 8000000]},
        "restart_required_for_larger_uploads": false
    }))
}

pub fn capability_block(state: &AppState, capability: &str) -> ApiResult<Value> {
    let key = match capability {
        "deep_solve" => "solve",
        "deep_research" => "research",
        "deep_question" => "question",
        "math_animator" => "math_animator",
        "visualize" => "visualize",
        _ => "chat",
    };
    Ok(read(state, "capability_settings")?[key].clone())
}

pub fn network(state: &AppState) -> ApiResult<Value> {
    let url = state.runtime_url.lock().clone();
    let port = url::Url::parse(&url)
        .ok()
        .and_then(|url| url.port())
        .unwrap_or(0);
    let auth = state
        .database
        .get_setting("auth")?
        .unwrap_or_else(|| json!({}));
    Ok(json!({
        "desktop_managed": true,
        "settings": {"backend_port": port, "frontend_port": port, "public_api_base": "", "cors_origins": []},
        "effective": {
            "backend_url": url, "frontend_url": url, "browser_api_base": url,
            "api_base_source": "desktop_loopback", "cors_mode": "permissive",
            "cors_origins": [], "allow_remote_http_origins": false
        },
        "auth": {
            "enabled": auth.get("enabled").and_then(Value::as_bool).unwrap_or(false),
            "cookie_secure": false, "cookie_samesite": "lax", "cross_site_cookie_ready": false
        },
        "restart_required": false
    }))
}

pub fn readiness(state: &AppState) -> ApiResult<Value> {
    let catalog = state
        .database
        .get_setting("catalog")?
        .unwrap_or_else(|| json!({}));
    let mut rows = Vec::new();
    for (service, label) in [
        ("llm", "对话模型"),
        ("task", "任务模型"),
        ("embedding", "向量模型"),
        ("search", "搜索服务"),
        ("tts", "语音合成"),
        ("stt", "语音识别"),
        ("imagegen", "图像生成"),
        ("videogen", "视频生成"),
    ] {
        let bucket = &catalog["services"][service];
        let profile_id = bucket["active_profile_id"]
            .as_str()
            .or_else(|| bucket["profiles"][0]["id"].as_str());
        let profile = bucket["profiles"].as_array().and_then(|profiles| {
            profiles
                .iter()
                .find(|profile| profile["id"].as_str() == profile_id)
        });
        let model_id = bucket["active_model_id"]
            .as_str()
            .or_else(|| profile.and_then(|profile| profile["models"][0]["id"].as_str()));
        let selected = super::catalog::service_selection(&catalog, service, profile_id, model_id);
        let configured = selected.as_ref().is_some_and(|(profile, model)| {
            if service == "search" {
                return profile
                    .get("provider")
                    .and_then(Value::as_str)
                    .is_some_and(|v| !v.is_empty());
            }
            model
                .as_ref()
                .is_some_and(|model| super::provider_probe::llm_settings(profile, model).is_ok())
        });
        let (status, detail) = if configured {
            // 这里只验证本地配置完整性，不冒充远端连接测试成功。
            ("enabled_verified", "configuration_verified")
        } else {
            ("not_selected", "active_profile_not_selected")
        };
        rows.push(json!({
            "id": format!("catalog.{service}"), "section": "catalog", "label": label,
            "state": status, "detail_code": detail,
            "enabled": configured, "available": configured, "configured": configured,
            "verified": configured, "required": service == "llm"
        }));
    }
    let parser = super::document_settings::document_payload(state)?;
    let parser_id = parser
        .get("engine")
        .and_then(Value::as_str)
        .unwrap_or("text_only");
    let parser_ready = parser["readiness"][parser_id]["ready"]
        .as_bool()
        .unwrap_or(false);
    rows.push(json!({
        "id": "document_parsing.selected", "section": "document_parsing", "label": "当前文档解析器",
        "state": if parser_ready {"enabled_verified"} else {"misconfigured"},
        "detail_code": if parser_ready {"configuration_verified"} else {"selected_parser_unavailable"},
        "enabled": true, "available": parser_ready, "configured": true, "verified": parser_ready, "required": true
    }));
    let knowledge = state.database.list_resources("knowledge_base")?;
    let knowledge_ready = knowledge
        .iter()
        .any(|kb| kb.get("status").and_then(Value::as_str) == Some("ready"));
    rows.push(json!({
        "id": "knowledge.rag", "section": "knowledge", "label": "本地知识库检索",
        "state": if knowledge_ready {"enabled_verified"} else {"not_selected"},
        "detail_code": if knowledge_ready {"knowledge_base_ready"} else {"no_knowledge_base"},
        "enabled": knowledge_ready, "available": knowledge_ready, "configured": knowledge_ready,
        "verified": knowledge_ready, "required": false
    }));
    for (id, label) in [
        ("runtime.database", "SQLite 数据库"),
        ("runtime.rust", "Rust 本地服务"),
    ] {
        rows.push(json!({
            "id": id, "section": "runtime", "label": label, "state": "enabled_verified",
            "detail_code": "coordination_ready", "enabled": true, "available": true,
            "configured": true, "verified": true, "required": true
        }));
    }
    let mut summary = json!({
        "enabled_verified": 0, "available_disabled": 0, "unavailable": 0,
        "misconfigured": 0, "not_selected": 0
    });
    let mut notices = Vec::new();
    for row in &rows {
        let status = row["state"].as_str().unwrap_or("unavailable");
        summary[status] = json!(summary[status].as_u64().unwrap_or(0) + 1);
        if row["required"] == true && status != "enabled_verified" {
            notices.push(json!({
                "code": row["detail_code"], "row_id": row["id"],
                "section": row["section"], "severity": "blocker"
            }));
        }
    }
    Ok(json!({
        "schema_version": "deeptutor.settings-readiness/v2", "ok": notices.is_empty(),
        "summary": summary, "rows": rows, "notices": notices
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_legacy_settings_keep_complete_defaults() {
        let mut value = defaults("capability_settings");
        overlay(
            &mut value,
            &json!({"chat": {"temperature": 0.7}, "solve": null}),
            false,
            "",
        )
        .unwrap();
        assert_eq!(value["chat"]["stage_budgets"]["responding"], 8000);
        assert_eq!(value["solve"]["max_rounds"], 12);
        assert_eq!(value["chat"]["temperature"], 0.7);
    }

    #[test]
    fn invalid_new_settings_fail_instead_of_persisting() {
        for value in [
            json!({"trace_count": null}),
            json!({"trace_count": 101}),
            json!({"trace_count": 4.5}),
        ] {
            assert!(overlay(&mut defaults("chat_starters"), &value, true, "").is_err());
        }
    }

    #[test]
    fn 越界错误包含完整字段路径实际值和范围() {
        let error = overlay(
            &mut defaults("capability_settings"),
            &json!({"chat":{"max_rounds":330}}),
            true,
            "capability_settings",
        )
        .unwrap_err();
        let error = format!("{error:?}");
        assert!(error.contains("capability_settings.chat.max_rounds"));
        assert!(error.contains("330"));
        assert!(error.contains("1–50"));
    }
}
