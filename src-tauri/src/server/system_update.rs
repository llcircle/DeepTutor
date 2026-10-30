use super::auth::{require_admin, Principal};
use super::AppState;
use crate::update::{
    self, installation_info, select_release_assets, update_available, ReleaseCache, ReleaseInfo,
    UpdatePaths,
};
use axum::extract::{Extension, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Debug, Deserialize)]
pub(super) struct ManagedUpdateRequest {
    confirmation: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct UpdateSettingsRequest {
    enabled: bool,
}

pub(super) async fn get_status(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
) -> Response {
    match checked_status(&state, &principal, false).await {
        Ok(body) => json_no_store(StatusCode::OK, body),
        Err(error) => json_no_store(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"detail": error.message}),
        ),
    }
}

pub(super) async fn check_for_update(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
) -> Response {
    if let Err(error) = require_admin(&principal) {
        return error.into_response();
    }
    match checked_status(&state, &principal, true).await {
        Ok(body) => json_no_store(StatusCode::OK, body),
        Err(error) => error.into_response(),
    }
}

pub(super) async fn update_settings(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Json(body): Json<UpdateSettingsRequest>,
) -> Response {
    if let Err(error) = require_admin(&principal) {
        return error.into_response();
    }
    let patch = json!({
        "enabled": body.enabled,
        "version_check_enabled": body.enabled
    });
    if let Err(error) = state.database.merge_setting("update", &patch) {
        return super::ApiError::internal(error).into_response();
    }
    match checked_status(&state, &principal, false).await {
        Ok(body) => json_no_store(StatusCode::OK, body),
        Err(error) => error.into_response(),
    }
}

pub(super) async fn get_job(State(state): State<AppState>) -> Response {
    let paths = UpdatePaths::new(&state.paths);
    match paths.load_job() {
        Ok(Some(job)) => json_no_store(StatusCode::OK, job.public_payload()),
        Ok(None) => json_no_store(StatusCode::OK, Value::Null),
        Err(error) => super::ApiError::internal(error).into_response(),
    }
}

pub(super) async fn request_update(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Json(body): Json<ManagedUpdateRequest>,
) -> Response {
    if let Err(error) = require_admin(&principal) {
        return error.into_response();
    }
    if body.confirmation != "update-and-restart" {
        return super::ApiError::bad_request("更新请求必须明确确认 update-and-restart")
            .into_response();
    }
    if !checks_enabled(&state) {
        return super::ApiError::conflict("版本检查已关闭").into_response();
    }
    let installation = installation_info(&state.paths);
    if !installation.automatic_update || !state.update_can_restart() {
        return super::ApiError::conflict(
            "当前运行方式不支持安全的便携版自更新，请手动覆盖官方便携包。",
        )
        .into_response();
    }

    let release = match resolve_release(&state, false).await {
        Ok((release, _cache, _cached)) => release,
        Err(error) => return error.into_response(),
    };
    if !update_available(&release) {
        return super::ApiError::conflict("当前已是最新稳定版本").into_response();
    }
    let (archive, checksum) = match select_release_assets(&release) {
        Ok(assets) => assets,
        Err(message) => return super::ApiError::conflict(message).into_response(),
    };
    let _ = update::reconcile_startup(&state.paths);
    if !state.begin_update_maintenance() {
        return super::ApiError::conflict("另一个更新任务或维护操作正在进行").into_response();
    }
    let active_turns = match state.has_active_turns_anywhere() {
        Ok(active) => active,
        Err(error) => {
            state.end_update_maintenance();
            return super::ApiError::internal(error).into_response();
        }
    };
    if active_turns {
        state.end_update_maintenance();
        return super::ApiError::conflict("请先结束所有正在运行的对话再更新").into_response();
    }
    let paths = UpdatePaths::new(&state.paths);
    let job = match update::create_job(&paths, &release, archive, checksum) {
        Ok(job) => job,
        Err(error) => {
            state.end_update_maintenance();
            return super::ApiError::conflict(error).into_response();
        }
    };
    let payload = job.public_payload();
    let task_state = state.clone();
    let job_id = job.id;
    tokio::spawn(async move {
        match update::prepare_and_handoff(&task_state.paths, &job_id).await {
            Ok(()) => {
                tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
                if !task_state.request_update_restart() {
                    update::mark_job_failed(
                        &UpdatePaths::new(&task_state.paths),
                        &job_id,
                        "当前 Tauri 宿主无法安全重启，更新已撤销",
                    );
                    task_state.end_update_maintenance();
                }
            }
            Err(_) => {
                task_state.end_update_maintenance();
            }
        }
    });
    json_no_store(StatusCode::ACCEPTED, payload)
}

async fn checked_status(
    state: &AppState,
    principal: &Principal,
    force: bool,
) -> super::ApiResult<Value> {
    let enabled = checks_enabled(state);
    let paths = UpdatePaths::new(&state.paths);
    let mut check_error = String::new();
    let mut cache = update::release_cache(&paths);
    let mut cached = false;
    if enabled {
        if force
            || cache
                .as_ref()
                .is_none_or(|value| !update::cache_is_fresh(value))
        {
            match resolve_release(state, force).await {
                Ok((_release, resolved, was_cached)) => {
                    cache = Some(resolved);
                    cached = was_cached && !force;
                }
                Err(error) => {
                    if force {
                        return Err(error);
                    }
                    check_error = error.message;
                }
            }
        } else {
            cached = true;
        }
    }
    Ok(status_payload(
        state,
        principal,
        enabled,
        cache.as_ref(),
        cached,
        &check_error,
    ))
}

async fn resolve_release(
    state: &AppState,
    force: bool,
) -> super::ApiResult<(ReleaseInfo, ReleaseCache, bool)> {
    let paths = UpdatePaths::new(&state.paths);
    if !force {
        if let Some(cache) = update::release_cache(&paths) {
            if update::cache_is_fresh(&cache) {
                return Ok((cache.release.clone(), cache, true));
            }
        }
    }
    let client = update::updater_client().map_err(super::ApiError::internal)?;
    let release = update::check_release(&client)
        .await
        .map_err(super::ApiError::provider)?;
    let cache = update::write_release_cache(&paths, &release).map_err(super::ApiError::internal)?;
    Ok((release, cache, false))
}

fn checks_enabled(state: &AppState) -> bool {
    state
        .database
        .get_setting("update")
        .ok()
        .flatten()
        .and_then(|value| {
            value
                .get("version_check_enabled")
                .or_else(|| value.get("enabled"))
                .and_then(Value::as_bool)
        })
        .unwrap_or(true)
}

fn status_payload(
    state: &AppState,
    principal: &Principal,
    enabled: bool,
    cache: Option<&ReleaseCache>,
    cached: bool,
    check_error: &str,
) -> Value {
    let mut installation = installation_info(&state.paths);
    installation.automatic_update &= state.update_can_restart();
    let release = cache.map(|value| &value.release);
    json!({
        "current_version": update::CURRENT_VERSION,
        "check_enabled": enabled,
        "checked_at": cache.map(|value| value.checked_at.as_str()).unwrap_or(""),
        "cached": cached,
        "check_error": check_error,
        "update_available": release.is_some_and(update_available),
        "release": release.map(public_release),
        "installation": installation,
        "launcher_managed": installation.automatic_update,
        "is_admin": principal.is_admin(),
        "job": current_job(state)
    })
}

fn public_release(release: &ReleaseInfo) -> Value {
    json!({
        "version": release.version,
        "name": release.name,
        "published_at": release.published_at,
        "url": release.url,
        "excerpt": release.excerpt,
        "migration_warning": release.migration_warning
    })
}

fn current_job(state: &AppState) -> Value {
    UpdatePaths::new(&state.paths)
        .load_job()
        .ok()
        .flatten()
        .map(|job| job.public_payload())
        .unwrap_or(Value::Null)
}

fn json_no_store(status: StatusCode, body: Value) -> Response {
    let mut response = (status, Json(body)).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
