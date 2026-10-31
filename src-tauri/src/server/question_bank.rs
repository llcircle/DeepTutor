use super::ScopedState;
use super::{ApiError, ApiResult, AppState};
use crate::storage::Database;
use axum::body::Body;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use base64::Engine;
use calamine::{open_workbook_from_rs, Data, Reader, Xlsx};
use chrono::{Duration as ChronoDuration, NaiveDate, TimeZone};
use chrono_tz::Tz;
use rusqlite::types::Value as SqlValue;
use rusqlite::{params, params_from_iter, OptionalExtension};
use rust_xlsxwriter::Workbook;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Cursor;
use std::path::Path as FilePath;
use std::str::FromStr;
use uuid::Uuid;

const ENTRY_SELECT: &str = r#"
    SELECT e.id, e.session_id, COALESCE(s.title, '') AS session_title,
           e.origin_type, e.origin_ref, e.turn_id, e.question_id, e.question,
           e.question_type, e.options_json, e.correct_answer, e.explanation,
           e.difficulty, e.user_answer, e.user_answer_images_json, e.source,
           e.material_id, e.material_title, e.section_id, e.section_title,
           e.score_trend, e.assessment_type, e.result, e.mastery_path_id,
           e.knowledge_point_id, e.attempt_count, e.hints_used, e.confidence,
           e.response_time, e.quality, e.is_correct, e.resolved, e.bookmarked,
           e.followup_session_id, e.ai_judgment, e.created_at, e.updated_at,
           r.entry_id AS practice_entry_id, r.is_mistake, r.due_at,
           r.review_count, r.last_answer, r.last_rating
    FROM qn_entries e
    LEFT JOIN sessions s ON s.id = e.session_id AND s.deleted_at IS NULL
    LEFT JOIN qn_practice_state r ON r.entry_id = e.id
"#;

#[derive(Debug, Deserialize)]
struct EntryQuery {
    category_id: Option<i64>,
    #[serde(default)]
    mistakes_only: bool,
    #[serde(default)]
    uncategorized: bool,
    bookmarked: Option<bool>,
    is_correct: Option<bool>,
    #[serde(default)]
    source: String,
    #[serde(default)]
    material_id: String,
    #[serde(default)]
    section_id: String,
    #[serde(default)]
    assessment_type: String,
    #[serde(default)]
    result: String,
    #[serde(default)]
    mastery_path_id: String,
    #[serde(default)]
    knowledge_point_id: String,
    resolved: Option<bool>,
    #[serde(default)]
    score_trend: String,
    #[serde(default)]
    search: String,
    #[serde(default = "default_sort")]
    sort: String,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    offset: usize,
}

#[derive(Debug, Deserialize)]
struct LookupQuery {
    #[serde(default)]
    session_id: String,
    #[serde(default = "default_origin_type")]
    origin_type: String,
    #[serde(default)]
    origin_ref: String,
    question_id: String,
    turn_id: Option<String>,
    #[serde(default)]
    missing_ok: bool,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/question-notebook/categories",
            get(list_categories).post(create_category),
        )
        .route(
            "/question-notebook/categories/{category_id}",
            patch(rename_category).delete(delete_category),
        )
        .route("/question-notebook/entries", get(list_entries))
        .route("/question-notebook/entries/upsert", post(upsert_entry))
        .route(
            "/question-notebook/entries/categories/bulk",
            post(bulk_link_categories),
        )
        .route(
            "/question-notebook/entries/lookup/by-question",
            get(lookup_entry),
        )
        .route(
            "/question-notebook/entries/{entry_id}",
            get(get_entry).patch(update_entry).delete(delete_entry),
        )
        .route(
            "/question-notebook/entries/{entry_id}/categories",
            post(add_entry_category),
        )
        .route(
            "/question-notebook/entries/{entry_id}/categories/{category_id}",
            axum::routing::delete(remove_entry_category),
        )
        .route("/question-notebook/stats", get(question_stats))
        .route("/question-notebook/materials", get(question_materials))
        .route("/question-notebook/practice/summary", get(practice_summary))
        .route("/question-notebook/practice/queue", get(practice_queue))
        .route(
            "/question-notebook/practice/analytics",
            get(practice_analytics),
        )
        .route(
            "/question-notebook/practice/questions/{entry_id}",
            get(practice_question),
        )
        .route(
            "/question-notebook/practice/questions/{entry_id}/check",
            post(practice_check),
        )
        .route(
            "/question-notebook/practice/questions/{entry_id}/review",
            post(practice_review),
        )
        .route(
            "/question-notebook/practice/import/template",
            get(practice_import_template),
        )
        .route(
            "/question-notebook/practice/import/preview",
            post(practice_import_preview),
        )
        .route(
            "/question-notebook/practice/import/commit",
            post(practice_import_commit),
        )
}

pub fn initialize(database: &Database) -> Result<(), String> {
    database.with_connection(|connection| {
        connection
            .execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS qn_categories (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    name TEXT NOT NULL COLLATE NOCASE UNIQUE,
                    created_at REAL NOT NULL
                );

                CREATE TABLE IF NOT EXISTS qn_entries (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL DEFAULT '',
                    origin_type TEXT NOT NULL DEFAULT 'conversation',
                    origin_ref TEXT NOT NULL DEFAULT '',
                    turn_id TEXT NOT NULL DEFAULT '',
                    question_id TEXT NOT NULL,
                    question TEXT NOT NULL,
                    question_type TEXT NOT NULL DEFAULT '',
                    options_json TEXT NOT NULL DEFAULT '{}',
                    correct_answer TEXT NOT NULL DEFAULT '',
                    explanation TEXT NOT NULL DEFAULT '',
                    difficulty TEXT NOT NULL DEFAULT '',
                    user_answer TEXT NOT NULL DEFAULT '',
                    user_answer_images_json TEXT NOT NULL DEFAULT '[]',
                    source TEXT NOT NULL DEFAULT 'deep_question',
                    material_id TEXT NOT NULL DEFAULT '',
                    material_title TEXT NOT NULL DEFAULT '',
                    section_id TEXT NOT NULL DEFAULT '',
                    section_title TEXT NOT NULL DEFAULT '',
                    score_trend TEXT NOT NULL DEFAULT 'new',
                    assessment_type TEXT NOT NULL DEFAULT '',
                    result TEXT NOT NULL DEFAULT '',
                    mastery_path_id TEXT NOT NULL DEFAULT '',
                    knowledge_point_id TEXT NOT NULL DEFAULT '',
                    attempt_count INTEGER NOT NULL DEFAULT 1,
                    hints_used INTEGER NOT NULL DEFAULT 0,
                    confidence REAL,
                    response_time REAL,
                    quality REAL,
                    is_correct INTEGER NOT NULL DEFAULT 0,
                    resolved INTEGER NOT NULL DEFAULT 0,
                    bookmarked INTEGER NOT NULL DEFAULT 0,
                    followup_session_id TEXT NOT NULL DEFAULT '',
                    ai_judgment TEXT NOT NULL DEFAULT '',
                    created_at REAL NOT NULL,
                    updated_at REAL NOT NULL,
                    UNIQUE(origin_type, origin_ref, question_id, turn_id)
                );
                CREATE INDEX IF NOT EXISTS idx_qn_entries_created
                    ON qn_entries(created_at DESC, id DESC);
                CREATE INDEX IF NOT EXISTS idx_qn_entries_source
                    ON qn_entries(source, material_id, created_at DESC);
                CREATE INDEX IF NOT EXISTS idx_qn_entries_mastery
                    ON qn_entries(mastery_path_id, knowledge_point_id);
                CREATE INDEX IF NOT EXISTS idx_qn_entries_flags
                    ON qn_entries(is_correct, resolved, bookmarked);

                CREATE TABLE IF NOT EXISTS qn_entry_categories (
                    entry_id INTEGER NOT NULL REFERENCES qn_entries(id) ON DELETE CASCADE,
                    category_id INTEGER NOT NULL REFERENCES qn_categories(id) ON DELETE CASCADE,
                    PRIMARY KEY(entry_id, category_id)
                );
                CREATE INDEX IF NOT EXISTS idx_qn_category_entries
                    ON qn_entry_categories(category_id, entry_id);

                CREATE TABLE IF NOT EXISTS qn_practice_state (
                    entry_id INTEGER PRIMARY KEY REFERENCES qn_entries(id) ON DELETE CASCADE,
                    is_mistake INTEGER NOT NULL DEFAULT 1,
                    first_wrong_at REAL NOT NULL,
                    due_at REAL NOT NULL,
                    interval_days REAL NOT NULL DEFAULT 1,
                    ease REAL NOT NULL DEFAULT 2.5,
                    streak INTEGER NOT NULL DEFAULT 0,
                    lapses INTEGER NOT NULL DEFAULT 0,
                    review_count INTEGER NOT NULL DEFAULT 0,
                    last_review_at REAL,
                    last_answer TEXT,
                    last_rating TEXT,
                    version INTEGER NOT NULL DEFAULT 0
                );
                CREATE INDEX IF NOT EXISTS idx_qn_practice_due
                    ON qn_practice_state(due_at, entry_id);

                CREATE TABLE IF NOT EXISTS qn_practice_events (
                    request_id TEXT PRIMARY KEY,
                    entry_id INTEGER NOT NULL REFERENCES qn_entries(id) ON DELETE CASCADE,
                    rating TEXT NOT NULL,
                    answer TEXT NOT NULL,
                    reviewed_at REAL NOT NULL,
                    outcome_json TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_qn_practice_history
                    ON qn_practice_events(entry_id, reviewed_at);

                CREATE TABLE IF NOT EXISTS qn_imports (
                    token TEXT PRIMARY KEY,
                    filename TEXT NOT NULL,
                    target TEXT NOT NULL,
                    payload_json TEXT NOT NULL,
                    created_at REAL NOT NULL,
                    result_json TEXT
                );
                "#,
            )
            .map_err(|error| format!("无法初始化题库数据库：{error}"))
    })
}

async fn list_entries(
    ScopedState(state): ScopedState,
    Query(query): Query<EntryQuery>,
) -> ApiResult<Json<Value>> {
    let limit = query.limit.clamp(1, 200);
    state
        .database
        .with_connection(|connection| {
            let (where_clause, values) = entry_filters(&query);
            let total: i64 = connection
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM qn_entries e LEFT JOIN sessions s \
                     ON s.id=e.session_id AND s.deleted_at IS NULL \
                     LEFT JOIN qn_practice_state r ON r.entry_id=e.id {where_clause}"
                    ),
                    params_from_iter(values.iter()),
                    |row| row.get(0),
                )
                .map_err(|error| format!("无法统计题库条目：{error}"))?;
            let order = if query.sort == "oldest" {
                "e.created_at ASC, e.id ASC"
            } else {
                "e.created_at DESC, e.id DESC"
            };
            let sql = format!("{ENTRY_SELECT} {where_clause} ORDER BY {order} LIMIT ? OFFSET ?");
            let mut extended = values;
            extended.push(SqlValue::Integer(limit as i64));
            extended.push(SqlValue::Integer(query.offset as i64));
            let mut statement = connection
                .prepare(&sql)
                .map_err(|error| format!("无法准备题库查询：{error}"))?;
            let mut entries = statement
                .query_map(params_from_iter(extended.iter()), entry_from_row)
                .map_err(|error| format!("无法读取题库条目：{error}"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| format!("无法解析题库条目：{error}"))?;
            drop(statement);
            attach_categories(connection, &mut entries)?;
            Ok(Json(json!({"items": entries, "total": total})))
        })
        .map_err(ApiError::from)
}

async fn lookup_entry(
    ScopedState(state): ScopedState,
    Query(query): Query<LookupQuery>,
) -> ApiResult<axum::response::Response> {
    let origin_ref = if query.origin_ref.trim().is_empty() {
        query.session_id.trim().to_string()
    } else {
        query.origin_ref.trim().to_string()
    };
    if origin_ref.is_empty() {
        return Err(ApiError::bad_request("session_id 或 origin_ref 不能为空"));
    }
    if query.origin_type == "conversation"
        && !query.session_id.trim().is_empty()
        && origin_ref != query.session_id.trim()
    {
        return Err(ApiError::bad_request(
            "会话题目的 origin_ref 必须与 session_id 一致",
        ));
    }
    let found = state
        .database
        .with_connection(|connection| {
            find_entry(
                connection,
                &query.origin_type,
                &origin_ref,
                &query.question_id,
                query.turn_id.as_deref(),
            )
        })
        .map_err(ApiError::from)?;
    match found {
        Some(entry) => Ok(Json(entry).into_response()),
        None if query.missing_ok => Ok(StatusCode::NO_CONTENT.into_response()),
        None => Err(ApiError::not_found("题库条目不存在")),
    }
}

async fn get_entry(
    ScopedState(state): ScopedState,
    Path(entry_id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let entry = state
        .database
        .with_connection(|connection| entry_by_id(connection, entry_id))
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::not_found("题库条目不存在"))?;
    Ok(Json(entry))
}

async fn upsert_entry(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let prepared = prepare_entry_upsert(&state, &body)?;
    let entry = state
        .database
        .with_connection(|connection| save_entry(connection, prepared))
        .map_err(ApiError::from)?;
    Ok(Json(entry))
}

async fn update_entry(
    ScopedState(state): ScopedState,
    Path(entry_id): Path<i64>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let mut updates = Vec::new();
    let mut values = Vec::new();
    for (field, column, kind) in [
        ("bookmarked", "bookmarked", "bool"),
        ("followup_session_id", "followup_session_id", "text"),
        ("ai_judgment", "ai_judgment", "text"),
        ("resolved", "resolved", "bool"),
    ] {
        let Some(value) = body.get(field) else {
            continue;
        };
        match kind {
            "bool" => {
                let value = value
                    .as_bool()
                    .ok_or_else(|| ApiError::bad_request(format!("{field} 必须是布尔值")))?;
                updates.push(format!("{column}=?"));
                values.push(SqlValue::Integer(value as i64));
            }
            _ => {
                let value = value
                    .as_str()
                    .ok_or_else(|| ApiError::bad_request(format!("{field} 必须是字符串")))?;
                updates.push(format!("{column}=?"));
                values.push(SqlValue::Text(value.to_string()));
            }
        }
    }
    if updates.is_empty() {
        return Err(ApiError::bad_request("没有可更新的字段"));
    }
    updates.push("updated_at=?".to_string());
    values.push(SqlValue::Real(now()));
    values.push(SqlValue::Integer(entry_id));
    let changed = state
        .database
        .with_connection(|connection| {
            connection
                .execute(
                    &format!("UPDATE qn_entries SET {} WHERE id=?", updates.join(", ")),
                    params_from_iter(values.iter()),
                )
                .map_err(|error| format!("无法更新题库条目：{error}"))
        })
        .map_err(ApiError::from)?;
    if changed == 0 {
        return Err(ApiError::not_found("题库条目不存在"));
    }
    Ok(Json(json!({"updated": true, "id": entry_id})))
}

async fn delete_entry(
    ScopedState(state): ScopedState,
    Path(entry_id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let images = state
        .database
        .with_connection(|connection| {
            let raw: Option<String> = connection
                .query_row(
                    "SELECT user_answer_images_json FROM qn_entries WHERE id=?1",
                    params![entry_id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| format!("无法读取题目图片：{error}"))?;
            let deleted = connection
                .execute("DELETE FROM qn_entries WHERE id=?1", params![entry_id])
                .map_err(|error| format!("无法删除题库条目：{error}"))?;
            Ok((deleted, raw))
        })
        .map_err(ApiError::from)?;
    if images.0 == 0 {
        return Err(ApiError::not_found("题库条目不存在"));
    }
    if let Some(raw) = images.1 {
        let _ = delete_answer_images(&state, &raw);
    }
    Ok(Json(json!({"deleted": true, "id": entry_id})))
}

async fn list_categories(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    let categories = state
        .database
        .with_connection(|connection| read_categories(connection))
        .map_err(ApiError::from)?;
    Ok(Json(Value::Array(categories)))
}

async fn create_category(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let name = category_name(&body)?;
    let category = state
        .database
        .with_connection(|connection| {
            connection
                .execute(
                    "INSERT INTO qn_categories(name, created_at) VALUES(?1, ?2)",
                    params![name, now()],
                )
                .map_err(|error| {
                    if error.to_string().contains("UNIQUE") {
                        "已存在同名分类".to_string()
                    } else {
                        format!("无法创建题库分类：{error}")
                    }
                })?;
            let id = connection.last_insert_rowid();
            read_category(connection, id)
        })
        .map_err(|error| {
            if error == "已存在同名分类" {
                ApiError::conflict(error)
            } else {
                ApiError::from(error)
            }
        })?;
    Ok((StatusCode::CREATED, Json(category)))
}

async fn rename_category(
    ScopedState(state): ScopedState,
    Path(category_id): Path<i64>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let name = category_name(&body)?;
    let changed = state
        .database
        .with_connection(|connection| {
            connection
                .execute(
                    "UPDATE qn_categories SET name=?2 WHERE id=?1",
                    params![category_id, name],
                )
                .map_err(|error| {
                    if error.to_string().contains("UNIQUE") {
                        "已存在同名分类".to_string()
                    } else {
                        format!("无法重命名题库分类：{error}")
                    }
                })
        })
        .map_err(|error| {
            if error == "已存在同名分类" {
                ApiError::conflict(error)
            } else {
                ApiError::from(error)
            }
        })?;
    if changed == 0 {
        return Err(ApiError::not_found("分类不存在"));
    }
    Ok(Json(
        json!({"updated": true, "id": category_id, "name": name}),
    ))
}

async fn delete_category(
    ScopedState(state): ScopedState,
    Path(category_id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let changed = state
        .database
        .with_connection(|connection| {
            connection
                .execute(
                    "DELETE FROM qn_categories WHERE id=?1",
                    params![category_id],
                )
                .map_err(|error| format!("无法删除题库分类：{error}"))
        })
        .map_err(ApiError::from)?;
    if changed == 0 {
        return Err(ApiError::not_found("分类不存在"));
    }
    Ok(Json(json!({"deleted": true, "id": category_id})))
}

async fn add_entry_category(
    ScopedState(state): ScopedState,
    Path(entry_id): Path<i64>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let category_id = body
        .get("category_id")
        .and_then(Value::as_i64)
        .ok_or_else(|| ApiError::bad_request("缺少 category_id"))?;
    let changed = state
        .database
        .with_connection(|connection| {
            let exists: bool = connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM qn_entries WHERE id=?1)",
                    params![entry_id],
                    |row| row.get(0),
                )
                .map_err(|error| format!("无法检查题库条目：{error}"))?;
            if !exists {
                return Ok(0);
            }
            connection
                .execute(
                    "INSERT OR IGNORE INTO qn_entry_categories(entry_id, category_id) \
                     VALUES(?1, ?2)",
                    params![entry_id, category_id],
                )
                .map_err(|error| format!("无法关联题库分类：{error}"))
        })
        .map_err(ApiError::from)?;
    if changed == 0 {
        return Err(ApiError::bad_request("题库条目不存在或分类关联已存在"));
    }
    Ok(Json(
        json!({"added": true, "entry_id": entry_id, "category_id": category_id}),
    ))
}

async fn remove_entry_category(
    ScopedState(state): ScopedState,
    Path((entry_id, category_id)): Path<(i64, i64)>,
) -> ApiResult<Json<Value>> {
    let changed = state
        .database
        .with_connection(|connection| {
            connection
                .execute(
                    "DELETE FROM qn_entry_categories WHERE entry_id=?1 AND category_id=?2",
                    params![entry_id, category_id],
                )
                .map_err(|error| format!("无法移除题库分类：{error}"))
        })
        .map_err(ApiError::from)?;
    if changed == 0 {
        return Err(ApiError::not_found("分类关联不存在"));
    }
    Ok(Json(
        json!({"removed": true, "entry_id": entry_id, "category_id": category_id}),
    ))
}

async fn bulk_link_categories(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let entry_ids = body
        .get("entry_ids")
        .and_then(Value::as_array)
        .ok_or_else(|| ApiError::bad_request("entry_ids 必须是数组"))?
        .iter()
        .filter_map(Value::as_i64)
        .collect::<Vec<_>>();
    if entry_ids.is_empty() || entry_ids.len() > 500 {
        return Err(ApiError::bad_request("一次必须选择 1 至 500 道题"));
    }
    let category_id = body
        .get("category_id")
        .and_then(Value::as_i64)
        .ok_or_else(|| ApiError::bad_request("缺少 category_id"))?;
    let link = body.get("link").and_then(Value::as_bool).unwrap_or(true);
    let changed = state
        .database
        .with_connection(|connection| {
            let transaction = connection
                .transaction()
                .map_err(|error| format!("无法开始分类事务：{error}"))?;
            let mut changed = 0usize;
            for id in &entry_ids {
                let result = if link {
                    transaction.execute(
                        "INSERT OR IGNORE INTO qn_entry_categories(entry_id, category_id) \
                         SELECT id, ?2 FROM qn_entries WHERE id=?1",
                        params![id, category_id],
                    )
                } else {
                    transaction.execute(
                        "DELETE FROM qn_entry_categories WHERE entry_id=?1 AND category_id=?2",
                        params![id, category_id],
                    )
                }
                .map_err(|error| format!("无法批量更新题库分类：{error}"))?;
                changed += result;
            }
            transaction
                .commit()
                .map_err(|error| format!("无法提交分类事务：{error}"))?;
            Ok(changed)
        })
        .map_err(ApiError::from)?;
    Ok(Json(json!({
        "changed": changed,
        "requested": entry_ids.len(),
        "category_id": category_id,
        "link": link
    })))
}

async fn question_stats(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    let stats = state
        .database
        .with_connection(|connection| {
            connection
                .query_row(
                    r#"
                    SELECT COUNT(*),
                           COALESCE(SUM(CASE WHEN is_correct=0 THEN 1 ELSE 0 END), 0),
                           COALESCE(SUM(CASE WHEN resolved=0 THEN 1 ELSE 0 END), 0),
                           COALESCE(SUM(bookmarked), 0),
                           COALESCE(SUM(CASE WHEN NOT EXISTS(
                               SELECT 1 FROM qn_entry_categories c WHERE c.entry_id=e.id
                           ) THEN 1 ELSE 0 END), 0)
                    FROM qn_entries e
                    "#,
                    [],
                    |row| {
                        Ok(json!({
                            "total": row.get::<_, i64>(0)?,
                            "wrong": row.get::<_, i64>(1)?,
                            "unresolved": row.get::<_, i64>(2)?,
                            "bookmarked": row.get::<_, i64>(3)?,
                            "uncategorized": row.get::<_, i64>(4)?
                        }))
                    },
                )
                .map_err(|error| format!("无法统计题库：{error}"))
        })
        .map_err(ApiError::from)?;
    Ok(Json(stats))
}

async fn question_materials(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    let materials = state
        .database
        .with_connection(|connection| {
            let mut statement = connection
                .prepare(
                    r#"
                    SELECT COALESCE(NULLIF(source, ''), 'deep_question'),
                           COALESCE(material_id, ''),
                           COALESCE(material_title, ''),
                           COUNT(*),
                           COALESCE(SUM(CASE WHEN resolved=0 THEN 1 ELSE 0 END), 0)
                    FROM qn_entries
                    GROUP BY source, material_id, material_title
                    ORDER BY MIN(created_at) DESC
                    "#,
                )
                .map_err(|error| format!("无法准备题库材料查询：{error}"))?;
            let rows = statement
                .query_map([], |row| {
                    Ok(json!({
                        "source": row.get::<_, String>(0)?,
                        "material_id": row.get::<_, String>(1)?,
                        "material_title": row.get::<_, String>(2)?,
                        "entry_count": row.get::<_, i64>(3)?,
                        "unresolved_count": row.get::<_, i64>(4)?
                    }))
                })
                .map_err(|error| format!("无法读取题库材料：{error}"))?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|error| format!("无法解析题库材料：{error}"))
        })
        .map_err(ApiError::from)?;
    Ok(Json(Value::Array(materials)))
}

struct PreparedEntry {
    origin_type: String,
    origin_ref: String,
    session_id: String,
    turn_id: String,
    question_id: String,
    fields: Value,
    images: Option<Vec<Value>>,
}

fn prepare_entry_upsert(state: &AppState, body: &Value) -> Result<PreparedEntry, ApiError> {
    let origin_type = string_or(body, "origin_type", "conversation");
    let session_id = string_or(body, "session_id", "").trim().to_string();
    let mut origin_ref = string_or(body, "origin_ref", "").trim().to_string();
    if origin_type == "conversation" {
        if session_id.is_empty() {
            return Err(ApiError::unprocessable("会话题目必须提供 session_id"));
        }
        if !origin_ref.is_empty() && origin_ref != session_id {
            return Err(ApiError::unprocessable(
                "会话题目的 origin_ref 必须与 session_id 一致",
            ));
        }
        origin_ref = session_id.clone();
    } else if origin_ref.is_empty() {
        return Err(ApiError::unprocessable(format!(
            "{origin_type} 题目必须提供 origin_ref"
        )));
    }
    let question_id = string_or(body, "question_id", "").trim().to_string();
    let question = string_or(body, "question", "").trim().to_string();
    if question_id.is_empty() || question.is_empty() {
        return Err(ApiError::unprocessable("题目 ID 和题干不能为空"));
    }
    let owner = answer_image_owner(&session_id, &origin_type, &origin_ref);
    let images = if body.get("user_answer_images").is_some() {
        Some(
            persist_answer_images(state, &owner, body.get("user_answer_images"))
                .map_err(ApiError::internal)?,
        )
    } else {
        None
    };
    Ok(PreparedEntry {
        origin_type,
        origin_ref,
        session_id,
        turn_id: string_or(body, "turn_id", "").trim().to_string(),
        question_id,
        fields: body.clone(),
        images,
    })
}

fn save_entry(
    connection: &mut rusqlite::Connection,
    prepared: PreparedEntry,
) -> Result<Value, String> {
    let transaction = connection
        .transaction()
        .map_err(|error| format!("无法开始题库存档事务：{error}"))?;
    let existing: Option<(i64, String, String, String, String)> = transaction
        .query_row(
            r#"
            SELECT id, question, options_json, correct_answer, user_answer_images_json
            FROM qn_entries
            WHERE origin_type=?1 AND origin_ref=?2 AND question_id=?3 AND turn_id=?4
            "#,
            params![
                prepared.origin_type,
                prepared.origin_ref,
                prepared.question_id,
                prepared.turn_id
            ],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()
        .map_err(|error| format!("无法查找已有题目：{error}"))?;
    let fields = &prepared.fields;
    let question = string_or(fields, "question", "");
    let options_json = fields
        .get("options")
        .cloned()
        .unwrap_or_else(|| json!({}))
        .to_string();
    let correct_answer = string_or(fields, "correct_answer", "");
    let result = string_or(fields, "result", "");
    let is_correct = fields
        .get("is_correct")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let timestamp = now();
    let id = if let Some((id, old_question, old_options, old_answer, old_images)) = existing {
        let images_json = prepared
            .images
            .as_ref()
            .map(|images| Value::Array(images.clone()).to_string())
            .unwrap_or(old_images);
        transaction
            .execute(
                r#"
                UPDATE qn_entries SET
                    session_id=?2, question=?3, question_type=?4, options_json=?5,
                    correct_answer=?6, explanation=?7, difficulty=?8, user_answer=?9,
                    user_answer_images_json=?10, source=?11, material_id=?12,
                    material_title=?13, section_id=?14, section_title=?15,
                    score_trend=?16, assessment_type=?17, result=?18,
                    mastery_path_id=?19, knowledge_point_id=?20, attempt_count=?21,
                    hints_used=?22, confidence=?23, response_time=?24, quality=?25,
                    is_correct=?26, updated_at=?27
                WHERE id=?1
                "#,
                params![
                    id,
                    prepared.session_id,
                    question,
                    string_or(fields, "question_type", ""),
                    options_json,
                    correct_answer,
                    string_or(fields, "explanation", ""),
                    string_or(fields, "difficulty", ""),
                    string_or(fields, "user_answer", ""),
                    images_json,
                    string_or(fields, "source", "deep_question"),
                    string_or(fields, "material_id", ""),
                    string_or(fields, "material_title", ""),
                    string_or(fields, "section_id", ""),
                    string_or(fields, "section_title", ""),
                    string_or(fields, "score_trend", "new"),
                    string_or(fields, "assessment_type", ""),
                    result,
                    string_or(fields, "mastery_path_id", ""),
                    string_or(fields, "knowledge_point_id", ""),
                    integer_or(fields, "attempt_count", 1),
                    integer_or(fields, "hints_used", 0),
                    optional_number(fields, "confidence"),
                    optional_number(fields, "response_time"),
                    optional_number(fields, "quality"),
                    is_correct as i64,
                    timestamp
                ],
            )
            .map_err(|error| format!("无法更新题库条目：{error}"))?;
        if old_question != question || old_options != options_json || old_answer != correct_answer {
            transaction
                .execute(
                    "UPDATE qn_practice_state SET version=version+1 WHERE entry_id=?1",
                    params![id],
                )
                .map_err(|error| format!("无法更新复习题目版本：{error}"))?;
        }
        id
    } else {
        let images_json = prepared
            .images
            .as_ref()
            .map(|images| Value::Array(images.clone()).to_string())
            .unwrap_or_else(|| "[]".to_string());
        transaction
            .execute(
                r#"
                INSERT INTO qn_entries(
                    session_id, origin_type, origin_ref, turn_id, question_id,
                    question, question_type, options_json, correct_answer,
                    explanation, difficulty, user_answer, user_answer_images_json,
                    source, material_id, material_title, section_id, section_title,
                    score_trend, assessment_type, result, mastery_path_id,
                    knowledge_point_id, attempt_count, hints_used, confidence,
                    response_time, quality, is_correct, created_at, updated_at
                ) VALUES(
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                    ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24,
                    ?25, ?26, ?27, ?28, ?29, ?30, ?30
                )
                "#,
                params![
                    prepared.session_id,
                    prepared.origin_type,
                    prepared.origin_ref,
                    prepared.turn_id,
                    prepared.question_id,
                    question,
                    string_or(fields, "question_type", ""),
                    options_json,
                    correct_answer,
                    string_or(fields, "explanation", ""),
                    string_or(fields, "difficulty", ""),
                    string_or(fields, "user_answer", ""),
                    images_json,
                    string_or(fields, "source", "deep_question"),
                    string_or(fields, "material_id", ""),
                    string_or(fields, "material_title", ""),
                    string_or(fields, "section_id", ""),
                    string_or(fields, "section_title", ""),
                    string_or(fields, "score_trend", "new"),
                    string_or(fields, "assessment_type", ""),
                    result,
                    string_or(fields, "mastery_path_id", ""),
                    string_or(fields, "knowledge_point_id", ""),
                    integer_or(fields, "attempt_count", 1),
                    integer_or(fields, "hints_used", 0),
                    optional_number(fields, "confidence"),
                    optional_number(fields, "response_time"),
                    optional_number(fields, "quality"),
                    is_correct as i64,
                    timestamp
                ],
            )
            .map_err(|error| format!("无法写入题库条目：{error}"))?;
        transaction.last_insert_rowid()
    };
    if !is_correct && result != "ungraded" {
        transaction
            .execute(
                r#"
                INSERT INTO qn_practice_state(
                    entry_id, is_mistake, first_wrong_at, due_at,
                    interval_days, ease, version
                ) VALUES(?1, 1, ?2, ?3, 1, 2.5, 0)
                ON CONFLICT(entry_id) DO UPDATE SET
                    is_mistake=1,
                    first_wrong_at=CASE WHEN qn_practice_state.is_mistake=0
                        THEN excluded.first_wrong_at
                        ELSE qn_practice_state.first_wrong_at END,
                    due_at=CASE WHEN qn_practice_state.is_mistake=0
                        THEN excluded.due_at ELSE qn_practice_state.due_at END,
                    version=qn_practice_state.version +
                        CASE WHEN qn_practice_state.is_mistake=0 THEN 1 ELSE 0 END
                "#,
                params![id, timestamp, timestamp + 86_400.0],
            )
            .map_err(|error| format!("无法创建复习状态：{error}"))?;
    }
    transaction
        .commit()
        .map_err(|error| format!("无法提交题库存档事务：{error}"))?;
    entry_by_id(connection, id)?.ok_or_else(|| "题库存档后无法读取".to_string())
}

fn find_entry(
    connection: &rusqlite::Connection,
    origin_type: &str,
    origin_ref: &str,
    question_id: &str,
    turn_id: Option<&str>,
) -> Result<Option<Value>, String> {
    let id: Option<i64> = if let Some(turn_id) = turn_id.filter(|value| !value.is_empty()) {
        connection
            .query_row(
                r#"SELECT id FROM qn_entries
                   WHERE origin_type=?1 AND origin_ref=?2 AND question_id=?3 AND turn_id=?4"#,
                params![origin_type, origin_ref, question_id, turn_id],
                |row| row.get(0),
            )
            .optional()
    } else {
        connection
            .query_row(
                r#"SELECT id FROM qn_entries
                   WHERE origin_type=?1 AND origin_ref=?2 AND question_id=?3
                   ORDER BY id DESC LIMIT 1"#,
                params![origin_type, origin_ref, question_id],
                |row| row.get(0),
            )
            .optional()
    }
    .map_err(|error| format!("无法查找题库条目：{error}"))?;
    id.map(|id| entry_by_id(connection, id))
        .transpose()
        .map(Option::flatten)
}

fn entry_by_id(connection: &rusqlite::Connection, entry_id: i64) -> Result<Option<Value>, String> {
    let sql = format!("{ENTRY_SELECT} WHERE e.id=?1");
    let mut entry = connection
        .query_row(&sql, params![entry_id], entry_from_row)
        .optional()
        .map_err(|error| format!("无法读取题库条目：{error}"))?;
    if let Some(entry) = entry.as_mut() {
        let mut entries = vec![std::mem::take(entry)];
        attach_categories(connection, &mut entries)?;
        return Ok(entries.pop());
    }
    Ok(None)
}

/// 把题库条目整理成来源索引中的可读文本块。
pub fn source_block(state: &AppState, entry_id: i64) -> Result<Option<Value>, String> {
    let entry = state
        .database
        .with_connection(|connection| entry_by_id(connection, entry_id))?;
    let Some(entry) = entry else {
        return Ok(None);
    };
    let question = entry
        .get("question")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let title = question.chars().take(80).collect::<String>();
    let mut lines = vec![
        format!("# 题库题目 #{entry_id}"),
        String::new(),
        format!("题目：{question}"),
    ];
    if let Some(options) = entry.get("options").filter(|value| {
        value.as_object().is_some_and(|object| !object.is_empty())
            || value.as_array().is_some_and(|array| !array.is_empty())
    }) {
        lines.push(format!(
            "选项：{}",
            serde_json::to_string_pretty(options).unwrap_or_default()
        ));
    }
    for (label, key) in [
        ("正确答案", "correct_answer"),
        ("用户答案", "user_answer"),
        ("解析", "explanation"),
        ("来源", "source"),
    ] {
        if let Some(text) = entry
            .get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
        {
            lines.push(format!("{label}：{text}"));
        }
    }
    Ok(Some(json!({
        "id": entry_id,
        "title": if title.is_empty() { format!("题库题目 #{entry_id}") } else { title },
        "content": lines.join("\n")
    })))
}

fn entry_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let options = parse_json_column(row.get::<_, String>(9)?, json!({}));
    let images = parse_json_column(row.get::<_, String>(14)?, json!([]));
    let practice_entry_id = row.get::<_, Option<i64>>(37)?;
    let practice = practice_entry_id.map(|_| {
        json!({
            "is_mistake": row.get::<_, Option<i64>>(38).unwrap_or(Some(0)).unwrap_or(0) != 0,
            "due_at": row.get::<_, Option<f64>>(39).ok().flatten(),
            "review_count": row.get::<_, Option<i64>>(40).unwrap_or(Some(0)).unwrap_or(0),
            "last_answer": row.get::<_, Option<String>>(41).ok().flatten(),
            "last_rating": row.get::<_, Option<String>>(42).ok().flatten()
        })
    });
    Ok(json!({
        "id": row.get::<_, i64>(0)?,
        "session_id": row.get::<_, String>(1)?,
        "session_title": row.get::<_, String>(2)?,
        "origin_type": row.get::<_, String>(3)?,
        "origin_ref": row.get::<_, String>(4)?,
        "turn_id": row.get::<_, String>(5)?,
        "question_id": row.get::<_, String>(6)?,
        "question": row.get::<_, String>(7)?,
        "question_type": row.get::<_, String>(8)?,
        "options": options,
        "correct_answer": row.get::<_, String>(10)?,
        "explanation": row.get::<_, String>(11)?,
        "difficulty": row.get::<_, String>(12)?,
        "user_answer": row.get::<_, String>(13)?,
        "user_answer_images": images,
        "source": row.get::<_, String>(15)?,
        "material_id": row.get::<_, String>(16)?,
        "material_title": row.get::<_, String>(17)?,
        "section_id": row.get::<_, String>(18)?,
        "section_title": row.get::<_, String>(19)?,
        "score_trend": row.get::<_, String>(20)?,
        "assessment_type": row.get::<_, String>(21)?,
        "result": row.get::<_, String>(22)?,
        "mastery_path_id": row.get::<_, String>(23)?,
        "knowledge_point_id": row.get::<_, String>(24)?,
        "attempt_count": row.get::<_, i64>(25)?,
        "hints_used": row.get::<_, i64>(26)?,
        "confidence": row.get::<_, Option<f64>>(27)?,
        "response_time": row.get::<_, Option<f64>>(28)?,
        "quality": row.get::<_, Option<f64>>(29)?,
        "is_correct": row.get::<_, i64>(30)? != 0,
        "resolved": row.get::<_, i64>(31)? != 0,
        "bookmarked": row.get::<_, i64>(32)? != 0,
        "followup_session_id": row.get::<_, String>(33)?,
        "ai_judgment": row.get::<_, String>(34)?,
        "created_at": row.get::<_, f64>(35)?,
        "updated_at": row.get::<_, f64>(36)?,
        "practice": practice
    }))
}

fn attach_categories(
    connection: &rusqlite::Connection,
    entries: &mut [Value],
) -> Result<(), String> {
    if entries.is_empty() {
        return Ok(());
    }
    let wanted = entries
        .iter()
        .filter_map(|entry| entry.get("id").and_then(Value::as_i64))
        .collect::<HashSet<_>>();
    let mut statement = connection
        .prepare(
            r#"
            SELECT ec.entry_id, c.id, c.name, c.created_at,
                   (SELECT COUNT(*) FROM qn_entry_categories x WHERE x.category_id=c.id)
            FROM qn_entry_categories ec
            JOIN qn_categories c ON c.id=ec.category_id
            ORDER BY c.name COLLATE NOCASE
            "#,
        )
        .map_err(|error| format!("无法准备题目分类查询：{error}"))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                json!({
                    "id": row.get::<_, i64>(1)?,
                    "name": row.get::<_, String>(2)?,
                    "created_at": row.get::<_, f64>(3)?,
                    "entry_count": row.get::<_, i64>(4)?
                }),
            ))
        })
        .map_err(|error| format!("无法读取题目分类：{error}"))?;
    let mut mapping: HashMap<i64, Vec<Value>> = HashMap::new();
    for row in rows {
        let (entry_id, category) = row.map_err(|error| format!("无法解析题目分类：{error}"))?;
        if wanted.contains(&entry_id) {
            mapping.entry(entry_id).or_default().push(category);
        }
    }
    for entry in entries {
        if let Some(id) = entry.get("id").and_then(Value::as_i64) {
            entry["categories"] = Value::Array(mapping.remove(&id).unwrap_or_default());
        }
    }
    Ok(())
}

fn read_categories(connection: &rusqlite::Connection) -> Result<Vec<Value>, String> {
    let mut statement = connection
        .prepare(
            r#"
            SELECT c.id, c.name, c.created_at,
                   (SELECT COUNT(*) FROM qn_entry_categories ec WHERE ec.category_id=c.id)
            FROM qn_categories c ORDER BY c.name COLLATE NOCASE
            "#,
        )
        .map_err(|error| format!("无法准备分类列表查询：{error}"))?;
    let categories = statement
        .query_map([], category_from_row)
        .map_err(|error| format!("无法读取分类列表：{error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("无法解析分类列表：{error}"))?;
    Ok(categories)
}

fn read_category(connection: &rusqlite::Connection, category_id: i64) -> Result<Value, String> {
    connection
        .query_row(
            r#"
            SELECT c.id, c.name, c.created_at,
                   (SELECT COUNT(*) FROM qn_entry_categories ec WHERE ec.category_id=c.id)
            FROM qn_categories c WHERE c.id=?1
            "#,
            params![category_id],
            category_from_row,
        )
        .map_err(|error| format!("无法读取题库分类：{error}"))
}

fn category_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    Ok(json!({
        "id": row.get::<_, i64>(0)?,
        "name": row.get::<_, String>(1)?,
        "created_at": row.get::<_, f64>(2)?,
        "entry_count": row.get::<_, i64>(3)?
    }))
}

fn entry_filters(query: &EntryQuery) -> (String, Vec<SqlValue>) {
    let mut conditions = vec!["1=1".to_string()];
    let mut values = Vec::new();
    if let Some(category_id) = query.category_id {
        conditions.push(
            "EXISTS(SELECT 1 FROM qn_entry_categories c \
             WHERE c.entry_id=e.id AND c.category_id=?)"
                .into(),
        );
        values.push(SqlValue::Integer(category_id));
    } else if query.uncategorized {
        conditions
            .push("NOT EXISTS(SELECT 1 FROM qn_entry_categories c WHERE c.entry_id=e.id)".into());
    }
    if query.mistakes_only {
        conditions.push("r.is_mistake=1".into());
    }
    for (value, column) in [
        (query.bookmarked, "e.bookmarked"),
        (query.is_correct, "e.is_correct"),
        (query.resolved, "e.resolved"),
    ] {
        if let Some(value) = value {
            conditions.push(format!("{column}=?"));
            values.push(SqlValue::Integer(value as i64));
        }
    }
    for (value, column) in [
        (&query.source, "e.source"),
        (&query.material_id, "e.material_id"),
        (&query.section_id, "e.section_id"),
        (&query.assessment_type, "e.assessment_type"),
        (&query.result, "e.result"),
        (&query.mastery_path_id, "e.mastery_path_id"),
        (&query.knowledge_point_id, "e.knowledge_point_id"),
        (&query.score_trend, "e.score_trend"),
    ] {
        if !value.trim().is_empty() {
            conditions.push(format!("{column}=?"));
            values.push(SqlValue::Text(value.trim().to_string()));
        }
    }
    if !query.search.trim().is_empty() {
        conditions
            .push("(e.question LIKE ? OR e.explanation LIKE ? OR e.material_title LIKE ?)".into());
        let pattern = format!("%{}%", query.search.trim());
        values.extend([
            SqlValue::Text(pattern.clone()),
            SqlValue::Text(pattern.clone()),
            SqlValue::Text(pattern),
        ]);
    }
    (format!("WHERE {}", conditions.join(" AND ")), values)
}

fn prepare_images(images: &Value) -> Vec<Value> {
    images
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter(|item| item.is_object())
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

fn persist_answer_images(
    state: &AppState,
    owner_id: &str,
    images: Option<&Value>,
) -> Result<Vec<Value>, String> {
    let images = images.cloned().unwrap_or_else(|| json!([]));
    let mut records = Vec::new();
    for image in prepare_images(&images) {
        let id = image
            .get("id")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(safe_component)
            .unwrap_or_else(short_id);
        let filename = image
            .get("filename")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(safe_filename)
            .unwrap_or_else(|| "answer.png".to_string());
        let mime_type = image
            .get("mime_type")
            .and_then(Value::as_str)
            .unwrap_or("image/png");
        let existing_url = image
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let mut url = existing_url.to_string();
        if url.is_empty() {
            let Some(encoded) = image.get("base64").and_then(Value::as_str) else {
                continue;
            };
            let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(encoded) else {
                tracing::warn!(%filename, "答题图片 base64 无效，已跳过");
                continue;
            };
            let directory = state
                .paths
                .data
                .join("uploads")
                .join("attachments")
                .join(safe_component(owner_id))
                .join(&id);
            std::fs::create_dir_all(&directory)
                .map_err(|error| format!("无法创建答题图片目录：{error}"))?;
            std::fs::write(directory.join(&filename), bytes)
                .map_err(|error| format!("无法保存答题图片：{error}"))?;
            url = format!(
                "/files/attachments/{}/{}/{}",
                percent_encode_component(owner_id),
                id,
                filename
            );
        }
        records.push(json!({
            "id": id,
            "url": url,
            "filename": filename,
            "mime_type": mime_type
        }));
    }
    Ok(records)
}

fn delete_answer_images(state: &AppState, raw: &str) -> Result<(), String> {
    let images: Value = serde_json::from_str(raw).unwrap_or_else(|_| json!([]));
    for image in images.as_array().into_iter().flatten() {
        let Some(url) = image.get("url").and_then(Value::as_str) else {
            continue;
        };
        let Some(relative) = url.strip_prefix("/files/attachments/") else {
            continue;
        };
        let mut path = state.paths.data.join("uploads").join("attachments");
        for segment in relative.split('/') {
            if !segment.is_empty() && segment != "." && segment != ".." {
                path.push(segment);
            }
        }
        if path.starts_with(&state.paths.data) {
            let _ = std::fs::remove_file(path);
        }
    }
    Ok(())
}

fn answer_image_owner(session_id: &str, origin_type: &str, origin_ref: &str) -> String {
    if !session_id.is_empty() {
        return session_id.to_string();
    }
    let digest = Sha256::digest(format!("{origin_type}:{origin_ref}").as_bytes());
    format!("question-notebook-{}", hex::encode(&digest[..12]))
}

fn category_name(body: &Value) -> Result<String, ApiError> {
    body.get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty() && name.chars().count() <= 100)
        .map(String::from)
        .ok_or_else(|| ApiError::bad_request("分类名称长度必须为 1 至 100 个字符"))
}

fn string_or(body: &Value, key: &str, default: &str) -> String {
    body.get(key)
        .and_then(Value::as_str)
        .unwrap_or(default)
        .to_string()
}

fn integer_or(body: &Value, key: &str, default: i64) -> i64 {
    body.get(key).and_then(Value::as_i64).unwrap_or(default)
}

fn optional_number(body: &Value, key: &str) -> Option<f64> {
    body.get(key).and_then(Value::as_f64)
}

fn parse_json_column(raw: String, fallback: Value) -> Value {
    serde_json::from_str(&raw).unwrap_or(fallback)
}

fn safe_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .take(120)
        .collect()
}

fn safe_filename(value: &str) -> String {
    let name = FilePath::new(value)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("answer.png");
    let cleaned = safe_component(name);
    if cleaned.is_empty() {
        "answer.png".to_string()
    } else {
        cleaned
    }
}

fn percent_encode_component(value: &str) -> String {
    value
        .bytes()
        .flat_map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                vec![byte as char]
            } else {
                format!("%{byte:02X}").chars().collect()
            }
        })
        .collect()
}

fn short_id() -> String {
    Uuid::new_v4().simple().to_string()[..12].to_string()
}

fn now() -> f64 {
    chrono::Utc::now().timestamp_micros() as f64 / 1_000_000.0
}

/// 题库直接存储在 SQLite 主表中；会话被删除后，其关联条目不再触发工具挂载。
pub fn has_entries(state: &AppState) -> Result<bool, String> {
    state.database.with_connection(|connection| {
        connection
            .query_row(
                r#"
                SELECT EXISTS(
                    SELECT 1
                    FROM qn_entries e
                    LEFT JOIN sessions s
                        ON s.id = e.session_id AND s.deleted_at IS NULL
                    WHERE e.session_id = '' OR s.deleted_at IS NULL
                    LIMIT 1
                )
                "#,
                [],
                |row| row.get::<_, i64>(0),
            )
            .map(|value| value != 0)
            .map_err(|error| format!("无法检查题库条目：{error}"))
    })
}

/// 为学习导航提供题库入口，只保留导航和路由所需的稳定字段。
pub(crate) fn navigation_entries(state: &AppState) -> Result<Vec<Value>, String> {
    state.database.with_connection(|connection| {
        let mut statement = connection
            .prepare(
                r#"
                SELECT id, question, created_at, updated_at
                FROM qn_entries
                ORDER BY updated_at DESC, id DESC
                "#,
            )
            .map_err(|error| format!("无法准备学习练习查询：{error}"))?;
        let rows = statement
            .query_map([], |row| {
                Ok(json!({
                    "id": row.get::<_, i64>(0)?,
                    "question": row.get::<_, String>(1)?,
                    "created_at": row.get::<_, f64>(2)?,
                    "updated_at": row.get::<_, f64>(3)?
                }))
            })
            .map_err(|error| format!("无法读取学习练习列表：{error}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("无法解析学习练习列表：{error}"))
    })
}

fn default_sort() -> String {
    "recent".into()
}

fn default_limit() -> usize {
    50
}

fn default_origin_type() -> String {
    "conversation".into()
}

const DUE_AT_SQL: &str = r#"
    CASE WHEN (e.resolved=1 OR e.is_correct=1)
         AND COALESCE(r.review_count, 0)=0
         THEN e.updated_at + 259200
         ELSE COALESCE(r.due_at, e.created_at)
    END
"#;

#[derive(Debug, Deserialize)]
struct PracticeQuery {
    #[serde(default = "default_timezone")]
    timezone: String,
    category_id: Option<i64>,
    #[serde(default = "default_queue_limit")]
    limit: usize,
}

#[derive(Debug, Deserialize)]
struct AnalyticsQuery {
    #[serde(default = "default_timezone")]
    timezone: String,
    #[serde(default = "default_analytics_days")]
    days: usize,
}

#[derive(Debug, Clone, Copy, Default)]
struct PracticeCounts {
    questions: i64,
    mistakes: i64,
    reviews: i64,
}

#[derive(Debug)]
struct ScheduledReview {
    interval_days: f64,
    ease: f64,
    streak: i64,
    lapses: i64,
    due_at: f64,
    last_review_at: f64,
    review_count: i64,
    version: i64,
}

async fn practice_summary(
    ScopedState(state): ScopedState,
    Query(query): Query<PracticeQuery>,
) -> ApiResult<Json<Value>> {
    let (day_start, day_end) =
        practice_day_bounds(&query.timezone, now()).map_err(ApiError::unprocessable)?;
    let timestamp = now();
    let summary = state
        .database
        .with_connection(|connection| {
            let mut summary = connection
                .query_row(
                    &format!(
                        r#"
                        SELECT COUNT(*),
                               COALESCE(SUM(r.is_mistake=1), 0),
                               COALESCE(SUM({DUE_AT_SQL} <= ?1), 0),
                               COALESCE(SUM({DUE_AT_SQL} <= ?2), 0),
                               MIN({DUE_AT_SQL})
                        FROM qn_entries e
                        LEFT JOIN sessions s ON s.id=e.session_id
                        LEFT JOIN qn_practice_state r ON r.entry_id=e.id
                        WHERE e.session_id='' OR s.deleted_at IS NULL
                        "#
                    ),
                    params![timestamp, day_start],
                    |row| {
                        Ok(json!({
                            "total": row.get::<_, i64>(0)?,
                            "mistakes": row.get::<_, i64>(1)?,
                            "due": row.get::<_, i64>(2)?,
                            "overdue": row.get::<_, i64>(3)?,
                            "next_due_at": row.get::<_, Option<f64>>(4)?,
                            "reviewed_today": 0,
                            "day_end": day_end,
                            "timezone": query.timezone
                        }))
                    },
                )
                .map_err(|error| format!("无法统计复习概况：{error}"))?;
            let reviewed: i64 = connection
                .query_row(
                    r#"
                    SELECT COUNT(DISTINCT ev.entry_id)
                    FROM qn_practice_events ev
                    JOIN qn_entries e ON e.id=ev.entry_id
                    LEFT JOIN sessions s ON s.id=e.session_id
                    WHERE ev.reviewed_at>=?1 AND ev.reviewed_at<?2
                      AND (e.session_id='' OR s.deleted_at IS NULL)
                    "#,
                    params![day_start, day_end],
                    |row| row.get(0),
                )
                .map_err(|error| format!("无法统计今日复习：{error}"))?;
            summary["reviewed_today"] = Value::from(reviewed);
            Ok(summary)
        })
        .map_err(ApiError::from)?;
    Ok(Json(summary))
}

async fn practice_queue(
    ScopedState(state): ScopedState,
    Query(query): Query<PracticeQuery>,
) -> ApiResult<Json<Value>> {
    practice_day_bounds(&query.timezone, now()).map_err(ApiError::unprocessable)?;
    let limit = query.limit.clamp(1, 100);
    let entries = state
        .database
        .with_connection(|connection| {
            let mut conditions = vec![
                format!("{DUE_AT_SQL} <= ?1"),
                "(e.session_id='' OR s.deleted_at IS NULL)".to_string(),
            ];
            let mut values = vec![SqlValue::Real(now())];
            if let Some(category_id) = query.category_id {
                conditions.push(
                    "EXISTS(SELECT 1 FROM qn_entry_categories c \
                     WHERE c.entry_id=e.id AND c.category_id=?)"
                        .into(),
                );
                values.push(SqlValue::Integer(category_id));
            }
            values.push(SqlValue::Integer(limit as i64));
            let sql = format!(
                r#"
                SELECT e.id
                FROM qn_entries e
                LEFT JOIN sessions s ON s.id=e.session_id
                LEFT JOIN qn_practice_state r ON r.entry_id=e.id
                WHERE {}
                ORDER BY {DUE_AT_SQL}, e.id
                LIMIT ?
                "#,
                conditions.join(" AND ")
            );
            let mut statement = connection
                .prepare(&sql)
                .map_err(|error| format!("无法准备复习队列：{error}"))?;
            let ids = statement
                .query_map(params_from_iter(values.iter()), |row| row.get::<_, i64>(0))
                .map_err(|error| format!("无法读取复习队列：{error}"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| format!("无法解析复习队列：{error}"))?;
            drop(statement);
            ids.into_iter()
                .map(|id| practice_question_value(connection, id))
                .collect::<Result<Vec<_>, _>>()
        })
        .map_err(ApiError::from)?;
    Ok(Json(Value::Array(entries)))
}

async fn practice_question(
    ScopedState(state): ScopedState,
    Path(entry_id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let question = state
        .database
        .with_connection(|connection| practice_question_value(connection, entry_id))
        .map_err(ApiError::from)?;
    Ok(Json(question))
}

async fn practice_check(
    ScopedState(state): ScopedState,
    Path(entry_id): Path<i64>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let answer = body.get("answer").and_then(Value::as_str).unwrap_or("");
    let entry = state
        .database
        .with_connection(|connection| entry_by_id(connection, entry_id))
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::not_found("题目不存在"))?;
    Ok(Json(json!({"correct": check_answer_value(&entry, answer)})))
}

async fn practice_review(
    ScopedState(state): ScopedState,
    Path(entry_id): Path<i64>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let request_id = body
        .get("request_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if request_id.len() < 16 || request_id.len() > 100 {
        return Err(ApiError::unprocessable("request_id 长度必须为 16 至 100"));
    }
    let version = body
        .get("version")
        .and_then(Value::as_i64)
        .ok_or_else(|| ApiError::unprocessable("缺少复习版本号"))?;
    let rating = body
        .get("rating")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if !matches!(rating.as_str(), "again" | "hard" | "good" | "easy") {
        return Err(ApiError::unprocessable("未知的复习评级"));
    }
    let answer = body.get("answer").and_then(Value::as_str).unwrap_or("");
    let self_report = body
        .get("self_report")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let outcome = state
        .database
        .with_connection(|connection| {
            review_entry(
                connection,
                entry_id,
                &request_id,
                version,
                &rating,
                answer,
                self_report,
            )
        })
        .map_err(|error| {
            if error.starts_with("CONFLICT:") {
                ApiError::conflict(error.trim_start_matches("CONFLICT:"))
            } else if error.starts_with("NOT_FOUND:") {
                ApiError::not_found(error.trim_start_matches("NOT_FOUND:"))
            } else if error.starts_with("INVALID:") {
                ApiError::unprocessable(error.trim_start_matches("INVALID:"))
            } else {
                ApiError::from(error)
            }
        })?;
    Ok(Json(outcome))
}

fn review_entry(
    connection: &mut rusqlite::Connection,
    entry_id: i64,
    request_id: &str,
    version: i64,
    rating: &str,
    answer: &str,
    self_report: bool,
) -> Result<Value, String> {
    let transaction = connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|error| format!("无法开始复习事务：{error}"))?;
    if let Some((existing_entry, existing_rating, existing_answer, outcome, existing_self)) =
        transaction
            .query_row(
                r#"SELECT entry_id, rating, answer, outcome_json,
                          json_extract(outcome_json, '$.self_report')
                   FROM qn_practice_events WHERE request_id=?1"#,
                params![request_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<bool>>(4)?.unwrap_or(false),
                    ))
                },
            )
            .optional()
            .map_err(|error| format!("无法读取复习幂等记录：{error}"))?
    {
        if existing_entry != entry_id
            || existing_rating != rating
            || existing_answer != answer
            || existing_self != self_report
        {
            return Err("CONFLICT:这个提交 ID 已用于另一份答案，请重新打开题目".to_string());
        }
        return serde_json::from_str(&outcome)
            .map_err(|error| format!("复习幂等记录已损坏：{error}"));
    }
    let entry = entry_by_id(&transaction, entry_id)?
        .ok_or_else(|| "NOT_FOUND:题目不存在或当前工作区不可见".to_string())?;
    let state = practice_state(&transaction, entry_id)?;
    let current_version = state.get("version").and_then(Value::as_i64).unwrap_or(0);
    if current_version != version {
        return Err("CONFLICT:题目已在其他窗口更新，请重新加载后再复习".to_string());
    }
    let correct = if self_report {
        None
    } else {
        check_answer_value(&entry, answer)
    };
    if correct == Some(false) && rating != "again" {
        return Err("INVALID:答案不正确时必须选择“再来一次”".to_string());
    }
    let reviewed_at = now();
    let scheduled =
        schedule_review(&state, rating, reviewed_at).map_err(|error| format!("INVALID:{error}"))?;
    let was_mistake = state
        .get("is_mistake")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let is_mistake = was_mistake || rating == "again";
    let first_wrong_at = state
        .get("first_wrong_at")
        .and_then(Value::as_f64)
        .unwrap_or(reviewed_at);
    transaction
        .execute(
            r#"
            INSERT INTO qn_practice_state(
                entry_id, is_mistake, first_wrong_at, due_at, interval_days,
                ease, streak, lapses, review_count, last_review_at,
                last_answer, last_rating, version
            ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
            ON CONFLICT(entry_id) DO UPDATE SET
                is_mistake=excluded.is_mistake,
                first_wrong_at=CASE WHEN qn_practice_state.is_mistake=0
                    AND excluded.is_mistake=1 THEN excluded.first_wrong_at
                    ELSE qn_practice_state.first_wrong_at END,
                due_at=excluded.due_at,
                interval_days=excluded.interval_days,
                ease=excluded.ease,
                streak=excluded.streak,
                lapses=excluded.lapses,
                review_count=excluded.review_count,
                last_review_at=excluded.last_review_at,
                last_answer=excluded.last_answer,
                last_rating=excluded.last_rating,
                version=excluded.version
            "#,
            params![
                entry_id,
                is_mistake as i64,
                first_wrong_at,
                scheduled.due_at,
                scheduled.interval_days,
                scheduled.ease,
                scheduled.streak,
                scheduled.lapses,
                scheduled.review_count,
                scheduled.last_review_at,
                answer,
                rating,
                scheduled.version
            ],
        )
        .map_err(|error| format!("无法保存复习状态：{error}"))?;
    if rating == "again" {
        transaction
            .execute(
                "UPDATE qn_entries SET resolved=0, updated_at=?2 WHERE id=?1",
                params![entry_id, reviewed_at],
            )
            .map_err(|error| format!("无法重置题目掌握状态：{error}"))?;
    }
    let outcome = json!({
        "entry_id": entry_id,
        "interval_days": scheduled.interval_days,
        "ease": scheduled.ease,
        "streak": scheduled.streak,
        "lapses": scheduled.lapses,
        "due_at": scheduled.due_at,
        "last_review_at": scheduled.last_review_at,
        "review_count": scheduled.review_count,
        "version": scheduled.version,
        "is_mistake": is_mistake,
        "correct": correct,
        "rating": rating,
        "self_report": self_report,
        "mastered": matches!(rating, "good" | "easy")
    });
    transaction
        .execute(
            "INSERT INTO qn_practice_events(request_id, entry_id, rating, answer, reviewed_at, outcome_json) \
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
            params![request_id, entry_id, rating, answer, reviewed_at, outcome.to_string()],
        )
        .map_err(|error| format!("无法记录复习历史：{error}"))?;
    transaction
        .commit()
        .map_err(|error| format!("无法提交复习事务：{error}"))?;
    Ok(outcome)
}

async fn practice_analytics(
    ScopedState(state): ScopedState,
    Query(query): Query<AnalyticsQuery>,
) -> ApiResult<Json<Value>> {
    if !matches!(query.days, 7 | 30 | 90) {
        return Err(ApiError::unprocessable("复习统计只支持 7、30 或 90 天"));
    }
    let analytics = state
        .database
        .with_connection(|connection| {
            practice_analytics_value(connection, &query.timezone, query.days)
        })
        .map_err(|error| {
            if error.starts_with("INVALID:") {
                ApiError::unprocessable(error.trim_start_matches("INVALID:"))
            } else {
                ApiError::from(error)
            }
        })?;
    Ok(Json(analytics))
}

fn practice_analytics_value(
    connection: &mut rusqlite::Connection,
    timezone: &str,
    days: usize,
) -> Result<Value, String> {
    let timestamp = now();
    let (_, day_end) =
        practice_day_bounds(timezone, timestamp).map_err(|error| format!("INVALID:{error}"))?;
    let tz = Tz::from_str(timezone).map_err(|_| "INVALID:未知时区".to_string())?;
    let today = local_date(tz, timestamp).ok_or_else(|| "INVALID:未知时区".to_string())?;
    let first = today - ChronoDuration::days(days as i64 - 1);
    let start = local_midnight(tz, first)?;
    let mut daily = BTreeMap::<NaiveDate, PracticeCounts>::new();
    for offset in 0..days {
        daily.insert(
            first + ChronoDuration::days(offset as i64),
            PracticeCounts::default(),
        );
    }
    let mut sources = BTreeMap::<String, PracticeCounts>::new();

    let mut statement = connection
        .prepare(
            r#"SELECT e.created_at, COALESCE(NULLIF(e.source, ''), 'deep_question')
               FROM qn_entries e LEFT JOIN sessions s ON s.id=e.session_id
               WHERE (e.session_id='' OR s.deleted_at IS NULL)
                 AND e.created_at>=?1 AND e.created_at<=?2"#,
        )
        .map_err(|error| format!("无法准备题目统计：{error}"))?;
    let rows = statement
        .query_map(params![start, timestamp], |row| {
            Ok((row.get::<_, f64>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| format!("无法读取题目统计：{error}"))?;
    for row in rows {
        let (at, source) = row.map_err(|error| format!("无法解析题目统计：{error}"))?;
        add_metric(&mut daily, &mut sources, tz, at, &source, "questions");
    }
    drop(statement);

    let mut statement = connection
        .prepare(
            r#"SELECT r.first_wrong_at, COALESCE(NULLIF(e.source, ''), 'deep_question')
               FROM qn_practice_state r JOIN qn_entries e ON e.id=r.entry_id
               LEFT JOIN sessions s ON s.id=e.session_id
               WHERE r.is_mistake=1 AND (e.session_id='' OR s.deleted_at IS NULL)
                 AND r.first_wrong_at>=?1 AND r.first_wrong_at<=?2"#,
        )
        .map_err(|error| format!("无法准备错题统计：{error}"))?;
    let rows = statement
        .query_map(params![start, timestamp], |row| {
            Ok((row.get::<_, f64>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| format!("无法读取错题统计：{error}"))?;
    for row in rows {
        let (at, source) = row.map_err(|error| format!("无法解析错题统计：{error}"))?;
        add_metric(&mut daily, &mut sources, tz, at, &source, "mistakes");
    }
    drop(statement);

    let mut statement = connection
        .prepare(
            r#"SELECT ev.reviewed_at, COALESCE(NULLIF(e.source, ''), 'deep_question')
               FROM qn_practice_events ev JOIN qn_entries e ON e.id=ev.entry_id
               LEFT JOIN sessions s ON s.id=e.session_id
               WHERE (e.session_id='' OR s.deleted_at IS NULL)
                 AND ev.reviewed_at>=?1 AND ev.reviewed_at<=?2"#,
        )
        .map_err(|error| format!("无法准备复习统计：{error}"))?;
    let rows = statement
        .query_map(params![start, timestamp], |row| {
            Ok((row.get::<_, f64>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| format!("无法读取复习统计：{error}"))?;
    for row in rows {
        let (at, source) = row.map_err(|error| format!("无法解析复习统计：{error}"))?;
        add_metric(&mut daily, &mut sources, tz, at, &source, "reviews");
    }
    drop(statement);

    let totals = daily
        .values()
        .fold(PracticeCounts::default(), |mut total, item| {
            total.questions += item.questions;
            total.mistakes += item.mistakes;
            total.reviews += item.reviews;
            total
        });
    let daily = daily
        .into_iter()
        .map(|(date, counts)| {
            json!({
                "date": date.format("%Y-%m-%d").to_string(),
                "questions": counts.questions,
                "mistakes": counts.mistakes,
                "reviews": counts.reviews
            })
        })
        .collect::<Vec<_>>();
    let sources = sources
        .into_iter()
        .map(|(source, counts)| {
            json!({
                "source": source,
                "questions": counts.questions,
                "mistakes": counts.mistakes,
                "reviews": counts.reviews
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "timezone": timezone,
        "days": days,
        "start_date": first.format("%Y-%m-%d").to_string(),
        "end_date": today.format("%Y-%m-%d").to_string(),
        "updated_at": timestamp,
        "daily": daily,
        "sources": sources,
        "totals": {
            "questions": totals.questions,
            "mistakes": totals.mistakes,
            "reviews": totals.reviews
        },
        "day_end": day_end
    }))
}

fn add_metric(
    daily: &mut BTreeMap<NaiveDate, PracticeCounts>,
    sources: &mut BTreeMap<String, PracticeCounts>,
    timezone: Tz,
    timestamp: f64,
    source: &str,
    metric: &str,
) {
    let Some(date) = local_date(timezone, timestamp) else {
        return;
    };
    let Some(day) = daily.get_mut(&date) else {
        return;
    };
    let source = sources.entry(source.to_string()).or_default();
    match metric {
        "questions" => {
            day.questions += 1;
            source.questions += 1;
        }
        "mistakes" => {
            day.mistakes += 1;
            source.mistakes += 1;
        }
        "reviews" => {
            day.reviews += 1;
            source.reviews += 1;
        }
        _ => {}
    }
}

async fn practice_import_template(
    Query(query): Query<HashMap<String, String>>,
) -> ApiResult<Response> {
    let format = query.get("format").map(String::as_str).unwrap_or("csv");
    if !matches!(format, "csv" | "xlsx") {
        return Err(ApiError::unprocessable("模板格式必须是 csv 或 xlsx"));
    }
    let rows = import_template_rows();
    let (bytes, content_type) = if format == "xlsx" {
        let mut workbook = Workbook::new();
        let worksheet = workbook.add_worksheet();
        for (row_index, row) in rows.iter().enumerate() {
            for (column_index, value) in row.iter().enumerate() {
                worksheet
                    .write_string(row_index as u32, column_index as u16, *value)
                    .map_err(|error| ApiError::internal(format!("无法生成 XLSX 模板：{error}")))?;
            }
        }
        (
            workbook
                .save_to_buffer()
                .map_err(|error| ApiError::internal(format!("无法生成 XLSX 模板：{error}")))?,
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        )
    } else {
        let mut writer = csv::Writer::from_writer(Vec::new());
        for row in &rows {
            writer
                .write_record(row)
                .map_err(|error| ApiError::internal(format!("无法生成 CSV 模板：{error}")))?;
        }
        let body = writer
            .into_inner()
            .map_err(|error| ApiError::internal(format!("无法生成 CSV 模板：{error}")))?;
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend(body);
        (bytes, "text/csv; charset=utf-8")
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"practice-template.{format}\""),
        )
        .body(Body::from(bytes))
        .map_err(|error| ApiError::internal(format!("无法构建下载响应：{error}")))
}

async fn practice_import_preview(
    ScopedState(state): ScopedState,
    mut multipart: Multipart,
) -> ApiResult<Json<Value>> {
    let mut filename = String::new();
    let mut target = "bank".to_string();
    let mut data = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|error| ApiError::bad_request(format!("无法读取上传表单：{error}")))?
    {
        match field.name().unwrap_or("") {
            "file" => {
                filename = field.file_name().unwrap_or("questions.csv").to_string();
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|error| ApiError::bad_request(format!("无法读取导入文件：{error}")))?;
                data = Some(bytes.to_vec());
            }
            "target" => {
                target = field
                    .text()
                    .await
                    .map_err(|error| ApiError::bad_request(format!("无法读取导入目标：{error}")))?;
            }
            _ => {}
        }
    }
    if !matches!(target.as_str(), "bank" | "mistakes") {
        return Err(ApiError::unprocessable("导入目标必须是 bank 或 mistakes"));
    }
    let data = data.ok_or_else(|| ApiError::bad_request("缺少导入文件"))?;
    let (questions, errors) =
        parse_import_file(&data, &filename).map_err(ApiError::unprocessable)?;
    let token = if errors.is_empty() {
        let token = Uuid::new_v4().simple().to_string();
        state
            .database
            .with_connection(|connection| {
                connection
                    .execute(
                        "DELETE FROM qn_imports WHERE created_at<?1 AND result_json IS NULL",
                        params![now() - 86_400.0],
                    )
                    .map_err(|error| format!("无法清理过期导入：{error}"))?;
                connection
                    .execute(
                        "INSERT INTO qn_imports(token, filename, target, payload_json, created_at) \
                         VALUES(?1, ?2, ?3, ?4, ?5)",
                        params![
                            token,
                            filename.chars().take(200).collect::<String>(),
                            target,
                            json!({"questions": questions}).to_string(),
                            now()
                        ],
                    )
                    .map_err(|error| format!("无法暂存导入：{error}"))
            })
            .map_err(ApiError::from)?;
        Some(token)
    } else {
        None
    };
    let samples = questions.iter().take(5).cloned().collect::<Vec<_>>();
    Ok(Json(json!({
        "token": token,
        "total": questions.len() + errors.len(),
        "valid": questions.len(),
        "errors": errors,
        "samples": samples
    })))
}

async fn practice_import_commit(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let token = body
        .get("token")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::unprocessable("缺少导入令牌"))?;
    if token.len() != 32 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ApiError::unprocessable("导入令牌格式无效"));
    }
    let result = state
        .database
        .with_connection(|connection| commit_import(connection, token))
        .map_err(|error| {
            if error.starts_with("NOT_FOUND:") {
                ApiError::not_found(error.trim_start_matches("NOT_FOUND:"))
            } else if error.starts_with("INVALID:") {
                ApiError::unprocessable(error.trim_start_matches("INVALID:"))
            } else {
                ApiError::from(error)
            }
        })?;
    Ok(Json(result))
}

fn commit_import(connection: &mut rusqlite::Connection, token: &str) -> Result<Value, String> {
    let transaction = connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|error| format!("无法开始导入事务：{error}"))?;
    let staged = transaction
        .query_row(
            "SELECT filename, target, payload_json, created_at, result_json \
             FROM qn_imports WHERE token=?1",
            params![token],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, f64>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .optional()
        .map_err(|error| format!("无法读取导入任务：{error}"))?
        .ok_or_else(|| "NOT_FOUND:找不到导入预览，请重新选择文件".to_string())?;
    if let Some(result) = staged.4 {
        return serde_json::from_str(&result)
            .map_err(|error| format!("导入结果记录已损坏：{error}"));
    }
    let timestamp = now();
    if timestamp - staged.3 > 86_400.0 {
        return Err("INVALID:导入预览已过期，请重新选择文件".to_string());
    }
    let payload: Value =
        serde_json::from_str(&staged.2).map_err(|error| format!("导入预览数据已损坏：{error}"))?;
    let questions = payload
        .get("questions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut created = 0i64;
    let mut duplicates = 0i64;
    for question in questions {
        let question_id = question
            .get("question_id")
            .and_then(Value::as_str)
            .ok_or_else(|| "INVALID:导入题目缺少内容标识".to_string())?;
        let result = if staged.1 == "mistakes" {
            "incorrect"
        } else {
            "ungraded"
        };
        let changed = transaction
            .execute(
                r#"
                INSERT OR IGNORE INTO qn_entries(
                    origin_type, origin_ref, question_id, question, question_type,
                    options_json, correct_answer, explanation, difficulty, user_answer,
                    source, result, assessment_type, material_title, material_id,
                    created_at, updated_at
                ) VALUES(
                    'external_import', 'practice-import', ?1, ?2, ?3, ?4, ?5,
                    ?6, ?7, ?8, 'import', ?9, 'quiz', ?10, ?11, ?12, ?12
                )
                "#,
                params![
                    question_id,
                    string_or(&question, "question", ""),
                    string_or(&question, "question_type", ""),
                    question
                        .get("options")
                        .cloned()
                        .unwrap_or_else(|| json!({}))
                        .to_string(),
                    string_or(&question, "correct_answer", ""),
                    string_or(&question, "explanation", ""),
                    string_or(&question, "difficulty", ""),
                    string_or(&question, "user_answer", ""),
                    result,
                    staged.0,
                    format!("import:{token}"),
                    timestamp
                ],
            )
            .map_err(|error| format!("无法写入导入题目：{error}"))?;
        if changed == 0 {
            duplicates += 1;
        } else {
            created += 1;
        }
        let entry_id: i64 = transaction
            .query_row(
                r#"SELECT id FROM qn_entries
                   WHERE origin_type='external_import' AND origin_ref='practice-import'
                     AND question_id=?1 AND turn_id=''
                   ORDER BY id DESC LIMIT 1"#,
                params![question_id],
                |row| row.get(0),
            )
            .map_err(|error| format!("无法读取导入题目编号：{error}"))?;
        if staged.1 == "mistakes" {
            transaction
                .execute(
                    r#"
                    INSERT INTO qn_practice_state(
                        entry_id, is_mistake, first_wrong_at, due_at
                    ) VALUES(?1, 1, ?2, ?3)
                    ON CONFLICT(entry_id) DO UPDATE SET
                        is_mistake=CASE WHEN qn_practice_state.is_mistake=0 THEN 1
                            ELSE qn_practice_state.is_mistake END,
                        due_at=CASE WHEN qn_practice_state.is_mistake=0 THEN excluded.due_at
                            ELSE qn_practice_state.due_at END,
                        version=qn_practice_state.version +
                            CASE WHEN qn_practice_state.is_mistake=0 THEN 1 ELSE 0 END
                    "#,
                    params![entry_id, timestamp, timestamp + 86_400.0],
                )
                .map_err(|error| format!("无法创建导入错题复习状态：{error}"))?;
        }
        if let Some(tags) = question.get("tags").and_then(Value::as_array) {
            for tag in tags
                .iter()
                .filter_map(Value::as_str)
                .filter(|tag| !tag.is_empty())
            {
                transaction
                    .execute(
                        "INSERT OR IGNORE INTO qn_categories(name, created_at) VALUES(?1, ?2)",
                        params![tag, timestamp],
                    )
                    .map_err(|error| format!("无法创建导入标签：{error}"))?;
                transaction
                    .execute(
                        r#"INSERT OR IGNORE INTO qn_entry_categories(entry_id, category_id)
                           SELECT ?1, id FROM qn_categories WHERE name=?2"#,
                        params![entry_id, tag],
                    )
                    .map_err(|error| format!("无法关联导入标签：{error}"))?;
            }
        }
    }
    let result = json!({
        "created": created,
        "duplicates": duplicates,
        "total": created + duplicates
    });
    transaction
        .execute(
            "UPDATE qn_imports SET result_json=?2, payload_json='[]' WHERE token=?1",
            params![token, result.to_string()],
        )
        .map_err(|error| format!("无法完成导入任务：{error}"))?;
    transaction
        .commit()
        .map_err(|error| format!("无法提交导入事务：{error}"))?;
    Ok(result)
}

fn practice_question_value(
    connection: &mut rusqlite::Connection,
    entry_id: i64,
) -> Result<Value, String> {
    let entry = entry_by_id(connection, entry_id)?
        .ok_or_else(|| "题目不存在或当前工作区不可见".to_string())?;
    let state = practice_state(connection, entry_id)?;
    Ok(json!({
        "entry": entry,
        "state": {
            "entry_id": entry_id,
            "version": state.get("version").and_then(Value::as_i64).unwrap_or(0),
            "review_count": state.get("review_count").and_then(Value::as_i64).unwrap_or(0),
            "is_mistake": state.get("is_mistake").and_then(Value::as_bool).unwrap_or(false),
            "due_at": state.get("due_at").cloned().unwrap_or(Value::Null)
        },
        "content_workspace_id": "",
        "content_workspace_name": ""
    }))
}

fn practice_state(connection: &rusqlite::Connection, entry_id: i64) -> Result<Value, String> {
    connection
        .query_row(
            r#"
            SELECT entry_id, is_mistake, first_wrong_at, due_at, interval_days,
                   ease, streak, lapses, review_count, last_review_at,
                   last_answer, last_rating, version
            FROM qn_practice_state WHERE entry_id=?1
            "#,
            params![entry_id],
            |row| {
                Ok(json!({
                    "entry_id": row.get::<_, i64>(0)?,
                    "is_mistake": row.get::<_, i64>(1)? != 0,
                    "first_wrong_at": row.get::<_, f64>(2)?,
                    "due_at": row.get::<_, f64>(3)?,
                    "interval_days": row.get::<_, f64>(4)?,
                    "ease": row.get::<_, f64>(5)?,
                    "streak": row.get::<_, i64>(6)?,
                    "lapses": row.get::<_, i64>(7)?,
                    "review_count": row.get::<_, i64>(8)?,
                    "last_review_at": row.get::<_, Option<f64>>(9)?,
                    "last_answer": row.get::<_, Option<String>>(10)?,
                    "last_rating": row.get::<_, Option<String>>(11)?,
                    "version": row.get::<_, i64>(12)?
                }))
            },
        )
        .optional()
        .map_err(|error| format!("无法读取复习状态：{error}"))?
        .map(Ok)
        .unwrap_or_else(|| {
            Ok(json!({
                "entry_id": entry_id,
                "is_mistake": false,
                "interval_days": 1.0,
                "ease": 2.5,
                "streak": 0,
                "lapses": 0,
                "review_count": 0,
                "version": 0,
                "due_at": null
            }))
        })
}

fn schedule_review(state: &Value, rating: &str, timestamp: f64) -> Result<ScheduledReview, String> {
    const DAY: f64 = 86_400.0;
    let mut interval = state
        .get("interval_days")
        .and_then(Value::as_f64)
        .unwrap_or(1.0);
    let mut ease = state.get("ease").and_then(Value::as_f64).unwrap_or(2.5);
    let mut streak = state.get("streak").and_then(Value::as_i64).unwrap_or(0);
    let mut lapses = state.get("lapses").and_then(Value::as_i64).unwrap_or(0);
    let previous_reviews = state
        .get("review_count")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    match rating {
        "again" => {
            interval = 10.0 / 1440.0;
            streak = 0;
            lapses += 1;
            ease = (ease - 0.2).max(1.3);
        }
        "hard" => {
            interval = (interval * 1.2).max(1.0);
            ease = (ease - 0.15).max(1.3);
            streak = 0;
        }
        "good" | "easy" => {
            interval = if interval <= 1.0 || previous_reviews == 0 {
                3.0
            } else {
                interval * ease
            };
            if rating == "easy" {
                interval *= 1.3;
                ease = (ease + 0.15).min(3.0);
            }
            streak += 1;
        }
        _ => return Err("未知的复习评级".to_string()),
    }
    interval = (interval * 1000.0).round() / 1000.0;
    interval = interval.min(365.0);
    Ok(ScheduledReview {
        interval_days: interval,
        ease,
        streak,
        lapses,
        due_at: timestamp + interval * DAY,
        last_review_at: timestamp,
        review_count: previous_reviews + 1,
        version: state.get("version").and_then(Value::as_i64).unwrap_or(0) + 1,
    })
}

fn check_answer_value(entry: &Value, answer: &str) -> Option<bool> {
    let kind = entry
        .get("question_type")
        .and_then(Value::as_str)
        .unwrap_or("");
    let options = entry.get("options").and_then(Value::as_object)?;
    let expected = entry
        .get("correct_answer")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if options.is_empty()
        || expected.is_empty()
        || matches!(
            kind,
            "qualitative" | "short_answer" | "essay" | "free_response"
        )
    {
        return None;
    }
    let expected_keys = answer_keys(expected, options, kind);
    if expected_keys.is_empty()
        || !expected_keys
            .iter()
            .all(|key| options.contains_key(key.as_str()))
    {
        return None;
    }
    let actual_keys = answer_keys(answer, options, kind);
    Some(!actual_keys.is_empty() && actual_keys == expected_keys)
}

fn answer_keys(
    value: &str,
    options: &serde_json::Map<String, Value>,
    kind: &str,
) -> HashSet<String> {
    fn folded(value: &str) -> String {
        value
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_uppercase()
    }
    let needle = folded(value);
    for (key, text) in options {
        let text = text.as_str().unwrap_or("");
        let candidates = [
            folded(key),
            folded(text),
            folded(&format!("{key}. {text}")),
            folded(&format!("{key}) {text}")),
        ];
        if candidates.contains(&needle) {
            return HashSet::from([key.clone()]);
        }
    }
    let mut parts = value
        .split(|ch: char| matches!(ch, ',' | ';' | '，' | '；') || ch.is_whitespace())
        .map(|part| part.trim().to_uppercase())
        .filter(|part| !part.is_empty())
        .collect::<HashSet<_>>();
    if matches!(kind, "multi_choice" | "multiple_select")
        && parts.len() == 1
        && !parts.iter().all(|part| options.contains_key(part.as_str()))
    {
        parts = value
            .trim()
            .to_uppercase()
            .chars()
            .map(|ch| ch.to_string())
            .collect();
    }
    parts
}

fn practice_day_bounds(timezone: &str, timestamp: f64) -> Result<(f64, f64), String> {
    let tz = Tz::from_str(timezone).map_err(|_| "未知时区".to_string())?;
    let date = local_date(tz, timestamp).ok_or_else(|| "未知时区".to_string())?;
    Ok((
        local_midnight(tz, date)?,
        local_midnight(tz, date + ChronoDuration::days(1))?,
    ))
}

fn local_date(timezone: Tz, timestamp: f64) -> Option<NaiveDate> {
    timezone
        .timestamp_opt(timestamp as i64, 0)
        .single()
        .map(|value| value.date_naive())
}

fn local_midnight(timezone: Tz, date: NaiveDate) -> Result<f64, String> {
    let naive = date
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| "无效日期".to_string())?;
    timezone
        .from_local_datetime(&naive)
        .earliest()
        .or_else(|| timezone.from_local_datetime(&naive).latest())
        .map(|value| value.timestamp() as f64)
        .ok_or_else(|| "无法解析本地午夜".to_string())
}

fn parse_import_file(data: &[u8], filename: &str) -> Result<(Vec<Value>, Vec<Value>), String> {
    const MAX_BYTES: usize = 5 * 1024 * 1024;
    const MAX_ROWS: usize = 500;
    if data.is_empty() || data.len() > MAX_BYTES {
        return Err("请选择不超过 5 MB 的非空文件".to_string());
    }
    let extension = FilePath::new(filename)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let rows = match extension.as_str() {
        "json" => {
            let values: Value =
                serde_json::from_slice(data).map_err(|error| format!("JSON 解析失败：{error}"))?;
            let values = values
                .as_array()
                .ok_or_else(|| "JSON 必须是题目对象数组".to_string())?;
            if values.len() > MAX_ROWS {
                return Err("一次最多导入 500 道题".to_string());
            }
            values
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    let object = value
                        .as_object()
                        .ok_or_else(|| format!("第 {} 条不是对象", index + 1))?;
                    Ok((index + 1, object.clone()))
                })
                .collect::<Result<Vec<_>, String>>()?
        }
        "csv" | "tsv" => parse_csv_rows(data, &extension, MAX_ROWS)?,
        "xlsx" => parse_xlsx_rows(data, MAX_ROWS)?,
        _ => return Err("仅支持 .xlsx、.csv、.tsv 和 .json".to_string()),
    };
    if rows.is_empty() {
        return Err("文件为空".to_string());
    }
    let mut questions = Vec::new();
    let mut errors = Vec::new();
    for (row_number, row) in rows {
        match normalize_import_question(&row) {
            Ok(question) => questions.push(question),
            Err(message) => errors.push(json!({"row": row_number, "message": message})),
        }
    }
    if questions.is_empty() && errors.is_empty() {
        return Err("文件中没有题目".to_string());
    }
    Ok((questions, errors))
}

fn parse_csv_rows(
    data: &[u8],
    extension: &str,
    max_rows: usize,
) -> Result<Vec<(usize, serde_json::Map<String, Value>)>, String> {
    let content = String::from_utf8_lossy(data);
    let content = content.trim_start_matches('\u{FEFF}');
    let delimiter = if extension == "tsv" {
        b'\t'
    } else if content.lines().next().unwrap_or("").matches(';').count()
        > content.lines().next().unwrap_or("").matches(',').count()
    {
        b';'
    } else {
        b','
    };
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .delimiter(delimiter)
        .flexible(true)
        .from_reader(content.as_bytes());
    let mut raw = Vec::new();
    for row in reader.records() {
        let record = row.map_err(|error| format!("CSV 解析失败：{error}"))?;
        if record.len() > 64 {
            return Err("一次最多导入 64 列".to_string());
        }
        raw.push(
            record
                .iter()
                .map(|value| Value::String(value.trim().to_string()))
                .collect::<Vec<_>>(),
        );
        if raw.len() > max_rows + 1 {
            return Err("一次最多导入 500 道题".to_string());
        }
    }
    rows_from_table(raw, max_rows)
}

fn parse_xlsx_rows(
    data: &[u8],
    max_rows: usize,
) -> Result<Vec<(usize, serde_json::Map<String, Value>)>, String> {
    let cursor = Cursor::new(data.to_vec());
    let mut workbook: Xlsx<_> =
        open_workbook_from_rs(cursor).map_err(|error| format!("无法打开 XLSX：{error}"))?;
    let sheet = workbook
        .worksheet_range_at(0)
        .ok_or_else(|| "工作簿没有工作表".to_string())?
        .map_err(|error| format!("无法读取工作表：{error}"))?;
    let mut raw = Vec::new();
    for row in sheet.rows() {
        if row.len() > 64 {
            return Err("一次最多导入 64 列".to_string());
        }
        raw.push(row.iter().map(data_cell).map(Value::String).collect());
        if raw.len() > max_rows + 1 {
            return Err("一次最多导入 500 道题".to_string());
        }
    }
    rows_from_table(raw, max_rows)
}

fn data_cell(cell: &Data) -> String {
    match cell {
        Data::Empty => String::new(),
        Data::String(value) => value.trim().to_string(),
        Data::Float(value) => value.to_string(),
        Data::Int(value) => value.to_string(),
        Data::Bool(value) => value.to_string(),
        Data::DateTime(value) => value.to_string(),
        Data::DateTimeIso(value) => value.clone(),
        Data::DurationIso(value) => value.clone(),
        Data::Error(value) => format!("#错误:{value:?}"),
    }
}

fn rows_from_table(
    raw: Vec<Vec<Value>>,
    max_rows: usize,
) -> Result<Vec<(usize, serde_json::Map<String, Value>)>, String> {
    if raw.len() < 2 {
        return Err("文件没有题目数据".to_string());
    }
    if raw.len() > max_rows + 1 {
        return Err("一次最多导入 500 道题".to_string());
    }
    let headers = raw[0]
        .iter()
        .map(|value| value.as_str().unwrap_or("").trim().to_lowercase())
        .collect::<Vec<_>>();
    let nonempty = headers
        .iter()
        .filter(|value| !value.is_empty())
        .collect::<HashSet<_>>();
    if nonempty.len() != headers.iter().filter(|value| !value.is_empty()).count() {
        return Err("列名不能重复".to_string());
    }
    let mut rows = Vec::new();
    for (index, row) in raw.into_iter().enumerate().skip(1) {
        if row
            .iter()
            .all(|value| value.as_str().map(str::trim).is_none_or(str::is_empty))
        {
            continue;
        }
        let mut object = serde_json::Map::new();
        for (header, value) in headers.iter().zip(row) {
            if !header.is_empty() {
                object.insert(header.clone(), value);
            }
        }
        rows.push((index + 1, object));
    }
    Ok(rows)
}

fn normalize_import_question(raw: &serde_json::Map<String, Value>) -> Result<Value, String> {
    let row = raw
        .iter()
        .map(|(key, value)| (key.trim().to_lowercase(), value.clone()))
        .collect::<HashMap<_, _>>();
    let field = |aliases: &[&str]| {
        aliases
            .iter()
            .find_map(|alias| row.get(*alias))
            .map(cell_text)
            .unwrap_or_default()
    };
    let question = field(&["question", "题目", "题干"]);
    let correct_answer = field(&["correct_answer", "answer", "答案", "正确答案"]);
    if question.is_empty() || correct_answer.is_empty() {
        return Err("题目和正确答案不能为空".to_string());
    }
    let mut options = parse_import_options(row.get("options"), row.get("选项"))?;
    for letter in "ABCDEFGHIJ".chars() {
        let key = letter.to_ascii_lowercase().to_string();
        let chinese = format!("选项{key}");
        if let Some(value) = row.get(&key).or_else(|| row.get(&chinese)) {
            let text = cell_text(value);
            if !text.is_empty() {
                options.insert(letter.to_string(), Value::String(text));
            }
        }
    }
    if options.len() > 10 {
        return Err("选项最多 10 个".to_string());
    }
    let raw_type = field(&["question_type", "type", "题型"]).to_lowercase();
    let question_type = normalize_question_type(&raw_type, !options.is_empty())?;
    let mut answer = correct_answer;
    if question_type == "true_false" {
        options = serde_json::Map::from_iter([
            ("T".into(), Value::String("True".into())),
            ("F".into(), Value::String("False".into())),
        ]);
        answer = match answer.to_lowercase().as_str() {
            "true" | "正确" | "对" | "是" | "t" | "1" => "T".to_string(),
            "false" | "错误" | "错" | "否" | "f" | "0" => "F".to_string(),
            _ => return Err("判断题答案必须是 true 或 false".to_string()),
        };
    }
    if matches!(question_type.as_str(), "single_choice" | "multi_choice") {
        if options.len() < 2 {
            return Err("选择题至少需要两个选项".to_string());
        }
        if let Some((key, _)) = options
            .iter()
            .find(|(_, text)| cell_text(text).eq_ignore_ascii_case(&answer))
        {
            answer = key.clone();
        }
        let mut keys = answer
            .split(|ch: char| matches!(ch, ',' | ';' | '，' | '；') || ch.is_whitespace())
            .map(|value| value.trim().to_uppercase())
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>();
        if question_type == "multi_choice"
            && keys.len() == 1
            && !options.contains_key(keys[0].as_str())
        {
            keys = keys[0].chars().map(|ch| ch.to_string()).collect();
        }
        let unique = keys.into_iter().collect::<HashSet<_>>();
        if unique.is_empty()
            || unique.iter().any(|key| !options.contains_key(key.as_str()))
            || (question_type == "single_choice" && unique.len() != 1)
        {
            return Err("答案必须引用已有选项（多选示例：A,C）".to_string());
        }
        let mut keys = unique.into_iter().collect::<Vec<_>>();
        keys.sort();
        answer = keys.join(",");
    }
    let tags = row
        .get("tags")
        .or_else(|| row.get("标签"))
        .or_else(|| row.get("分类"))
        .map(|value| {
            if let Some(values) = value.as_array() {
                values
                    .iter()
                    .map(cell_text)
                    .filter(|tag| !tag.is_empty())
                    .collect()
            } else {
                cell_text(value)
                    .split(|ch| matches!(ch, ',' | ';' | '，' | '；'))
                    .map(str::trim)
                    .filter(|tag| !tag.is_empty())
                    .map(String::from)
                    .collect()
            }
        })
        .unwrap_or_else(Vec::new);
    let tags = tags
        .into_iter()
        .collect::<HashSet<_>>()
        .into_iter()
        .take(20)
        .collect::<Vec<_>>();
    let identity = json!({
        "question": question,
        "question_type": question_type,
        "options": options,
        "correct_answer": answer
    });
    let digest = Sha256::digest(identity.to_string().as_bytes());
    Ok(json!({
        "question_id": format!("import:{}", hex::encode(digest)),
        "question": question,
        "question_type": question_type,
        "options": options,
        "correct_answer": answer,
        "explanation": field(&["explanation", "解析"]),
        "difficulty": field(&["difficulty", "难度"]),
        "user_answer": field(&["user_answer", "我的答案", "作答"]),
        "tags": tags
    }))
}

fn parse_import_options(
    primary: Option<&Value>,
    fallback: Option<&Value>,
) -> Result<serde_json::Map<String, Value>, String> {
    let Some(value) = primary.or(fallback) else {
        return Ok(serde_json::Map::new());
    };
    if let Some(text) = value.as_str() {
        if text.trim().is_empty() {
            return Ok(serde_json::Map::new());
        }
        let parsed: Value =
            serde_json::from_str(text).map_err(|_| "options 必须是 JSON 对象或数组".to_string())?;
        return parse_import_options(Some(&parsed), None);
    }
    if let Some(object) = value.as_object() {
        return Ok(object
            .iter()
            .filter_map(|(key, value)| {
                let text = cell_text(value);
                (!text.is_empty()).then(|| (key.trim().to_uppercase(), Value::String(text)))
            })
            .collect());
    }
    if let Some(values) = value.as_array() {
        return Ok(values
            .iter()
            .enumerate()
            .filter_map(|(index, value)| {
                let text = cell_text(value);
                (!text.is_empty()).then(|| {
                    (
                        char::from(b'A' + index as u8).to_string(),
                        Value::String(text),
                    )
                })
            })
            .collect());
    }
    Err("options 必须是 JSON 对象或数组".to_string())
}

fn normalize_question_type(raw: &str, has_options: bool) -> Result<String, String> {
    let value = match raw {
        "" if has_options => "single_choice",
        "" => "short_answer",
        "single_choice" | "multiple_choice" | "mcq" | "单选" | "单选题" => "single_choice",
        "multi_choice" | "多选" | "多选题" => "multi_choice",
        "true_false" | "判断" | "判断题" => "true_false",
        "fill_blank" | "填空" | "填空题" => "fill_blank",
        "short_answer" | "essay" | "free_response" | "简答" | "简答题" => "short_answer",
        _ => return Err("未知题型".to_string()),
    };
    Ok(value.to_string())
}

fn cell_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(value) => value.trim().to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::Array(_) | Value::Object(_) => value.to_string(),
    }
}

fn import_template_rows() -> Vec<Vec<&'static str>> {
    vec![
        vec![
            "question",
            "question_type",
            "A",
            "B",
            "C",
            "D",
            "correct_answer",
            "explanation",
            "tags",
            "user_answer",
        ],
        vec![
            "2 + 2 = ?",
            "single_choice",
            "3",
            "4",
            "5",
            "6",
            "B",
            "2 + 2 = 4",
            "Math,Addition",
            "",
        ],
        vec![
            "Select the prime numbers",
            "multi_choice",
            "2",
            "3",
            "4",
            "6",
            "A,B",
            "2 and 3 are prime",
            "Math",
            "",
        ],
        vec![
            "The Earth orbits the Sun",
            "true_false",
            "",
            "",
            "",
            "",
            "true",
            "",
            "Science",
            "",
        ],
        vec![
            "Explain photosynthesis",
            "short_answer",
            "",
            "",
            "",
            "",
            "Plants convert light into chemical energy.",
            "",
            "Biology",
            "",
        ],
    ]
}

fn default_timezone() -> String {
    "UTC".into()
}

fn default_queue_limit() -> usize {
    20
}

fn default_analytics_days() -> usize {
    30
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 分类筛选会拒绝目录逃逸式文件名() {
        assert_eq!(safe_filename("../../answer.png"), "answer_png");
        assert_eq!(safe_component("../evil"), "___evil");
    }

    #[test]
    fn 分类筛选包含分类与未分类条件() {
        let query = EntryQuery {
            category_id: None,
            mistakes_only: false,
            uncategorized: true,
            bookmarked: Some(true),
            is_correct: None,
            source: "deep_question".into(),
            material_id: String::new(),
            section_id: String::new(),
            assessment_type: String::new(),
            result: String::new(),
            mastery_path_id: String::new(),
            knowledge_point_id: String::new(),
            resolved: None,
            score_trend: String::new(),
            search: String::new(),
            sort: "recent".into(),
            limit: 50,
            offset: 0,
        };
        let (sql, values) = entry_filters(&query);
        assert!(sql.contains("NOT EXISTS"));
        assert!(sql.contains("e.bookmarked=?"));
        assert!(sql.contains("e.source=?"));
        assert_eq!(values.len(), 2);
    }
}
