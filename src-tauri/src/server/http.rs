use super::error::{ApiError, ApiResult};
use super::AppState;
use super::ScopedState;
use axum::body::{Body, Bytes};
use axum::extract::{Multipart, Path as AxumPath, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use uuid::Uuid;

#[derive(Debug, Deserialize)]
struct Pagination {
    limit: Option<usize>,
    offset: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct SearchQuery {
    q: String,
    limit: Option<usize>,
    offset: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct SessionListQuery {
    limit: Option<usize>,
    offset: Option<usize>,
    workspace_id: Option<String>,
    all_workspaces: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct TraceQuery {
    after_seq: Option<usize>,
    limit: Option<usize>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .merge(super::agent_config::router())
        .merge(super::knowledge::router())
        .merge(super::knowledge_sources::router())
        .merge(super::knowledge_visuals::router())
        .merge(super::rag_config::router())
        .merge(super::marginnote4::router())
        .merge(super::mastery::router())
        .merge(super::notebook::router())
        .merge(super::calendar::router())
        .merge(super::document_settings::router())
        .merge(super::documents_api::router())
        .merge(super::office_preview::router())
        .merge(super::memory::router())
        .route("/modules", get(super::modules::list_modules))
        .merge(super::partner_groups::api_router())
        .merge(super::partners::api_router())
        .merge(super::plugins::router())
        .merge(super::provider_auth::router())
        .merge(super::question_bank::router())
        .merge(super::reading::router())
        .merge(super::skills::router())
        .merge(super::video_learning::router())
        .merge(super::visualizers::router())
        .merge(super::subagents::router())
        .merge(super::mcp_cli::router())
        .merge(super::settings_services::router())
        .merge(super::workspace_data::router())
        .merge(super::ws_books::router())
        .merge(super::auth::router())
        .merge(super::multi_user::router())
        .route("/system/status", get(system_status))
        .route("/system/runtime", get(system_runtime))
        .route("/system/memory", get(system_memory))
        .route("/system/test/llm", post(test_llm))
        .route("/system/test/embeddings", post(test_embeddings))
        .route("/system/test/search", post(test_search))
        .route(
            "/system/update",
            get(super::system_update::get_status).post(super::system_update::request_update),
        )
        .route(
            "/system/update/check",
            post(super::system_update::check_for_update),
        )
        .route(
            "/system/update/settings",
            put(super::system_update::update_settings),
        )
        .route("/system/update/job", get(super::system_update::get_job))
        .route("/settings", get(get_settings))
        .route("/settings/ui", get(get_ui_settings).put(put_ui_settings))
        .route("/settings/catalog", get(get_catalog).put(put_catalog))
        .route("/settings/llm-options", get(get_llm_options))
        .route(
            "/settings/draft",
            get(get_draft).put(put_draft).delete(delete_draft),
        )
        .route("/settings/apply", post(apply_settings))
        .route("/settings/apply/service", post(apply_settings_service))
        .route("/settings/apply/provider", post(apply_settings_provider))
        .route("/settings/apply/registry", post(apply_settings_registry))
        .route("/settings/network", get(get_network).put(put_network))
        .route("/settings/enabled-tools", put(put_enabled_tools))
        .route("/settings/theme", put(put_theme))
        .route("/settings/language", put(put_language))
        .route("/settings/chat-response-timeout", put(put_chat_timeout))
        .route("/settings/voice-autoplay", put(put_voice_autoplay))
        .route("/settings/voice-math-speak", put(put_voice_math_speak))
        .route(
            "/settings/chat-starters",
            get(get_chat_starters).put(put_chat_starters),
        )
        .route(
            "/settings/chat-attachments",
            get(get_chat_attachments).put(put_chat_attachments),
        )
        .route("/settings/themes", get(get_themes))
        .route("/settings/sidebar", get(get_sidebar))
        .route(
            "/settings/sidebar/description",
            put(put_sidebar_description),
        )
        .route("/settings/sidebar/nav-order", put(put_sidebar_order))
        .route(
            "/settings/page-customization/{section}",
            get(get_page_customization),
        )
        .route(
            "/settings/page-customization/chat",
            put(put_page_customization_chat),
        )
        .route(
            "/settings/page-customization/learning",
            put(put_page_customization_learning),
        )
        .route("/settings/readiness", get(settings_readiness))
        .route("/settings/usage", get(settings_usage))
        .route("/settings/tour/status", get(tour_status))
        .route("/settings/tour/complete", post(tour_complete))
        .route("/settings/tour/reopen", post(tour_reopen))
        .route("/settings/reset", post(settings_reset))
        .route("/settings/presets", get(settings_presets))
        .route(
            "/settings/presets/{preset_id}/draft",
            post(settings_preset_draft),
        )
        .route("/settings/fetch-models", post(fetch_models))
        .route("/settings/model-capabilities", post(model_capabilities))
        .route("/settings/test-provider", post(test_provider))
        .route("/settings/profile", get(settings_profile))
        .route("/settings/profile/diff", post(settings_profile_diff))
        .route("/settings/provider-choices", get(settings_provider_choices))
        .route(
            "/settings/workspace",
            get(get_kv_workspace).put(put_kv_workspace),
        )
        .route("/settings/workspace/resources", get(workspace_resources))
        .route(
            "/settings/workspace/resource-usage",
            get(workspace_resource_usage),
        )
        .route("/settings/workspace/validate", post(workspace_validate))
        .route("/settings/tests/{service}/start", post(start_service_test))
        .route(
            "/settings/tests/{service}/{run_id}/events",
            get(service_test_events),
        )
        .route(
            "/settings/tests/{service}/{run_id}/cancel",
            post(cancel_service_test),
        )
        .route("/sessions", get(list_sessions))
        .route("/sessions/capabilities", get(session_capabilities))
        .route("/sessions/search", get(search_sessions))
        .route("/sessions/recycle-bin", get(recycle_bin))
        .route(
            "/sessions/submission-replacement",
            post(submission_replacement),
        )
        .route(
            "/sessions/{session_id}",
            get(get_session)
                .patch(rename_session)
                .delete(delete_session),
        )
        .route(
            "/sessions/{session_id}/organization",
            get(get_organization).patch(patch_organization),
        )
        .route("/sessions/{session_id}/threads", get(get_threads))
        .route(
            "/sessions/{session_id}/reply-language",
            patch(patch_reply_language),
        )
        .route(
            "/sessions/{session_id}/branch-selection",
            put(put_branch_selection),
        )
        .route("/sessions/{session_id}/ask-hint", get(session_ask_hint))
        .route(
            "/sessions/{session_id}/migrate-watching",
            post(migrate_watching),
        )
        .route("/sessions/{session_id}/restore", post(restore_session))
        .route("/sessions/{session_id}/purge", delete(purge_session))
        .route(
            "/sessions/{session_id}/messages/{message_id}",
            delete(delete_message),
        )
        .route(
            "/sessions/{session_id}/messages/{message_id}/events",
            get(message_events),
        )
        .route(
            "/sessions/{session_id}/quiz-results",
            post(record_quiz_results),
        )
        .route("/capabilities/registered", get(registered_capabilities))
        .route(
            "/capabilities/settings",
            get(get_capability_settings).put(put_capability_settings),
        )
        .route("/tools", get(list_tools))
        .route("/dashboard/recent", get(dashboard_recent))
        .route("/dashboard/suggestions", get(dashboard_suggestions))
        .route(
            "/dashboard/suggestions/refresh",
            post(dashboard_refresh_suggestions),
        )
        .route("/dashboard/learning-index", get(dashboard_learning_index))
        .route(
            "/dashboard/learning-library/{kind}",
            get(dashboard_learning_library),
        )
        .route(
            "/dashboard/source-library/{kind}",
            get(dashboard_source_library),
        )
        .route("/dashboard/{entry_id}", get(dashboard_entry))
        .route("/task-board", get(task_board))
        .route("/task-board/cards", post(task_board_add_card))
        .route("/task-board/cards/{card_id}", patch(task_board_patch_card))
        .route("/task-board/colors", put(task_board_colors))
        .route("/task-board/events", get(task_board_events))
        .route("/task-board/sessions/{session_id}", put(task_board_session))
        .route(
            "/task-board/sessions/{session_id}/status",
            patch(task_board_session_status),
        )
        .route("/learning-journal", get(learning_journal))
        .route(
            "/imports/chat-history",
            get(imports_list).post(imports_start),
        )
        .route("/personas", get(personas_list).post(persona_create))
        .route(
            "/personas/{name}",
            get(persona_get).put(persona_update).delete(persona_delete),
        )
        .route("/voice/tts", post(voice_tts))
        .route("/voice/stt", post(voice_stt))
        .fallback(api_not_found)
}

pub async fn health(ScopedState(state): ScopedState) -> Json<Value> {
    Json(json!({
        "status": "ready",
        "runtime": "rust",
        "uptime_seconds": state.started_at.elapsed().as_secs()
    }))
}

pub async fn health_live() -> Json<Value> {
    Json(json!({"status": "alive"}))
}

async fn api_not_found() -> Response {
    ApiError::not_found("本地 Rust 服务中不存在这个 API").into_response()
}

async fn system_status(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    let catalog = state
        .database
        .get_setting("catalog")?
        .unwrap_or_else(|| json!({}));
    let configured = active_llm(&catalog).is_some();
    Ok(Json(json!({
        "backend": {
            "status": "healthy",
            "timestamp": chrono::Utc::now().to_rfc3339()
        },
        "llm": {
            "status": if configured {"configured"} else {"not_configured"},
            "model": active_model_name(&catalog)
        },
        "embeddings": {"status": "optional"},
        "search": {"status": "optional"},
        "database": state.database.path().display().to_string(),
        "runtime": "rust"
    })))
}

async fn system_runtime(ScopedState(state): ScopedState) -> Json<Value> {
    Json(json!({
        "protocol_version": "2.0",
        "minimum_web_protocol_version": "2.0",
        "coordination_mode": "memory",
        "worker_id": "rust-local",
        "worker_count": 1,
        "redis_configured": false,
        "redis_status": "not_configured",
        "leader_healthy": true,
        "lease_ttl_seconds": 30,
        "renew_interval_seconds": 10,
        "recovery_interval_seconds": 5,
        "recovery_backlog": 0,
        "owner_turn_count": state.active_turns.lock().len(),
        "uptime_seconds": state.started_at.elapsed().as_secs()
    }))
}

async fn system_memory(ScopedState(state): ScopedState) -> Json<Value> {
    Json(json!({
        "process": "deeptutor-desktop",
        "runtime": "rust",
        "uptime_seconds": state.started_at.elapsed().as_secs(),
        "database": state.database.path().display().to_string(),
        "data_root": state.paths.data.display().to_string(),
        "cache_root": state.paths.cache.display().to_string()
    }))
}

async fn test_llm(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    let catalog = state
        .database
        .get_setting("catalog")?
        .unwrap_or_else(|| json!({}));
    Ok(Json(
        super::provider_probe::probe_once(state, "llm", catalog).await,
    ))
}

async fn test_embeddings(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    let catalog = state
        .database
        .get_setting("catalog")?
        .unwrap_or_else(|| json!({}));
    Ok(Json(
        super::provider_probe::probe_once(state, "embedding", catalog).await,
    ))
}

async fn test_search(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    let catalog = state
        .database
        .get_setting("catalog")?
        .unwrap_or_else(|| json!({}));
    Ok(Json(
        super::provider_probe::probe_once(state, "search", catalog).await,
    ))
}

async fn get_settings(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    let ui = state
        .database
        .get_setting("ui")?
        .unwrap_or_else(|| json!({}));
    let catalog = state
        .database
        .get_setting("catalog")?
        .unwrap_or_else(|| json!({}));
    Ok(Json(json!({
        "ui": ui,
        "catalog": catalog,
        "providers": provider_options(),
        "connection_targets": connection_targets(),
        "task_kinds": task_kinds()
    })))
}

async fn get_ui_settings(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(
        state
            .database
            .get_setting("ui")?
            .unwrap_or_else(|| json!({})),
    ))
}

async fn put_ui_settings(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    Ok(Json(state.database.merge_setting("ui", &body)?))
}

async fn get_catalog(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(
        state
            .database
            .get_setting("catalog")?
            .unwrap_or_else(|| json!({})),
    ))
}

async fn put_catalog(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    state.database.set_setting("catalog", &body)?;
    Ok(Json(body))
}

async fn get_llm_options(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    let catalog = state
        .database
        .get_setting("catalog")?
        .unwrap_or_else(|| json!({}));
    let mut options = Vec::new();
    let mut active = Value::Null;
    if let Some(service) = catalog.get("services").and_then(|value| value.get("llm")) {
        let active_profile_id = service.get("active_profile_id").and_then(Value::as_str);
        let active_model_id = service.get("active_model_id").and_then(Value::as_str);
        if let Some(profiles) = service.get("profiles").and_then(Value::as_array) {
            for profile in profiles {
                let profile_id = profile.get("id").and_then(Value::as_str).unwrap_or("");
                let profile_name = profile
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(profile_id);
                if let Some(models) = profile.get("models").and_then(Value::as_array) {
                    for model in models {
                        let model_id = model.get("id").and_then(Value::as_str).unwrap_or("");
                        let is_active = active_profile_id == Some(profile_id)
                            && active_model_id == Some(model_id);
                        options.push(json!({
                            "profile_id": profile_id,
                            "profile_name": profile_name,
                            "model_id": model_id,
                            "model_name": model.get("name").or_else(|| model.get("model")).cloned().unwrap_or(Value::Null),
                            "model": model.get("model").cloned().unwrap_or(Value::Null),
                            "provider": profile.get("binding").or_else(|| profile.get("provider")).cloned().unwrap_or(Value::Null),
                            "is_active_default": is_active
                        }));
                        if is_active {
                            active = json!({"profile_id": profile_id, "model_id": model_id});
                        }
                    }
                }
            }
        }
    }
    Ok(Json(json!({"active": active, "options": options})))
}

async fn get_draft(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(json!({
        "draft": state.database.get_setting("settings_draft")?
    })))
}

async fn put_draft(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let mut stored = body;
    stored["version"] = Value::from(1);
    stored["updated_at"] = Value::String(chrono::Utc::now().to_rfc3339());
    state.database.set_setting("settings_draft", &stored)?;
    Ok(Json(json!({"draft": stored})))
}

async fn delete_draft(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    state.database.set_setting("settings_draft", &Value::Null)?;
    Ok(Json(json!({"deleted": true})))
}

async fn apply_settings(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    if let Some(catalog) = body.get("catalog") {
        if !catalog.is_null() {
            state.database.set_setting("catalog", catalog)?;
        }
    }
    if let Some(ui) = body.get("ui") {
        state.database.merge_setting("ui", ui)?;
    }
    if let Some(extensions) = body.get("extensions").and_then(Value::as_object) {
        for (key, value) in extensions {
            state
                .database
                .set_setting(&format!("extension:{key}"), value)?;
        }
    }
    state.database.set_setting("settings_draft", &Value::Null)?;
    Ok(Json(json!({"ok": true})))
}

async fn apply_settings_service(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let service_name = body.get("service").and_then(Value::as_str).unwrap_or("");
    let service = body
        .get("service_config")
        .or_else(|| body.get("config"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    if service_name.is_empty() {
        return Err(ApiError::bad_request("缺少服务名称"));
    }
    let mut catalog = state
        .database
        .get_setting("catalog")?
        .unwrap_or_else(|| json!({}));
    catalog["services"][service_name] = service;
    state.database.set_setting("catalog", &catalog)?;
    Ok(Json(json!({"ok": true, "catalog": catalog})))
}

async fn apply_settings_provider(state: ScopedState, body: Json<Value>) -> ApiResult<Json<Value>> {
    apply_settings_service(state, body).await
}

async fn apply_settings_registry(state: ScopedState, body: Json<Value>) -> ApiResult<Json<Value>> {
    apply_settings(state, body).await
}

async fn get_network(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(
        state
            .database
            .get_setting("network")?
            .unwrap_or_else(|| json!({})),
    ))
}

async fn put_network(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    Ok(Json(state.database.merge_setting("network", &body)?))
}

async fn put_enabled_tools(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let tools = body
        .get("tools")
        .or_else(|| body.get("enabled_optional_tools"))
        .cloned()
        .unwrap_or(body);
    state.database.set_setting("enabled_tools", &tools)?;
    Ok(Json(json!({"ok": true, "enabled_optional_tools": tools})))
}

async fn put_theme(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let ui = state.database.merge_setting(
        "ui",
        &json!({"theme": body.get("theme").cloned().unwrap_or(Value::Null)}),
    )?;
    Ok(Json(json!({"ok": true, "ui": ui})))
}

async fn put_language(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let ui = state.database.merge_setting(
        "ui",
        &json!({
            "language": body.get("language").cloned().unwrap_or(Value::Null),
            "response_language": body.get("response_language").or_else(|| body.get("language")).cloned().unwrap_or(Value::Null)
        }),
    )?;
    Ok(Json(json!({"ok": true, "ui": ui})))
}

async fn put_chat_timeout(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    Ok(Json(state.database.merge_setting("chat", &body)?))
}

async fn put_voice_autoplay(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    Ok(Json(state.database.merge_setting("voice", &body)?))
}

async fn put_voice_math_speak(state: ScopedState, body: Json<Value>) -> ApiResult<Json<Value>> {
    put_voice_autoplay(state, body).await
}

async fn get_chat_starters(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(
        state
            .database
            .get_setting("chat_starters")?
            .unwrap_or_else(|| json!({"starters": []})),
    ))
}

async fn put_chat_starters(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    state.database.set_setting("chat_starters", &body)?;
    Ok(Json(body))
}

async fn get_chat_attachments(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(
        state
            .database
            .get_setting("chat_attachments")?
            .unwrap_or_else(|| json!({"attachments": []})),
    ))
}

async fn put_chat_attachments(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    state.database.set_setting("chat_attachments", &body)?;
    Ok(Json(body))
}

async fn get_themes() -> Json<Value> {
    Json(json!([
        {"id": "light", "name": "明亮"},
        {"id": "dark", "name": "深色"},
        {"id": "snow", "name": "雪白"},
        {"id": "glass", "name": "玻璃"}
    ]))
}

async fn get_sidebar(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(state.database.get_setting("sidebar")?.unwrap_or_else(
        || json!({"description": "", "nav_order": []}),
    )))
}

async fn put_sidebar_description(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    Ok(Json(state.database.merge_setting("sidebar", &body)?))
}

async fn put_sidebar_order(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    Ok(Json(state.database.merge_setting("sidebar", &body)?))
}

async fn get_page_customization(
    ScopedState(state): ScopedState,
    AxumPath(section): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    let all = state
        .database
        .get_setting("page_customization")?
        .unwrap_or_else(|| json!({}));
    Ok(Json(
        all.get(&section).cloned().unwrap_or_else(|| json!({})),
    ))
}

async fn put_page_customization_chat(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let current = state
        .database
        .get_setting("page_customization")?
        .unwrap_or_else(|| json!({}));
    Ok(Json(state.database.merge_setting(
        "page_customization",
        &json!({"chat": merge_values(current.get("chat").cloned().unwrap_or_else(|| json!({})), body)}),
    )?))
}

async fn put_page_customization_learning(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let current = state
        .database
        .get_setting("page_customization")?
        .unwrap_or_else(|| json!({}));
    Ok(Json(state.database.merge_setting(
        "page_customization",
        &json!({"learning": merge_values(current.get("learning").cloned().unwrap_or_else(|| json!({})), body)}),
    )?))
}

async fn settings_readiness(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    let catalog = state
        .database
        .get_setting("catalog")?
        .unwrap_or_else(|| json!({}));
    Ok(Json(json!({
        "ready": active_llm(&catalog).is_some(),
        "services": {
            "llm": if active_llm(&catalog).is_some() {"configured"} else {"not_configured"},
            "database": "ready",
            "rust_runtime": "ready"
        }
    })))
}

async fn settings_usage(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(
        state.database.get_setting("usage")?.unwrap_or_else(|| {
            json!({
                "totals": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0, "cost": 0},
                "by_model": [],
                "by_day": []
            })
        }),
    ))
}

async fn tour_status(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(state.database.get_setting("tour")?.unwrap_or_else(
        || json!({"completed": false, "reopened": false}),
    )))
}

async fn tour_complete(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(
        state
            .database
            .merge_setting("tour", &json!({"completed": true}))?,
    ))
}

async fn tour_reopen(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(state.database.merge_setting(
        "tour",
        &json!({"completed": false, "reopened": true}),
    )?))
}

async fn settings_reset(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    state.database.set_setting(
        "ui",
        &json!({
            "theme": "snow",
            "language": "zh",
            "response_language": "zh",
            "code_block_theme": "github-dark",
            "code_block_show_line_numbers": true,
            "code_block_wrap_long_lines": false
        }),
    )?;
    Ok(Json(json!({"ok": true})))
}

async fn settings_presets() -> Json<Value> {
    Json(json!([
        {"id": "balanced", "name": "平衡"},
        {"id": "local", "name": "本地模型"},
        {"id": "research", "name": "研究"}
    ]))
}

async fn settings_preset_draft(
    ScopedState(state): ScopedState,
    AxumPath(preset_id): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    let catalog = state
        .database
        .get_setting("catalog")?
        .unwrap_or_else(|| json!({}));
    Ok(Json(json!({
        "preset_id": preset_id,
        "catalog": catalog,
        "extensions": {}
    })))
}

async fn fetch_models(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let base_url = body
        .get("base_url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim_end_matches('/');
    let api_key = body.get("api_key").and_then(Value::as_str).unwrap_or("");
    if base_url.is_empty() {
        return Err(ApiError::bad_request("缺少 base_url"));
    }
    let url = format!("{base_url}/models");
    let mut request = state.client.get(url);
    if !api_key.is_empty() {
        request = request.bearer_auth(api_key);
    }
    let response = request
        .send()
        .await
        .map_err(|error| ApiError::provider(format!("无法获取模型列表：{error}")))?;
    let status = response.status();
    let payload = response.json::<Value>().await.unwrap_or_else(|_| json!({}));
    if !status.is_success() {
        return Err(ApiError::provider(format!("模型服务返回 HTTP {status}")));
    }
    let models = payload
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|row| row.get("id").and_then(Value::as_str).map(String::from))
        .map(|id| json!({"id": id, "name": id}))
        .collect::<Vec<_>>();
    Ok(Json(json!({"models": models})))
}

async fn model_capabilities(Json(body): Json<Value>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "capabilities": body.get("capabilities").cloned().unwrap_or_else(|| json!({}))
    }))
}

async fn test_provider(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let profile = body.get("profile").cloned().unwrap_or(body);
    let base_url = profile
        .get("base_url")
        .and_then(Value::as_str)
        .unwrap_or("");
    if base_url.is_empty() {
        return Ok(Json(json!({"ok": false, "message": "缺少服务地址"})));
    }
    let endpoint = format!("{}/models", base_url.trim_end_matches('/'));
    let mut request = state.client.get(endpoint);
    if let Some(key) = profile.get("api_key").and_then(Value::as_str) {
        if !key.is_empty() {
            request = request.bearer_auth(key);
        }
    }
    match request.send().await {
        Ok(response) => Ok(Json(json!({
            "ok": response.status().is_success(),
            "message": format!("HTTP {}", response.status())
        }))),
        Err(error) => Ok(Json(json!({"ok": false, "message": error.to_string()}))),
    }
}

async fn settings_profile(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    super::settings_profile::export_profile(&state).map(Json)
}

async fn settings_profile_diff(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    super::settings_profile::review_import(&state, &body).map(Json)
}

async fn settings_provider_choices() -> Json<Value> {
    Json(provider_options())
}

async fn get_kv_workspace(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    get_raw_setting(&state, "workspace")
}
async fn put_kv_workspace(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    put_raw_setting(&state, "workspace", body)
}

fn get_raw_setting(state: &AppState, key: &str) -> ApiResult<Json<Value>> {
    Ok(Json(
        state
            .database
            .get_setting(key)?
            .unwrap_or_else(|| json!({})),
    ))
}

fn put_raw_setting(state: &AppState, key: &str, body: Value) -> ApiResult<Json<Value>> {
    Ok(Json(state.database.merge_setting(key, &body)?))
}

async fn list_workspaces(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(json!({
        "workspaces": state.database.list_resources("workspace")?
    })))
}

async fn create_workspace(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let id = body
        .get("id")
        .and_then(Value::as_str)
        .map(String::from)
        .unwrap_or_else(|| format!("workspace-{}", Uuid::new_v4()));
    let mut value = body;
    value["id"] = Value::String(id.clone());
    value["path"] = value.get("path").cloned().unwrap_or_else(|| {
        Value::String(
            state
                .paths
                .data
                .join("workspaces")
                .join(&id)
                .display()
                .to_string(),
        )
    });
    Ok(Json(state.database.put_resource(
        "workspace",
        &id,
        &value,
    )?))
}

async fn workspace_resources(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(
        json!({"resources": state.database.list_resources("workspace_resource")?}),
    ))
}

async fn workspace_resource_usage(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    let bytes = directory_size(&state.paths.root);
    Ok(Json(json!({
        "root": state.paths.root.display().to_string(),
        "total_bytes": bytes,
        "data_bytes": directory_size(&state.paths.data),
        "cache_bytes": directory_size(&state.paths.cache)
    })))
}

async fn workspace_validate(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(json!({
        "valid": true,
        "root": state.paths.root.display().to_string()
    })))
}

async fn start_service_test(
    ScopedState(state): ScopedState,
    AxumPath(service): AxumPath<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    if !matches!(
        service.as_str(),
        "llm" | "task" | "embedding" | "search" | "tts" | "stt" | "imagegen" | "videogen"
    ) {
        return Err(ApiError::bad_request(format!(
            "不支持的服务诊断类型：{service}"
        )));
    }
    let catalog = body
        .get("catalog")
        .cloned()
        .filter(Value::is_object)
        .or_else(|| state.database.get_setting("catalog").ok().flatten())
        .unwrap_or_else(|| json!({}));
    let run_id = Uuid::new_v4().to_string();
    let run = std::sync::Arc::new(super::provider_probe::ServiceTestRun::new(service));
    {
        let mut runs = state.service_tests.lock();
        if runs.len() >= 64 {
            let stale = runs
                .iter()
                .filter(|(_, candidate)| candidate.is_done())
                .map(|(id, _)| id.clone())
                .take(runs.len().saturating_sub(63))
                .collect::<Vec<_>>();
            for id in stale {
                runs.remove(&id);
            }
        }
        runs.insert(run_id.clone(), run.clone());
    }
    let task_state = state.clone();
    let task_run = run.clone();
    tokio::spawn(async move {
        super::provider_probe::execute(task_state, task_run, catalog).await;
    });
    Ok(Json(json!({"run_id": run_id})))
}

async fn service_test_events(
    ScopedState(state): ScopedState,
    AxumPath((service, run_id)): AxumPath<(String, String)>,
) -> ApiResult<Response> {
    let run = state
        .service_tests
        .lock()
        .get(&run_id)
        .cloned()
        .ok_or_else(|| ApiError::not_found("诊断任务不存在或已过期。"))?;
    if run.service_name() != service {
        return Err(ApiError::not_found("诊断任务不属于该服务。"));
    }
    let runs = state.service_tests.clone();
    let body = Body::from_stream(async_stream::stream! {
        let mut sent = 0usize;
        let mut heartbeat_ticks = 0u16;
        loop {
            let events = run.snapshot(sent);
            sent += events.len();
            for event in events {
                yield Ok::<Bytes, std::convert::Infallible>(Bytes::from(format!(
                    "data: {}\n\n",
                    serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_string())
                )));
            }

            if run.is_done() {
                // `done` 与最后一条事件之间存在极短发布窗口，再取一次尾部。
                let tail = run.snapshot(sent);
                if tail.is_empty() {
                    break;
                }
                for event in tail {
                    yield Ok::<Bytes, std::convert::Infallible>(Bytes::from(format!(
                        "data: {}\n\n",
                        serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_string())
                    )));
                }
                break;
            }

            heartbeat_ticks += 1;
            if heartbeat_ticks >= 150 {
                heartbeat_ticks = 0;
                yield Ok::<Bytes, std::convert::Infallible>(Bytes::from_static(
                    b"event: heartbeat\ndata: {}\n\n",
                ));
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        // 完成后保留一小段时间，便于客户端晚到重连并读取完整事件。
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        runs.lock().remove(&run_id);
    });
    Ok((
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-store"),
            (header::CONNECTION, "keep-alive"),
        ],
        body,
    )
        .into_response())
}

async fn cancel_service_test(
    ScopedState(state): ScopedState,
    AxumPath((service, run_id)): AxumPath<(String, String)>,
) -> ApiResult<Json<Value>> {
    let run = state
        .service_tests
        .lock()
        .get(&run_id)
        .cloned()
        .ok_or_else(|| ApiError::not_found("诊断任务不存在或已过期。"))?;
    if run.service_name() != service {
        return Err(ApiError::not_found("诊断任务不属于该服务。"));
    }
    run.cancel();
    Ok(Json(json!({"cancelled": true})))
}

async fn list_sessions(
    ScopedState(state): ScopedState,
    Query(query): Query<SessionListQuery>,
) -> ApiResult<Json<Value>> {
    let sessions = state.database.list_sessions(
        query.limit.unwrap_or(50).clamp(1, 200),
        query.offset.unwrap_or(0),
    )?;
    Ok(Json(json!({"sessions": sessions})))
}

async fn session_capabilities() -> Json<Value> {
    Json(json!({
        "submission_replacement": true,
        "turn_replay": true,
        "durable_turns": true,
        "web_protocol_version": "2.0"
    }))
}

async fn search_sessions(
    ScopedState(state): ScopedState,
    Query(query): Query<SearchQuery>,
) -> ApiResult<Json<Value>> {
    if query.q.trim().is_empty() {
        return Err(ApiError::bad_request("搜索词不能为空"));
    }
    Ok(Json(state.database.search_sessions(
        query.q.trim(),
        query.limit.unwrap_or(50).clamp(1, 100),
        query.offset.unwrap_or(0),
    )?))
}

async fn recycle_bin(
    ScopedState(state): ScopedState,
    Query(query): Query<Pagination>,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!({
        "sessions": state.database.list_recycle_bin(
            query.limit.unwrap_or(50).clamp(1, 200),
            query.offset.unwrap_or(0)
        )?
    })))
}

async fn submission_replacement(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let original_id = body
        .get("original_submission_id")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::unprocessable("缺少原始提交 ID。"))?;
    let request = body
        .get("request")
        .cloned()
        .filter(Value::is_object)
        .ok_or_else(|| ApiError::unprocessable("替换请求必须是 JSON 对象。"))?;
    super::engine::replace_submission(state, original_id, request)
        .await
        .map(Json)
        .map_err(ApiError::conflict)
}

async fn get_session(
    ScopedState(state): ScopedState,
    AxumPath(session_id): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    state
        .database
        .get_session(&session_id)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("会话不存在"))
}

async fn rename_session(
    ScopedState(state): ScopedState,
    AxumPath(session_id): AxumPath<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let title = body
        .get("title")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad_request("缺少会话标题"))?;
    if !state.database.update_session_title(&session_id, title)? {
        return Err(ApiError::not_found("会话不存在"));
    }
    Ok(Json(json!({
        "session": state.database.get_session_meta(&session_id)?
    })))
}

async fn delete_session(
    ScopedState(state): ScopedState,
    AxumPath(session_id): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    if let Some(turn) = state.database.get_active_turn(&session_id)? {
        if let Some(turn_id) = turn.get("id").and_then(Value::as_str) {
            state.cancel_turn(turn_id);
        }
    }
    if !state.database.soft_delete_session(&session_id)? {
        return Err(ApiError::not_found("会话不存在"));
    }
    Ok(Json(json!({"deleted": true, "session_id": session_id})))
}

async fn get_organization(
    ScopedState(state): ScopedState,
    AxumPath(session_id): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!({
        "session": state
            .database
            .get_session_meta(&session_id)?
            .ok_or_else(|| ApiError::not_found("会话不存在"))?
    })))
}

async fn patch_organization(
    ScopedState(state): ScopedState,
    AxumPath(session_id): AxumPath<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let session = state
        .database
        .update_session_preferences(&session_id, &body)?
        .ok_or_else(|| ApiError::not_found("会话不存在"))?;
    Ok(Json(json!({"session": session})))
}

async fn get_threads(
    ScopedState(state): ScopedState,
    AxumPath(session_id): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    let threads = state
        .database
        .list_sessions(200, 0)?
        .into_iter()
        .filter(|session| {
            session
                .get("preferences")
                .and_then(|value| value.get("parent_session_id"))
                .and_then(Value::as_str)
                == Some(session_id.as_str())
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({"threads": threads})))
}

async fn patch_reply_language(
    ScopedState(state): ScopedState,
    AxumPath(session_id): AxumPath<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let session = state
        .database
        .update_session_preferences(
            &session_id,
            &json!({"reply_language_override": body.get("language").cloned().unwrap_or(Value::Null)}),
        )?
        .ok_or_else(|| ApiError::not_found("会话不存在"))?;
    Ok(Json(json!({"session": session})))
}

async fn put_branch_selection(
    ScopedState(state): ScopedState,
    AxumPath(session_id): AxumPath<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let selected = body
        .get("selected_branches")
        .cloned()
        .unwrap_or_else(|| json!({}));
    state
        .database
        .update_session_preferences(&session_id, &json!({"selected_branches": selected.clone()}))?;
    Ok(Json(json!({"selected_branches": selected})))
}

async fn session_ask_hint(
    ScopedState(state): ScopedState,
    AxumPath(session_id): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    super::hints::get_ask_hint(&state, &session_id)
        .await
        .map(Json)
        .map_err(ApiError::internal)
}

async fn migrate_watching(
    ScopedState(state): ScopedState,
    AxumPath(session_id): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    let session = state
        .database
        .update_session_preferences(
            &session_id,
            &json!({
                "capability": "chat",
                "workspace_mode": "immersive_reading",
                "session_kind": "immersive_reading"
            }),
        )?
        .ok_or_else(|| ApiError::not_found("会话不存在"))?;
    Ok(Json(session))
}

async fn restore_session(
    ScopedState(state): ScopedState,
    AxumPath(session_id): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    if !state.database.restore_session(&session_id)? {
        return Err(ApiError::not_found("回收站中没有该会话"));
    }
    Ok(Json(json!({
        "restored": true,
        "session": state.database.get_session_meta(&session_id)?
    })))
}

async fn purge_session(
    ScopedState(state): ScopedState,
    AxumPath(session_id): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    if !state.database.purge_session(&session_id)? {
        return Err(ApiError::not_found("回收站中没有该会话"));
    }
    Ok(Json(json!({"purged": true, "session_id": session_id})))
}

async fn delete_message(
    ScopedState(state): ScopedState,
    AxumPath((session_id, message_id)): AxumPath<(String, i64)>,
) -> ApiResult<Json<Value>> {
    if !state.database.delete_message(&session_id, message_id)? {
        return Err(ApiError::not_found("消息不存在"));
    }
    Ok(Json(json!({"deleted": true, "message_id": message_id})))
}

async fn message_events(
    ScopedState(state): ScopedState,
    AxumPath((session_id, message_id)): AxumPath<(String, i64)>,
    Query(query): Query<TraceQuery>,
) -> ApiResult<Json<Value>> {
    let message = state
        .database
        .get_message(&session_id, message_id)?
        .ok_or_else(|| ApiError::not_found("消息不存在"))?;
    let all_events = message
        .get("events")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let after_seq = query.after_seq.unwrap_or(0);
    let limit = query.limit.unwrap_or(500).clamp(1, 1000);
    let events = all_events
        .iter()
        .filter(|event| event.get("seq").and_then(Value::as_u64).unwrap_or(0) > after_seq as u64)
        .take(limit)
        .cloned()
        .collect::<Vec<_>>();
    Ok(Json(json!({
        "session_id": session_id,
        "message_id": message_id,
        "turn_id": message.get("metadata").and_then(|value| value.get("turn_id")).cloned().unwrap_or(Value::Null),
        "events": events,
        "total": all_events.len(),
        "last_seq": message.get("metadata").and_then(|value| value.get("last_seq")).and_then(Value::as_u64).unwrap_or(0),
        "next_seq": Value::Null,
        "complete": true
    })))
}

async fn record_quiz_results(
    ScopedState(state): ScopedState,
    AxumPath(session_id): AxumPath<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let answers = body
        .get("answers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if answers.is_empty() {
        return Err(ApiError::bad_request("题库结果不能为空"));
    }
    let content = format!(
        "题库练习结果：{} 题，正确 {} 题",
        answers.len(),
        answers
            .iter()
            .filter(|item| item.get("is_correct").and_then(Value::as_bool) == Some(true))
            .count()
    );
    state.database.add_message(crate::storage::NewMessage {
        session_id: &session_id,
        role: "user",
        content: &content,
        capability: Some("deep_question"),
        events: json!([]),
        attachments: json!([]),
        metadata: json!({"quiz_results": answers}),
        parent_message_id: None,
    })?;
    let id = format!("quiz-{}", Uuid::new_v4());
    state.database.put_resource(
        "quiz_result",
        &id,
        &json!({"session_id": session_id, "answers": answers}),
    )?;
    Ok(Json(json!({
        "recorded": true,
        "session_id": session_id,
        "answer_count": answers.len(),
        "notebook_count": 0,
        "content": content
    })))
}

async fn registered_capabilities(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    let capabilities = [
        ("chat", "对话"),
        ("ask_questions", "引导提问"),
        ("audio_overview", "音频概览"),
        ("deep_solve", "深度解题"),
        ("deep_question", "深度出题"),
        ("deep_research", "深度研究"),
        ("visualize", "可视化"),
        ("math_animator", "数学动画"),
        ("mastery_path", "掌握路径"),
        ("immersive_reading", "沉浸阅读"),
        ("video_lesson", "视频课程"),
    ];
    let mut rows = capabilities
        .into_iter()
        .map(|(id, name)| {
            json!({
                "id": id,
                "kind": "turn",
                "available": true,
                "manifest": {"name": id, "display_name": name, "description": name},
                "config_schema": {}
            })
        })
        .collect::<Vec<_>>();
    rows.extend(super::plugins::plugin_capability_catalog(&state).map_err(ApiError::internal)?);
    Ok(Json(json!({"capabilities": rows})))
}

async fn get_capability_settings(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    get_raw_setting(&state, "capability_settings")
}

async fn put_capability_settings(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    put_raw_setting(&state, "capability_settings", body)
}

async fn list_tools(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    let enabled = state
        .database
        .get_setting("enabled_tools")?
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default();
    let enabled_names = enabled.iter().filter_map(Value::as_str).collect::<Vec<_>>();
    let definitions = [
        ("brainstorm", "头脑风暴", "生成有依据的多方向想法"),
        ("web_search", "网页搜索", "搜索互联网并附带来源"),
        ("paper_search", "论文搜索", "检索 arXiv 论文"),
        ("zotero_search", "Zotero 搜索", "搜索本地 Zotero 文库"),
        ("reason", "深度推理", "使用专用推理提示词"),
        ("geogebra_analysis", "GeoGebra 分析", "分析数学图像"),
        ("imagegen", "图像生成", "按描述生成图像"),
        ("videogen", "视频生成", "按描述生成视频"),
    ];
    let tools = definitions
        .into_iter()
        .map(|(name, zh, description)| {
            json!({
                "name": name,
                "description": description,
                "description_i18n": {"zh": description, "en": description},
                "parameters": [],
                "hints": {"zh": {}, "en": {}},
                "aliases": [],
                "toggleable": true,
                "enabled": enabled_names.contains(&name),
                "coming_soon": false,
                "capability": null,
                "available": name != "zotero_search",
                "unavailable_reason": if name == "zotero_search" {Some("not_configured")} else {None}
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({
        "tools": tools,
        "enabled_optional_tools": enabled_names
    })))
}

async fn dashboard_recent(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(json!({
        "sessions": state.database.list_sessions(6, 0)?,
        "items": state.database.list_resources("dashboard_recent")?
    })))
}

async fn dashboard_suggestions(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(json!({
        "suggestions": state.database.list_resources("dashboard_suggestion")?
    })))
}

async fn dashboard_refresh_suggestions() -> Json<Value> {
    Json(json!({"suggestions": []}))
}

async fn dashboard_learning_index(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(learning_index_payload(&state)))
}

/// 汇总五个学习入口，并让单个数据源失败只影响对应入口。
fn learning_index_payload(state: &AppState) -> Value {
    let mut failed = Vec::new();
    let books = read_learning_rows("books", &mut failed, || {
        super::ws_books::api_books_list(state).and_then(|payload| {
            payload
                .get("books")
                .and_then(Value::as_array)
                .cloned()
                .ok_or_else(|| "书籍列表响应缺少 books 字段".to_string())
        })
    });
    let mastery = read_learning_rows("mastery", &mut failed, || {
        super::mastery::api_topic_rows(state)
    });
    let reading = read_learning_rows("reading", &mut failed, || {
        super::reading::api_workspace_rows(state)
    });
    let watching = read_learning_rows("watching", &mut failed, || {
        Ok(state
            .database
            .list_sessions(10_000, 0)?
            .into_iter()
            .filter(is_watching_session)
            .collect())
    });
    let practice = read_learning_rows("practice", &mut failed, || {
        super::question_bank::navigation_entries(state)
    });

    json!({
        "sources": {
            "books": books,
            "mastery": mastery,
            "reading": reading,
            "watching": watching,
            "practice": practice
        },
        "failed": failed
    })
}

fn read_learning_rows(
    kind: &'static str,
    failed: &mut Vec<&'static str>,
    reader: impl FnOnce() -> Result<Vec<Value>, String>,
) -> Vec<Value> {
    match reader() {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(kind, error = %error, "学习导航数据源读取失败");
            failed.push(kind);
            Vec::new()
        }
    }
}

fn is_watching_session(session: &Value) -> bool {
    if session.get("archived").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    let preferences = session.get("preferences").and_then(Value::as_object);
    preferences
        .and_then(|value| value.get("workspace_mode"))
        .and_then(Value::as_str)
        == Some("immersive_watching")
        || preferences
            .and_then(|value| value.get("capability"))
            .and_then(Value::as_str)
            == Some("immersive_watching")
}

async fn dashboard_learning_library(
    ScopedState(state): ScopedState,
    AxumPath(kind): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!({
        "items": state.database.list_resources(&format!("learning_library:{kind}"))?
    })))
}

async fn dashboard_source_library(
    ScopedState(state): ScopedState,
    AxumPath(kind): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    Ok(Json(json!({
        "items": state.database.list_resources(&format!("source_library:{kind}"))?
    })))
}

async fn dashboard_entry(
    ScopedState(state): ScopedState,
    AxumPath(entry_id): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    state
        .database
        .get_resource("dashboard", &entry_id)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("仪表盘条目不存在"))
}

async fn task_board(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    let value = state
        .database
        .get_resource("task_board", "default")?
        .unwrap_or_else(default_task_board);
    Ok(Json(value))
}

async fn task_board_add_card(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let mut board = state
        .database
        .get_resource("task_board", "default")?
        .unwrap_or_else(default_task_board);
    let id = body
        .get("id")
        .and_then(Value::as_str)
        .map(String::from)
        .unwrap_or_else(|| format!("card-{}", Uuid::new_v4()));
    let mut card = body;
    card["id"] = Value::String(id);
    card["created_at"] = Value::from(crate::storage::now());
    board["cards"].as_array_mut().unwrap().push(card);
    board["revision"] = Value::from(board["revision"].as_i64().unwrap_or(0) + 1);
    Ok(Json(state.database.put_resource(
        "task_board",
        "default",
        &board,
    )?))
}

async fn task_board_patch_card(
    ScopedState(state): ScopedState,
    AxumPath(card_id): AxumPath<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let mut board = state
        .database
        .get_resource("task_board", "default")?
        .unwrap_or_else(default_task_board);
    if let Some(cards) = board.get_mut("cards").and_then(Value::as_array_mut) {
        if let Some(card) = cards
            .iter_mut()
            .find(|card| card.get("id").and_then(Value::as_str) == Some(card_id.as_str()))
        {
            merge_values_in_place(card, &body);
        }
    }
    board["revision"] = Value::from(board["revision"].as_i64().unwrap_or(0) + 1);
    Ok(Json(state.database.put_resource(
        "task_board",
        "default",
        &board,
    )?))
}

async fn task_board_colors(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let mut board = state
        .database
        .get_resource("task_board", "default")?
        .unwrap_or_else(default_task_board);
    board["colors"] = body.get("colors").cloned().unwrap_or_else(|| json!({}));
    board["revision"] = Value::from(board["revision"].as_i64().unwrap_or(0) + 1);
    Ok(Json(state.database.put_resource(
        "task_board",
        "default",
        &board,
    )?))
}

async fn task_board_events() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        "event: ping\ndata: {}\n\n",
    )
        .into_response()
}

async fn task_board_session(
    ScopedState(state): ScopedState,
    AxumPath(session_id): AxumPath<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    state
        .database
        .put_resource("task_board_session", &session_id, &body)?;
    task_board(ScopedState(state)).await
}

async fn task_board_session_status(
    state: ScopedState,
    path: AxumPath<String>,
    body: Json<Value>,
) -> ApiResult<Json<Value>> {
    task_board_session(state, path, body).await
}

fn default_task_board() -> Value {
    json!({"cards": [], "colors": {}, "revision": 0})
}

async fn learning_journal(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(super::learning_journal::overview(&state)?))
}

async fn imports_list(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(
        json!({"sessions": state.database.list_resources("chat_import")?}),
    ))
}

async fn imports_start(Json(body): Json<Value>) -> Json<Value> {
    Json(
        json!({"accepted": true, "imported": 0, "source": body.get("source").cloned().unwrap_or(Value::Null)}),
    )
}

async fn kb_list(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(Value::Array(
        state.database.list_resources("knowledge_base")?,
    )))
}

async fn kb_create(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let name = body
        .get("name")
        .or_else(|| body.get("kb_name"))
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad_request("缺少知识库名称"))?;
    let id = safe_file_name(name)?;
    let directory = state.paths.data.join("knowledge").join(&id);
    std::fs::create_dir_all(&directory)
        .map_err(|error| ApiError::internal(format!("无法创建知识库目录：{error}")))?;
    let value = state.database.put_resource(
        "knowledge_base",
        &id,
        &json!({
            "name": name,
            "description": body.get("description").cloned().unwrap_or(Value::Null),
            "provider": "native_full_text",
            "path": directory.display().to_string(),
            "file_count": 0
        }),
    )?;
    Ok(Json(value))
}

async fn kb_health() -> Json<Value> {
    Json(json!({"status": "healthy", "engine": "native_rust"}))
}

async fn kb_supported_types() -> Json<Value> {
    Json(json!({
        "types": ["pdf", "txt", "md", "docx", "epub", "html", "csv", "xlsx"],
        "engine": "rust"
    }))
}

async fn kb_default(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(
        state
            .database
            .get_setting("default_kb")?
            .unwrap_or(Value::Null),
    ))
}

async fn kb_set_default(
    ScopedState(state): ScopedState,
    AxumPath(kb_name): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    state
        .database
        .set_setting("default_kb", &Value::String(kb_name.clone()))?;
    Ok(Json(Value::String(kb_name)))
}

async fn kb_get(
    ScopedState(state): ScopedState,
    AxumPath(kb_name): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    state
        .database
        .get_resource("knowledge_base", &kb_name)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("知识库不存在"))
}

async fn kb_delete(
    ScopedState(state): ScopedState,
    AxumPath(kb_name): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    state.database.delete_resource("knowledge_base", &kb_name)?;
    Ok(Json(json!({"deleted": true})))
}

async fn kb_files(
    ScopedState(state): ScopedState,
    AxumPath(kb_name): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    Ok(Json(Value::Array(
        state
            .database
            .list_resources(&format!("kb_file:{kb_name}"))?,
    )))
}

async fn kb_config(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    get_raw_setting(&state, "knowledge_config")
}

async fn kb_put_config(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    put_raw_setting(&state, "knowledge_config", body)
}

async fn kb_indexing_readiness() -> Json<Value> {
    Json(json!({"ready": true, "engine": "native_rust"}))
}

async fn kb_indexing_run() -> Json<Value> {
    Json(json!({"status": "idle", "task_id": null}))
}

async fn kb_progress() -> Json<Value> {
    Json(json!({"status": "idle", "processed": 0, "total": 0}))
}

async fn documents_list(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(
        json!({"documents": state.database.list_resources("document")?}),
    ))
}

async fn documents_create(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let id = format!("doc-{}", Uuid::new_v4());
    let mut value = body;
    value["doc_id"] = Value::String(id.clone());
    Ok(Json(state.database.put_resource("document", &id, &value)?))
}

async fn document_get(
    ScopedState(state): ScopedState,
    AxumPath(doc_id): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    state
        .database
        .get_resource("document", &doc_id)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("文档不存在"))
}

async fn document_update(
    ScopedState(state): ScopedState,
    AxumPath(doc_id): AxumPath<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let current = state
        .database
        .get_resource("document", &doc_id)?
        .unwrap_or_else(|| json!({}));
    Ok(Json(state.database.put_resource(
        "document",
        &doc_id,
        &merge_values(current, body),
    )?))
}

async fn document_delete(
    ScopedState(state): ScopedState,
    AxumPath(doc_id): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    state.database.delete_resource("document", &doc_id)?;
    Ok(Json(json!({"deleted": true})))
}

async fn calendars_list(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(
        json!({"calendars": state.database.list_resources("calendar")?}),
    ))
}

async fn calendar_create(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let id = format!("calendar-{}", Uuid::new_v4());
    let mut value = body;
    value["id"] = Value::String(id.clone());
    Ok(Json(state.database.put_resource("calendar", &id, &value)?))
}

async fn calendar_events(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(Value::Array(
        state.database.list_resources("calendar_event")?,
    )))
}

async fn calendar_event_create(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let id = body
        .get("id")
        .and_then(Value::as_str)
        .map(String::from)
        .unwrap_or_else(|| format!("event-{}", Uuid::new_v4()));
    let mut value = body;
    value["id"] = Value::String(id.clone());
    Ok(Json(state.database.put_resource(
        "calendar_event",
        &id,
        &value,
    )?))
}

async fn calendar_event_update(
    ScopedState(state): ScopedState,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let current = state
        .database
        .get_resource("calendar_event", &id)?
        .unwrap_or_else(|| json!({}));
    Ok(Json(state.database.put_resource(
        "calendar_event",
        &id,
        &merge_values(current, body),
    )?))
}

async fn calendar_event_delete(
    ScopedState(state): ScopedState,
    AxumPath(id): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    state.database.delete_resource("calendar_event", &id)?;
    Ok(Json(json!({"deleted": true})))
}

async fn calendar_layers(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(
        json!({"layers": state.database.list_resources("calendar_layer")?}),
    ))
}

async fn calendar_search(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(
        json!({"events": state.database.list_resources("calendar_event")?}),
    ))
}

async fn calendar_task_blocks(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(
        json!({"blocks": state.database.list_resources("task_block")?}),
    ))
}

async fn personas_list(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(Value::Array(
        state.database.list_resources("persona")?,
    )))
}

async fn persona_create(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad_request("缺少人格名称"))?;
    Ok(Json(state.database.put_resource("persona", name, &body)?))
}

async fn persona_get(
    ScopedState(state): ScopedState,
    AxumPath(name): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    state
        .database
        .get_resource("persona", &name)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("人格不存在"))
}

async fn persona_update(
    ScopedState(state): ScopedState,
    AxumPath(name): AxumPath<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    Ok(Json(state.database.put_resource("persona", &name, &body)?))
}

async fn persona_delete(
    ScopedState(state): ScopedState,
    AxumPath(name): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    state.database.delete_resource("persona", &name)?;
    Ok(Json(json!({"deleted": true})))
}

async fn plugins_state() -> Json<Value> {
    Json(json!({"plugins": [], "enabled": []}))
}

async fn plugins_extensions() -> Json<Value> {
    Json(json!({"extensions": []}))
}

async fn plugins_catalog() -> Json<Value> {
    Json(json!({"plugins": []}))
}

async fn voice_tts(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Response> {
    let catalog = super::catalog::resolve_catalog_connections(
        &state
            .database
            .get_setting("catalog")?
            .unwrap_or_else(|| json!({})),
    );
    let Some((profile, _model)) = active_service(&catalog, "tts") else {
        return Err(ApiError::bad_request("尚未配置 TTS 服务"));
    };
    let profile_id = profile
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad_request("TTS 配置缺少 ID"))?;
    let model_id = catalog
        .pointer("/services/tts/active_model_id")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad_request("尚未选择 TTS 模型"))?;
    let text = body.get("text").and_then(Value::as_str).unwrap_or("");
    if text.trim().is_empty() || text.chars().count() > 100_000 {
        return Err(ApiError::bad_request("语音文本不能为空"));
    }
    let voice = body.get("voice").and_then(Value::as_str);
    let response_format = body
        .get("format")
        .or_else(|| body.get("response_format"))
        .and_then(Value::as_str);
    let (audio, content_type) = super::media_services::synthesize_active_tts(
        &state,
        &catalog,
        profile_id,
        model_id,
        text,
        voice,
        response_format,
    )
    .await
    .map_err(voice_media_error)?;
    let content_type = header::HeaderValue::from_str(&content_type)
        .map_err(|_| ApiError::internal("语音服务返回了无效的媒体类型。"))?;
    let mut response = audio.into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, content_type);
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    Ok(response)
}

async fn voice_stt(
    ScopedState(state): ScopedState,
    mut multipart: Multipart,
) -> ApiResult<Json<Value>> {
    const MAX_STT_BYTES: usize = 25 * 1024 * 1024;
    let mut audio = Vec::new();
    let mut filename = "audio.webm".to_string();
    let mut content_type = "application/octet-stream".to_string();
    let mut language: Option<String> = None;
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|error| ApiError::bad_request(format!("无法读取上传内容：{error}")))?
    {
        match field.name().unwrap_or_default() {
            "file" => {
                filename = field
                    .file_name()
                    .filter(|value| !value.is_empty())
                    .unwrap_or("audio.webm")
                    .to_string();
                content_type = field
                    .content_type()
                    .unwrap_or("application/octet-stream")
                    .to_string();
                while let Some(chunk) = field
                    .chunk()
                    .await
                    .map_err(|error| ApiError::bad_request(format!("无法读取音频：{error}")))?
                {
                    if audio.len().saturating_add(chunk.len()) > MAX_STT_BYTES {
                        return Err(ApiError::new(
                            StatusCode::PAYLOAD_TOO_LARGE,
                            "payload_too_large",
                            "音频超过 25 MB 限制。",
                            false,
                        ));
                    }
                    audio.extend_from_slice(&chunk);
                }
            }
            "language" => {
                language = field
                    .text()
                    .await
                    .ok()
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty());
            }
            _ => {}
        }
    }
    if audio.is_empty() {
        return Err(ApiError::bad_request("音频上传为空。"));
    }
    let catalog = super::catalog::resolve_catalog_connections(
        &state
            .database
            .get_setting("catalog")?
            .unwrap_or_else(|| json!({})),
    );
    let Some((profile, _model)) = active_service(&catalog, "stt") else {
        return Err(ApiError::bad_request("尚未配置 STT 服务"));
    };
    let profile_id = profile
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad_request("STT 配置缺少 ID"))?;
    let model_id = catalog
        .pointer("/services/stt/active_model_id")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad_request("尚未选择 STT 模型"))?;
    let text = super::media_services::transcribe_audio_with_language(
        &state,
        &catalog,
        profile_id,
        model_id,
        &audio,
        &filename,
        &content_type,
        language.as_deref(),
    )
    .await
    .map_err(voice_media_error)?;
    Ok(Json(json!({"text": text})))
}

fn voice_media_error(error: super::media_services::MediaError) -> ApiError {
    match &error {
        super::media_services::MediaError::Invalid(message) => {
            ApiError::bad_request(message.clone())
        }
        super::media_services::MediaError::Timeout => ApiError::new(
            StatusCode::GATEWAY_TIMEOUT,
            "provider_timeout",
            error.public_message(),
            true,
        ),
        _ => ApiError::provider(error.public_message()),
    }
}

fn active_llm(catalog: &Value) -> Option<(Value, Value)> {
    active_service(catalog, "llm")
}

fn active_service(catalog: &Value, service: &str) -> Option<(Value, Value)> {
    let service_value = catalog.get("services")?.get(service)?;
    let profiles = service_value.get("profiles")?.as_array()?;
    let profile = profiles.iter().find(|profile| {
        profile.get("id").and_then(Value::as_str)
            == service_value
                .get("active_profile_id")
                .and_then(Value::as_str)
    })?;
    let models = profile.get("models")?.as_array()?;
    let model = models.iter().find(|model| {
        model.get("id").and_then(Value::as_str)
            == service_value.get("active_model_id").and_then(Value::as_str)
    })?;
    Some((profile.clone(), model.clone()))
}

fn active_model_name(catalog: &Value) -> Value {
    active_llm(catalog)
        .and_then(|(_, model)| model.get("model").cloned())
        .unwrap_or(Value::Null)
}

fn provider_options() -> Value {
    let provider = |value: &str, label: &str, base_url: &str, default_model: &str| {
        json!({
            "value": value,
            "label": label,
            "base_url": base_url,
            "default_model": default_model,
            "api_formats": ["auto", "openai_chat", "openai_responses", "anthropic"],
            "default_api_format": if value == "anthropic" {"anthropic"} else {"openai_chat"}
        })
    };
    let llm = json!([
        provider("openai", "OpenAI", "https://api.openai.com/v1", "gpt-4.1"),
        provider(
            "anthropic",
            "Anthropic",
            "https://api.anthropic.com/v1",
            "claude-sonnet-4-5"
        ),
        provider(
            "deepseek",
            "DeepSeek",
            "https://api.deepseek.com/v1",
            "deepseek-chat"
        ),
        provider(
            "openrouter",
            "OpenRouter",
            "https://openrouter.ai/api/v1",
            "openai/gpt-4.1"
        ),
        provider("ollama", "Ollama", "http://127.0.0.1:11434/v1", "qwen3"),
        provider("lmstudio", "LM Studio", "http://127.0.0.1:1234/v1", ""),
        provider("custom", "自定义 OpenAI 兼容服务", "", "")
    ]);
    let search = json!([
        {"value": "none", "label": "未配置"},
        {"value": "brave", "label": "Brave Search"},
        {"value": "tavily", "label": "Tavily"},
        {"value": "searxng", "label": "SearXNG"},
        {"value": "custom", "label": "自定义搜索"}
    ]);
    json!({
        "llm": llm,
        "task": llm,
        "embedding": [provider("openai", "OpenAI", "https://api.openai.com/v1", "text-embedding-3-large")],
        "search": search,
        "tts": [provider("openai", "OpenAI", "https://api.openai.com/v1", "gpt-4o-mini-tts")],
        "stt": [provider("openai", "OpenAI", "https://api.openai.com/v1", "gpt-4o-mini-transcribe")],
        "imagegen": [provider("openai", "OpenAI", "https://api.openai.com/v1", "gpt-image-1")],
        "videogen": [provider("custom", "自定义视频服务", "", "")]
    })
}

fn connection_targets() -> Value {
    json!([
        {
            "provider": "openai",
            "label": "OpenAI",
            "default_base_url": "https://api.openai.com/v1",
            "services": {
                "llm": {"provider": "openai", "base_url": "https://api.openai.com/v1", "default_model": "gpt-4.1"},
                "embedding": {"provider": "openai", "base_url": "https://api.openai.com/v1", "default_model": "text-embedding-3-large"},
                "tts": {"provider": "openai", "base_url": "https://api.openai.com/v1", "default_model": "gpt-4o-mini-tts"},
                "stt": {"provider": "openai", "base_url": "https://api.openai.com/v1", "default_model": "gpt-4o-mini-transcribe"},
                "imagegen": {"provider": "openai", "base_url": "https://api.openai.com/v1", "default_model": "gpt-image-1"}
            }
        },
        {
            "provider": "anthropic",
            "label": "Anthropic",
            "default_base_url": "https://api.anthropic.com/v1",
            "services": {
                "llm": {"provider": "anthropic", "base_url": "https://api.anthropic.com/v1", "default_model": "claude-sonnet-4-5"}
            }
        }
    ])
}

fn task_kinds() -> Value {
    json!([
        {"id": "chat", "group": "对话"},
        {"id": "title", "group": "对话"},
        {"id": "hint", "group": "对话"},
        {"id": "research", "group": "能力"},
        {"id": "visualize", "group": "能力"}
    ])
}

fn merge_values(mut left: Value, right: Value) -> Value {
    merge_values_in_place(&mut left, &right);
    left
}

fn merge_values_in_place(left: &mut Value, right: &Value) {
    match (left, right) {
        (Value::Object(left), Value::Object(right)) => {
            for (key, value) in right {
                if let Some(existing) = left.get_mut(key) {
                    merge_values_in_place(existing, value);
                } else {
                    left.insert(key.clone(), value.clone());
                }
            }
        }
        (left, right) => *left = right.clone(),
    }
}

fn safe_file_name(name: &str) -> ApiResult<String> {
    let trimmed = name.trim();
    if trimmed.is_empty()
        || trimmed == "."
        || trimmed == ".."
        || trimmed.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|'])
    {
        return Err(ApiError::bad_request("名称包含无效字符"));
    }
    Ok(trimmed.chars().take(120).collect())
}

fn directory_size(path: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| {
            let path = entry.path();
            if path.is_dir() {
                directory_size(&path)
            } else {
                entry.metadata().map(|metadata| metadata.len()).unwrap_or(0)
            }
        })
        .sum()
}

#[allow(dead_code)]
fn empty_object() -> Value {
    Value::Object(Map::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::portable::PortablePaths;
    use crate::storage::Database;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn test_paths(root: PathBuf) -> PortablePaths {
        PortablePaths {
            home: root.join("home"),
            data: root.join("data"),
            cache: root.join("cache"),
            temp: root.join("temp"),
            logs: root.join("logs"),
            webview: root.join("cache/webview2"),
            uploads: root.join("data/uploads"),
            outputs: root.join("data/outputs"),
            database: root.join("data/deeptutor.sqlite3"),
            root,
        }
    }

    fn test_state() -> (AppState, TempDir) {
        let temporary = tempfile::tempdir().expect("创建测试目录");
        let paths = test_paths(temporary.path().to_path_buf());
        let database = Database::new(&paths.database);
        database.initialize().expect("初始化测试数据库");
        let state = AppState::new(database, paths).expect("创建测试应用状态");
        (state, temporary)
    }

    #[test]
    fn api_router_constructs_without_overlapping_routes() {
        let _router: Router<AppState> = router();
    }

    #[test]
    fn learning_index_keeps_the_frontend_contract() {
        let (state, _temporary) = test_state();
        let payload = learning_index_payload(&state);
        let sources = payload.get("sources").expect("学习索引必须包含 sources");

        for kind in ["books", "mastery", "reading", "watching", "practice"] {
            assert!(
                sources.get(kind).and_then(Value::as_array).is_some(),
                "学习索引缺少数组字段 {kind}"
            );
        }
        assert_eq!(payload.get("failed"), Some(&json!([])));
    }

    #[test]
    fn learning_index_reads_every_local_source_and_filters_watching() {
        let (state, _temporary) = test_state();
        let now = crate::storage::now();
        state
            .database
            .put_resource(
                "book",
                "book-1",
                &json!({"title": "测试书籍", "status": "reading", "page_count": 2}),
            )
            .expect("写入测试书籍");
        state
            .database
            .put_resource(
                "mastery_topic",
                "topic-1",
                &json!({
                    "path_id": "topic-1",
                    "name": "测试掌握路径",
                    "goal": "完成测试",
                    "status": "active",
                    "created_at": now,
                    "updated_at": now,
                    "revision": 1,
                    "modules": [],
                    "mastery_levels": {},
                    "overrides": {},
                    "sources": [],
                    "review_settings": {"desired_retention": 0.9, "scope": "path"},
                    "learner_profile": null,
                    "session_ids": []
                }),
            )
            .expect("写入测试掌握路径");
        state
            .database
            .put_resource(
                "reading_workspace",
                "reading-1",
                &json!({
                    "workspace_id": "reading-1",
                    "title": "测试阅读集合",
                    "description": "",
                    "color": "",
                    "active_material_id": null,
                    "tabs": [],
                    "created_at": now,
                    "updated_at": now
                }),
            )
            .expect("写入测试阅读集合");
        state
            .database
            .create_session(
                "watching-1",
                "测试观看会话",
                &json!({"workspace_mode": "immersive_watching"}),
                "",
            )
            .expect("写入测试观看会话");
        state
            .database
            .create_session("chat-1", "普通会话", &json!({"workspace_mode": "chat"}), "")
            .expect("写入普通会话");
        state
            .database
            .with_connection(|connection| {
                connection
                    .execute(
                        r#"
                        INSERT INTO qn_entries(question_id, question, created_at, updated_at)
                        VALUES('question-1', '测试练习题', ?1, ?1)
                        "#,
                        rusqlite::params![now],
                    )
                    .map_err(|error| error.to_string())?;
                Ok(())
            })
            .expect("写入测试练习题");

        let payload = learning_index_payload(&state);
        let sources = payload.get("sources").expect("学习索引必须包含 sources");
        assert_eq!(sources.pointer("/books/0/title"), Some(&json!("测试书籍")));
        assert_eq!(
            sources.pointer("/mastery/0/name"),
            Some(&json!("测试掌握路径"))
        );
        assert_eq!(
            sources.pointer("/mastery/0/map/counts/total"),
            Some(&json!(0))
        );
        assert_eq!(
            sources.pointer("/reading/0/title"),
            Some(&json!("测试阅读集合"))
        );
        assert_eq!(
            sources
                .get("watching")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(1)
        );
        assert_eq!(
            sources.pointer("/watching/0/title"),
            Some(&json!("测试观看会话"))
        );
        assert_eq!(
            sources.pointer("/practice/0/question"),
            Some(&json!("测试练习题"))
        );
        assert_eq!(payload.get("failed"), Some(&json!([])));
    }
}
