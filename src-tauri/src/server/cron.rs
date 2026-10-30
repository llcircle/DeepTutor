use super::{auth, engine, AppState};
use chrono::{
    DateTime, Datelike, Duration as ChronoDuration, LocalResult, NaiveDate, NaiveDateTime,
    TimeZone, Utc,
};
use chrono_tz::Tz;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const JOB_DOMAIN: &str = "desktop_cron_job";
const MAX_JOBS_PER_STATE: usize = 100;
const MAX_MESSAGE_CHARS: usize = 12_000;
const RETRY_AFTER_SECONDS: i64 = 60;
const DEFAULT_TIMEZONE: &str = "Asia/Shanghai";

#[derive(Clone, Debug)]
enum CronSchedule {
    At {
        run_at: DateTime<Utc>,
    },
    Every {
        seconds: u64,
    },
    Expression {
        raw: String,
        timezone: Tz,
        fields: CronFields,
    },
}

#[derive(Clone, Debug)]
struct CronFields {
    minutes: Vec<u32>,
    hours: Vec<u32>,
    days: Vec<u32>,
    months: Vec<u32>,
    weekdays: Vec<u32>,
    day_any: bool,
    weekday_any: bool,
}

/// 执行 cron 工具的 `schedule`、`list`、`cancel` 三个动作。
pub async fn handle(
    state: &AppState,
    session_id: &str,
    language: &str,
    arguments: &Value,
) -> Result<(String, Value), String> {
    let action = string_arg(arguments, "action").to_ascii_lowercase();
    match action.as_str() {
        "schedule" => schedule_job(state, session_id, language, arguments),
        "list" => list_jobs(state, session_id),
        "cancel" => cancel_job(state, session_id, arguments),
        _ => Err("cron action 必须是 schedule、list 或 cancel。".to_string()),
    }
}

fn schedule_job(
    state: &AppState,
    session_id: &str,
    language: &str,
    arguments: &Value,
) -> Result<(String, Value), String> {
    if session_id.trim().is_empty() {
        return Err("当前上下文没有可安排定时任务的会话。".to_string());
    }
    if state.database.get_session(session_id)?.is_none() {
        return Err("要安排定时任务的会话不存在。".to_string());
    }
    let message = string_arg(arguments, "message");
    if message.is_empty() {
        return Err("schedule 需要非空 message。".to_string());
    }
    if message.chars().count() > MAX_MESSAGE_CHARS {
        return Err(format!("定时任务消息不能超过 {MAX_MESSAGE_CHARS} 个字符。"));
    }
    let existing = state.database.list_resources(JOB_DOMAIN)?;
    if existing.len() >= MAX_JOBS_PER_STATE {
        return Err(format!(
            "当前账户最多保留 {MAX_JOBS_PER_STATE} 个定时任务，请先取消不再使用的任务。"
        ));
    }

    let at = optional_string_arg(arguments, "at");
    let every_seconds = arguments.get("every_seconds").and_then(Value::as_u64);
    let expression = optional_string_arg(arguments, "cron_expr");
    let schedule_count = usize::from(at.is_some())
        + usize::from(every_seconds.is_some())
        + usize::from(expression.is_some());
    if schedule_count != 1 {
        return Err("at、every_seconds、cron_expr 必须且只能提供一个。".to_string());
    }

    let timezone_name =
        optional_string_arg(arguments, "tz").unwrap_or_else(|| DEFAULT_TIMEZONE.to_string());
    let timezone = parse_timezone(&timezone_name)?;
    let now = Utc::now();
    let schedule = if let Some(at) = at {
        let run_at = parse_at(&at, timezone)?;
        if run_at <= now {
            return Err("at 必须是未来的 ISO 8601 时间。".to_string());
        }
        CronSchedule::At { run_at }
    } else if let Some(seconds) = every_seconds {
        if !(30..=31_536_000).contains(&seconds) {
            return Err("every_seconds 必须在 30 到 31536000 秒之间。".to_string());
        }
        CronSchedule::Every { seconds }
    } else {
        let raw = expression.expect("已确认 cron_expr 存在");
        CronSchedule::Expression {
            fields: parse_cron_fields(&raw)?,
            raw,
            timezone,
        }
    };
    let next_run = next_run(&schedule, now)?
        .ok_or_else(|| "无法计算下一次执行时间，请检查时间与时区。".to_string())?;
    let job_id = format!("cron_{}", uuid::Uuid::new_v4().simple());
    let name = optional_string_arg(arguments, "name").unwrap_or_else(|| {
        message
            .chars()
            .take(48)
            .collect::<String>()
            .trim()
            .to_string()
    });
    let delete_after_run = arguments
        .get("delete_after_run")
        .and_then(Value::as_bool)
        .unwrap_or(matches!(schedule, CronSchedule::At { .. }));
    let trigger = schedule_json(&schedule);
    let job = json!({
        "id": job_id,
        "name": name,
        "message": message,
        "session_id": session_id,
        "language": if language.trim().is_empty() { "zh" } else { language },
        "capability": "chat",
        "trigger": trigger,
        "timezone": timezone_name,
        "next_run_at": next_run.to_rfc3339(),
        "created_at": state.database.get_session(session_id)?
            .and_then(|_| Some(Utc::now().to_rfc3339()))
            .unwrap_or_else(|| Utc::now().to_rfc3339()),
        "delete_after_run": delete_after_run,
        "enabled": true,
        "attempts": 0,
        "last_status": "scheduled",
        "last_turn_id": Value::Null,
        "last_run_at": Value::Null,
        "last_error": ""
    });
    let stored = state.database.put_resource(JOB_DOMAIN, &job_id, &job)?;
    let text = format!(
        "定时任务已创建。\n- ID：{job_id}\n- 名称：{}\n- 下次执行：{}\n- 时区：{}",
        stored.get("name").and_then(Value::as_str).unwrap_or(""),
        format_datetime(next_run),
        timezone_name
    );
    Ok((
        text,
        json!({
            "kind": "cron_schedule",
            "job": public_job(&stored)
        }),
    ))
}

fn list_jobs(state: &AppState, session_id: &str) -> Result<(String, Value), String> {
    if session_id.trim().is_empty() {
        return Err("当前上下文没有可读取定时任务的会话。".to_string());
    }
    let mut jobs = state
        .database
        .list_resources(JOB_DOMAIN)?
        .into_iter()
        .filter(|job| job.get("session_id").and_then(Value::as_str) == Some(session_id))
        .map(|job| public_job(&job))
        .collect::<Vec<_>>();
    jobs.sort_by(|left, right| {
        left.get("next_run_at")
            .and_then(Value::as_str)
            .cmp(&right.get("next_run_at").and_then(Value::as_str))
    });
    let text = if jobs.is_empty() {
        "当前会话没有定时任务。".to_string()
    } else {
        let mut lines = vec!["当前会话的定时任务：".to_string()];
        for job in &jobs {
            lines.push(format!(
                "- {}（{}）：{}，上次状态：{}",
                job.get("name").and_then(Value::as_str).unwrap_or(""),
                job.get("id").and_then(Value::as_str).unwrap_or(""),
                job.get("next_run_at")
                    .and_then(Value::as_str)
                    .unwrap_or("未安排"),
                job.get("last_status").and_then(Value::as_str).unwrap_or("")
            ));
        }
        lines.join("\n")
    };
    Ok((
        text,
        json!({
            "kind": "cron_list",
            "jobs": jobs
        }),
    ))
}

fn cancel_job(
    state: &AppState,
    session_id: &str,
    arguments: &Value,
) -> Result<(String, Value), String> {
    let job_id = string_arg(arguments, "job_id");
    if job_id.is_empty() {
        return Err("cancel 需要 job_id。".to_string());
    }
    let job = state
        .database
        .get_resource(JOB_DOMAIN, &job_id)?
        .ok_or_else(|| format!("找不到定时任务 {job_id}。"))?;
    if job.get("session_id").and_then(Value::as_str) != Some(session_id) {
        return Err("只能取消当前会话中的定时任务。".to_string());
    }
    state.database.delete_resource(JOB_DOMAIN, &job_id)?;
    Ok((
        format!("定时任务 {job_id} 已取消。"),
        json!({
            "kind": "cron_cancel",
            "job_id": job_id,
            "cancelled": true
        }),
    ))
}

/// 启动一个由应用生命周期管理的调度循环。
pub fn start_scheduler(state: AppState, shutdown: CancellationToken) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(10));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = interval.tick() => {
                    if let Err(error) = sweep_all(&state).await {
                        tracing::warn!(%error, "定时任务扫描失败");
                    }
                }
            }
        }
    })
}

async fn sweep_all(central: &AppState) -> Result<(), String> {
    let mut states = vec![central.clone()];
    if let Ok(accounts) = auth::list_accounts(central) {
        for account in accounts {
            if account.disabled {
                continue;
            }
            if let Ok(state) = central.user_state(&account.id) {
                states.push(state);
            }
        }
    }
    let mut seen = HashSet::new();
    for state in states {
        if !seen.insert(state.database.path().to_path_buf()) {
            continue;
        }
        if let Err(error) = run_due(&state).await {
            tracing::warn!(
                database = %state.database.path().display(),
                %error,
                "扫描账户定时任务失败"
            );
        }
    }
    Ok(())
}

async fn run_due(state: &AppState) -> Result<(), String> {
    if !state.turns_open() {
        return Ok(());
    }
    let now = Utc::now();
    let mut due = state
        .database
        .list_resources(JOB_DOMAIN)?
        .into_iter()
        .filter(|job| job.get("enabled").and_then(Value::as_bool).unwrap_or(true))
        .filter_map(|job| {
            let next = job
                .get("next_run_at")
                .and_then(Value::as_str)
                .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                .map(|value| value.with_timezone(&Utc))?;
            (next <= now).then_some((next, job))
        })
        .collect::<Vec<_>>();
    due.sort_by_key(|(next, _)| *next);
    for (scheduled_at, mut job) in due.into_iter().take(20) {
        let job_id = job
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if job_id.is_empty() {
            continue;
        }
        let session_id = job
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if session_id.is_empty() {
            mark_failure(state, &mut job, "定时任务缺少会话 ID")?;
            continue;
        }
        if state.database.get_active_turn(&session_id)?.is_some() {
            job["last_status"] = Value::String("skipped_active_turn".to_string());
            job["last_error"] = Value::String("会话已有运行中的回合".to_string());
            job["next_run_at"] = Value::String((now + ChronoDuration::seconds(30)).to_rfc3339());
            state.database.put_resource(JOB_DOMAIN, &job_id, &job)?;
            continue;
        }
        let request = json!({
            "session_id": session_id,
            "content": job.get("message").and_then(Value::as_str).unwrap_or(""),
            "capability": job.get("capability").and_then(Value::as_str).unwrap_or("chat"),
            "language": job.get("language").and_then(Value::as_str).unwrap_or("zh"),
            "persist_user_message": true,
            "client_submission_id": format!("cron:{}:{}", job_id, scheduled_at.timestamp()),
            "cron_trigger": true
        });
        match engine::start_turn(state.clone(), request).await {
            Ok(turn) => {
                let turn_id = turn
                    .get("turn_id")
                    .or_else(|| turn.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let delete_after_run = job
                    .get("delete_after_run")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if delete_after_run {
                    state.database.delete_resource(JOB_DOMAIN, &job_id)?;
                    continue;
                }
                let schedule = schedule_from_job(&job)?;
                let next = next_run(&schedule, Utc::now())?
                    .ok_or_else(|| "重复任务无法计算下一次运行时间。".to_string())?;
                job["last_status"] = Value::String("started".to_string());
                job["last_turn_id"] = Value::String(turn_id);
                job["last_run_at"] = Value::String(Utc::now().to_rfc3339());
                job["last_error"] = Value::String(String::new());
                job["next_run_at"] = Value::String(next.to_rfc3339());
                job["attempts"] = Value::from(0);
                state.database.put_resource(JOB_DOMAIN, &job_id, &job)?;
            }
            Err(error) => {
                let attempts = job.get("attempts").and_then(Value::as_u64).unwrap_or(0) + 1;
                job["attempts"] = Value::from(attempts);
                job["last_status"] = Value::String("start_failed".to_string());
                job["last_error"] = Value::String(truncate(&error, 1_000));
                job["last_run_at"] = Value::String(Utc::now().to_rfc3339());
                job["next_run_at"] = Value::String(
                    (Utc::now() + ChronoDuration::seconds(RETRY_AFTER_SECONDS)).to_rfc3339(),
                );
                if attempts >= 5 {
                    job["enabled"] = Value::Bool(false);
                    job["last_status"] = Value::String("paused_after_errors".to_string());
                }
                state.database.put_resource(JOB_DOMAIN, &job_id, &job)?;
            }
        }
    }
    Ok(())
}

fn mark_failure(state: &AppState, job: &mut Value, error: &str) -> Result<(), String> {
    let Some(job_id) = job.get("id").and_then(Value::as_str).map(str::to_string) else {
        return Ok(());
    };
    job["last_status"] = Value::String("invalid_job".to_string());
    job["last_error"] = Value::String(error.to_string());
    job["enabled"] = Value::Bool(false);
    job["next_run_at"] = Value::Null;
    state.database.put_resource(JOB_DOMAIN, &job_id, job)?;
    Ok(())
}

fn schedule_from_job(job: &Value) -> Result<CronSchedule, String> {
    let trigger = job
        .get("trigger")
        .ok_or_else(|| "定时任务缺少 trigger".to_string())?;
    match trigger.get("kind").and_then(Value::as_str) {
        Some("at") => {
            let raw = trigger
                .get("run_at")
                .and_then(Value::as_str)
                .ok_or_else(|| "at 任务缺少 run_at".to_string())?;
            Ok(CronSchedule::At {
                run_at: DateTime::parse_from_rfc3339(raw)
                    .map_err(|error| format!("at 时间无效：{error}"))?
                    .with_timezone(&Utc),
            })
        }
        Some("every") => Ok(CronSchedule::Every {
            seconds: trigger
                .get("seconds")
                .and_then(Value::as_u64)
                .ok_or_else(|| "every 任务缺少 seconds".to_string())?,
        }),
        Some("cron") => {
            let raw = trigger
                .get("expression")
                .and_then(Value::as_str)
                .ok_or_else(|| "cron 任务缺少 expression".to_string())?;
            let timezone = parse_timezone(
                trigger
                    .get("timezone")
                    .and_then(Value::as_str)
                    .unwrap_or(DEFAULT_TIMEZONE),
            )?;
            Ok(CronSchedule::Expression {
                fields: parse_cron_fields(raw)?,
                raw: raw.to_string(),
                timezone,
            })
        }
        _ => Err("定时任务 trigger.kind 无效。".to_string()),
    }
}

fn schedule_json(schedule: &CronSchedule) -> Value {
    match schedule {
        CronSchedule::At { run_at } => {
            json!({"kind": "at", "run_at": run_at.to_rfc3339()})
        }
        CronSchedule::Every { seconds } => {
            json!({"kind": "every", "seconds": seconds})
        }
        CronSchedule::Expression { raw, timezone, .. } => json!({
            "kind": "cron",
            "expression": raw,
            "timezone": timezone.name()
        }),
    }
}

fn next_run(
    schedule: &CronSchedule,
    after: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>, String> {
    match schedule {
        CronSchedule::At { run_at } => Ok((*run_at > after).then_some(*run_at)),
        CronSchedule::Every { seconds } => Ok(Some(
            after + ChronoDuration::seconds((*seconds).min(i64::MAX as u64) as i64),
        )),
        CronSchedule::Expression {
            fields, timezone, ..
        } => next_expression_run(fields, *timezone, after).map(Some),
    }
}

fn next_expression_run(
    fields: &CronFields,
    timezone: Tz,
    after: DateTime<Utc>,
) -> Result<DateTime<Utc>, String> {
    let local = after.with_timezone(&timezone);
    let start_date = local.date_naive();
    let mut year = start_date.year();
    let mut month = start_date.month();
    for _ in 0..=120 {
        if fields.months.contains(&month) {
            let days_in_month = days_in_month(year, month);
            for day in 1..=days_in_month {
                let Some(date) = NaiveDate::from_ymd_opt(year, month, day) else {
                    continue;
                };
                if date < start_date {
                    continue;
                }
                let weekday = date.weekday().num_days_from_sunday();
                let day_matches = fields.days.contains(&day);
                let weekday_matches = fields.weekdays.contains(&weekday);
                let date_matches = if !fields.day_any && !fields.weekday_any {
                    day_matches || weekday_matches
                } else {
                    day_matches && weekday_matches
                };
                if !date_matches {
                    continue;
                }
                for hour in &fields.hours {
                    for minute in &fields.minutes {
                        let Some(naive) = date.and_hms_opt(*hour, *minute, 0) else {
                            continue;
                        };
                        let mut candidates = Vec::new();
                        match timezone.from_local_datetime(&naive) {
                            LocalResult::Single(value) => candidates.push(value),
                            LocalResult::Ambiguous(first, second) => {
                                candidates.push(first);
                                candidates.push(second);
                            }
                            LocalResult::None => {}
                        }
                        candidates.sort_by_key(|value| value.with_timezone(&Utc));
                        if let Some(candidate) = candidates
                            .into_iter()
                            .map(|value| value.with_timezone(&Utc))
                            .find(|value| *value > after)
                        {
                            return Ok(candidate);
                        }
                    }
                }
            }
        }
        if month == 12 {
            year += 1;
            month = 1;
        } else {
            month += 1;
        }
    }
    Err("未来十年内没有符合 cron 表达式的时间。".to_string())
}

fn parse_cron_fields(raw: &str) -> Result<CronFields, String> {
    let fields = raw.split_whitespace().collect::<Vec<_>>();
    if fields.len() != 5 {
        return Err("cron_expr 必须是五段表达式：分 时 日 月 周。".to_string());
    }
    Ok(CronFields {
        minutes: parse_cron_field(fields[0], 0, 59, false)?,
        hours: parse_cron_field(fields[1], 0, 23, false)?,
        days: parse_cron_field(fields[2], 1, 31, false)?,
        months: parse_cron_field(fields[3], 1, 12, false)?,
        weekdays: parse_cron_field(fields[4], 0, 6, true)?,
        day_any: field_covers_range(fields[2], 1, 31, false)?,
        weekday_any: field_covers_range(fields[4], 0, 6, true)?,
    })
}

fn parse_cron_field(
    raw: &str,
    minimum: u32,
    maximum: u32,
    sunday_seven: bool,
) -> Result<Vec<u32>, String> {
    if raw.is_empty() {
        return Err("cron 字段不能为空。".to_string());
    }
    let effective_max = if sunday_seven { 7 } else { maximum };
    let mut values = Vec::new();
    for component in raw.split(',') {
        if component.is_empty() {
            return Err(format!("cron 字段 `{raw}` 含有空列表项。"));
        }
        let (base, step) = match component.split_once('/') {
            Some((base, step)) => {
                let step = step
                    .parse::<u32>()
                    .map_err(|_| format!("cron 步长 `{step}` 无效。"))?;
                if step == 0 {
                    return Err("cron 步长必须大于 0。".to_string());
                }
                (base, step)
            }
            None => (component, 1),
        };
        let (start, end) = if base == "*" {
            (minimum, effective_max)
        } else if let Some((start, end)) = base.split_once('-') {
            let start = parse_cron_number(start)?;
            let end = parse_cron_number(end)?;
            (start, end)
        } else {
            let value = parse_cron_number(base)?;
            (value, value)
        };
        if start < minimum || end > effective_max || start > end {
            return Err(format!(
                "cron 字段 `{component}` 超出范围 {minimum}-{effective_max}。"
            ));
        }
        let mut value = start;
        while value <= end {
            values.push(if sunday_seven && value == 7 { 0 } else { value });
            value = match value.checked_add(step) {
                Some(next) => next,
                None => break,
            };
        }
    }
    values.sort_unstable();
    values.dedup();
    if values.is_empty() {
        return Err(format!("cron 字段 `{raw}` 没有有效取值。"));
    }
    Ok(values)
}

fn parse_cron_number(raw: &str) -> Result<u32, String> {
    raw.parse::<u32>()
        .map_err(|_| format!("cron 数值 `{raw}` 无效。"))
}

fn field_covers_range(
    raw: &str,
    minimum: u32,
    maximum: u32,
    sunday_seven: bool,
) -> Result<bool, String> {
    let values = parse_cron_field(raw, minimum, maximum, sunday_seven)?;
    let expected = (maximum - minimum + 1) as usize;
    Ok(values.len() == expected
        && (minimum..=maximum).all(|value| values.binary_search(&value).is_ok()))
}

fn parse_timezone(raw: &str) -> Result<Tz, String> {
    raw.trim()
        .parse::<Tz>()
        .map_err(|_| format!("未知 IANA 时区：{raw}。"))
}

fn parse_at(raw: &str, timezone: Tz) -> Result<DateTime<Utc>, String> {
    if let Ok(value) = DateTime::parse_from_rfc3339(raw) {
        return Ok(value.with_timezone(&Utc));
    }
    let naive = NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M"))
        .or_else(|_| NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S"))
        .map_err(|_| "at 必须是 ISO 8601 时间或 YYYY-MM-DD HH:MM 本地时间。".to_string())?;
    match timezone.from_local_datetime(&naive) {
        LocalResult::Single(value) => Ok(value.with_timezone(&Utc)),
        LocalResult::Ambiguous(first, _) => Ok(first.with_timezone(&Utc)),
        LocalResult::None => Err("at 落在夏令时跳变区间，无法确定实际时间。".to_string()),
    }
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let next = if month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1)
    }
    .expect("月份范围有效");
    next.pred_opt().map(|date| date.day()).unwrap_or(31)
}

fn public_job(job: &Value) -> Value {
    json!({
        "id": job.get("id"),
        "name": job.get("name"),
        "session_id": job.get("session_id"),
        "trigger": job.get("trigger"),
        "timezone": job.get("timezone"),
        "next_run_at": job.get("next_run_at"),
        "delete_after_run": job.get("delete_after_run"),
        "enabled": job.get("enabled"),
        "attempts": job.get("attempts"),
        "last_status": job.get("last_status"),
        "last_turn_id": job.get("last_turn_id"),
        "last_run_at": job.get("last_run_at"),
        "last_error": job.get("last_error"),
        "created_at": job.get("created_at"),
        "updated_at": job.get("updated_at")
    })
}

fn format_datetime(value: DateTime<Utc>) -> String {
    value.to_rfc3339()
}

fn string_arg(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn optional_string_arg(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn truncate(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn five_field_cron_supports_ranges_lists_and_steps() {
        let fields = parse_cron_fields("*/15 9-18 1,15 * 1-5").expect("cron 有效");
        assert_eq!(fields.minutes, vec![0, 15, 30, 45]);
        assert_eq!(fields.hours, (9..=18).collect::<Vec<_>>());
        assert_eq!(fields.days, vec![1, 15]);
        assert_eq!(fields.weekdays, vec![1, 2, 3, 4, 5]);
        assert!(!fields.day_any);
        assert!(!fields.weekday_any);
    }

    #[test]
    fn next_cron_time_respects_timezone() {
        let fields = parse_cron_fields("0 9 * * 1-5").expect("cron 有效");
        let after = DateTime::parse_from_rfc3339("2026-10-30T02:00:00Z")
            .expect("时间有效")
            .with_timezone(&Utc);
        let next =
            next_expression_run(&fields, chrono_tz::Asia::Shanghai, after).expect("存在下次时间");
        assert_eq!(next.to_rfc3339(), "2026-11-02T01:00:00+00:00");
    }

    #[test]
    fn one_time_schedule_does_not_repeat() {
        let at = Utc::now() + ChronoDuration::minutes(5);
        let schedule = CronSchedule::At { run_at: at };
        assert_eq!(next_run(&schedule, Utc::now()).expect("可计算"), Some(at));
        assert_eq!(
            next_run(&schedule, at + ChronoDuration::seconds(1)).expect("可计算"),
            None
        );
    }

    #[test]
    fn invalid_step_is_rejected() {
        assert!(parse_cron_fields("*/0 * * * *").is_err());
    }

    #[test]
    fn month_end_is_handled() {
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(2026, 2), 28);
    }
}
