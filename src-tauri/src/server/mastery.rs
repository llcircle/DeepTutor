use super::error::{ApiError, ApiResult};
use super::AppState;
use super::ScopedState;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::routing::{delete, get, patch, post, put};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use uuid::Uuid;

const TOPIC_DOMAIN: &str = "mastery_topic";
const EVENT_DOMAIN_PREFIX: &str = "mastery_event:";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/mastery-paths/topics", get(list_topics).post(create_topic))
        .route("/mastery-paths/topics/index", get(topic_index))
        .route("/mastery-paths/topics/draft", post(topic_draft))
        .route(
            "/mastery-paths/topics/{path_id}",
            get(get_topic).delete(delete_topic),
        )
        .route(
            "/mastery-paths/topics/{path_id}/review-settings",
            get(get_review_settings).put(update_review_settings),
        )
        .route(
            "/mastery-paths/topics/{path_id}/sessions/{session_id}/mode",
            put(update_session_mode),
        )
        .route("/mastery-paths/topics/{path_id}/map", put(update_topic_map))
        .route(
            "/mastery-paths/topics/{path_id}/objectives/{objective_id}/override",
            post(update_topic_objective),
        )
        .route(
            "/mastery-paths/topics/{path_id}/sessions",
            get(topic_sessions),
        )
        .route(
            "/mastery-paths/topics/{path_id}/ask-hint",
            get(topic_ask_hint),
        )
        .route("/mastery-paths/reading/records", get(reading_records))
        .route("/mastery-paths/progress", get(list_progress))
        .route(
            "/mastery-paths/progress/{path_id}",
            get(progress_detail)
                .patch(rename_progress)
                .delete(delete_progress),
        )
        .route(
            "/mastery-paths/progress/{path_id}/init-modules",
            post(init_modules),
        )
        .route("/mastery-paths/progress/{path_id}/map", get(progress_map))
        .route(
            "/mastery-paths/progress/{path_id}/board",
            get(progress_board),
        )
        .route(
            "/mastery-paths/progress/{path_id}/events",
            get(progress_events),
        )
        .route(
            "/mastery-paths/progress/{path_id}/sessions",
            get(progress_sessions),
        )
        .route(
            "/mastery-paths/progress/{path_id}/objectives/{objective_id}",
            get(objective_report),
        )
        .route(
            "/mastery-paths/progress/{path_id}/redo",
            post(redo_progress),
        )
        .route(
            "/mastery-paths/progress/{path_id}/skip-question",
            post(skip_question),
        )
        .route(
            "/mastery-paths/progress/{path_id}/import-from-book",
            post(import_from_book),
        )
        .route(
            "/mastery-paths/progress/{path_id}/generate-from-notebook",
            post(generate_from_notebook),
        )
        .route(
            "/mastery-paths/progress/{path_id}/generate-from-reading",
            post(generate_from_reading),
        )
}

#[derive(Debug, Deserialize)]
struct EventsQuery {
    after_revision: Option<u64>,
}

async fn list_topics(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(json!({"topics": api_topic_rows(&state)?})))
}

/// 供学习导航直接复用掌握路径的完整视图，避免返回缺少统计字段的原始资源。
pub(crate) fn api_topic_rows(state: &AppState) -> Result<Vec<Value>, String> {
    state
        .database
        .list_resources(TOPIC_DOMAIN)?
        .into_iter()
        .map(|topic| topic_view(state, &topic).map_err(|error| error.message))
        .collect()
}

/// 供工具门控判断是否存在可导航的掌握主题。
pub fn has_topics(state: &AppState) -> Result<bool, String> {
    Ok(!state.database.list_resources(TOPIC_DOMAIN)?.is_empty())
}

async fn topic_index(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    let topics = state
        .database
        .list_resources(TOPIC_DOMAIN)?
        .into_iter()
        .map(|topic| {
            json!({
                "path_id": topic.get("path_id").and_then(Value::as_str).unwrap_or_default(),
                "name": topic.get("name").and_then(Value::as_str).unwrap_or_default(),
                "emoji": topic.get("emoji").and_then(Value::as_str).unwrap_or("📚")
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({"topics": topics})))
}

async fn topic_draft(Json(body): Json<Value>) -> Json<Value> {
    Json(draft_payload(&body))
}

async fn create_topic(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let goal = body
        .get("goal")
        .and_then(Value::as_str)
        .unwrap_or("新的学习主题")
        .trim()
        .to_string();
    let path_id = format!("path-{}", Uuid::new_v4());
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(&goal)
        .to_string();
    let modules = body
        .get("modules")
        .and_then(Value::as_array)
        .filter(|modules| !modules.is_empty())
        .cloned()
        .unwrap_or_else(|| draft_modules(&goal, 4));
    let modules = normalize_modules(modules);
    let now = crate::storage::now();
    let sources = body
        .get("sources")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let emoji = body
        .get("emoji")
        .and_then(Value::as_str)
        .unwrap_or("🧭")
        .to_string();
    let description = body
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let topic = json!({
        "path_id": path_id,
        "name": name,
        "goal": goal,
        "description": description,
        "emoji": emoji,
        "status": "active",
        "map_seed": rand_seed(&path_id),
        "created_at": now,
        "updated_at": now,
        "revision": 1,
        "modules": modules,
        "mastery_levels": {},
        "overrides": {},
        "sources": sources,
        "review_settings": {"desired_retention": 0.9, "scope": "path"},
        "learner_profile": Value::Null,
        "session_ids": []
    });
    let stored = state
        .database
        .put_resource(TOPIC_DOMAIN, &path_id, &topic)?;
    append_event(
        &state,
        &path_id,
        "topic_created",
        "create",
        json!({"topic": topic_view(&state, &stored)?}),
        "",
        "",
    )?;
    Ok(Json(topic_view(&state, &stored)?))
}

async fn get_topic(
    ScopedState(state): ScopedState,
    Path(path_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let topic = load_topic(&state, &path_id)?;
    Ok(Json(topic_view(&state, &topic)?))
}

async fn delete_topic(
    ScopedState(state): ScopedState,
    Path(path_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let existed = state.database.delete_resource(TOPIC_DOMAIN, &path_id)?;
    Ok(Json(json!({"deleted": existed, "path_id": path_id})))
}

async fn update_review_settings(
    ScopedState(state): ScopedState,
    Path(path_id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let mut topic = load_topic(&state, &path_id)?;
    let desired = body
        .get("desired_retention")
        .and_then(Value::as_f64)
        .unwrap_or(0.9)
        .clamp(0.5, 0.99);
    topic["review_settings"] = json!({"desired_retention": desired, "scope": "path"});
    topic["updated_at"] = Value::from(crate::storage::now());
    topic["revision"] = Value::from(next_revision(&topic));
    let stored = state
        .database
        .put_resource(TOPIC_DOMAIN, &path_id, &topic)?;
    append_event(
        &state,
        &path_id,
        "review_settings_updated",
        "settings",
        json!({"desired_retention": desired}),
        "",
        "",
    )?;
    Ok(Json(topic_view(&state, &stored)?))
}

async fn get_review_settings(
    ScopedState(state): ScopedState,
    Path(path_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let topic = load_topic(&state, &path_id)?;
    Ok(Json(topic.get("review_settings").cloned().unwrap_or_else(
        || json!({"desired_retention": 0.9, "scope": "path"}),
    )))
}

async fn update_session_mode(
    ScopedState(state): ScopedState,
    Path((path_id, session_id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let topic = load_topic(&state, &path_id)?;
    let mode = body.get("mode").and_then(Value::as_str).unwrap_or("learn");
    let preferences = json!({
        "mastery_path_id": path_id,
        "mastery_session_mode": mode
    });
    state
        .database
        .update_session_preferences(&session_id, &preferences)?;
    let mut topic = topic;
    topic["updated_at"] = Value::from(crate::storage::now());
    state
        .database
        .put_resource(TOPIC_DOMAIN, &path_id, &topic)?;
    append_event(
        &state,
        &path_id,
        "session_mode_changed",
        "session_mode",
        json!({"mode": mode}),
        &session_id,
        "",
    )?;
    Ok(Json(json!({"session_id": session_id, "mode": mode})))
}

async fn update_topic_map(
    ScopedState(state): ScopedState,
    Path(path_id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let modules = body
        .get("modules")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| ApiError::bad_request("缺少 modules。"))?;
    let mut topic = load_topic(&state, &path_id)?;
    topic["modules"] = Value::Array(normalize_modules(modules));
    topic["updated_at"] = Value::from(crate::storage::now());
    topic["revision"] = Value::from(next_revision(&topic));
    let stored = state
        .database
        .put_resource(TOPIC_DOMAIN, &path_id, &topic)?;
    append_event(
        &state,
        &path_id,
        "map_updated",
        "map",
        json!({"modules": stored.get("modules").cloned().unwrap_or_else(|| json!([]))}),
        "",
        "",
    )?;
    Ok(Json(topic_view(&state, &stored)?))
}

async fn update_topic_objective(
    ScopedState(state): ScopedState,
    Path((path_id, objective_id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let mut topic = load_topic(&state, &path_id)?;
    let mastered = body
        .get("mastered")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let note = body
        .get("note")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let overrides = topic["overrides"]
        .as_object_mut()
        .ok_or_else(|| ApiError::internal("学习路径覆盖数据损坏。"))?;
    overrides.insert(
        objective_id.clone(),
        json!({"mastered": mastered, "note": note}),
    );
    topic["updated_at"] = Value::from(crate::storage::now());
    topic["revision"] = Value::from(next_revision(&topic));
    let stored = state
        .database
        .put_resource(TOPIC_DOMAIN, &path_id, &topic)?;
    let revision = stored
        .get("revision")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    append_event(
        &state,
        &path_id,
        "objective_override",
        "objective",
        json!({"objective_id": objective_id, "mastered": mastered, "note": note}),
        "",
        "",
    )?;
    Ok(Json(json!({
        "status": "ok",
        "path_revision": revision,
        "map": path_map(&stored)
    })))
}

async fn topic_sessions(
    ScopedState(state): ScopedState,
    Path(path_id): Path<String>,
) -> ApiResult<Json<Value>> {
    load_topic(&state, &path_id)?;
    let sessions = topic_sessions_for(&state, &path_id)?;
    Ok(Json(json!({"path_id": path_id, "sessions": sessions})))
}

async fn topic_ask_hint(
    ScopedState(state): ScopedState,
    Path(path_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let topic = load_topic(&state, &path_id)?;
    let next = next_objective(&topic);
    let hint = next
        .get("knowledge_point_name")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(|name| format!("试着先用一句话解释“{name}”，再举一个自己的生活例子。"))
        .unwrap_or_default();
    Ok(Json(json!({"hint": hint})))
}

async fn reading_records(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(json!({
        "records": state.database.list_resources("learning_record")?
    })))
}

async fn list_progress(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    let summaries = state
        .database
        .list_resources(TOPIC_DOMAIN)?
        .into_iter()
        .map(|topic| progress_summary(&topic))
        .collect::<Vec<_>>();
    Ok(Json(json!({"summaries": summaries, "errors": []})))
}

async fn progress_detail(
    ScopedState(state): ScopedState,
    Path(path_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let topic = load_topic(&state, &path_id)?;
    let average = average_mastery(&topic);
    Ok(Json(json!({
        "book_id": path_id,
        "modules": topic.get("modules").cloned().unwrap_or_else(|| json!([])),
        "mastery_levels": topic.get("mastery_levels").cloned().unwrap_or_else(|| json!({})),
        "current_module_id": next_objective(&topic).get("module_id").cloned().unwrap_or(Value::Null),
        "current_stage": if average >= 0.8 { "review" } else { "learning" },
        "diagnostic": topic.get("diagnostic").cloned().unwrap_or(Value::Null)
    })))
}

async fn rename_progress(
    ScopedState(state): ScopedState,
    Path(path_id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let mut topic = load_topic(&state, &path_id)?;
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    topic["name"] = Value::String(if name.is_empty() {
        topic
            .get("goal")
            .and_then(Value::as_str)
            .unwrap_or("学习路径")
            .to_string()
    } else {
        name
    });
    topic["updated_at"] = Value::from(crate::storage::now());
    topic["revision"] = Value::from(next_revision(&topic));
    let stored = state
        .database
        .put_resource(TOPIC_DOMAIN, &path_id, &topic)?;
    append_event(
        &state,
        &path_id,
        "path_renamed",
        "rename",
        json!({"name": stored.get("name").cloned().unwrap_or(Value::Null)}),
        "",
        "",
    )?;
    Ok(Json(json!({
        "status": "ok",
        "name": stored.get("name").cloned().unwrap_or(Value::Null)
    })))
}

async fn delete_progress(
    ScopedState(state): ScopedState,
    Path(path_id): Path<String>,
) -> ApiResult<Json<Value>> {
    delete_topic(ScopedState(state), Path(path_id)).await
}

async fn init_modules(
    ScopedState(state): ScopedState,
    Path(path_id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let modules = body
        .get("modules")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| ApiError::bad_request("缺少 modules。"))?;
    let mut topic = load_or_create_legacy_topic(&state, &path_id)?;
    topic["modules"] = Value::Array(normalize_modules(modules));
    topic["updated_at"] = Value::from(crate::storage::now());
    topic["revision"] = Value::from(next_revision(&topic));
    let stored = state
        .database
        .put_resource(TOPIC_DOMAIN, &path_id, &topic)?;
    append_event(
        &state,
        &path_id,
        "modules_initialized",
        "init_modules",
        json!({"modules": stored.get("modules").cloned().unwrap_or_else(|| json!([]))}),
        "",
        "",
    )?;
    Ok(Json(
        json!({"status": "ok", "path": topic_view(&state, &stored)?}),
    ))
}

async fn progress_map(
    ScopedState(state): ScopedState,
    Path(path_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let topic = load_topic(&state, &path_id)?;
    Ok(Json(json!({
        "book_id": path_id,
        "name": topic.get("name").cloned().unwrap_or(Value::Null),
        "path_revision": topic.get("revision").cloned().unwrap_or(Value::from(1)),
        "next": next_objective(&topic),
        "map": path_map(&topic)
    })))
}

async fn progress_board(
    ScopedState(state): ScopedState,
    Path(path_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let topic = load_topic(&state, &path_id)?;
    let modules = topic
        .get("modules")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut all_cards = Vec::new();
    let mut board_modules = Vec::new();
    for (module_index, module) in modules.iter().enumerate() {
        let module_id = module.get("id").and_then(Value::as_str).unwrap_or("");
        let module_name = module.get("name").and_then(Value::as_str).unwrap_or("");
        let mut cards = Vec::new();
        for (point_index, point) in module
            .get("knowledge_points")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            let id = point.get("id").and_then(Value::as_str).unwrap_or("");
            let (status, mastery) = objective_state(&topic, point, module);
            let card = json!({
                "id": id,
                "name": point.get("name").and_then(Value::as_str).unwrap_or(""),
                "type": point.get("type").and_then(Value::as_str).unwrap_or("concept"),
                "module_id": module_id,
                "module_name": module_name,
                "status": status,
                "mastery_level": mastery,
                "next_review_at": Value::Null,
                "position": {
                    "column": module_index.min(4),
                    "row": point_index
                }
            });
            cards.push(card.clone());
            all_cards.push(card);
        }
        let mastered = cards
            .iter()
            .filter(|card| card.get("status").and_then(Value::as_str) == Some("mastered"))
            .count();
        board_modules.push(json!({
            "id": module_id,
            "name": module_name,
            "order": module.get("order").and_then(Value::as_u64).unwrap_or((module_index + 1) as u64),
            "mastered": mastered,
            "total": cards.len(),
            "cards": cards
        }));
    }
    Ok(Json(json!({
        "book_id": path_id,
        "name": topic.get("name").cloned().unwrap_or(Value::Null),
        "path_revision": topic.get("revision").cloned().unwrap_or(Value::from(1)),
        "cards": all_cards,
        "modules": board_modules
    })))
}

async fn progress_events(
    ScopedState(state): ScopedState,
    Path(path_id): Path<String>,
    Query(query): Query<EventsQuery>,
) -> ApiResult<Json<Value>> {
    load_topic(&state, &path_id)?;
    let after = query.after_revision.unwrap_or(0);
    let events = stored_events(&state, &path_id)?
        .into_iter()
        .filter(|event| event.get("revision").and_then(Value::as_u64).unwrap_or(0) > after)
        .collect::<Vec<_>>();
    Ok(Json(json!({"events": events})))
}

async fn progress_sessions(
    ScopedState(state): ScopedState,
    Path(path_id): Path<String>,
) -> ApiResult<Json<Value>> {
    load_topic(&state, &path_id)?;
    let session_ids = topic_sessions_for(&state, &path_id)?
        .into_iter()
        .filter_map(|session| {
            session
                .get("session_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect::<Vec<_>>();
    Ok(Json(
        json!({"book_id": path_id, "session_ids": session_ids}),
    ))
}

async fn objective_report(
    ScopedState(state): ScopedState,
    Path((path_id, objective_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let topic = load_topic(&state, &path_id)?;
    let (module, point) = find_objective(&topic, &objective_id)
        .ok_or_else(|| ApiError::not_found("学习目标不存在。"))?;
    let (status, mastery) = objective_state(&topic, &point, &module);
    let override_value = topic
        .get("overrides")
        .and_then(|value| value.get(&objective_id))
        .cloned()
        .unwrap_or_else(|| json!({}));
    Ok(Json(json!({
        "objective": {
            "id": objective_id,
            "name": point.get("name").cloned().unwrap_or(Value::Null),
            "type": point.get("type").cloned().unwrap_or_else(|| Value::String("concept".into())),
            "module_name": module.get("name").cloned().unwrap_or(Value::Null),
            "status": status,
            "gate": if point.get("type").and_then(Value::as_str) == Some("skill") { "qualitative" } else { "quantitative" },
            "mastered": status == "mastered",
            "assessed_mastered": status == "mastered",
            "required_visual_tasks": point.get("required_visual_tasks").cloned().unwrap_or_else(|| json!([])),
            "demonstrated_visual_tasks": [],
            "mastery_source": if override_value.get("mastered").and_then(Value::as_bool) == Some(true) { "learner" } else { "system" },
            "override_note": override_value.get("note").cloned().unwrap_or_else(|| Value::String(String::new())),
            "mastery": mastery,
            "threshold": module.get("pass_threshold").and_then(Value::as_f64).unwrap_or(0.8),
            "attempts": [],
            "correct_count": 0,
            "explanation": "",
            "review": Value::Null,
            "evidence": [],
            "evidence_count": 0,
            "errors": []
        }
    })))
}

async fn redo_progress(
    ScopedState(state): ScopedState,
    Path(path_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let mut topic = load_topic(&state, &path_id)?;
    topic["mastery_levels"] = json!({});
    topic["overrides"] = json!({});
    topic["updated_at"] = Value::from(crate::storage::now());
    topic["revision"] = Value::from(next_revision(&topic));
    let stored = state
        .database
        .put_resource(TOPIC_DOMAIN, &path_id, &topic)?;
    append_event(
        &state,
        &path_id,
        "progress_reset",
        "redo",
        json!({}),
        "",
        "",
    )?;
    Ok(Json(
        json!({"status": "ok", "path": topic_view(&state, &stored)?}),
    ))
}

async fn skip_question(
    ScopedState(state): ScopedState,
    Path(path_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let mut topic = load_topic(&state, &path_id)?;
    topic["pending_question"] = Value::Null;
    topic["updated_at"] = Value::from(crate::storage::now());
    state
        .database
        .put_resource(TOPIC_DOMAIN, &path_id, &topic)?;
    append_event(
        &state,
        &path_id,
        "pending_question_skipped",
        "skip_question",
        json!({}),
        "",
        "",
    )?;
    Ok(Json(json!({"status": "ok"})))
}

async fn import_from_book(
    ScopedState(state): ScopedState,
    Path(path_id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let chapters = body
        .get("chapters")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let modules = chapters
        .into_iter()
        .enumerate()
        .map(|(index, chapter)| {
            let title = chapter
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("章节");
            let points = chapter
                .get("knowledge_points")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|point| point.as_str().map(str::to_string))
                .collect::<Vec<_>>();
            module_from_strings(index, title, &points)
        })
        .collect::<Vec<_>>();
    let mut topic = load_or_create_legacy_topic(&state, &path_id)?;
    topic["modules"] = Value::Array(normalize_modules(modules));
    topic["revision"] = Value::from(next_revision(&topic));
    topic["updated_at"] = Value::from(crate::storage::now());
    let stored = state
        .database
        .put_resource(TOPIC_DOMAIN, &path_id, &topic)?;
    append_event(
        &state,
        &path_id,
        "modules_imported",
        "import_from_book",
        json!({}),
        "",
        "",
    )?;
    Ok(Json(
        json!({"status": "ok", "path": topic_view(&state, &stored)?}),
    ))
}

async fn generate_from_notebook(
    ScopedState(state): ScopedState,
    Path(path_id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let records = body
        .get("records")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let modules = records
        .chunks(2)
        .enumerate()
        .map(|(index, records)| {
            let title = records
                .first()
                .and_then(|record| record.get("title"))
                .and_then(Value::as_str)
                .unwrap_or("笔记本主题");
            let points = records
                .iter()
                .filter_map(|record| record.get("title").and_then(Value::as_str))
                .map(str::to_string)
                .collect::<Vec<_>>();
            module_from_strings(index, title, &points)
        })
        .collect::<Vec<_>>();
    let modules = normalize_modules(modules);
    let mut topic = load_or_create_legacy_topic(&state, &path_id)?;
    topic["modules"] = Value::Array(modules.clone());
    topic["revision"] = Value::from(next_revision(&topic));
    topic["updated_at"] = Value::from(crate::storage::now());
    let stored = state
        .database
        .put_resource(TOPIC_DOMAIN, &path_id, &topic)?;
    append_event(
        &state,
        &path_id,
        "modules_generated",
        "generate_from_notebook",
        json!({}),
        "",
        "",
    )?;
    let _ = stored;
    Ok(Json(json!({"modules": modules})))
}

async fn generate_from_reading(
    ScopedState(state): ScopedState,
    Path(path_id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let workspace_id = body
        .get("workspace_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiError::bad_request("缺少 workspace_id。"))?;
    let material_ids = body
        .get("material_ids")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let records = super::reading::mastery_source_records(&state, workspace_id, &material_ids)?;
    generate_from_notebook(
        ScopedState(state),
        Path(path_id),
        Json(json!({"records": records})),
    )
    .await
}

pub async fn websocket(ws: WebSocketUpgrade, ScopedState(state): ScopedState) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    let (mut sender, mut receiver) = socket.split();
    let mut live = state.mastery_events.subscribe();
    let mut path_id = String::new();
    let mut cursor = 0u64;
    loop {
        tokio::select! {
            incoming = receiver.next() => {
                let Some(Ok(message)) = incoming else {
                    break;
                };
                match message {
                    Message::Text(text) => {
                        let Ok(command) = serde_json::from_str::<Value>(&text) else {
                            if sender.send(Message::Text(json!({"type":"error","content":"订阅消息不是合法 JSON。"}).to_string().into())).await.is_err() {
                                break;
                            }
                            continue;
                        };
                        if command.get("type").and_then(Value::as_str) != Some("subscribe") {
                            continue;
                        }
                        let Some(requested_path) = command.get("path_id").and_then(Value::as_str) else {
                            continue;
                        };
                        path_id = requested_path.to_string();
                        cursor = command.get("after_revision").and_then(Value::as_u64).unwrap_or(0);
                        if state.database.get_resource(TOPIC_DOMAIN, &path_id).ok().flatten().is_none() {
                            let frame = json!({"type":"error","content":"学习路径不存在。"});
                            if sender.send(Message::Text(frame.to_string().into())).await.is_err() {
                                break;
                            }
                            continue;
                        }
                        let events = stored_events(&state, &path_id).unwrap_or_default();
                        let revision = events.last().and_then(|event| event.get("revision")).and_then(Value::as_u64).unwrap_or(0);
                        let replay = events.into_iter().filter(|event| event.get("revision").and_then(Value::as_u64).unwrap_or(0) > cursor).collect::<Vec<_>>();
                        let frame = json!({
                            "type": "subscribed",
                            "path_id": path_id,
                            "revision": revision,
                            "events": replay
                        });
                        cursor = revision;
                        if sender.send(Message::Text(frame.to_string().into())).await.is_err() {
                            break;
                        }
                    }
                    Message::Ping(value) => {
                        if sender.send(Message::Pong(value)).await.is_err() {
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            event = live.recv() => {
                let Ok(event) = event else {
                    continue;
                };
                if path_id.is_empty() || event.get("path_id").and_then(Value::as_str) != Some(path_id.as_str()) {
                    continue;
                }
                let revision = event.get("revision").and_then(Value::as_u64).unwrap_or(0);
                if revision <= cursor {
                    continue;
                }
                cursor = revision;
                let frame = json!({
                    "type": "topic_event",
                    "path_id": path_id,
                    "revision": revision,
                    "reason": event.get("reason").cloned().unwrap_or_else(|| Value::String("update".into())),
                    "sequence": event.get("sequence").cloned().unwrap_or_else(|| Value::from(revision)),
                    "events": [event]
                });
                if sender.send(Message::Text(frame.to_string().into())).await.is_err() {
                    break;
                }
            }
        }
    }
}

fn load_topic(state: &AppState, path_id: &str) -> ApiResult<Value> {
    state
        .database
        .get_resource(TOPIC_DOMAIN, path_id)?
        .ok_or_else(|| ApiError::not_found("学习路径不存在。"))
}

fn load_or_create_legacy_topic(state: &AppState, path_id: &str) -> ApiResult<Value> {
    if let Some(topic) = state.database.get_resource(TOPIC_DOMAIN, path_id)? {
        return Ok(topic);
    }
    let now = crate::storage::now();
    let topic = json!({
        "path_id": path_id,
        "name": path_id,
        "goal": path_id,
        "description": "",
        "emoji": "🧭",
        "status": "active",
        "map_seed": rand_seed(path_id),
        "created_at": now,
        "updated_at": now,
        "revision": 1,
        "modules": [],
        "mastery_levels": {},
        "overrides": {},
        "sources": [],
        "review_settings": {"desired_retention": 0.9, "scope": "path"},
        "learner_profile": Value::Null,
        "session_ids": []
    });
    Ok(state.database.put_resource(TOPIC_DOMAIN, path_id, &topic)?)
}

fn topic_view(state: &AppState, topic: &Value) -> ApiResult<Value> {
    let path_id = topic
        .get("path_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let sessions = topic_sessions_for(state, path_id)?;
    let mut view = topic.clone();
    view["metadata"] = json!({
        "path_id": path_id,
        "goal": topic.get("goal").cloned().unwrap_or(Value::Null),
        "description": topic.get("description").cloned().unwrap_or(Value::Null),
        "emoji": topic.get("emoji").cloned().unwrap_or(Value::Null),
        "map_seed": topic.get("map_seed").cloned().unwrap_or(Value::from(0)),
        "status": topic.get("status").cloned().unwrap_or_else(|| Value::String("active".into())),
        "created_at": topic.get("created_at").cloned().unwrap_or(Value::from(0)),
        "updated_at": topic.get("updated_at").cloned().unwrap_or(Value::from(0))
    });
    view["path_revision"] = topic.get("revision").cloned().unwrap_or(Value::from(1));
    view["next"] = next_objective(topic);
    view["map"] = path_map(topic);
    view["reviews"] = json!([]);
    view["session_count"] = Value::from(sessions.len());
    view["updated_at"] = topic
        .get("updated_at")
        .cloned()
        .unwrap_or_else(|| Value::from(crate::storage::now()));
    Ok(view)
}

fn path_map(topic: &Value) -> Value {
    let modules = topic
        .get("modules")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut counts = Map::new();
    counts.insert("mastered".into(), Value::from(0));
    counts.insert("learning".into(), Value::from(0));
    counts.insert("new".into(), Value::from(0));
    counts.insert("total".into(), Value::from(0));
    let mut map_modules = Vec::new();
    for (module_index, module) in modules.iter().enumerate() {
        let mut points = Vec::new();
        let mut mastered = 0u64;
        for point in module
            .get("knowledge_points")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let (status, mastery) = objective_state(topic, point, module);
            if status == "mastered" {
                mastered += 1;
            }
            increment_count(&mut counts, &status);
            increment_count(&mut counts, "total");
            let id = point.get("id").and_then(Value::as_str).unwrap_or("");
            let override_value = topic
                .get("overrides")
                .and_then(|value| value.get(id))
                .cloned()
                .unwrap_or_else(|| json!({}));
            points.push(json!({
                "id": id,
                "name": point.get("name").and_then(Value::as_str).unwrap_or(""),
                "type": point.get("type").and_then(Value::as_str).unwrap_or("concept"),
                "prerequisite_ids": point.get("prerequisite_ids").cloned().unwrap_or_else(|| json!([])),
                "topic_source_ids": point.get("topic_source_ids").cloned().unwrap_or_else(|| json!([])),
                "status": status,
                "mastery": mastery,
                "mastery_source": if override_value.get("mastered").and_then(Value::as_bool) == Some(true) { "learner" } else if mastery > 0.0 { "system" } else { "" },
                "override_note": override_value.get("note").cloned().unwrap_or_else(|| Value::String(String::new())),
                "deferred": false
            }));
        }
        map_modules.push(json!({
            "id": module.get("id").cloned().unwrap_or(Value::Null),
            "name": module.get("name").cloned().unwrap_or(Value::Null),
            "objective": module.get("objective").and_then(Value::as_str).unwrap_or(""),
            "order": module.get("order").and_then(Value::as_u64).unwrap_or((module_index + 1) as u64),
            "mastered": mastered,
            "total": points.len(),
            "knowledge_points": points
        }));
    }
    json!({
        "name": topic.get("name").cloned().unwrap_or(Value::Null),
        "counts": counts,
        "due_reviews": 0,
        "complete": counts.get("mastered").and_then(Value::as_u64).unwrap_or(0) > 0
            && counts.get("mastered") == counts.get("total"),
        "modules": map_modules
    })
}

fn next_objective(topic: &Value) -> Value {
    for module in topic
        .get("modules")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        for point in module
            .get("knowledge_points")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let (status, mastery) = objective_state(topic, point, module);
            if status != "mastered" {
                return json!({
                    "action": if status == "learning" { "practice" } else { "learn" },
                    "knowledge_point_id": point.get("id").and_then(Value::as_str).unwrap_or(""),
                    "knowledge_point_name": point.get("name").and_then(Value::as_str).unwrap_or(""),
                    "knowledge_point_type": point.get("type").and_then(Value::as_str).unwrap_or("concept"),
                    "status": status,
                    "gate": if point.get("type").and_then(Value::as_str) == Some("skill") { "qualitative" } else { "quantitative" },
                    "mastery": mastery,
                    "threshold": module.get("pass_threshold").and_then(Value::as_f64).unwrap_or(0.8),
                    "reason": if status == "learning" { "继续练习以巩固当前目标" } else { "这是下一步尚未开始的目标" },
                    "forgetting_risk": 0.0,
                    "pending_prompt": "",
                    "session_id": "",
                    "module_id": module.get("id").cloned().unwrap_or(Value::Null)
                });
            }
        }
    }
    json!({
        "action": "complete",
        "knowledge_point_id": "",
        "knowledge_point_name": "",
        "knowledge_point_type": "",
        "status": "mastered",
        "gate": "quantitative",
        "mastery": 1.0,
        "threshold": 0.8,
        "reason": "所有目标均已掌握",
        "forgetting_risk": 0.0,
        "pending_prompt": "",
        "session_id": "",
        "module_id": Value::Null
    })
}

fn objective_state(topic: &Value, point: &Value, module: &Value) -> (&'static str, f64) {
    let id = point.get("id").and_then(Value::as_str).unwrap_or("");
    if topic
        .get("overrides")
        .and_then(|value| value.get(id))
        .and_then(|value| value.get("mastered"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        return ("mastered", 1.0);
    }
    let mastery = topic
        .get("mastery_levels")
        .and_then(|value| value.get(id))
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
        .clamp(0.0, 1.0);
    let threshold = module
        .get("pass_threshold")
        .and_then(Value::as_f64)
        .unwrap_or(0.8);
    if mastery >= threshold {
        ("mastered", mastery)
    } else if mastery > 0.0 {
        ("learning", mastery)
    } else {
        ("new", 0.0)
    }
}

fn find_objective(topic: &Value, objective_id: &str) -> Option<(Value, Value)> {
    for module in topic
        .get("modules")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        for point in module
            .get("knowledge_points")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if point.get("id").and_then(Value::as_str) == Some(objective_id) {
                return Some((module.clone(), point.clone()));
            }
        }
    }
    None
}

fn progress_summary(topic: &Value) -> Value {
    let average = average_mastery(topic) * 100.0;
    let modules = topic
        .get("modules")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let point_count = modules
        .iter()
        .map(|module| {
            module
                .get("knowledge_points")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0)
        })
        .sum::<usize>();
    json!({
        "book_id": topic.get("path_id").cloned().unwrap_or(Value::Null),
        "name": topic.get("name").cloned().unwrap_or(Value::Null),
        "modules_count": modules.len(),
        "kp_count": point_count,
        "current_stage": if average >= 80.0 { "review" } else { "learning" },
        "avg_mastery_pct": average.round(),
        "updated_at": topic.get("updated_at").cloned().unwrap_or(Value::from(0))
    })
}

fn average_mastery(topic: &Value) -> f64 {
    let mut total = 0.0;
    let mut count = 0usize;
    for module in topic
        .get("modules")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        for point in module
            .get("knowledge_points")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            total += objective_state(topic, point, module).1;
            count += 1;
        }
    }
    if count == 0 {
        0.0
    } else {
        total / count as f64
    }
}

fn draft_payload(body: &Value) -> Value {
    let goal = body
        .get("goal")
        .and_then(Value::as_str)
        .unwrap_or("新的学习主题");
    let source_count = body
        .get("sources")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let module_limit = (3 + source_count.min(5)).min(8);
    let modules = draft_modules(goal, module_limit);
    let sources = body
        .get("sources")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let documents = sources
        .iter()
        .filter(|source| source.get("kind").and_then(Value::as_str) != Some("goal"))
        .count();
    json!({
        "description": format!("围绕“{goal}”从基础概念逐步推进到迁移应用的自适应学习路线。"),
        "modules": modules,
        "sources": sources,
        "module_limit": module_limit,
        "coverage": {
            "documents": documents,
            "covered": documents,
            "missing": [],
            "reported": documents > 0
        }
    })
}

fn draft_modules(goal: &str, count: usize) -> Vec<Value> {
    let templates = [
        ("建立基础", vec!["核心定义", "基本术语", "前置知识"]),
        ("理解核心机制", vec!["关键原理", "推理过程", "常见误区"]),
        ("练习与应用", vec!["典型例题", "迁移应用", "问题诊断"]),
        ("综合与复习", vec!["知识连接", "综合任务", "间隔复习"]),
        ("项目实践", vec!["方案设计", "执行验证", "反思改进"]),
    ];
    let count = count.clamp(1, templates.len());
    (0..count)
        .map(|index| {
            let template = &templates[index];
            let name = if index == 0 {
                format!("{goal}：{}", template.0)
            } else {
                template.0.to_string()
            };
            module_from_strings(
                index,
                &name,
                &template.1.iter().map(|v| v.to_string()).collect::<Vec<_>>(),
            )
        })
        .collect()
}

fn module_from_strings(index: usize, title: &str, points: &[String]) -> Value {
    json!({
        "id": format!("module-{}", Uuid::new_v4()),
        "name": title,
        "objective": format!("完成“{title}”中的关键目标并达到 80% 掌握度。"),
        "order": index + 1,
        "pass_threshold": 0.8,
        "knowledge_points": points.iter().map(|name| {
            json!({
                "id": format!("kp-{}", Uuid::new_v4()),
                "name": name,
                "type": "concept",
                "required_visual_tasks": [],
                "prerequisite_ids": [],
                "topic_source_ids": []
            })
        }).collect::<Vec<_>>()
    })
}

fn normalize_modules(modules: Vec<Value>) -> Vec<Value> {
    modules
        .into_iter()
        .enumerate()
        .map(|(module_index, module)| {
            let module_id = module
                .get("id")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("module-{}", Uuid::new_v4()));
            let points = module
                .get("knowledge_points")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|point| {
                    let id = point
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|value| !value.trim().is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("kp-{}", Uuid::new_v4()));
                    json!({
                        "id": id,
                        "name": point.get("name").and_then(Value::as_str).unwrap_or("学习目标"),
                        "type": point.get("type").and_then(Value::as_str).unwrap_or("concept"),
                        "module_id": module_id,
                        "required_visual_tasks": point.get("required_visual_tasks").cloned().unwrap_or_else(|| json!([])),
                        "prerequisite_ids": point.get("prerequisite_ids").cloned().unwrap_or_else(|| json!([])),
                        "topic_source_ids": point.get("topic_source_ids").cloned().unwrap_or_else(|| json!([]))
                    })
                })
                .collect::<Vec<_>>();
            json!({
                "id": module_id,
                "name": module.get("name").and_then(Value::as_str).unwrap_or("学习模块"),
                "objective": module.get("objective").and_then(Value::as_str).unwrap_or(""),
                "order": module.get("order").and_then(Value::as_u64).unwrap_or((module_index + 1) as u64),
                "pass_threshold": module.get("pass_threshold").and_then(Value::as_f64).unwrap_or(0.8),
                "knowledge_points": points
            })
        })
        .collect()
}

fn topic_sessions_for(state: &AppState, path_id: &str) -> ApiResult<Vec<Value>> {
    let sessions = state.database.list_sessions(500, 0)?;
    Ok(sessions
        .into_iter()
        .filter(|session| {
            session
                .get("preferences")
                .and_then(|value| value.get("mastery_path_id"))
                .and_then(Value::as_str)
                == Some(path_id)
        })
        .map(|session| {
            let session_id = session.get("id").and_then(Value::as_str).unwrap_or("");
            let messages = state.database.get_messages(session_id).unwrap_or_default();
            json!({
                "session_id": session_id,
                "title": session.get("title").cloned().unwrap_or(Value::Null),
                "created_at": session.get("created_at").cloned().unwrap_or(Value::from(0)),
                "updated_at": session.get("updated_at").cloned().unwrap_or(Value::from(0)),
                "status": session.get("status").cloned().unwrap_or_else(|| Value::String("idle".into())),
                "active_turn_id": session.get("active_turn_id").cloned().unwrap_or_else(|| Value::String(String::new())),
                "message_count": messages.len(),
                "last_message": messages.last().and_then(|message| message.get("content")).and_then(Value::as_str).unwrap_or(""),
                "pinned": session.get("pinned").and_then(Value::as_bool).unwrap_or(false),
                "archived": session.get("archived").and_then(Value::as_bool).unwrap_or(false),
                "has_pending_question": false
            })
        })
        .collect())
}

fn stored_events(state: &AppState, path_id: &str) -> ApiResult<Vec<Value>> {
    let mut events = state
        .database
        .list_resources(&format!("{EVENT_DOMAIN_PREFIX}{path_id}"))?;
    events.sort_by_key(|event| event.get("revision").and_then(Value::as_u64).unwrap_or(0));
    Ok(events)
}

fn append_event(
    state: &AppState,
    path_id: &str,
    event_type: &str,
    reason: &str,
    payload: Value,
    session_id: &str,
    turn_id: &str,
) -> ApiResult<Value> {
    let mut topic = load_topic(state, path_id)?;
    let revision = next_revision(&topic);
    topic["revision"] = Value::from(revision);
    topic["updated_at"] = Value::from(crate::storage::now());
    state.database.put_resource(TOPIC_DOMAIN, path_id, &topic)?;
    let event = json!({
        "id": revision,
        "revision": revision,
        "event_type": event_type,
        "payload": payload,
        "session_id": session_id,
        "turn_id": turn_id,
        "created_at": crate::storage::now(),
        "path_id": path_id,
        "reason": reason,
        "sequence": revision
    });
    let event_id = format!("{revision:020}");
    state.database.put_resource(
        &format!("{EVENT_DOMAIN_PREFIX}{path_id}"),
        &event_id,
        &event,
    )?;
    let _ = state.mastery_events.send(event.clone());
    Ok(event)
}

fn next_revision(topic: &Value) -> u64 {
    topic
        .get("revision")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .saturating_add(1)
}

fn increment_count(counts: &mut Map<String, Value>, key: &str) {
    let next = counts.get(key).and_then(Value::as_u64).unwrap_or(0) + 1;
    counts.insert(key.to_string(), Value::from(next));
}

fn rand_seed(value: &str) -> u64 {
    value.bytes().fold(0x811c9dc5u64, |hash, byte| {
        (hash ^ byte as u64).wrapping_mul(0x01000193)
    })
}

/// 为聊天工具生成可解析的掌握主题导航数据。
pub fn navigation_topics(state: &AppState, query: &str) -> Result<Value, String> {
    let query = query.trim().to_lowercase();
    let mut topics = Vec::new();
    for topic in state.database.list_resources(TOPIC_DOMAIN)? {
        let name = topic
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("未命名主题");
        let goal = topic.get("goal").and_then(Value::as_str).unwrap_or("");
        let modules = topic
            .get("modules")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let matches = query.is_empty()
            || name.to_lowercase().contains(&query)
            || goal.to_lowercase().contains(&query)
            || modules.iter().any(|module| {
                module
                    .get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| name.to_lowercase().contains(&query))
            });
        if !matches {
            continue;
        }
        let map = path_map(&topic);
        let counts = map.get("counts").cloned().unwrap_or_else(|| json!({}));
        let sessions = topic_sessions_for(
            state,
            topic.get("path_id").and_then(Value::as_str).unwrap_or(""),
        )
        .map_err(|error| error.to_string())?;
        topics.push(json!({
            "path_id": topic.get("path_id").cloned().unwrap_or(Value::Null),
            "name": name,
            "emoji": topic.get("emoji").cloned().unwrap_or_else(|| Value::String("📚".into())),
            "goal": goal,
            "status": topic.get("status").cloned().unwrap_or_else(|| Value::String("active".into())),
            "mastered": counts.get("mastered").cloned().unwrap_or(Value::from(0)),
            "learning": counts.get("learning").cloned().unwrap_or(Value::from(0)),
            "new": counts.get("new").cloned().unwrap_or(Value::from(0)),
            "objectives": counts.get("total").cloned().unwrap_or(Value::from(0)),
            "due_reviews": 0,
            "session_count": sessions.len(),
            "modules": modules.iter().enumerate().map(|(index, module)| {
                json!({
                    "module_id": module.get("id").cloned().unwrap_or(Value::Null),
                    "name": module.get("name").cloned().unwrap_or(Value::Null),
                    "order": module.get("order").cloned().unwrap_or_else(|| Value::from(index + 1))
                })
            }).collect::<Vec<_>>()
        }));
    }
    Ok(json!({
        "query": query,
        "topics": topics
    }))
}

/// 列出某个掌握主题下可恢复的历史学习会话。
pub fn navigation_sessions(state: &AppState, path_id: &str) -> Result<Value, String> {
    let topic = load_topic(state, path_id).map_err(|error| error.to_string())?;
    let rows = topic_sessions_for(state, path_id).map_err(|error| error.to_string())?;
    let sessions = rows
        .into_iter()
        .filter(|session| {
            !session
                .get("archived")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .map(|session| {
            let running = session.get("status").and_then(Value::as_str) == Some("running")
                || session
                    .get("active_turn_id")
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.is_empty());
            json!({
                "session_id": session.get("session_id").cloned().unwrap_or(Value::Null),
                "title": session.get("title").cloned().unwrap_or(Value::Null),
                "message_count": session.get("message_count").cloned().unwrap_or(Value::from(0)),
                "updated_at": session.get("updated_at").cloned().unwrap_or(Value::from(0)),
                "awaiting_answer": session.get("has_pending_question").and_then(Value::as_bool).unwrap_or(false),
                "running": running,
                "last_message": session.get("last_message").cloned().unwrap_or(Value::Null)
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "path_id": path_id,
        "path_name": topic.get("name").cloned().unwrap_or(Value::Null),
        "sessions": sessions
    }))
}

/// 生成掌握学习界面可直接识别的交接卡片数据。
pub fn navigation_handoff(
    state: &AppState,
    kind: &str,
    path_id: &str,
    session_id: &str,
    opening_message: &str,
    module_ref: &str,
    reason: &str,
) -> Result<Value, String> {
    if !matches!(kind, "open" | "new") {
        return Err("交接类型必须是 open 或 new。".to_string());
    }
    let topic = load_topic(state, path_id).map_err(|error| error.to_string())?;
    let sessions = topic_sessions_for(state, path_id).map_err(|error| error.to_string())?;
    let session = if kind == "open" {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return Err("打开既有会话时必须提供 session_id。".to_string());
        }
        Some(
            sessions
                .iter()
                .find(|session| {
                    session.get("session_id").and_then(Value::as_str) == Some(session_id)
                })
                .cloned()
                .ok_or_else(|| format!("该主题下不存在会话 {session_id}。"))?,
        )
    } else {
        None
    };
    let module_ref = module_ref.trim().to_lowercase();
    let module = if module_ref.is_empty() {
        None
    } else {
        topic
            .get("modules")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|module| {
                module
                    .get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| id.to_lowercase() == module_ref)
                    || module
                        .get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|name| name.to_lowercase() == module_ref)
                    || module
                        .get("order")
                        .and_then(Value::as_u64)
                        .is_some_and(|order| order.to_string() == module_ref)
            })
    };
    if !module_ref.is_empty() && module.is_none() {
        return Err(format!("主题中没有匹配的课程模块：{module_ref}"));
    }
    let map = path_map(&topic);
    let counts = map.get("counts").cloned().unwrap_or_else(|| json!({}));
    let session_running = session.as_ref().is_some_and(|session| {
        session.get("status").and_then(Value::as_str) == Some("running")
            || session
                .get("active_turn_id")
                .and_then(Value::as_str)
                .is_some_and(|value| !value.is_empty())
    });
    Ok(json!({
        "kind": kind,
        "path_id": topic.get("path_id").cloned().unwrap_or(Value::Null),
        "path_name": topic.get("name").cloned().unwrap_or(Value::Null),
        "emoji": topic.get("emoji").cloned().unwrap_or(Value::Null),
        "session_id": session.as_ref().and_then(|value| value.get("session_id")).cloned().unwrap_or_else(|| Value::String(String::new())),
        "session_title": session.as_ref().and_then(|value| value.get("title")).cloned().unwrap_or_else(|| Value::String(String::new())),
        "session_messages": session.as_ref().and_then(|value| value.get("message_count")).cloned().unwrap_or(Value::from(0)),
        "session_updated_at": session.as_ref().and_then(|value| value.get("updated_at")).cloned().unwrap_or(Value::from(0)),
        "session_awaiting": session.as_ref().and_then(|value| value.get("has_pending_question")).cloned().unwrap_or(Value::Bool(false)),
        "session_running": session_running,
        "module_id": module.and_then(|value| value.get("id")).cloned().unwrap_or_else(|| Value::String(String::new())),
        "module_name": module.and_then(|value| value.get("name")).cloned().unwrap_or_else(|| Value::String(String::new())),
        "opening_message": opening_message.trim(),
        "reason": reason.trim(),
        "due_reviews": 0,
        "mastered": counts.get("mastered").cloned().unwrap_or(Value::from(0)),
        "objectives": counts.get("total").cloned().unwrap_or(Value::from(0)),
        "instruction": "学习者现在看到目的地卡片。只用一句话说明将获得什么以及为何值得，不要复述链接，不要声称已自动打开，也不要在此窗口开始教授该主题。"
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 模块规范化为每个目标补齐稳定结构() {
        let modules = normalize_modules(vec![json!({
            "name": "基础",
            "knowledge_points": [{"name": "定义", "type": "concept"}]
        })]);
        assert_eq!(modules.len(), 1);
        assert!(modules[0]["id"].as_str().is_some());
        assert_eq!(
            modules[0]["knowledge_points"][0]["module_id"],
            modules[0]["id"]
        );
    }
}
