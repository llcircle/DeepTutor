use super::{ApiError, ApiResult, AppState};
use axum::extract::Query;
use chrono::{Datelike, Days, TimeZone, Utc};
use chrono_tz::Tz;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};

#[derive(Default, Deserialize)]
pub struct UsageQuery {
    year: Option<i32>,
    timezone: Option<String>,
}

fn number(value: &Value, key: &str) -> f64 {
    value
        .get(key)
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or(0.0)
}

#[derive(Default)]
struct Totals(BTreeMap<String, f64>);

impl Totals {
    fn add(&mut self, value: &Value) {
        for key in [
            "prompt_tokens",
            "completion_tokens",
            "total_tokens",
            "total_calls",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
            "cache_input_tokens",
            "cache_reported_calls",
            "duration_seconds",
            "estimated_calls",
            "ttft_calls",
            "generation_seconds",
            "timed_completion_tokens",
        ] {
            *self.0.entry(key.into()).or_default() += number(value, key);
        }
        *self.0.entry("ttft_total".into()).or_default() +=
            number(value, "ttft_seconds") * number(value, "ttft_calls");
    }

    fn result(&self) -> Value {
        let n = |key: &str| *self.0.get(key).unwrap_or(&0.0);
        let mut result = json!({});
        for key in [
            "prompt_tokens",
            "completion_tokens",
            "total_tokens",
            "total_calls",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
            "cache_input_tokens",
            "cache_reported_calls",
            "estimated_calls",
        ] {
            result[key] = json!(n(key) as u64);
        }
        result["duration_seconds"] = json!(n("duration_seconds"));
        for (key, numerator, denominator) in [
            (
                "cache_hit_rate",
                "cache_read_input_tokens",
                "cache_input_tokens",
            ),
            ("ttft_seconds", "ttft_total", "ttft_calls"),
            (
                "tokens_per_second",
                "timed_completion_tokens",
                "generation_seconds",
            ),
        ] {
            result[key] = if n(denominator) > 0.0 {
                json!(n(numerator) / n(denominator))
            } else {
                Value::Null
            };
        }
        result
    }
}

fn normalized_summary(raw: &Value) -> Value {
    let mut summary = raw.clone();
    if !summary.is_object() {
        return json!({});
    }
    if summary.get("total_tokens").is_none() {
        summary["total_tokens"] =
            json!(number(raw, "prompt_tokens") + number(raw, "completion_tokens"));
    }
    if summary.get("total_calls").is_none() {
        summary["total_calls"] = json!(raw.get("calls").and_then(Value::as_u64).unwrap_or(1));
    }
    summary
}

fn summaries(message: &Value) -> Vec<Value> {
    for pointer in ["/metadata/usage_summary", "/metadata/cost_summary"] {
        if let Some(value) = message.pointer(pointer).filter(|v| v.is_object()) {
            return vec![normalized_summary(value)];
        }
    }
    // result 是中间快照，done 覆盖同一回合；不得重复累加两个事件。
    let mut result = Vec::new();
    let mut pending = None;
    for event in message
        .get("events")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match event.get("type").and_then(Value::as_str).unwrap_or("") {
            "result" => {
                pending = event
                    .pointer("/metadata/metadata/usage_summary")
                    .or_else(|| event.pointer("/metadata/metadata/cost_summary"))
                    .cloned();
            }
            "done" => {
                if let Some(value) = event.pointer("/metadata/usage_summary") {
                    pending = Some(value.clone());
                }
                if let Some(value) = pending.take().filter(Value::is_object) {
                    result.push(normalized_summary(&value));
                }
            }
            _ => {}
        }
    }
    if let Some(value) = pending.filter(Value::is_object) {
        result.push(normalized_summary(&value));
    }
    result
}

pub fn aggregate(records: &[Value], year: i32, zone: Tz) -> Value {
    let mut days = BTreeMap::new();
    let mut date = chrono::NaiveDate::from_ymd_opt(year, 1, 1).unwrap();
    while date.year() == year {
        let key = date.to_string();
        days.insert(key.clone(), json!({"date": key, "total_tokens": 0, "total_calls": 0, "turns": 0, "tracked_turns": 0}));
        date = date.checked_add_days(Days::new(1)).unwrap();
    }
    let mut totals = Totals::default();
    let mut models: BTreeMap<(String, String), Totals> = BTreeMap::new();
    let mut sessions = HashSet::new();
    let mut activities = HashSet::new();
    let mut tracked = HashSet::new();
    let mut daily_activities = HashSet::new();
    let mut daily_tracked = HashSet::new();
    for message in records {
        let timestamp = number(message, "created_at");
        let Some(stamp) = Utc.timestamp_opt(timestamp as i64, 0).single() else {
            continue;
        };
        let local = stamp.with_timezone(&zone);
        if local.year() != year {
            continue;
        }
        let key = local.date_naive().to_string();
        let daily = days.get_mut(&key).unwrap();
        let session = message["session_id"].as_str().unwrap_or("").to_string();
        let activity = format!(
            "{session}:{}",
            message.get("activity_id").unwrap_or(&message["id"])
        );
        if !session.is_empty() {
            sessions.insert(session);
        }
        activities.insert(activity.clone());
        if daily_activities.insert((key.clone(), activity.clone())) {
            daily["turns"] = json!(daily["turns"].as_u64().unwrap_or(0) + 1);
        }
        if message["role"] != "assistant" {
            continue;
        }
        let summaries = summaries(message);
        if summaries
            .iter()
            .any(|value| number(value, "total_tokens") > 0.0)
        {
            tracked.insert(activity.clone());
            if daily_tracked.insert((key.clone(), activity)) {
                daily["tracked_turns"] = json!(daily["tracked_turns"].as_u64().unwrap_or(0) + 1);
            }
        }
        for summary in summaries {
            totals.add(&summary);
            for key in ["total_tokens", "total_calls"] {
                daily[key] = json!(daily[key].as_u64().unwrap_or(0) + number(&summary, key) as u64);
            }
            if let Some(calls) = summary
                .get("call_details")
                .and_then(Value::as_array)
                .filter(|v| !v.is_empty())
            {
                for call in calls.iter().filter(|v| v.is_object()) {
                    let mut value = normalized_summary(call);
                    value["total_calls"] = json!(1);
                    if call
                        .get("cache_read_input_tokens")
                        .is_some_and(|v| !v.is_null())
                    {
                        value["cache_reported_calls"] = json!(1);
                        value["cache_input_tokens"] = json!(number(call, "prompt_tokens"));
                    }
                    if call.get("ttft_seconds").is_some_and(Value::is_number) {
                        value["ttft_calls"] = json!(1);
                    }
                    models.entry(model_key(call)).or_default().add(&value);
                }
            } else if let Some(by_model) = summary
                .get("by_model")
                .and_then(Value::as_object)
                .filter(|v| !v.is_empty())
            {
                for (model, value) in by_model {
                    models
                        .entry((String::new(), model.clone()))
                        .or_default()
                        .add(&normalized_summary(value));
                }
            } else {
                models.entry(model_key(&summary)).or_default().add(&summary);
            }
        }
    }
    let mut models = models
        .into_iter()
        .map(|((provider, model), totals)| {
            let mut value = totals.result();
            value["provider"] = json!(provider);
            value["model"] = json!(model);
            value
        })
        .collect::<Vec<_>>();
    models.sort_by_key(|value| std::cmp::Reverse(value["total_tokens"].as_u64().unwrap_or(0)));
    json!({
        "year": year, "timezone": zone.name(), "totals": totals.result(),
        "active_days": days.values().filter(|day| day["turns"].as_u64().unwrap_or(0) > 0).count(),
        "days": days.into_values().collect::<Vec<_>>(), "models": models,
        "sessions": sessions.len(), "turns": activities.len(), "tracked_turns": tracked.len(),
        "updated_at": crate::storage::now()
    })
}

fn model_key(value: &Value) -> (String, String) {
    (
        value["provider"].as_str().unwrap_or("").into(),
        value["model"].as_str().unwrap_or("").into(),
    )
}

pub async fn get(
    super::ScopedState(state): super::ScopedState,
    Query(query): Query<UsageQuery>,
) -> ApiResult<axum::Json<Value>> {
    let year = query.year.unwrap_or_else(|| Utc::now().year());
    if !(1970..=9998).contains(&year) {
        return Err(ApiError::bad_request("统计年份必须在 1970 至 9998 之间。"));
    }
    let zone: Tz = query
        .timezone
        .as_deref()
        .unwrap_or("UTC")
        .parse()
        .map_err(|_| ApiError::bad_request("统计时区无效。"))?;
    let start = zone
        .with_ymd_and_hms(year, 1, 1, 0, 0, 0)
        .earliest()
        .ok_or_else(|| ApiError::bad_request("统计起始日期无效。"))?
        .timestamp() as f64;
    let end = zone
        .with_ymd_and_hms(year + 1, 1, 1, 0, 0, 0)
        .earliest()
        .ok_or_else(|| ApiError::bad_request("统计结束日期无效。"))?
        .timestamp() as f64;
    let value = tokio::task::spawn_blocking(move || {
        state
            .database
            .usage_records(start, end)
            .map(|records| aggregate(&records, year, zone))
    })
    .await
    .map_err(|e| ApiError::internal(format!("统计任务失败：{e}")))??;
    Ok(axum::Json(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leap_year_timezone_and_event_snapshots_are_aggregated_once() {
        let usage = json!({"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15, "total_calls": 2});
        let records = vec![json!({
            "id": 1, "activity_id": 1, "session_id": "s", "role": "assistant",
            "created_at": Utc.with_ymd_and_hms(2024, 2, 28, 18, 0, 0).unwrap().timestamp(),
            "events": [
                {"type": "result", "metadata": {"metadata": {"usage_summary": usage}}},
                {"type": "done", "metadata": {"usage_summary": usage}}
            ]
        })];
        let value = aggregate(&records, 2024, chrono_tz::Asia::Shanghai);
        assert_eq!(value["days"].as_array().unwrap().len(), 366);
        assert_eq!(value["totals"]["total_tokens"], 15);
        assert_eq!(value["totals"]["total_calls"], 2);
        assert_eq!(value["days"][59]["date"], "2024-02-29");
        assert_eq!(value["days"][59]["turns"], 1);
        assert_eq!(value["models"][0]["model"], "");
    }

    #[test]
    fn activity_and_usage_are_separate_and_metadata_wins() {
        let stamp = Utc
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .unwrap()
            .timestamp();
        let rows = vec![
            json!({"id": 1, "activity_id": 1, "role": "user", "created_at": stamp}),
            json!({"id": 2, "activity_id": 1, "role": "assistant", "created_at": stamp,
                "metadata": {"usage_summary": {"total_tokens": 20, "model": "m", "provider": "local"}},
                "events": [{"type": "done", "metadata": {"usage_summary": {"total_tokens": 99}}}]}),
            json!({"id": 3, "role": "user", "created_at": stamp}),
        ];
        let value = aggregate(&rows, 2026, chrono_tz::UTC);
        assert_eq!(value["turns"], 2);
        assert_eq!(value["tracked_turns"], 1);
        assert_eq!(value["totals"]["total_tokens"], 20);
        assert_eq!(value["models"][0]["model"], "m");
        assert_eq!(value["totals"]["cache_hit_rate"], Value::Null);
    }
}
