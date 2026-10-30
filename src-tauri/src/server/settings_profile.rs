use super::error::{ApiError, ApiResult};
use super::AppState;
use chrono::Utc;
use serde_json::{json, Map, Value};

const PROFILE_SCHEMA_VERSION: &str = "deeptutor.settings-profile/v1";

const SYSTEM_FIELDS: &[&str] = &[
    "version_check_enabled",
    "backend_workers",
    "disable_ssl_verify",
    "sandbox_allow_subprocess",
    "capability_routing_enabled",
    "web_search_source_filtering",
    "chat_attachment_max_file_mb",
    "chat_attachment_max_total_mb",
    "chat_attachment_max_chars_per_doc",
    "chat_attachment_max_chars_total",
];

const AUTH_FIELDS: &[&str] = &["enabled", "token_expire_hours", "cookie_secure"];

const COORDINATION_FIELDS: &[&str] = &[
    "backend",
    "key_prefix",
    "lease_ttl_seconds",
    "renew_interval_seconds",
    "recovery_interval_seconds",
    "stream_retention_seconds",
];

const CATALOG_PROFILE_FIELDS: &[&str] = &[
    "id",
    "name",
    "binding",
    "provider",
    "connection_id",
    "api_format",
    "wire_api",
    "api_version",
    "provider_only",
];

const CATALOG_MODEL_FIELDS: &[&str] = &[
    "id",
    "name",
    "model",
    "dimension",
    "supported_dimensions",
    "capabilities",
    "voice",
    "response_format",
    "size",
    "quality",
    "style",
    "aspect_ratio",
    "duration",
    "resolution",
    "language",
    "sample_rate",
    "speed",
    "request_timeout",
];

/// 导出不含凭据与部署地址的有效设置档案。
pub fn export_profile(state: &AppState) -> ApiResult<Value> {
    let catalog = state
        .database
        .get_setting("catalog")?
        .unwrap_or_else(|| json!({}));
    Ok(json!({
        "schema_version": PROFILE_SCHEMA_VERSION,
        "generated_at": Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "profile": {
            "settings": {
                "system": selected_setting(state, "system", SYSTEM_FIELDS)?,
                "auth": selected_setting(state, "auth", AUTH_FIELDS)?,
                "integrations": export_integrations(state)?,
                "document_parsing": export_document_parsing(state)?,
                "llamaindex": export_safe_setting(state, "llamaindex")?,
                "graphrag": export_safe_setting(state, "graphrag")?,
                "lightrag": export_safe_setting(state, "lightrag")?,
                "catalog": export_catalog(&catalog)
            }
        }
    }))
}

/// 只读比较导入档案与当前有效设置，不写入任何配置。
pub fn review_import(state: &AppState, payload: &Value) -> ApiResult<Value> {
    let payload = payload
        .as_object()
        .ok_or_else(|| ApiError::unprocessable("设置档案必须是 JSON 对象。"))?;
    if payload.get("schema_version").and_then(Value::as_str) != Some(PROFILE_SCHEMA_VERSION) {
        return Err(ApiError::unprocessable(format!(
            "不支持的设置档案版本，需要 {PROFILE_SCHEMA_VERSION}。"
        )));
    }
    let profile = payload
        .get("profile")
        .and_then(Value::as_object)
        .ok_or_else(|| ApiError::unprocessable("profile 必须是 JSON 对象。"))?;
    let settings = profile
        .get("settings")
        .and_then(Value::as_object)
        .ok_or_else(|| ApiError::unprocessable("profile.settings 必须是 JSON 对象。"))?;
    let unknown_profile_fields = profile
        .keys()
        .filter(|key| key.as_str() != "settings")
        .cloned()
        .collect::<Vec<_>>();
    if !unknown_profile_fields.is_empty() {
        return Err(ApiError::unprocessable(format!(
            "设置档案包含未知顶层字段：{}",
            unknown_profile_fields.join(", ")
        )));
    }

    let current = export_profile(state)?;
    let current_settings = current
        .pointer("/profile/settings")
        .and_then(Value::as_object)
        .ok_or_else(|| ApiError::internal("无法构造当前设置档案。"))?;
    let mut changes = Vec::new();
    let mut unsupported = Vec::new();
    diff_value(
        &Value::Object(settings.clone()),
        &Value::Object(current_settings.clone()),
        "settings",
        &mut changes,
        &mut unsupported,
    );
    changes.sort_by(|left, right| {
        left.get("path")
            .and_then(Value::as_str)
            .cmp(&right.get("path").and_then(Value::as_str))
    });
    unsupported.sort_by(|left, right| {
        left.get("path")
            .and_then(Value::as_str)
            .cmp(&right.get("path").and_then(Value::as_str))
    });

    Ok(json!({
        "schema_version": PROFILE_SCHEMA_VERSION,
        "changes": changes,
        "unsupported": unsupported,
        "summary": {
            "changed": changes.len(),
            "unsupported": unsupported.len(),
            "compatible": unsupported.is_empty()
        }
    }))
}

fn selected_setting(state: &AppState, key: &str, fields: &[&str]) -> ApiResult<Value> {
    let value = state
        .database
        .get_setting(key)?
        .unwrap_or_else(|| json!({}));
    let Some(object) = value.as_object() else {
        return Ok(json!({}));
    };
    Ok(Value::Object(
        fields
            .iter()
            .filter_map(|field| {
                object
                    .get(*field)
                    .cloned()
                    .map(|value| ((*field).to_string(), value))
            })
            .collect(),
    ))
}

fn export_integrations(state: &AppState) -> ApiResult<Value> {
    let value = state
        .database
        .get_setting("integrations")?
        .unwrap_or_else(|| json!({}));
    let coordination = value
        .get("turn_coordination")
        .and_then(Value::as_object)
        .map(|object| {
            Value::Object(
                COORDINATION_FIELDS
                    .iter()
                    .filter_map(|field| {
                        object
                            .get(*field)
                            .cloned()
                            .map(|value| ((*field).to_string(), value))
                    })
                    .collect(),
            )
        })
        .unwrap_or_else(|| json!({}));
    Ok(json!({"turn_coordination": coordination}))
}

fn export_document_parsing(state: &AppState) -> ApiResult<Value> {
    let value = state
        .database
        .get_setting("document_parsing")?
        .unwrap_or_else(|| json!({}));
    let mut public = Map::new();
    if let Some(engine) = value.get("engine") {
        public.insert("engine".into(), engine.clone());
    }
    if let Some(engines) = value.get("engines").and_then(Value::as_object) {
        let mut public_engines = Map::new();
        for (name, engine) in engines {
            if name == "tika" {
                let present = engine.get("server_url").is_some_and(value_present);
                public_engines.insert(name.clone(), json!({"server_url_present": present}));
                continue;
            }
            public_engines.insert(name.clone(), redact_structure(engine, true));
        }
        public.insert("engines".into(), Value::Object(public_engines));
    }
    Ok(Value::Object(public))
}

fn export_safe_setting(state: &AppState, key: &str) -> ApiResult<Value> {
    Ok(redact_structure(
        &state
            .database
            .get_setting(key)?
            .unwrap_or_else(|| json!({})),
        true,
    ))
}

fn export_catalog(catalog: &Value) -> Value {
    let mut public_services = Map::new();
    if let Some(services) = catalog.get("services").and_then(Value::as_object) {
        for (name, service) in services {
            let profiles = service
                .get("profiles")
                .and_then(Value::as_array)
                .map(|profiles| {
                    profiles
                        .iter()
                        .filter_map(Value::as_object)
                        .map(export_catalog_profile)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            public_services.insert(
                name.clone(),
                json!({
                    "active_profile_id": service.get("active_profile_id").cloned().unwrap_or(Value::Null),
                    "active_model_id": service.get("active_model_id").cloned().unwrap_or(Value::Null),
                    "profiles": profiles
                }),
            );
        }
    }
    json!({"services": public_services})
}

fn export_catalog_profile(profile: &Map<String, Value>) -> Value {
    let mut public = selected_fields(profile, CATALOG_PROFILE_FIELDS);
    for key in ["api_key", "token", "proxy", "extra_headers"] {
        if let Some(value) = profile.get(key) {
            public.insert(key.to_string(), json!({"present": value_present(value)}));
        }
    }
    if let Some(models) = profile.get("models").and_then(Value::as_array) {
        public.insert(
            "models".into(),
            Value::Array(
                models
                    .iter()
                    .filter_map(Value::as_object)
                    .map(|model| Value::Object(selected_fields(model, CATALOG_MODEL_FIELDS)))
                    .collect(),
            ),
        );
    }
    Value::Object(public)
}

fn selected_fields(object: &Map<String, Value>, fields: &[&str]) -> Map<String, Value> {
    fields
        .iter()
        .filter_map(|field| {
            object
                .get(*field)
                .cloned()
                .map(|value| ((*field).to_string(), value))
        })
        .collect()
}

fn redact_structure(value: &Value, include_non_sensitive: bool) -> Value {
    match value {
        Value::Object(object) => {
            let mut public = Map::new();
            for (key, child) in object {
                if is_secret_field(key) {
                    public.insert(key.clone(), json!({"present": value_present(child)}));
                } else if is_deployment_field(key) {
                    public.insert(key.clone(), Value::Null);
                } else if include_non_sensitive {
                    public.insert(key.clone(), redact_structure(child, true));
                }
            }
            Value::Object(public)
        }
        Value::Array(rows) => Value::Array(
            rows.iter()
                .map(|row| redact_structure(row, include_non_sensitive))
                .collect(),
        ),
        _ => value.clone(),
    }
}

fn diff_value(
    proposed: &Value,
    current: &Value,
    path: &str,
    changes: &mut Vec<Value>,
    unsupported: &mut Vec<Value>,
) {
    if let Some(proposed_object) = proposed.as_object() {
        let Some(current_object) = current.as_object() else {
            unsupported.push(json!({"path": path, "reason": "type_mismatch"}));
            return;
        };
        for (key, child) in proposed_object {
            let child_path = join_path(path, key);
            let mut reason = field_restriction(key);
            if reason == Some("secret_not_exported")
                && is_secret_anchor(child)
                && current_object.get(key).is_some_and(is_secret_anchor)
            {
                reason = None;
            }
            if let Some(reason) = reason {
                unsupported.push(json!({"path": child_path, "reason": reason}));
                continue;
            }
            let Some(current_child) = current_object.get(key) else {
                unsupported.push(json!({"path": child_path, "reason": "unknown_field"}));
                continue;
            };
            diff_value(child, current_child, &child_path, changes, unsupported);
        }
        return;
    }

    if let (Some(proposed_rows), Some(current_rows)) = (proposed.as_array(), current.as_array()) {
        if proposed_rows.len() != current_rows.len() {
            unsupported.push(json!({"path": path, "reason": "list_length_changed"}));
            return;
        }
        if proposed_rows
            .iter()
            .zip(current_rows)
            .all(|(left, right)| left.is_object() && right.is_object())
        {
            for (index, (left, right)) in proposed_rows.iter().zip(current_rows).enumerate() {
                diff_value(
                    left,
                    right,
                    &join_path(path, &index.to_string()),
                    changes,
                    unsupported,
                );
            }
            return;
        }
    }

    if proposed != current {
        changes.push(json!({
            "path": path,
            "kind": "changed",
            "current": public_value(current),
            "proposed": public_value(proposed)
        }));
    }
}

fn public_value(value: &Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, child)| {
                    let value = if is_secret_field(key) {
                        json!({"present": value_present(child)})
                    } else if is_deployment_field(key) {
                        Value::Null
                    } else {
                        public_value(child)
                    };
                    (key.clone(), value)
                })
                .collect(),
        ),
        Value::Array(rows) => Value::Array(rows.iter().map(public_value).collect()),
        _ => value.clone(),
    }
}

fn field_restriction(field: &str) -> Option<&'static str> {
    if is_secret_field(field) {
        Some("secret_not_exported")
    } else if is_deployment_field(field) {
        Some("deployment_value_not_exported")
    } else {
        None
    }
}

fn is_secret_field(field: &str) -> bool {
    let normalized = field.to_ascii_lowercase();
    normalized == "token"
        || normalized == "proxy"
        || normalized.ends_with("_token")
        || [
            "api_key",
            "apikey",
            "api_token",
            "extra_headers",
            "password",
            "secret",
        ]
        .iter()
        .any(|hint| normalized.contains(hint))
}

fn is_deployment_field(field: &str) -> bool {
    let normalized = field.to_ascii_lowercase();
    normalized.starts_with("cors_")
        || ["url", "endpoint", "path", "host", "origin", "port", "dir"]
            .iter()
            .any(|hint| normalized == *hint || normalized.ends_with(&format!("_{hint}")))
}

fn is_secret_anchor(value: &Value) -> bool {
    is_presence_anchor(value)
        || value
            .as_object()
            .is_some_and(|object| !object.is_empty() && object.values().all(is_presence_anchor))
}

fn is_presence_anchor(value: &Value) -> bool {
    value.as_object().is_some_and(|object| {
        object.len() == 1 && object.get("present").is_some_and(Value::is_boolean)
    })
}

fn value_present(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
        _ => true,
    }
}

fn join_path(parent: &str, key: &str) -> String {
    if parent.is_empty() {
        key.to_string()
    } else {
        format!("{parent}.{key}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 导出结构会隐藏密钥和部署地址() {
        let value = json!({
            "api_key": "secret-value",
            "server_url": "http://127.0.0.1:9998",
            "enabled": true
        });
        assert_eq!(
            redact_structure(&value, true),
            json!({
                "api_key": {"present": true},
                "server_url": null,
                "enabled": true
            })
        );
    }

    #[test]
    fn 差异审查保留可读字段并拒绝未知字段() {
        let mut changes = Vec::new();
        let mut unsupported = Vec::new();
        diff_value(
            &json!({"settings": {"enabled": false, "new_field": 1}}),
            &json!({"settings": {"enabled": true}}),
            "profile",
            &mut changes,
            &mut unsupported,
        );
        assert_eq!(changes.len(), 1);
        assert_eq!(
            unsupported[0].get("reason").and_then(Value::as_str),
            Some("unknown_field")
        );
    }
}
