use serde_json::{json, Map, Value};
use std::collections::HashMap;

pub const CATALOG_SECRET_MASK: &str = "***";

const CONNECTABLE_SERVICES: [&str; 7] = [
    "llm",
    "task",
    "embedding",
    "tts",
    "stt",
    "imagegen",
    "videogen",
];

/// 将设置接口返回的密钥占位符替换为同一配置项中保存的真实值。
pub fn restore_catalog_secrets(proposed: &Value, current: &Value) -> Value {
    let mut restored = proposed.clone();
    let current_connections = indexed_objects(current.get("connections"));
    if let Some(connections) = restored
        .get_mut("connections")
        .and_then(Value::as_array_mut)
    {
        for connection in connections {
            let Some(connection) = connection.as_object_mut() else {
                continue;
            };
            let id = string_field(connection, "id");
            if let Some(saved) = current_connections.get(&id) {
                restore_profile_secrets(connection, saved);
            }
        }
    }

    let current_services = current
        .get("services")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Some(services) = restored.get_mut("services").and_then(Value::as_object_mut) {
        for (service_name, proposed_service) in services {
            let Some(proposed_service) = proposed_service.as_object_mut() else {
                continue;
            };
            let current_profiles = current_services
                .get(service_name)
                .map(|service| indexed_objects(service.get("profiles")))
                .unwrap_or_default();
            if let Some(profiles) = proposed_service
                .get_mut("profiles")
                .and_then(Value::as_array_mut)
            {
                for profile in profiles {
                    let Some(profile) = profile.as_object_mut() else {
                        continue;
                    };
                    let id = string_field(profile, "id");
                    if let Some(saved) = current_profiles.get(&id) {
                        restore_profile_secrets(profile, saved);
                    }
                }
            }
        }
    }
    restored
}

/// 在内存中把连接的凭据镜像到关联服务，不修改磁盘上的设置。
pub fn resolve_catalog_connections(catalog: &Value) -> Value {
    let mut resolved = catalog.clone();
    let mut connections = HashMap::<String, Map<String, Value>>::new();
    if let Some(rows) = resolved
        .get_mut("connections")
        .and_then(Value::as_array_mut)
    {
        for connection in rows {
            let Some(connection) = connection.as_object_mut() else {
                continue;
            };
            let id = match string_field(connection, "id") {
                value if !value.is_empty() => value,
                _ => {
                    let id = format!("conn-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
                    connection.insert("id".into(), Value::String(id.clone()));
                    id
                }
            };
            connection
                .entry("provider")
                .or_insert_with(|| Value::String(String::new()));
            connection
                .entry("name")
                .or_insert_with(|| Value::String(id.clone()));
            for field in ["api_key", "base_url", "api_version"] {
                connection
                    .entry(field)
                    .or_insert_with(|| Value::String(String::new()));
            }
            connection
                .entry("extra_headers")
                .or_insert_with(|| json!({}));
            connections.insert(id, connection.clone());
        }
    }

    let Some(services) = resolved.get_mut("services").and_then(Value::as_object_mut) else {
        return resolved;
    };
    for service_name in CONNECTABLE_SERVICES {
        let Some(service) = services
            .get_mut(service_name)
            .and_then(Value::as_object_mut)
        else {
            continue;
        };
        let Some(profiles) = service.get_mut("profiles").and_then(Value::as_array_mut) else {
            continue;
        };
        for profile in profiles {
            let Some(profile) = profile.as_object_mut() else {
                continue;
            };
            apply_connection(profile, service_name, &mut connections);
        }
    }
    resolved
}

pub fn selected_catalog(
    catalog: &Value,
    service_name: &str,
    profile_id: &str,
    model_id: Option<&str>,
) -> Result<Value, String> {
    let mut selected = catalog.clone();
    let service = selected
        .get_mut("services")
        .and_then(Value::as_object_mut)
        .and_then(|services| services.get_mut(service_name))
        .and_then(Value::as_object_mut)
        .ok_or_else(|| "所选服务配置已不存在。".to_string())?;
    let profile = service
        .get("profiles")
        .and_then(Value::as_array)
        .and_then(|profiles| {
            profiles
                .iter()
                .find(|profile| profile.get("id").and_then(Value::as_str) == Some(profile_id))
        })
        .cloned()
        .ok_or_else(|| "所选服务配置已不存在。".to_string())?;
    if service_name != "search" {
        let model_id = model_id.ok_or_else(|| "请选择模型。".to_string())?;
        if !profile
            .get("models")
            .and_then(Value::as_array)
            .is_some_and(|models| {
                models
                    .iter()
                    .any(|model| model.get("id").and_then(Value::as_str) == Some(model_id))
            })
        {
            return Err("所选模型已不存在。".to_string());
        }
    }
    service.insert(
        "active_profile_id".into(),
        Value::String(profile_id.to_string()),
    );
    if let Some(model_id) = model_id {
        service.insert(
            "active_model_id".into(),
            Value::String(model_id.to_string()),
        );
    }
    Ok(selected)
}

/// 从目录中取出当前激活或指定的配置对象。
pub fn service_selection(
    catalog: &Value,
    service_name: &str,
    profile_id: Option<&str>,
    model_id: Option<&str>,
) -> Option<(Value, Option<Value>)> {
    let service = catalog.get("services")?.get(service_name)?;
    let requested_profile = profile_id
        .map(str::to_owned)
        .or_else(|| string(service.get("active_profile_id")));
    let profile = service
        .get("profiles")?
        .as_array()?
        .iter()
        .find(|profile| profile.get("id").and_then(Value::as_str) == requested_profile.as_deref())?
        .clone();
    if service_name == "search" {
        let profile = resolve_profile_provider(catalog, service_name, &profile, None).ok()?;
        return Some((profile, None));
    }
    let requested_model = model_id
        .map(str::to_owned)
        .or_else(|| string(service.get("active_model_id")));
    let model = profile
        .get("models")?
        .as_array()?
        .iter()
        .find(|model| model.get("id").and_then(Value::as_str) == requested_model.as_deref())
        .cloned()?;
    let profile = resolve_profile_provider(catalog, service_name, &profile, Some(&model)).ok()?;
    Some((profile, Some(model)))
}

/// 按前端契约解析模型级、配置级与旧版连接引用，绝不按名称或提供方猜测账户。
pub fn resolve_profile_provider(
    catalog: &Value,
    service_name: &str,
    profile: &Value,
    model: Option<&Value>,
) -> Result<Value, String> {
    let legacy_ref = profile
        .get("connection_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(|id| json!({"connection_id": id}));
    let reference = model
        .and_then(|model| model.get("provider_ref"))
        .filter(|value| !value.is_null())
        .or_else(|| profile.get("provider_ref").filter(|value| !value.is_null()))
        .or(legacy_ref.as_ref());
    let Some(reference) = reference else {
        return Ok(profile.clone());
    };
    let reference = reference
        .as_object()
        .ok_or_else(|| "模型的提供方引用无效，请在设置中重新选择提供方。".to_string())?;
    let connection_id = string_field(reference, "connection_id");
    let source_service = string_field(reference, "service");
    let source_profile_id = string_field(reference, "profile_id");
    let source = if !connection_id.is_empty() {
        catalog
            .get("connections")
            .and_then(Value::as_array)
            .and_then(|rows| {
                rows.iter()
                    .find(|row| row.get("id").and_then(Value::as_str) == Some(&connection_id))
            })
    } else if !source_service.is_empty() && !source_profile_id.is_empty() {
        catalog
            .get("services")
            .and_then(|services| services.get(&source_service))
            .and_then(|service| service.get("profiles"))
            .and_then(Value::as_array)
            .and_then(|rows| {
                rows.iter().find(|row| {
                    row.get("id").and_then(Value::as_str) == Some(&source_profile_id)
                        && row.get("provider_ref").is_none_or(Value::is_null)
                })
            })
    } else {
        None
    }
    .and_then(Value::as_object)
    .ok_or_else(|| "模型引用的提供方已不存在，请在设置中重新选择；请求尚未发送。".to_string())?;
    let service_override = source
        .get("service_overrides")
        .and_then(|overrides| {
            overrides.get(if service_name == "task" {
                "llm"
            } else {
                service_name
            })
        })
        .and_then(Value::as_object)
        .filter(|value| value.get("enabled").and_then(Value::as_bool) == Some(true));
    let binding = service_override
        .map(|value| string_field(value, "binding"))
        .filter(|value| !value.is_empty())
        .or_else(|| string(reference.get("binding")))
        .or_else(|| string(source.get("binding")))
        .or_else(|| string(source.get("provider")))
        .unwrap_or_else(|| "custom".to_string());
    let mut resolved = profile
        .as_object()
        .cloned()
        .ok_or_else(|| "模型配置格式无效。".to_string())?;
    resolved.insert(
        if service_name == "search" {
            "provider"
        } else {
            "binding"
        }
        .into(),
        Value::String(binding.clone()),
    );
    for field in ["api_key", "api_version", "extra_headers", "proxy", "app_id"] {
        resolved.insert(
            field.into(),
            source.get(field).cloned().unwrap_or_else(|| {
                if field == "extra_headers" {
                    json!({})
                } else {
                    json!("")
                }
            }),
        );
    }
    for field in ["api_format", "wire_api"] {
        resolved.insert(
            field.into(),
            source.get(field).cloned().unwrap_or_else(|| json!("auto")),
        );
    }
    let source_url = string_field(source, "base_url");
    let source_kind = if connection_id.is_empty() {
        source_service
    } else {
        string_field(source, "source_service")
    };
    let override_url = service_override
        .map(|value| string_field(value, "base_url"))
        .filter(|value| !value.is_empty());
    let base_url = if let Some(url) = override_url {
        url
    } else if !source_url.is_empty() {
        provider_endpoint(&source_url, &source_kind, service_name, &binding)
    } else {
        string_field(reference, "default_base_url")
    };
    resolved.insert("base_url".into(), Value::String(base_url));
    Ok(Value::Object(resolved))
}

fn provider_endpoint(url: &str, source_service: &str, service: &str, binding: &str) -> String {
    if service == source_service
        || (matches!(service, "llm" | "task") && matches!(source_service, "llm" | "task"))
    {
        return url.to_string();
    }
    let Ok(mut parsed) = url::Url::parse(url) else {
        return url.to_string();
    };
    let mut path = parsed.path().trim_end_matches('/').to_string();
    for suffix in [
        "/embeddings",
        "/api/embed",
        "/api/embeddings",
        "/v2/embed",
        "/audio/speech",
        "/audio/transcriptions",
        "/images/generations",
        "/videos/generations",
        "/chat/completions",
        "/responses",
        "/messages",
    ] {
        if let Some(base) = path.strip_suffix(suffix) {
            path = base.to_string();
            break;
        }
    }
    if service == "embedding" {
        match binding {
            "ollama" => {
                path = path
                    .trim_end_matches("/v1")
                    .trim_end_matches("/api")
                    .to_string();
                path.push_str("/api/embed");
            }
            "cohere" => {
                path = path
                    .trim_end_matches("/v1")
                    .trim_end_matches("/v2")
                    .to_string();
                path.push_str("/v2/embed");
            }
            _ => path.push_str("/embeddings"),
        }
    }
    parsed.set_path(&path);
    parsed.to_string()
}

pub fn extra_headers(profile: &Value) -> Vec<(String, String)> {
    let Some(value) = profile.get("extra_headers") else {
        return Vec::new();
    };
    let parsed = value
        .as_str()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .unwrap_or_else(|| value.clone());
    parsed
        .as_object()
        .map(|headers| {
            headers
                .iter()
                .filter(|(name, value)| {
                    !name.trim().is_empty()
                        && !value.is_null()
                        && !value
                            .as_str()
                            .is_some_and(|text| text == CATALOG_SECRET_MASK)
                })
                .map(|(name, value)| (name.clone(), scalar_string(value)))
                .collect()
        })
        .unwrap_or_default()
}

pub fn redact_catalog_secrets(catalog: &Value) -> Value {
    let mut redacted = catalog.clone();
    if let Some(connections) = redacted
        .get_mut("connections")
        .and_then(Value::as_array_mut)
    {
        for connection in connections {
            if let Some(connection) = connection.as_object_mut() {
                redact_profile(connection);
            }
        }
    }
    if let Some(services) = redacted.get_mut("services").and_then(Value::as_object_mut) {
        for service in services.values_mut() {
            if let Some(profiles) = service.get_mut("profiles").and_then(Value::as_array_mut) {
                for profile in profiles {
                    if let Some(profile) = profile.as_object_mut() {
                        redact_profile(profile);
                    }
                }
            }
        }
    }
    redacted
}

fn apply_connection(
    profile: &mut Map<String, Value>,
    service_name: &str,
    connections: &mut HashMap<String, Map<String, Value>>,
) {
    let connection_id = string_field(profile, "connection_id");
    if connection_id.is_empty() {
        return;
    }
    let Some(connection) = connections.get(&connection_id) else {
        return;
    };
    for field in ["api_key", "api_version", "extra_headers", "app_id"] {
        if let Some(value) = connection.get(field) {
            profile.insert(field.to_string(), value.clone());
        }
    }
    for field in ["api_format", "wire_api", "proxy"] {
        if let Some(value) = connection.get(field) {
            profile.insert(field.to_string(), value.clone());
        }
    }
    let service_override = connection
        .get("service_overrides")
        .and_then(Value::as_object)
        .and_then(|overrides| {
            overrides.get(match service_name {
                "task" => "llm",
                value => value,
            })
        })
        .and_then(Value::as_object)
        .filter(|value| value.get("enabled").and_then(Value::as_bool) == Some(true));
    if let Some(binding) = service_override.and_then(|value| value.get("binding")) {
        profile.insert(
            if service_name == "search" {
                "provider"
            } else {
                "binding"
            }
            .into(),
            binding.clone(),
        );
    }
    let override_base = service_override
        .and_then(|value| value.get("base_url"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let base_url = if override_base.is_empty() {
        connection
            .get("base_url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
    } else {
        override_base
    };
    if !base_url.is_empty() {
        let suffix = if service_name == "embedding" {
            "/embeddings"
        } else {
            ""
        };
        profile.insert(
            "base_url".into(),
            Value::String(format!("{}{suffix}", base_url.trim_end_matches('/'))),
        );
    }
}

fn indexed_objects(value: Option<&Value>) -> HashMap<String, Map<String, Value>> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .filter_map(|object| {
            let id = object.get("id").and_then(Value::as_str)?.trim();
            (!id.is_empty()).then(|| (id.to_string(), object.clone()))
        })
        .collect()
}

fn restore_profile_secrets(proposed: &mut Map<String, Value>, current: &Map<String, Value>) {
    for (key, value) in proposed.iter_mut() {
        if key == "extra_headers" || is_secret_field(key) {
            *value = restore_secret_value(value, current.get(key));
        }
    }
}

fn restore_secret_value(proposed: &Value, current: Option<&Value>) -> Value {
    if proposed.as_str() == Some(CATALOG_SECRET_MASK) {
        return current.cloned().unwrap_or(Value::Null);
    }
    match (proposed, current) {
        (Value::Object(proposed), Some(Value::Object(current))) => Value::Object(
            proposed
                .iter()
                .map(|(key, value)| (key.clone(), restore_secret_value(value, current.get(key))))
                .collect(),
        ),
        (Value::Array(proposed), Some(Value::Array(current))) => Value::Array(
            proposed
                .iter()
                .enumerate()
                .map(|(index, value)| restore_secret_value(value, current.get(index)))
                .collect(),
        ),
        _ => proposed.clone(),
    }
}

fn redact_profile(profile: &mut Map<String, Value>) {
    for (key, value) in profile.iter_mut() {
        if key == "extra_headers" || is_secret_field(key) {
            *value = redact_secret_value(value);
        }
    }
}

fn redact_secret_value(value: &Value) -> Value {
    match value {
        Value::String(value) if !value.is_empty() => Value::String(CATALOG_SECRET_MASK.into()),
        Value::Object(values) => Value::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), redact_secret_value(value)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.iter().map(redact_secret_value).collect()),
        _ => value.clone(),
    }
}

fn is_secret_field(name: &str) -> bool {
    let normalized = name.to_ascii_lowercase();
    ["api_key", "apikey", "token", "secret", "password"]
        .iter()
        .any(|hint| normalized.contains(hint))
}

fn string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn string_field(object: &Map<String, Value>, key: &str) -> String {
    object
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

fn scalar_string(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        _ => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn referenced_catalog() -> Value {
        serde_json::from_str(include_str!("../../tests/fixtures/referenced-catalog.json"))
            .expect("读取脱敏配置")
    }

    #[test]
    fn 模型级引用优先并且不会按相同名称猜测连接() {
        let mut catalog = referenced_catalog();
        catalog["connections"]
            .as_array_mut()
            .expect("连接")
            .push(json!({
                "id": "other", "name": "自定义网关", "provider": "custom",
                "base_url": "https://other.example/proxy", "api_key": "other-key",
                "api_format": "anthropic", "extra_headers": {"X-Gateway": "other"}
            }));
        let profile = &catalog["services"]["llm"]["profiles"][0];
        let mut model = profile["models"][0].clone();
        model["provider_ref"] = json!({"connection_id": "other", "binding": "custom"});
        let resolved = resolve_profile_provider(&catalog, "llm", profile, Some(&model))
            .expect("解析模型级引用");
        assert_eq!(resolved["base_url"], "https://other.example/proxy");
        assert_eq!(resolved["api_key"], "other-key");
        assert_eq!(resolved["api_format"], "anthropic");
        assert_eq!(resolved["extra_headers"]["X-Gateway"], "other");
        assert_eq!(profile["base_url"], "");
    }

    #[test]
    fn 服务选择支持新版引用与嵌入端点() {
        let mut catalog = referenced_catalog();
        for (service, profile, model, expected) in [
            (
                "llm",
                "llm-profile",
                "llm-model",
                "https://gateway.example/api/v3",
            ),
            (
                "embedding",
                "embedding-profile",
                "embedding-model",
                "https://gateway.example/api/v3/embeddings",
            ),
        ] {
            catalog["services"][service]["active_profile_id"] = json!(profile);
            catalog["services"][service]["active_model_id"] = json!(model);
            let (resolved, _) = service_selection(&catalog, service, None, None).expect("选择模型");
            assert_eq!(resolved["base_url"], expected);
            assert_eq!(resolved["api_key"], "fixture-key");
        }
    }

    #[test]
    fn 旧版服务配置引用和服务覆盖遵守同一契约() {
        let mut catalog = referenced_catalog();
        catalog["services"]["tts"] = json!({"profiles": [{
            "id": "old-account", "binding": "openai",
            "base_url": "https://legacy.example/v1/audio/speech", "api_key": "legacy-key",
            "service_overrides": {"llm": {"enabled": true, "base_url": "https://override.example/api/v3", "binding": "custom"}}
        }]});
        let mut profile = catalog["services"]["llm"]["profiles"][0].clone();
        profile["provider_ref"] =
            json!({"service": "tts", "profile_id": "old-account", "binding": "custom"});
        let resolved =
            resolve_profile_provider(&catalog, "llm", &profile, None).expect("解析旧账号引用");
        assert_eq!(resolved["base_url"], "https://override.example/api/v3");
        assert_eq!(resolved["api_key"], "legacy-key");
        catalog["services"]["tts"]["profiles"][0]["service_overrides"] = json!({});
        let resolved =
            resolve_profile_provider(&catalog, "llm", &profile, None).expect("转换语音端点");
        assert_eq!(resolved["base_url"], "https://legacy.example/v1");
    }

    #[test]
    fn 失效连接不会删除引用后使用旧地址() {
        let catalog = json!({
            "connections": [],
            "services": {"llm": {"profiles": [{
                "id": "profile", "connection_id": "deleted",
                "binding": "openai", "base_url": "https://api.openai.com/v1"
            }]}}
        });
        let catalog = resolve_catalog_connections(&catalog);
        let profile = &catalog["services"]["llm"]["profiles"][0];
        assert!(resolve_profile_provider(&catalog, "llm", profile, None).is_err());
    }

    #[test]
    fn restores_masked_secrets_without_mutating_inputs() {
        let live = json!({
            "connections": [{"id": "conn-1", "api_key": "live-key", "extra_headers": {"X-Key": "secret"}}],
            "services": {"tts": {"profiles": [{"id": "p1", "api_key": "profile-key"}]}}
        });
        let draft = json!({
            "connections": [{"id": "conn-1", "api_key": "***", "extra_headers": {"X-Key": "***"}}],
            "services": {"tts": {"profiles": [{"id": "p1", "api_key": "***"}]}}
        });
        let restored = restore_catalog_secrets(&draft, &live);
        assert_eq!(restored["connections"][0]["api_key"], "live-key");
        assert_eq!(
            restored["connections"][0]["extra_headers"]["X-Key"],
            "secret"
        );
        assert_eq!(
            restored["services"]["tts"]["profiles"][0]["api_key"],
            "profile-key"
        );
        assert_eq!(draft["connections"][0]["api_key"], "***");
    }

    #[test]
    fn linked_connection_credentials_are_mirrored_in_memory() {
        let catalog = json!({
            "connections": [{
                "id": "conn-1",
                "api_key": "key",
                "base_url": "https://api.example.com/v1",
                "extra_headers": {"X-Test": "1"},
                "service_overrides": {"tts": {"enabled": true, "binding": "openai"}}
            }],
            "services": {
                "tts": {
                    "profiles": [{"id": "p1", "connection_id": "conn-1"}]
                }
            }
        });
        let resolved = resolve_catalog_connections(&catalog);
        let profile = &resolved["services"]["tts"]["profiles"][0];
        assert_eq!(profile["api_key"], "key");
        assert_eq!(profile["binding"], "openai");
        assert_eq!(profile["base_url"], "https://api.example.com/v1");
    }

    #[test]
    fn service_selection_prefers_linked_connection_over_stale_profile() {
        let catalog = json!({
            "connections": [{
                "id": "conn-tju",
                "api_key": "linked-key",
                "base_url": "https://gateway.example/v1"
            }],
            "services": {
                "llm": {
                    "active_profile_id": "p1",
                    "active_model_id": "m1",
                    "profiles": [{
                        "id": "p1",
                        "connection_id": "conn-tju",
                        "binding": "openai",
                        "base_url": "https://api.openai.com/v1",
                        "api_key": "stale-key",
                        "models": [{"id": "m1", "model": "deepseek-v4.1-flash"}]
                    }]
                }
            }
        });

        let (profile, model) =
            service_selection(&catalog, "llm", None, None).expect("读取语言模型配置");
        assert_eq!(profile["base_url"], "https://gateway.example/v1");
        assert_eq!(profile["api_key"], "linked-key");
        assert_eq!(model.expect("模型")["model"], "deepseek-v4.1-flash");
    }
}
