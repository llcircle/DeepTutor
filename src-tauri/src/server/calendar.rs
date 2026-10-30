use super::error::{ApiError, ApiResult};
use super::AppState;
use super::ScopedState;
use axum::extract::{Path, Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use parking_lot::Mutex;
use rrule::RRuleSet;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::convert::Infallible;
use std::sync::LazyLock;
use std::time::Duration as StdDuration;
use uuid::Uuid;

const CALENDAR_DOMAIN: &str = "calendar";
const EVENT_DOMAIN: &str = "calendar_event";
const UNDO_DOMAIN: &str = "calendar_undo";
const AUTOMATION_DOMAIN: &str = "calendar_automation";
const REVISION_KEY: &str = "calendar_revision";

static CALENDAR_WRITE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[derive(Debug, Deserialize)]
struct RangeQuery {
    start: String,
    end: String,
    #[serde(default = "default_timezone")]
    tz: String,
}

#[derive(Debug, Deserialize)]
struct SearchQuery {
    q: String,
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct DeleteQuery {
    scope: Option<String>,
    occurrence: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UpdateEventRequest {
    changes: Value,
    scope: Option<String>,
    occurrence: Option<String>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/calendar/calendars",
            get(list_calendars).post(create_calendar),
        )
        .route(
            "/calendar/calendars/{id}",
            patch(update_calendar).delete(delete_calendar),
        )
        .route("/calendar/events", get(list_occurrences).post(create_event))
        .route(
            "/calendar/events/{id}",
            patch(update_event).delete(delete_event),
        )
        .route("/calendar/undo/{token}", post(undo))
        .route("/calendar/search", get(search))
        .route("/calendar/layers", get(layers))
        .route("/calendar/stream", get(stream))
        .route("/calendar/task-blocks", get(task_blocks))
        .route("/calendar/automations/{job_id}", delete(cancel_automation))
}

async fn list_calendars(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    ensure_default_calendar(&state)?;
    let mut calendars = state.database.list_resources(CALENDAR_DOMAIN)?;
    calendars.sort_by_key(|calendar| {
        calendar
            .get("position")
            .and_then(Value::as_i64)
            .unwrap_or_default()
    });
    Ok(Json(json!({
        "calendars": calendars,
        "revision": revision(&state)?
    })))
}

async fn create_calendar(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let _guard = CALENDAR_WRITE_LOCK.lock();
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| ApiError::bad_request("日历名称不能为空"))?;
    let now = iso_now();
    let id = Uuid::new_v4().to_string();
    let count = state.database.list_resources(CALENDAR_DOMAIN)?.len() as i64;
    let calendar = json!({
        "id": id,
        "name": name.chars().take(80).collect::<String>(),
        "color": body.get("color").and_then(Value::as_str).unwrap_or("#4f7cff"),
        "visible": true,
        "position": count,
        "is_default": false,
        "created_at": now,
        "updated_at": now
    });
    state
        .database
        .put_resource(CALENDAR_DOMAIN, &id, &calendar)?;
    let revision = bump_revision(&state)?;
    Ok(Json(json!({"calendar": calendar, "revision": revision})))
}

async fn update_calendar(
    ScopedState(state): ScopedState,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let _guard = CALENDAR_WRITE_LOCK.lock();
    let mut calendar = state
        .database
        .get_resource(CALENDAR_DOMAIN, &id)?
        .ok_or_else(|| ApiError::not_found("日历不存在"))?;
    for key in ["name", "color", "visible", "position"] {
        if let Some(value) = body.get(key) {
            calendar[key] = value.clone();
        }
    }
    calendar["updated_at"] = Value::String(iso_now());
    state
        .database
        .put_resource(CALENDAR_DOMAIN, &id, &calendar)?;
    let revision = bump_revision(&state)?;
    Ok(Json(json!({"calendar": calendar, "revision": revision})))
}

async fn delete_calendar(
    ScopedState(state): ScopedState,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let _guard = CALENDAR_WRITE_LOCK.lock();
    let calendar = state
        .database
        .get_resource(CALENDAR_DOMAIN, &id)?
        .ok_or_else(|| ApiError::not_found("日历不存在"))?;
    if calendar
        .get("is_default")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Err(ApiError::bad_request("默认日历不能删除"));
    }
    state.database.delete_resource(CALENDAR_DOMAIN, &id)?;
    for event in state.database.list_resources(EVENT_DOMAIN)? {
        if event.get("calendar_id").and_then(Value::as_str) == Some(id.as_str()) {
            if let Some(event_id) = event.get("id").and_then(Value::as_str) {
                state.database.delete_resource(EVENT_DOMAIN, event_id)?;
            }
        }
    }
    let revision = bump_revision(&state)?;
    Ok(Json(json!({"revision": revision})))
}

async fn list_occurrences(
    ScopedState(state): ScopedState,
    Query(query): Query<RangeQuery>,
) -> ApiResult<Json<Value>> {
    let (start, end, timezone) = parse_bounds(&query)?;
    let mut occurrences = Vec::new();
    for event in state.database.list_resources(EVENT_DOMAIN)? {
        occurrences.extend(expand_event(&event, start, end, timezone)?);
    }
    occurrences.sort_by(|left, right| {
        left.get("start")
            .and_then(Value::as_str)
            .cmp(&right.get("start").and_then(Value::as_str))
    });
    Ok(Json(json!({
        "occurrences": occurrences,
        "revision": revision(&state)?
    })))
}

async fn create_event(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let _guard = CALENDAR_WRITE_LOCK.lock();
    let event = normalize_event(&state, body, None)?;
    let id = event
        .get("id")
        .and_then(Value::as_str)
        .expect("规范化事件必须包含 id")
        .to_string();
    state.database.put_resource(EVENT_DOMAIN, &id, &event)?;
    let revision = bump_revision(&state)?;
    Ok(Json(json!({"event": event, "revision": revision})))
}

async fn update_event(
    ScopedState(state): ScopedState,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let _guard = CALENDAR_WRITE_LOCK.lock();
    let current = state
        .database
        .get_resource(EVENT_DOMAIN, &id)?
        .ok_or_else(|| ApiError::not_found("日历事件不存在"))?;
    let changes = body.get("changes").cloned().unwrap_or_else(|| json!({}));
    let scope = body.get("scope").and_then(Value::as_str).unwrap_or("all");
    if scope == "this" {
        let occurrence = body
            .get("occurrence")
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::bad_request("修改单次事件时必须提供 occurrence"))?;
        let mut override_event = merged_event(&current, &changes)?;
        override_event["id"] = Value::String(Uuid::new_v4().to_string());
        override_event["series_id"] = Value::String(id);
        override_event["recurrence_id"] = Value::String(occurrence.to_string());
        override_event["rrule"] = Value::Null;
        let override_id = override_event["id"]
            .as_str()
            .expect("覆盖事件必须有 id")
            .to_string();
        state
            .database
            .put_resource(EVENT_DOMAIN, &override_id, &override_event)?;
    } else {
        let updated = merged_event(&current, &changes)?;
        state.database.put_resource(EVENT_DOMAIN, &id, &updated)?;
    }
    let revision = bump_revision(&state)?;
    Ok(Json(json!({"revision": revision})))
}

async fn delete_event(
    ScopedState(state): ScopedState,
    Path(id): Path<String>,
    Query(query): Query<DeleteQuery>,
) -> ApiResult<Json<Value>> {
    let _guard = CALENDAR_WRITE_LOCK.lock();
    let mut event = state
        .database
        .get_resource(EVENT_DOMAIN, &id)?
        .ok_or_else(|| ApiError::not_found("日历事件不存在"))?;
    let token = Uuid::new_v4().to_string();
    let undo = json!({
        "id": token,
        "kind": "restore_event",
        "event_id": id,
        "event": event.clone(),
        "created_at": iso_now()
    });
    state.database.put_resource(UNDO_DOMAIN, &token, &undo)?;

    if query.scope.as_deref() == Some("this") {
        if let Some(occurrence) = query.occurrence.as_deref() {
            let mut exdates = event
                .get("exdates")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if !exdates
                .iter()
                .any(|value| value.as_str() == Some(occurrence))
            {
                exdates.push(Value::String(occurrence.to_string()));
            }
            event["exdates"] = Value::Array(exdates);
            event["updated_at"] = Value::String(iso_now());
            state.database.put_resource(EVENT_DOMAIN, &id, &event)?;
        }
    } else {
        state.database.delete_resource(EVENT_DOMAIN, &id)?;
    }
    let revision = bump_revision(&state)?;
    Ok(Json(json!({"revision": revision, "undo_token": token})))
}

async fn undo(
    ScopedState(state): ScopedState,
    Path(token): Path<String>,
) -> ApiResult<Json<Value>> {
    let _guard = CALENDAR_WRITE_LOCK.lock();
    let undo = state
        .database
        .get_resource(UNDO_DOMAIN, &token)?
        .ok_or_else(|| ApiError::not_found("撤销记录不存在或已失效"))?;
    let event = undo
        .get("event")
        .cloned()
        .ok_or_else(|| ApiError::internal("撤销记录缺少事件快照"))?;
    let event_id = event
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::internal("撤销事件缺少 id"))?;
    state
        .database
        .put_resource(EVENT_DOMAIN, event_id, &event)?;
    state.database.delete_resource(UNDO_DOMAIN, &token)?;
    let revision = bump_revision(&state)?;
    Ok(Json(json!({"revision": revision})))
}

async fn search(
    ScopedState(state): ScopedState,
    Query(query): Query<SearchQuery>,
) -> ApiResult<Json<Value>> {
    let needle = query.q.trim().to_lowercase();
    if needle.is_empty() || needle.chars().count() > 200 {
        return Err(ApiError::bad_request(
            "搜索词长度必须在 1 到 200 个字符之间",
        ));
    }
    let limit = query.limit.unwrap_or(50).clamp(1, 100);
    let events = state
        .database
        .list_resources(EVENT_DOMAIN)?
        .into_iter()
        .filter(|event| {
            ["title", "notes", "location"].iter().any(|key| {
                event
                    .get(*key)
                    .and_then(Value::as_str)
                    .is_some_and(|value| value.to_lowercase().contains(&needle))
            })
        })
        .take(limit)
        .map(|mut event| {
            event["next_start"] = Value::String(
                event
                    .get("start")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            );
            event
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({"events": events})))
}

async fn layers(
    ScopedState(state): ScopedState,
    Query(query): Query<RangeQuery>,
) -> ApiResult<Json<Value>> {
    let (start, end, timezone) = parse_bounds(&query)?;
    let automations = state
        .database
        .list_resources(AUTOMATION_DOMAIN)?
        .into_iter()
        .filter(|automation| {
            automation
                .get("at")
                .and_then(Value::as_str)
                .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                .is_some_and(|at| at >= start && at < end)
        })
        .collect::<Vec<_>>();
    let reviews = state
        .database
        .list_resources("calendar_review_layer")?
        .into_iter()
        .filter(|review| {
            review
                .get("date")
                .and_then(Value::as_str)
                .and_then(|date| NaiveDate::parse_from_str(date, "%Y-%m-%d").ok())
                .is_some_and(|date| {
                    date >= start.with_timezone(&timezone).date_naive()
                        && date < end.with_timezone(&timezone).date_naive()
                })
        })
        .collect::<Vec<_>>();
    let footprints = state
        .database
        .list_resources("calendar_footprint_layer")?
        .into_iter()
        .filter(|footprint| {
            footprint
                .get("date")
                .and_then(Value::as_str)
                .and_then(|date| NaiveDate::parse_from_str(date, "%Y-%m-%d").ok())
                .is_some_and(|date| {
                    date >= start.with_timezone(&timezone).date_naive()
                        && date < end.with_timezone(&timezone).date_naive()
                })
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({
        "workspace_id": "",
        "reviews": reviews,
        "automations": automations,
        "footprints": footprints
    })))
}

async fn stream(
    ScopedState(state): ScopedState,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let stream = async_stream::stream! {
        loop {
            let current = revision(&state).unwrap_or(0);
            yield Ok::<Event, Infallible>(
                Event::default()
                    .id(current.to_string())
                    .data(json!({"revision": current}).to_string())
            );
            tokio::time::sleep(StdDuration::from_secs(1)).await;
        }
    };
    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(StdDuration::from_secs(15))
            .text("heartbeat"),
    )
}

async fn task_blocks(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    let mut counts = Map::new();
    for block in state.database.list_resources("task_block")? {
        let key = block
            .get("date")
            .or_else(|| block.get("workspace_id"))
            .and_then(Value::as_str)
            .unwrap_or("unscheduled")
            .to_string();
        let count = counts.get(&key).and_then(Value::as_u64).unwrap_or(0) + 1;
        counts.insert(key, Value::from(count));
    }
    Ok(Json(Value::Object(counts)))
}

async fn cancel_automation(
    ScopedState(state): ScopedState,
    Path(job_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let existed = state
        .database
        .get_resource(AUTOMATION_DOMAIN, &job_id)?
        .is_some();
    if !existed {
        return Err(ApiError::not_found("自动化任务不存在"));
    }
    state.database.delete_resource(AUTOMATION_DOMAIN, &job_id)?;
    Ok(Json(json!({"ok": true})))
}

fn ensure_default_calendar(state: &AppState) -> ApiResult<()> {
    if state.database.list_resources(CALENDAR_DOMAIN)?.is_empty() {
        let now = iso_now();
        state.database.put_resource(
            CALENDAR_DOMAIN,
            "default",
            &json!({
                "id": "default",
                "name": "",
                "color": "#4f7cff",
                "visible": true,
                "position": 0,
                "is_default": true,
                "created_at": now,
                "updated_at": now
            }),
        )?;
    }
    Ok(())
}

fn revision(state: &AppState) -> ApiResult<i64> {
    Ok(state
        .database
        .get_setting(REVISION_KEY)?
        .and_then(|value| value.as_i64())
        .unwrap_or(0))
}

fn bump_revision(state: &AppState) -> ApiResult<i64> {
    let next = revision(state)?.saturating_add(1);
    state
        .database
        .set_setting(REVISION_KEY, &Value::from(next))?;
    Ok(next)
}

fn parse_bounds(query: &RangeQuery) -> ApiResult<(DateTime<Utc>, DateTime<Utc>, Tz)> {
    let timezone = query
        .tz
        .parse::<Tz>()
        .map_err(|_| ApiError::bad_request("未知时区"))?;
    let start = parse_instant(&query.start, timezone, false)?;
    let end = parse_instant(&query.end, timezone, false)?;
    if start >= end || end - start > Duration::days(400) {
        return Err(ApiError::bad_request("日期范围必须在 1 秒到 400 天之间"));
    }
    Ok((start, end, timezone))
}

fn parse_instant(value: &str, timezone: Tz, all_day: bool) -> ApiResult<DateTime<Utc>> {
    if all_day || value.len() == 10 {
        let date = NaiveDate::parse_from_str(value, "%Y-%m-%d")
            .map_err(|_| ApiError::bad_request("日期必须使用 YYYY-MM-DD 格式"))?;
        timezone
            .from_local_datetime(&date.and_hms_opt(0, 0, 0).expect("零点合法"))
            .single()
            .map(|value| value.with_timezone(&Utc))
            .ok_or_else(|| ApiError::bad_request("该日期在目标时区不存在"))
    } else {
        DateTime::parse_from_rfc3339(value)
            .map(|value| value.with_timezone(&Utc))
            .map_err(|_| ApiError::bad_request("时间必须使用 RFC3339 格式"))
    }
}

fn normalize_event(state: &AppState, body: Value, existing: Option<&Value>) -> ApiResult<Value> {
    ensure_default_calendar(state)?;
    let now = iso_now();
    let mut event = existing.cloned().unwrap_or_else(|| {
        json!({
            "id": Uuid::new_v4().to_string(),
            "calendar_id": "default",
            "title": "",
            "notes": "",
            "location": "",
            "all_day": false,
            "tzid": "UTC",
            "rrule": null,
            "reminders": [],
            "color": null,
            "task_id": null,
            "task_workspace_id": null,
            "exdates": [],
            "series_id": null,
            "recurrence_id": null,
            "source": "user",
            "created_at": now,
            "updated_at": now
        })
    });
    if let Some(object) = body.as_object() {
        for (key, value) in object {
            event[key] = value.clone();
        }
    }
    for key in ["start", "end"] {
        if event.get(key).and_then(Value::as_str).is_none() {
            return Err(ApiError::bad_request(format!("事件缺少 {key}")));
        }
    }
    let timezone = event
        .get("tzid")
        .and_then(Value::as_str)
        .unwrap_or("UTC")
        .parse::<Tz>()
        .map_err(|_| ApiError::bad_request("事件时区无效"))?;
    let all_day = event
        .get("all_day")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let start = parse_instant(
        event
            .get("start")
            .and_then(Value::as_str)
            .expect("已检查 start"),
        timezone,
        all_day,
    )?;
    let end = parse_instant(
        event
            .get("end")
            .and_then(Value::as_str)
            .expect("已检查 end"),
        timezone,
        all_day,
    )?;
    if end <= start {
        return Err(ApiError::bad_request("事件结束时间必须晚于开始时间"));
    }
    event["updated_at"] = Value::String(now);
    Ok(event)
}

fn merged_event(current: &Value, changes: &Value) -> ApiResult<Value> {
    let mut merged = current.clone();
    if let Some(object) = changes.as_object() {
        for (key, value) in object {
            if key != "id" && key != "created_at" {
                merged[key] = value.clone();
            }
        }
    }
    merged["updated_at"] = Value::String(iso_now());
    Ok(merged)
}

fn expand_event(
    event: &Value,
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
    query_timezone: Tz,
) -> ApiResult<Vec<Value>> {
    let all_day = event
        .get("all_day")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let event_timezone = event
        .get("tzid")
        .and_then(Value::as_str)
        .unwrap_or("UTC")
        .parse::<Tz>()
        .unwrap_or(query_timezone);
    let start_text = event
        .get("start")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::internal("日历事件缺少开始时间"))?;
    let end_text = event
        .get("end")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::internal("日历事件缺少结束时间"))?;
    let event_start = parse_instant(start_text, event_timezone, all_day)?;
    let event_end = parse_instant(end_text, event_timezone, all_day)?;
    let duration = event_end - event_start;
    let rrule = event
        .get("rrule")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty());
    let exdates = event
        .get("exdates")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(|value| parse_instant(value, event_timezone, all_day).ok())
        .collect::<Vec<_>>();
    let mut starts = Vec::new();
    if let Some(rule) = rrule {
        let rule = rule.trim().trim_start_matches("RRULE:");
        let set = format!(
            "DTSTART:{}\nRRULE:{}",
            event_start.format("%Y%m%dT%H%M%SZ"),
            rule
        );
        let parsed = set
            .parse::<RRuleSet>()
            .map_err(|error| ApiError::bad_request(format!("重复规则无效：{error}")))?;
        let result = parsed
            .after((range_start - duration).with_timezone(&rrule::Tz::UTC))
            .before(range_end.with_timezone(&rrule::Tz::UTC))
            .all(2000);
        starts.extend(
            result
                .dates
                .into_iter()
                .map(|value| value.with_timezone(&Utc)),
        );
    } else {
        starts.push(event_start);
    }
    let series_id = event
        .get("series_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let recurrence_event_id = event
        .get("recurrence_id")
        .and_then(Value::as_str)
        .and_then(|value| parse_instant(value, event_timezone, all_day).ok());
    let mut output = Vec::new();
    for start in starts {
        if exdates.iter().any(|exdate| *exdate == start)
            || recurrence_event_id.is_some_and(|recurrence_id| recurrence_id != start)
        {
            continue;
        }
        let end = start + duration;
        if start >= range_end || end <= range_start {
            continue;
        }
        let mut occurrence = event.clone();
        occurrence["id"] = Value::String(format!(
            "{}:{}",
            event.get("id").and_then(Value::as_str).unwrap_or("event"),
            start.timestamp()
        ));
        occurrence["event_id"] = event
            .get("id")
            .cloned()
            .unwrap_or_else(|| Value::String("event".into()));
        occurrence["series_id"] = series_id.clone().map(Value::String).unwrap_or(Value::Null);
        occurrence["occurrence"] = Value::String(if all_day {
            start
                .with_timezone(&event_timezone)
                .date_naive()
                .format("%Y-%m-%d")
                .to_string()
        } else {
            start.to_rfc3339()
        });
        occurrence["start"] = Value::String(if all_day {
            start
                .with_timezone(&event_timezone)
                .date_naive()
                .format("%Y-%m-%d")
                .to_string()
        } else {
            start.to_rfc3339()
        });
        occurrence["end"] = Value::String(if all_day {
            end.with_timezone(&event_timezone)
                .date_naive()
                .format("%Y-%m-%d")
                .to_string()
        } else {
            end.to_rfc3339()
        });
        occurrence["recurring"] = Value::Bool(rrule.is_some());
        let reminder_minutes = event
            .get("reminders")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_i64)
            .collect::<Vec<_>>();
        occurrence["reminder_times"] = Value::Array(
            reminder_minutes
                .into_iter()
                .map(|minutes| Value::String((start - Duration::minutes(minutes)).to_rfc3339()))
                .collect(),
        );
        output.push(occurrence);
    }
    Ok(output)
}

fn iso_now() -> String {
    Utc::now().to_rfc3339()
}

fn default_timezone() -> String {
    "UTC".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 重复事件会按时区展开() {
        let event = json!({
            "id": "e1",
            "start": "2026-01-01T09:00:00Z",
            "end": "2026-01-01T10:00:00Z",
            "all_day": false,
            "tzid": "UTC",
            "rrule": "FREQ=DAILY;COUNT=3",
            "exdates": []
        });
        let start = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let end = DateTime::parse_from_rfc3339("2026-01-05T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let occurrences = expand_event(&event, start, end, chrono_tz::UTC).unwrap();
        assert_eq!(occurrences.len(), 3);
    }

    #[test]
    fn 无效日期范围会被拒绝() {
        let query = RangeQuery {
            start: "2026-01-02".into(),
            end: "2026-01-01".into(),
            tz: "Asia/Shanghai".into(),
        };
        assert!(parse_bounds(&query).is_err());
    }
}
