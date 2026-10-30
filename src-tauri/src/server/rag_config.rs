use super::error::{ApiError, ApiResult};
use super::{AppState, ScopedState};
use axum::extract::{Path, Query};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Map, Value};

const CONFIG_PREFIX: &str = "rag_provider_config:";
const KB_CONFIGS_KEY: &str = "kb_configs";

#[derive(Debug, Default, Deserialize)]
struct ModelOptionsQuery {
    kinds: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ActiveModelPayload {
    kind: String,
    profile_id: String,
    model_id: String,
}

#[derive(Debug, Deserialize)]
struct ModePayload {
    mode: String,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/knowledge-bases/rag-providers/{provider}/mode",
            put(set_provider_mode),
        )
        .route(
            "/knowledge-bases/rag-pipelines/pageindex/config",
            get(get_pageindex_config).put(put_pageindex_config),
        )
        .route(
            "/knowledge-bases/rag-pipelines/ima/config",
            get(get_ima_config).put(put_ima_config),
        )
        .route(
            "/knowledge-bases/rag-pipelines/llamaindex/config",
            get(get_llamaindex_config).put(put_llamaindex_config),
        )
        .route(
            "/knowledge-bases/rag-pipelines/graphrag/config",
            get(get_graphrag_config).put(put_graphrag_config),
        )
        .route(
            "/knowledge-bases/rag-pipelines/lightrag/config",
            get(get_lightrag_config).put(put_lightrag_config),
        )
        .route(
            "/knowledge-bases/rag-pipelines/lightrag-server/config",
            get(get_lightrag_server_config).put(put_lightrag_server_config),
        )
        .route(
            "/knowledge-bases/rag-pipelines/lightrag/model-options",
            get(get_lightrag_model_options),
        )
        .route(
            "/knowledge-bases/rag-pipelines/model-options",
            get(get_model_options),
        )
        .route(
            "/knowledge-bases/rag-pipelines/graphrag/model-compatibility",
            post(graphrag_model_compatibility),
        )
        .route(
            "/knowledge-bases/rag-pipelines/active-model",
            put(set_active_model),
        )
        .route("/knowledge-bases/configs", get(get_kb_configs))
        .route("/knowledge-bases/configs/sync", post(sync_kb_configs))
}

async fn set_provider_mode(
    ScopedState(state): ScopedState,
    Path(provider): Path<String>,
    Json(body): Json<ModePayload>,
) -> ApiResult<Json<Value>> {
    let provider = provider.trim().to_ascii_lowercase();
    let supported = match provider.as_str() {
        "native" => vec!["hybrid", "keyword"],
        "llamaindex" | "lightrag" | "graphrag" | "pageindex" | "ima" => vec!["hybrid"],
        "lightrag-server" => vec!["local", "global", "hybrid", "naive", "mix"],
        _ => Vec::new(),
    };
    if supported.is_empty() {
        return Err(ApiError::not_found(format!(
            "检索引擎“{provider}”没有可配置的检索模式。"
        )));
    }
    let mode = body.mode.trim().to_ascii_lowercase();
    if !supported.contains(&mode.as_str()) {
        return Err(ApiError::bad_request(format!(
            "“{mode}”不是 {provider} 支持的检索模式，可选值：{}",
            supported.join("、")
        )));
    }
    let mut modes = state
        .database
        .get_setting("rag_provider_modes")?
        .unwrap_or_else(|| json!({}));
    modes[&provider] = Value::String(mode.clone());
    state.database.set_setting("rag_provider_modes", &modes)?;
    Ok(Json(json!({"provider": provider, "mode": mode})))
}

async fn get_pageindex_config(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(read_masked_config(&state, "pageindex")?))
}

async fn put_pageindex_config(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    update_secret_config(&state, "pageindex", &body, &["api_key"])?;
    Ok(Json(read_masked_config(&state, "pageindex")?))
}

async fn get_ima_config(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(read_masked_config(&state, "ima")?))
}

async fn put_ima_config(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    update_secret_config(&state, "ima", &body, &["client_id", "api_key"])?;
    Ok(Json(read_masked_config(&state, "ima")?))
}

async fn get_llamaindex_config(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    let mut value = read_config(&state, "llamaindex")?;
    value["effective_retrieval_profile"] = value
        .get("retrieval_profile")
        .cloned()
        .unwrap_or_else(|| Value::String("hybrid".into()));
    Ok(Json(value))
}

async fn put_llamaindex_config(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let mut current = read_config(&state, "llamaindex")?;
    merge_object(&mut current, &body);
    validate_numeric_range(&current, "top_k", 1, 100)?;
    validate_numeric_range(&current, "chunk_size", 128, 32_000)?;
    validate_numeric_range(&current, "chunk_overlap", 0, 8_000)?;
    write_config(&state, "llamaindex", &current)?;
    get_llamaindex_config(ScopedState(state)).await
}

async fn get_graphrag_config(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(read_config(&state, "graphrag")?))
}

async fn put_graphrag_config(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let mut current = read_config(&state, "graphrag")?;
    merge_object(&mut current, &body);
    validate_numeric_range(&current, "community_level", 0, 20)?;
    write_config(&state, "graphrag", &current)?;
    Ok(Json(current))
}

async fn get_lightrag_config(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(read_config(&state, "lightrag")?))
}

async fn put_lightrag_config(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let mut current = read_config(&state, "lightrag")?;
    if body.get("role_models").is_some_and(Value::is_null) {
        return Err(ApiError::unprocessable("LightRAG 角色模型配置不能清空。"));
    }
    merge_object(&mut current, &body);
    validate_numeric_range(&current, "top_k", 1, 100)?;
    validate_numeric_range(&current, "llm_timeout", 1, 3_600)?;
    write_config(&state, "lightrag", &current)?;
    Ok(Json(current))
}

async fn get_lightrag_server_config(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(read_masked_config(&state, "lightrag-server")?))
}

async fn put_lightrag_server_config(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let mut current = stored_config(&state, "lightrag-server")?;
    if let Some(server_url) = body.get("server_url").and_then(Value::as_str) {
        current["server_url"] = Value::String(normalize_base_url(server_url));
    }
    apply_optional_secret(&mut current, &body, "api_key");
    write_config(&state, "lightrag-server", &current)?;
    Ok(Json(read_masked_config(&state, "lightrag-server")?))
}

async fn get_lightrag_model_options(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(model_options_payload(&state, &["llm"])?))
}

async fn get_model_options(
    ScopedState(state): ScopedState,
    Query(query): Query<ModelOptionsQuery>,
) -> ApiResult<Json<Value>> {
    let requested = query
        .kinds
        .as_deref()
        .unwrap_or("llm,embedding")
        .split(',')
        .map(str::trim)
        .filter(|kind| matches!(*kind, "llm" | "embedding"))
        .collect::<Vec<_>>();
    let kinds = if requested.is_empty() {
        vec!["llm", "embedding"]
    } else {
        requested
    };
    Ok(Json(model_options_payload(&state, &kinds)?))
}

async fn set_active_model(
    ScopedState(state): ScopedState,
    Json(body): Json<ActiveModelPayload>,
) -> ApiResult<Json<Value>> {
    if !matches!(body.kind.as_str(), "llm" | "embedding") {
        return Err(ApiError::bad_request("仅支持切换 llm 或 embedding 模型。"));
    }
    let mut catalog = state
        .database
        .get_setting("catalog")?
        .unwrap_or_else(|| json!({}));
    let service = catalog
        .get_mut("services")
        .and_then(|services| services.get_mut(&body.kind))
        .ok_or_else(|| ApiError::not_found(format!("尚未配置 {} 模型。", body.kind)))?;
    let profile = service
        .get("profiles")
        .and_then(Value::as_array)
        .and_then(|profiles| {
            profiles.iter().find(|profile| {
                profile.get("id").and_then(Value::as_str) == Some(body.profile_id.as_str())
            })
        })
        .ok_or_else(|| ApiError::bad_request("模型配置档案不存在。"))?;
    if !profile
        .get("models")
        .and_then(Value::as_array)
        .is_some_and(|models| {
            models.iter().any(|model| {
                model.get("id").and_then(Value::as_str) == Some(body.model_id.as_str())
            })
        })
    {
        return Err(ApiError::bad_request("所选模型不在该配置档案中。"));
    }
    service["active_profile_id"] = Value::String(body.profile_id);
    service["active_model_id"] = Value::String(body.model_id);
    state.database.set_setting("catalog", &catalog)?;
    let payload = model_options_payload(&state, &[&body.kind])?;
    Ok(Json(payload[&body.kind].clone()))
}

async fn graphrag_model_compatibility(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let profile_id = body.get("profile_id").and_then(Value::as_str).unwrap_or("");
    let model_id = body.get("model_id").and_then(Value::as_str).unwrap_or("");
    if profile_id.is_empty() || model_id.is_empty() {
        return Err(ApiError::bad_request("profile_id 与 model_id 均不能为空。"));
    }
    let catalog = state
        .database
        .get_setting("catalog")?
        .unwrap_or_else(|| json!({}));
    let model = catalog
        .pointer("/services/llm/profiles")
        .and_then(Value::as_array)
        .and_then(|profiles| {
            profiles
                .iter()
                .find(|profile| profile.get("id").and_then(Value::as_str) == Some(profile_id))
        })
        .and_then(|profile| profile.get("models").and_then(Value::as_array))
        .and_then(|models| {
            models
                .iter()
                .find(|model| model.get("id").and_then(Value::as_str) == Some(model_id))
        })
        .ok_or_else(|| ApiError::bad_request("未找到指定的语言文字模型。"))?;
    let model_name = model
        .get("model")
        .or_else(|| model.get("name"))
        .and_then(Value::as_str)
        .unwrap_or(model_id);
    Ok(Json(json!({
        "status": "unverifiable",
        "compatible": Value::Null,
        "code": "native_probe_not_required",
        "message": "原生 Rust 运行时不会额外发起兼容性探测；模型将在下一次 GraphRAG 请求时验证。",
        "model": model_name,
        "binding": profile_id,
        "retryable": false
    })))
}

async fn get_kb_configs(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    let configs = state
        .database
        .get_setting(KB_CONFIGS_KEY)?
        .unwrap_or_else(|| json!({"knowledge_bases": {}}));
    Ok(Json(configs))
}

async fn sync_kb_configs(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    let mut root = state
        .database
        .get_setting(KB_CONFIGS_KEY)?
        .unwrap_or_else(|| json!({"knowledge_bases": {}}));
    if !root.get("knowledge_bases").is_some_and(Value::is_object) {
        root["knowledge_bases"] = json!({});
    }
    for kb in state.database.list_resources("knowledge_base")? {
        let Some(name) = kb
            .get("id")
            .or_else(|| kb.get("name"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let mut config = root["knowledge_bases"]
            .get(name)
            .cloned()
            .unwrap_or_else(|| json!({}));
        for key in [
            "rag_provider",
            "search_mode",
            "pageindex_mode",
            "embedding_model",
            "description",
            "type",
            "external_path",
            "vault_path",
            "server_url",
            "knowledge_base_id",
            "zim_name",
            "db_path",
        ] {
            if let Some(value) = kb.get(key) {
                config[key] = value.clone();
            }
        }
        config["name"] = kb
            .get("name")
            .cloned()
            .unwrap_or_else(|| Value::String(name.to_string()));
        root["knowledge_bases"][name] = config;
    }
    state.database.set_setting(KB_CONFIGS_KEY, &root)?;
    Ok(Json(json!({
        "status": "success",
        "message": "知识库配置已从原生元数据同步。",
        "configs": root
    })))
}

fn stored_config(state: &AppState, provider: &str) -> ApiResult<Value> {
    Ok(state
        .database
        .get_setting(&format!("{CONFIG_PREFIX}{provider}"))?
        .map(|value| merge_defaults(default_provider_config(provider), value))
        .unwrap_or_else(|| default_provider_config(provider)))
}

fn read_config(state: &AppState, provider: &str) -> ApiResult<Value> {
    stored_config(state, provider)
}

fn write_config(state: &AppState, provider: &str, value: &Value) -> ApiResult<()> {
    state
        .database
        .set_setting(&format!("{CONFIG_PREFIX}{provider}"), value)
        .map_err(ApiError::internal)
}

fn read_masked_config(state: &AppState, provider: &str) -> ApiResult<Value> {
    let stored = stored_config(state, provider)?;
    let mut public = stored.clone();
    let api_key_set = stored
        .get("api_key")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty());
    if let Some(object) = public.as_object_mut() {
        object.remove("api_key");
    }
    let public = match provider {
        "pageindex" => json!({
            "api_key_set": api_key_set,
            "configured": api_key_set
        }),
        "ima" => {
            let client_id = stored
                .get("client_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            json!({
                "client_id": client_id,
                "api_key_set": api_key_set,
                "configured": !client_id.is_empty() && api_key_set
            })
        }
        "lightrag-server" => {
            let server_url = stored
                .get("server_url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            json!({
                "server_url": server_url,
                "api_key_set": api_key_set,
                "configured": !server_url.is_empty()
            })
        }
        _ => public,
    };
    Ok(public)
}

fn update_secret_config(
    state: &AppState,
    provider: &str,
    body: &Value,
    allowed: &[&str],
) -> ApiResult<()> {
    let mut current = stored_config(state, provider)?;
    if let Some(object) = body.as_object() {
        for (key, value) in object {
            if !allowed.contains(&key.as_str()) {
                continue;
            }
            if key == "api_key" {
                apply_optional_secret(&mut current, body, key);
            } else if let Some(value) = value.as_str() {
                current[key] = Value::String(value.trim().to_string());
            }
        }
    }
    write_config(state, provider, &current)
}

fn apply_optional_secret(current: &mut Value, body: &Value, key: &str) {
    if let Some(value) = body.get(key).and_then(Value::as_str) {
        current[key] = Value::String(value.trim().to_string());
    }
}

fn merge_defaults(defaults: Value, stored: Value) -> Value {
    let mut result = defaults;
    merge_object(&mut result, &stored);
    result
}

fn merge_object(target: &mut Value, patch: &Value) {
    let Some(patch_object) = patch.as_object() else {
        return;
    };
    if !target.is_object() {
        *target = Value::Object(Map::new());
    }
    let Some(target_object) = target.as_object_mut() else {
        return;
    };
    for (key, value) in patch_object {
        if let Some(target_child) = target_object.get_mut(key) {
            if target_child.is_object() && value.is_object() {
                merge_object(target_child, value);
                continue;
            }
        }
        target_object.insert(key.clone(), value.clone());
    }
}

fn validate_numeric_range(value: &Value, key: &str, min: i64, max: i64) -> ApiResult<()> {
    if let Some(number) = value.get(key).and_then(Value::as_i64) {
        if number < min || number > max {
            return Err(ApiError::unprocessable(format!(
                "{key} 必须位于 {min} 到 {max} 之间。"
            )));
        }
    }
    Ok(())
}

fn normalize_base_url(value: &str) -> String {
    value.trim().trim_end_matches('/').to_string()
}

fn model_options_payload(state: &AppState, kinds: &[&str]) -> ApiResult<Value> {
    let catalog = state
        .database
        .get_setting("catalog")?
        .unwrap_or_else(|| json!({}));
    let mut output = Map::new();
    for kind in kinds {
        let service = catalog.pointer(&format!("/services/{kind}"));
        let active = json!({
            "profile_id": service
                .and_then(|value| value.get("active_profile_id"))
                .cloned()
                .unwrap_or(Value::Null),
            "model_id": service
                .and_then(|value| value.get("active_model_id"))
                .cloned()
                .unwrap_or(Value::Null)
        });
        let mut options = Vec::new();
        if let Some(profiles) = service
            .and_then(|value| value.get("profiles"))
            .and_then(Value::as_array)
        {
            for profile in profiles {
                let profile_id = profile
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let profile_name = profile
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(profile_id);
                if let Some(models) = profile.get("models").and_then(Value::as_array) {
                    for model in models {
                        let model_id = model.get("id").and_then(Value::as_str).unwrap_or_default();
                        let label = model
                            .get("name")
                            .or_else(|| model.get("model"))
                            .and_then(Value::as_str)
                            .unwrap_or(model_id);
                        let dimension = model
                            .get("dimension")
                            .and_then(Value::as_u64)
                            .map(|value| format!("{value}d"))
                            .unwrap_or_default();
                        options.push(json!({
                            "profile_id": profile_id,
                            "profile_name": profile_name,
                            "model_id": model_id,
                            "label": label,
                            "model": model.get("model").cloned().unwrap_or(Value::Null),
                            "detail": dimension
                        }));
                    }
                }
            }
        }
        output.insert(
            (*kind).to_string(),
            json!({"active": active, "options": options}),
        );
    }
    Ok(Value::Object(output))
}

fn default_provider_config(provider: &str) -> Value {
    match provider {
        "pageindex" => json!({
            "api_key_set": false,
            "configured": false
        }),
        "ima" => json!({
            "client_id": "",
            "api_key_set": false,
            "configured": false
        }),
        "llamaindex" => json!({
            "version": 1,
            "retrieval_profile": "hybrid",
            "top_k": 8,
            "vector_top_k_multiplier": 3,
            "bm25_top_k_multiplier": 3,
            "reranker_model": "",
            "rerank_top_k": 24,
            "vector_index_type": "flat",
            "hnsw_m": 32,
            "hnsw_ef_construction": 200,
            "hnsw_ef_search": 80,
            "chunk_size": 1200,
            "chunk_overlap": 180,
            "image_description_concurrency": 2,
            "image_description_batch_size": 4,
            "image_description_timeout_seconds": 60
        }),
        "graphrag" => json!({
            "version": 1,
            "response_type": "multiple paragraphs",
            "community_level": 2,
            "dynamic_community_selection": false
        }),
        "lightrag" => json!({
            "version": 1,
            "top_k": 8,
            "response_type": "multiple paragraphs",
            "max_concurrent_files": 2,
            "llm_model_max_async": 2,
            "entity_extract_max_gleaning": 1,
            "llm_timeout": 120,
            "llm_profile_id": "",
            "llm_model_id": ""
        }),
        "lightrag-server" => json!({
            "server_url": "",
            "api_key_set": false,
            "configured": false
        }),
        _ => json!({}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 配置合并不会覆盖未提交字段() {
        let mut current = json!({"top_k": 8, "nested": {"a": 1, "b": 2}});
        merge_object(&mut current, &json!({"top_k": 4, "nested": {"b": 3}}));
        assert_eq!(current, json!({"top_k": 4, "nested": {"a": 1, "b": 3}}));
    }

    #[test]
    fn 服务地址会去除末尾斜杠() {
        assert_eq!(
            normalize_base_url(" http://localhost:9621/ "),
            "http://localhost:9621"
        );
    }
}
