use super::ScopedState;
use super::{engine, ApiError, ApiResult, AppState};
use axum::body::{Body, Bytes};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::convert::Infallible;
use std::time::{Duration, Instant};
use tokio::sync::broadcast::error::RecvError;
use uuid::Uuid;

const PARTNER_DOMAIN: &str = "partner";
const PARTNER_SOUL_DOMAIN: &str = "partner_soul";
const SOUL_DOMAIN: &str = "partner_soul_library";
const LINK_DOMAIN: &str = "partner_link";
const LINK_CODE_DOMAIN: &str = "partner_link_code";
const ONBOARDING_DOMAIN: &str = "partner_channel_onboarding";
const MAX_SESSION_KEY: usize = 120;

type SocketSender = SplitSink<WebSocket, Message>;

#[derive(Debug, Deserialize)]
struct DetailQuery {
    #[serde(default)]
    include_secrets: bool,
}

#[derive(Debug, Deserialize)]
struct HistoryQuery {
    session_key: Option<String>,
    session_id: Option<String>,
    #[serde(default = "default_history_limit")]
    limit: usize,
}

#[derive(Debug, Deserialize)]
struct HistoryPageQuery {
    session_key: String,
    before: Option<usize>,
    #[serde(default = "default_page_limit")]
    limit: usize,
}

#[derive(Debug, Deserialize)]
struct SessionKeyRequest {
    session_key: String,
}

#[derive(Debug, Deserialize)]
struct BranchSessionRequest {
    source_key: String,
    new_key: String,
}

#[derive(Debug, Deserialize)]
struct ContinuityRequest {
    enabled: bool,
    session_key: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OnboardingStartRequest {
    channel: String,
}

#[derive(Debug, Deserialize)]
struct PartnerChatRequest {
    #[serde(default)]
    content: String,
    session_id: Option<String>,
    session_key: Option<String>,
    chat_id: Option<String>,
    #[serde(default)]
    attachments: Vec<Value>,
    #[serde(rename = "llmSelection")]
    llm_selection_camel: Option<Value>,
    llm_selection: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct ConsultationQuery {
    chat_session_id: String,
    partner_name: String,
}

pub(super) fn api_router() -> Router<AppState> {
    Router::new()
        .route("/partners", get(list_partners).post(create_partner))
        .route("/partners/souls", get(list_souls).post(create_soul))
        .route(
            "/partners/souls/{soul_id}",
            get(get_soul).put(update_soul).delete(delete_soul),
        )
        .route("/partners/soul-sources", get(soul_sources))
        .route("/partners/navigation", get(navigation))
        .route("/partners/recent", get(recent_partners))
        .route("/partners/consultation-session", get(consultation_session))
        .route("/partners/channels/schema", get(channel_schemas))
        .route("/partners/tool-options", get(tool_options))
        .route("/partners/commands/palette", get(command_palette))
        .route("/partners/drafts/{draft_id}", get(get_partner_draft))
        .route(
            "/partners/drafts/{draft_id}/confirm",
            post(confirm_partner_draft),
        )
        .route(
            "/partners/{partner_id}",
            get(get_partner)
                .patch(update_partner)
                .delete(delete_partner),
        )
        .route("/partners/{partner_id}/start", post(start_partner))
        .route("/partners/{partner_id}/stop", post(stop_partner))
        .route(
            "/partners/{partner_id}/soul",
            get(get_partner_soul).put(put_partner_soul),
        )
        .route("/partners/{partner_id}/workspaces", get(partner_workspaces))
        .route(
            "/partners/{partner_id}/assets",
            get(partner_assets).post(add_partner_assets),
        )
        .route(
            "/partners/{partner_id}/assets/{asset_type}/{name}",
            axum::routing::delete(remove_partner_asset),
        )
        .route(
            "/partners/{partner_id}/channels/reload",
            post(reload_channels),
        )
        .route(
            "/partners/{partner_id}/channels/status",
            get(channel_status),
        )
        .route(
            "/partners/{partner_id}/channels/weixin/qr",
            post(start_weixin_qr),
        )
        .route(
            "/partners/{partner_id}/channels/weixin/qr/{session_id}",
            get(poll_weixin_qr),
        )
        .route(
            "/partners/{partner_id}/channel-onboarding/start",
            post(start_channel_onboarding),
        )
        .route(
            "/partners/{partner_id}/channel-onboarding/{session_id}",
            get(get_channel_onboarding).delete(cancel_channel_onboarding),
        )
        .route(
            "/partners/{partner_id}/channel-onboarding/{session_id}/apply",
            post(apply_channel_onboarding),
        )
        .route("/partners/{partner_id}/history", get(partner_history))
        .route(
            "/partners/{partner_id}/history/page",
            get(partner_history_page),
        )
        .route(
            "/partners/{partner_id}/web-continuity",
            get(get_web_continuity).put(put_web_continuity),
        )
        .route("/partners/{partner_id}/sessions", get(partner_sessions))
        .route(
            "/partners/{partner_id}/sessions/archive",
            post(archive_partner_session),
        )
        .route(
            "/partners/{partner_id}/sessions/resume",
            post(resume_partner_session),
        )
        .route(
            "/partners/{partner_id}/sessions/delete",
            post(delete_partner_session),
        )
        .route(
            "/partners/{partner_id}/sessions/branch",
            post(branch_partner_session),
        )
        .route("/partners/{partner_id}/links", get(list_links))
        .route("/partners/{partner_id}/links/code", post(create_link_code))
        .route(
            "/partners/{partner_id}/links/{key}",
            axum::routing::delete(delete_link),
        )
        .route("/partners/{partner_id}/chat", post(partner_chat))
        .route(
            "/partners/{partner_id}/chat/execute-stream",
            post(partner_chat_stream),
        )
}

pub(super) fn ws_router() -> Router<AppState> {
    Router::new().route("/ws/partners/{partner_id}", get(partner_websocket))
}

async fn list_partners(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    let mut partners = state
        .database
        .list_resources(PARTNER_DOMAIN)?
        .into_iter()
        .map(|value| public_partner(&value, false))
        .collect::<Vec<_>>();
    partners.sort_by(|left, right| {
        right
            .get("updated_at")
            .and_then(Value::as_f64)
            .partial_cmp(&left.get("updated_at").and_then(Value::as_f64))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(Json(Value::Array(partners)))
}

async fn create_partner(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let name = required_text(&body, "name", "伙伴名称不能为空")?;
    let explicit_id = body
        .get("partner_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let partner_id = explicit_id
        .map(safe_slug)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| format!("partner-{}", short_id()));
    if state
        .database
        .get_resource(PARTNER_DOMAIN, &partner_id)?
        .is_some()
    {
        return Err(ApiError::conflict("伙伴标识已存在"));
    }
    let soul = resolve_soul(&state, body.get("soul"))?;
    let assets = normalize_assets(body.get("assets"))?;
    let now = now();
    let partner = json!({
        "partner_id": partner_id,
        "name": name,
        "description": string_field(&body, "description"),
        "owner_id": "local-admin",
        "can_manage": true,
        "workspace_id": string_field(&body, "workspace_id"),
        "channels": body.get("channels").filter(|value| value.is_object()).cloned().unwrap_or_else(|| json!({})),
        "llm_selection": body.get("llm_selection").cloned().unwrap_or(Value::Null),
        "backup_llm_selection": body.get("backup_llm_selection").cloned().unwrap_or(Value::Null),
        "language": string_field_or(&body, "language", "zh"),
        "emoji": string_field_or(&body, "emoji", "🤖"),
        "color": string_field_or(&body, "color", "#3b82f6"),
        "avatar": string_field(&body, "avatar"),
        "soul_origin": soul.1,
        "enabled_tools": body.get("enabled_tools").cloned().unwrap_or(Value::Null),
        "builtin_tools": body.get("builtin_tools").cloned().unwrap_or(Value::Null),
        "mcp_tools": body.get("mcp_tools").cloned().unwrap_or_else(|| json!([])),
        "assets": assets,
        "running": body.get("start").and_then(Value::as_bool).unwrap_or(true),
        "started_at": if body.get("start").and_then(Value::as_bool).unwrap_or(true) {
            json!(iso_timestamp(now))
        } else {
            Value::Null
        },
        "last_reload_error": Value::Null,
        "provisioning": {"copied": {}, "errors": []}
    });
    state
        .database
        .put_resource(PARTNER_DOMAIN, &partner_id, &partner)?;
    state.database.put_resource(
        PARTNER_SOUL_DOMAIN,
        &partner_id,
        &json!({"content": soul.0}),
    )?;
    Ok(Json(public_partner(&partner, true)))
}

async fn get_partner(
    ScopedState(state): ScopedState,
    Path(partner_id): Path<String>,
    Query(query): Query<DetailQuery>,
) -> ApiResult<Json<Value>> {
    let partner = load_partner(&state, &partner_id)?;
    let mut value = public_partner(&partner, true);
    if query.include_secrets {
        value["channels"] = partner
            .get("channels")
            .cloned()
            .unwrap_or_else(|| json!({}));
    }
    Ok(Json(value))
}

async fn update_partner(
    ScopedState(state): ScopedState,
    Path(partner_id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let mut partner = load_partner(&state, &partner_id)?;
    for key in [
        "workspace_id",
        "name",
        "description",
        "channels",
        "llm_selection",
        "backup_llm_selection",
        "language",
        "emoji",
        "color",
        "avatar",
        "enabled_tools",
        "builtin_tools",
        "mcp_tools",
    ] {
        if let Some(value) = body.get(key) {
            partner[key] = value.clone();
        }
    }
    if let Some(name) = partner.get("name").and_then(Value::as_str).map(str::trim) {
        if name.is_empty() {
            return Err(ApiError::bad_request("伙伴名称不能为空"));
        }
        partner["name"] = Value::String(name.to_string());
    }
    let saved = state
        .database
        .put_resource(PARTNER_DOMAIN, &partner_id, &partner)?;
    Ok(Json(public_partner(&saved, true)))
}

async fn delete_partner(
    ScopedState(state): ScopedState,
    Path(partner_id): Path<String>,
) -> ApiResult<Json<Value>> {
    load_partner(&state, &partner_id)?;
    state
        .database
        .delete_resource(PARTNER_DOMAIN, &partner_id)?;
    state
        .database
        .delete_resource(PARTNER_SOUL_DOMAIN, &partner_id)?;
    for session in raw_partner_sessions(&state, &partner_id)? {
        if let Some(id) = session.get("_session_id").and_then(Value::as_str) {
            let _ = state.database.soft_delete_session(id);
        }
    }
    Ok(Json(json!({"deleted": true, "partner_id": partner_id})))
}

async fn start_partner(
    ScopedState(state): ScopedState,
    Path(partner_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let mut partner = load_partner(&state, &partner_id)?;
    partner["running"] = Value::Bool(true);
    partner["started_at"] = Value::String(iso_timestamp(now()));
    partner["last_reload_error"] = Value::Null;
    let saved = state
        .database
        .put_resource(PARTNER_DOMAIN, &partner_id, &partner)?;
    Ok(Json(public_partner(&saved, true)))
}

async fn stop_partner(
    ScopedState(state): ScopedState,
    Path(partner_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let mut partner = load_partner(&state, &partner_id)?;
    cancel_partner_sessions(&state, &partner_id);
    partner["running"] = Value::Bool(false);
    partner["started_at"] = Value::Null;
    let saved = state
        .database
        .put_resource(PARTNER_DOMAIN, &partner_id, &partner)?;
    Ok(Json(public_partner(&saved, true)))
}

async fn get_partner_soul(
    ScopedState(state): ScopedState,
    Path(partner_id): Path<String>,
) -> ApiResult<Json<Value>> {
    load_partner(&state, &partner_id)?;
    let content = state
        .database
        .get_resource(PARTNER_SOUL_DOMAIN, &partner_id)?
        .and_then(|value| {
            value
                .get("content")
                .and_then(Value::as_str)
                .map(String::from)
        })
        .unwrap_or_default();
    Ok(Json(json!({"partner_id": partner_id, "content": content})))
}

async fn put_partner_soul(
    ScopedState(state): ScopedState,
    Path(partner_id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    load_partner(&state, &partner_id)?;
    let content = body
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad_request("缺少灵魂内容"))?;
    let saved = state.database.put_resource(
        PARTNER_SOUL_DOMAIN,
        &partner_id,
        &json!({"content": content}),
    )?;
    Ok(Json(json!({
        "partner_id": partner_id,
        "content": saved.get("content").cloned().unwrap_or_else(|| json!("")),
        "updated": true
    })))
}

async fn list_souls(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    Ok(Json(Value::Array(
        state.database.list_resources(SOUL_DOMAIN)?,
    )))
}

async fn create_soul(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let name = required_text(&body, "name", "灵魂名称不能为空")?;
    let requested_id = body.get("id").and_then(Value::as_str).unwrap_or(&name);
    let soul_id = safe_slug(requested_id);
    let soul_id = if soul_id.is_empty() {
        format!("soul-{}", short_id())
    } else {
        soul_id
    };
    if state
        .database
        .get_resource(SOUL_DOMAIN, &soul_id)?
        .is_some()
    {
        return Err(ApiError::conflict("灵魂模板已存在"));
    }
    let content = string_field(&body, "content");
    let saved = state.database.put_resource(
        SOUL_DOMAIN,
        &soul_id,
        &json!({"id": soul_id, "name": name, "content": content}),
    )?;
    Ok(Json(saved))
}

async fn get_soul(
    ScopedState(state): ScopedState,
    Path(soul_id): Path<String>,
) -> ApiResult<Json<Value>> {
    state
        .database
        .get_resource(SOUL_DOMAIN, &soul_id)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("灵魂模板不存在"))
}

async fn update_soul(
    ScopedState(state): ScopedState,
    Path(soul_id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let mut current = state
        .database
        .get_resource(SOUL_DOMAIN, &soul_id)?
        .ok_or_else(|| ApiError::not_found("灵魂模板不存在"))?;
    for key in ["name", "content"] {
        if let Some(value) = body.get(key) {
            current[key] = value.clone();
        }
    }
    current["id"] = Value::String(soul_id.clone());
    Ok(Json(state.database.put_resource(
        SOUL_DOMAIN,
        &soul_id,
        &current,
    )?))
}

async fn delete_soul(
    ScopedState(state): ScopedState,
    Path(soul_id): Path<String>,
) -> ApiResult<Json<Value>> {
    if !state.database.delete_resource(SOUL_DOMAIN, &soul_id)? {
        return Err(ApiError::not_found("灵魂模板不存在"));
    }
    Ok(Json(json!({"id": soul_id, "deleted": true})))
}

async fn soul_sources(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    let personas = state
        .database
        .list_resources("persona")?
        .into_iter()
        .map(|value| {
            json!({
                "name": value.get("id").or_else(|| value.get("name")).cloned().unwrap_or(Value::Null),
                "description": value.get("description").cloned().unwrap_or_else(|| json!("")),
                "content": value.get("content").or_else(|| value.get("system_prompt")).cloned().unwrap_or_else(|| json!(""))
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({
        "library": state.database.list_resources(SOUL_DOMAIN)?,
        "personas": personas
    })))
}

async fn navigation(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    let partners = state
        .database
        .list_resources(PARTNER_DOMAIN)?
        .into_iter()
        .map(|value| public_partner(&value, false))
        .collect::<Vec<_>>();
    let sessions = partners
        .iter()
        .filter_map(|partner| {
            let partner_id = partner.get("partner_id")?.as_str()?;
            let rows = raw_partner_sessions(&state, partner_id).ok()?;
            Some(
                rows.into_iter()
                    .filter(|row| {
                        row.get("archived")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                            == false
                    })
                    .map(|row| {
                        json!({
                            "kind": "partner",
                            "owner_id": partner_id,
                            "owner_name": partner.get("name").cloned().unwrap_or(Value::Null),
                            "session_key": row.get("session_key").cloned().unwrap_or(Value::Null),
                            "title": row.get("title").cloned().unwrap_or(Value::Null),
                            "updated_at": row.get("updated_at_unix").cloned().unwrap_or_else(|| json!(0))
                        })
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .flatten()
        .collect::<Vec<_>>();
    Ok(Json(json!({
        "partners": partners,
        "groups": [],
        "sessions": sessions,
        "unavailable": []
    })))
}

async fn recent_partners(
    ScopedState(state): ScopedState,
    Query(query): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let limit = query
        .get("limit")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(3)
        .clamp(1, 20);
    let mut rows = state
        .database
        .list_resources(PARTNER_DOMAIN)?
        .into_iter()
        .map(|value| public_partner(&value, false))
        .collect::<Vec<_>>();
    rows.truncate(limit);
    Ok(Json(Value::Array(rows)))
}

async fn consultation_session(
    ScopedState(state): ScopedState,
    Query(query): Query<ConsultationQuery>,
) -> ApiResult<Json<Value>> {
    if query.partner_name.trim().is_empty() {
        return Ok(Json(Value::Null));
    }
    let matches = state
        .database
        .list_resources(PARTNER_DOMAIN)?
        .into_iter()
        .filter(|partner| {
            partner.get("name").and_then(Value::as_str) == Some(query.partner_name.trim())
        })
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Ok(Json(Value::Null));
    }
    let partner_id = matches[0]
        .get("partner_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let continuity = web_continuity(&state, partner_id)?;
    let session_key = continuity
        .get("session_key")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or(&query.chat_session_id);
    Ok(Json(json!({
        "partner_id": partner_id,
        "session_key": session_key
    })))
}

async fn channel_schemas() -> Json<Value> {
    Json(json!({
        "channels": {
            "telegram": channel_schema("Telegram", &["token"], json!({"enabled": false, "token": ""})),
            "discord": channel_schema("Discord", &["token"], json!({"enabled": false, "token": ""})),
            "slack": channel_schema("Slack", &["bot_token", "app_token"], json!({"enabled": false, "bot_token": "", "app_token": ""})),
            "email": channel_schema("Email", &["imap_password", "smtp_password"], json!({
                "enabled": false,
                "imap_host": "",
                "imap_username": "",
                "imap_password": "",
                "smtp_host": "",
                "smtp_username": "",
                "smtp_password": ""
            })),
            "feishu": channel_schema("Feishu", &["app_secret", "verification_token"], json!({"enabled": false, "app_id": "", "app_secret": "", "verification_token": ""})),
            "wecom": channel_schema("WeCom", &["secret", "token", "encoding_aes_key"], json!({"enabled": false, "corp_id": "", "agent_id": "", "secret": "", "token": "", "encoding_aes_key": ""})),
            "weixin": channel_schema("Weixin", &["token"], json!({"enabled": false, "token": "", "base_url": ""})),
            "qq": channel_schema("QQ", &["token"], json!({"enabled": false, "base_url": "", "token": ""})),
            "matrix": channel_schema("Matrix", &["access_token"], json!({"enabled": false, "homeserver": "", "user_id": "", "access_token": ""})),
            "mattermost": channel_schema("Mattermost", &["token"], json!({"enabled": false, "base_url": "", "token": ""})),
            "dingtalk": channel_schema("DingTalk", &["client_secret"], json!({"enabled": false, "client_id": "", "client_secret": ""})),
            "whatsapp": channel_schema("WhatsApp", &["access_token", "verify_token"], json!({"enabled": false, "phone_number_id": "", "access_token": "", "verify_token": ""})),
            "napcat": channel_schema("NapCat", &["token"], json!({"enabled": false, "base_url": "", "token": ""})),
            "mochat": channel_schema("Mochat", &["token"], json!({"enabled": false, "base_url": "", "token": ""})),
            "msteams": channel_schema("Microsoft Teams", &["app_password"], json!({"enabled": false, "app_id": "", "app_password": "", "tenant_id": ""})),
            "zulip": channel_schema("Zulip", &["api_key"], json!({"enabled": false, "site": "", "email": "", "api_key": ""})),
            "lark_http": channel_schema("Lark HTTP", &["app_secret", "verification_token"], json!({"enabled": false, "app_id": "", "app_secret": "", "verification_token": ""}))
        }
    }))
}

async fn tool_options() -> Json<Value> {
    Json(json!({
        "tools": [
            {"name": "brainstorm", "description": "进行发散式头脑风暴"},
            {"name": "web_search", "description": "联网搜索并引用来源"},
            {"name": "paper_search", "description": "检索 arXiv 论文"},
            {"name": "zotero_search", "description": "检索 Zotero 文库"},
            {"name": "reason", "description": "执行深度推理"},
            {"name": "geogebra_analysis", "description": "分析数学图像并生成 GeoGebra 命令"},
            {"name": "imagegen", "description": "生成图片"},
            {"name": "videogen", "description": "生成视频"}
        ],
        "builtin_tools": [
            {"name": "rag", "description": "检索知识库"},
            {"name": "web_fetch", "description": "读取网页正文"},
            {"name": "ask_user", "description": "向用户追问"},
            {"name": "exec", "description": "执行受限的本地命令"}
        ],
        "mcp_tools": configured_mcp_tools()
    }))
}

fn configured_mcp_tools() -> Vec<Value> {
    Vec::new()
}

async fn command_palette() -> Json<Value> {
    Json(json!({
        "commands": [
            {"command": "/new", "description": "归档当前会话并开始新会话"},
            {"command": "/clear", "description": "归档当前会话并开始新会话"},
            {"command": "/branch", "description": "复制当前会话为新分支"},
            {"command": "/resume", "description": "恢复一个归档会话", "arg_hint": "<会话 ID>"},
            {"command": "/delete", "description": "删除一个会话", "arg_hint": "<会话 ID>"},
            {"command": "/sessions", "description": "列出全部会话"},
            {"command": "/stop", "description": "停止正在生成的回答"}
        ]
    }))
}

async fn get_partner_draft(
    ScopedState(state): ScopedState,
    Path(draft_id): Path<String>,
) -> ApiResult<Json<Value>> {
    state
        .database
        .get_resource("partner_draft", &draft_id)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("伙伴草稿不存在"))
}

async fn confirm_partner_draft(
    ScopedState(state): ScopedState,
    Path(draft_id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let mut draft = state
        .database
        .get_resource("partner_draft", &draft_id)?
        .ok_or_else(|| ApiError::not_found("伙伴草稿不存在"))?;
    if let Some(partner_id) = draft
        .get("created_partner_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        return Ok(Json(public_partner(
            &load_partner(&state, partner_id)?,
            true,
        )));
    }
    for key in ["name", "description", "language", "emoji", "color"] {
        if let Some(value) = body.get(key) {
            draft[key] = value.clone();
        }
    }
    if let Some(soul) = body.get("soul").and_then(Value::as_str) {
        draft["soul"] = json!({"source": "custom", "content": soul});
    }
    let mut request = draft;
    request["start"] = body.get("start").cloned().unwrap_or_else(|| json!(true));
    let created = create_partner(ScopedState(state.clone()), Json(request))
        .await?
        .0;
    let partner_id = created
        .get("partner_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    draft = state
        .database
        .get_resource("partner_draft", &draft_id)?
        .unwrap_or_else(|| json!({}));
    draft["status"] = Value::String("created".into());
    draft["created_partner_id"] = Value::String(partner_id);
    let _ = state
        .database
        .put_resource("partner_draft", &draft_id, &draft);
    Ok(Json(created))
}

async fn partner_workspaces(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    let mut workspaces = state.database.list_resources("workspace")?;
    if workspaces.is_empty() {
        workspaces.push(json!({
            "workspace_id": "default",
            "name": "默认工作区",
            "kind": "private"
        }));
    }
    Ok(Json(json!({"workspaces": workspaces})))
}

async fn partner_assets(
    ScopedState(state): ScopedState,
    Path(partner_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let partner = load_partner(&state, &partner_id)?;
    Ok(Json(assets_value(
        &state,
        partner.get("assets").cloned().unwrap_or_else(|| json!({})),
    )?))
}

async fn add_partner_assets(
    ScopedState(state): ScopedState,
    Path(partner_id): Path<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let mut partner = load_partner(&state, &partner_id)?;
    let additions = normalize_assets(Some(&body))?;
    let mut current = partner.get("assets").cloned().unwrap_or_else(|| json!({}));
    merge_asset_lists(&mut current, &additions);
    partner["assets"] = current.clone();
    let saved = state
        .database
        .put_resource(PARTNER_DOMAIN, &partner_id, &partner)?;
    Ok(Json(json!({
        "partner_id": partner_id,
        "assets": assets_value(&state, current)?,
        "copied": {},
        "errors": [],
        "partner": public_partner(&saved, true)
    })))
}

async fn remove_partner_asset(
    ScopedState(state): ScopedState,
    Path((partner_id, asset_type, name)): Path<(String, String, String)>,
) -> ApiResult<Json<Value>> {
    let mut partner = load_partner(&state, &partner_id)?;
    let field = match asset_type.as_str() {
        "knowledge_base" | "knowledge_bases" => "knowledge_bases",
        "skill" | "skills" => "skills",
        "notebook" | "notebooks" => "notebooks",
        _ => return Err(ApiError::bad_request("未知的资源类型")),
    };
    let mut assets = normalize_assets(partner.get("assets"))?;
    if let Some(values) = assets.get_mut(field).and_then(Value::as_array_mut) {
        values.retain(|value| value.as_str() != Some(name.as_str()));
    }
    partner["assets"] = assets.clone();
    state
        .database
        .put_resource(PARTNER_DOMAIN, &partner_id, &partner)?;
    Ok(Json(json!({
        "partner_id": partner_id,
        "assets": assets_value(&state, assets)?
    })))
}

async fn reload_channels(
    ScopedState(state): ScopedState,
    Path(partner_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let mut partner = load_partner(&state, &partner_id)?;
    partner["running"] = Value::Bool(true);
    partner["last_reload_error"] = Value::Null;
    state
        .database
        .put_resource(PARTNER_DOMAIN, &partner_id, &partner)?;
    channel_status(ScopedState(state), Path(partner_id)).await
}

async fn channel_status(
    ScopedState(state): ScopedState,
    Path(partner_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let partner = load_partner(&state, &partner_id)?;
    let running = partner
        .get("running")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let channels = partner
        .get("channels")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|(name, config)| {
            let enabled = config
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            (
                name,
                json!({
                    "enabled": enabled,
                    "running": enabled && running,
                    "setup": {
                        "status": if enabled && running {"running"} else if enabled {"configured"} else {"disabled"},
                        "message": if enabled {"原生 Rust 渠道配置已保存"} else {"渠道未启用"}
                    }
                }),
            )
        })
        .collect::<Map<String, Value>>();
    Ok(Json(json!({
        "partner_id": partner_id,
        "running": running,
        "channels": channels
    })))
}

async fn start_weixin_qr(
    ScopedState(state): ScopedState,
    Path(partner_id): Path<String>,
) -> ApiResult<Json<Value>> {
    load_partner(&state, &partner_id)?;
    let session_id = short_id();
    let value = json!({
        "session_id": session_id,
        "partner_id": partner_id,
        "status": "error",
        "error": "个人微信扫码需要外部微信网关；请改用 NapCat、QQ 或 Telegram 渠道。",
        "expires_in": 0
    });
    state
        .database
        .put_resource("partner_weixin_qr", &session_id, &value)?;
    Ok(Json(value))
}

async fn poll_weixin_qr(
    ScopedState(state): ScopedState,
    Path((partner_id, session_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    load_partner(&state, &partner_id)?;
    state
        .database
        .get_resource("partner_weixin_qr", &session_id)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("微信扫码会话不存在"))
}

async fn start_channel_onboarding(
    ScopedState(state): ScopedState,
    Path(partner_id): Path<String>,
    Json(body): Json<OnboardingStartRequest>,
) -> ApiResult<Json<Value>> {
    load_partner(&state, &partner_id)?;
    if !matches!(body.channel.as_str(), "feishu" | "wecom") {
        return Err(ApiError::unprocessable("仅飞书和企业微信支持扫码引导"));
    }
    let session_id = short_id();
    let value = json!({
        "session_id": session_id,
        "partner_id": partner_id,
        "channel": body.channel,
        "status": "pending_scan",
        "qr_payload": format!("deeptutor://channel-onboarding/{session_id}"),
        "qr_data_url": null,
        "fallback_url": "",
        "poll_interval_seconds": 2,
        "expires_at": iso_timestamp(now() + 600.0),
        "error_code": ""
    });
    state
        .database
        .put_resource(ONBOARDING_DOMAIN, &session_id, &value)?;
    Ok(Json(value))
}

async fn get_channel_onboarding(
    ScopedState(state): ScopedState,
    Path((partner_id, session_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    load_onboarding(&state, &partner_id, &session_id)
}

async fn cancel_channel_onboarding(
    ScopedState(state): ScopedState,
    Path((partner_id, session_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let mut value = load_onboarding_value(&state, &partner_id, &session_id)?;
    value["status"] = Value::String("cancelled".into());
    state
        .database
        .put_resource(ONBOARDING_DOMAIN, &session_id, &value)?;
    Ok(Json(value))
}

async fn apply_channel_onboarding(
    ScopedState(state): ScopedState,
    Path((partner_id, session_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let mut onboarding = load_onboarding_value(&state, &partner_id, &session_id)?;
    if onboarding.get("status").and_then(Value::as_str) != Some("pending_scan") {
        return Err(ApiError::conflict("该扫码会话当前不可应用"));
    }
    onboarding["status"] = Value::String("applied".into());
    state
        .database
        .put_resource(ONBOARDING_DOMAIN, &session_id, &onboarding)?;
    Ok(Json(json!({"session": onboarding})))
}

fn load_onboarding(state: &AppState, partner_id: &str, session_id: &str) -> ApiResult<Json<Value>> {
    Ok(Json(load_onboarding_value(state, partner_id, session_id)?))
}

fn load_onboarding_value(state: &AppState, partner_id: &str, session_id: &str) -> ApiResult<Value> {
    load_partner(state, partner_id)?;
    let value = state
        .database
        .get_resource(ONBOARDING_DOMAIN, session_id)?
        .ok_or_else(|| ApiError::not_found("扫码引导会话不存在"))?;
    if value.get("partner_id").and_then(Value::as_str) != Some(partner_id) {
        return Err(ApiError::not_found("扫码引导会话不存在"));
    }
    Ok(value)
}

async fn partner_history(
    ScopedState(state): ScopedState,
    Path(partner_id): Path<String>,
    Query(query): Query<HistoryQuery>,
) -> ApiResult<Json<Value>> {
    load_partner(&state, &partner_id)?;
    let session_key = query
        .session_key
        .or(query.session_id)
        .unwrap_or_else(|| "default".to_string());
    let session_id = partner_session_id(&partner_id, &session_key)?;
    let messages = state.database.get_messages(&session_id)?;
    let limit = query.limit.clamp(1, 500);
    let start = messages.len().saturating_sub(limit);
    let messages = messages
        .into_iter()
        .skip(start)
        .map(history_message)
        .collect::<Vec<_>>();
    Ok(Json(Value::Array(messages)))
}

async fn partner_history_page(
    ScopedState(state): ScopedState,
    Path(partner_id): Path<String>,
    Query(query): Query<HistoryPageQuery>,
) -> ApiResult<Json<Value>> {
    load_partner(&state, &partner_id)?;
    let session_id = partner_session_id(&partner_id, &query.session_key)?;
    let messages = state.database.get_messages(&session_id)?;
    let total = messages.len();
    let limit = query.limit.clamp(1, 200);
    let start = query
        .before
        .unwrap_or(total)
        .min(total)
        .saturating_sub(limit);
    let end = query.before.unwrap_or(total).min(total);
    let page = messages
        .into_iter()
        .skip(start)
        .take(end.saturating_sub(start))
        .map(history_message)
        .collect::<Vec<_>>();
    Ok(Json(json!({
        "messages": page,
        "next_before": if start > 0 { json!(start) } else { Value::Null },
        "start": start,
        "total": total
    })))
}

async fn get_web_continuity(
    ScopedState(state): ScopedState,
    Path(partner_id): Path<String>,
) -> ApiResult<Json<Value>> {
    load_partner(&state, &partner_id)?;
    Ok(Json(web_continuity(&state, &partner_id)?))
}

async fn put_web_continuity(
    ScopedState(state): ScopedState,
    Path(partner_id): Path<String>,
    Json(body): Json<ContinuityRequest>,
) -> ApiResult<Json<Value>> {
    load_partner(&state, &partner_id)?;
    let session_key = body
        .session_key
        .as_deref()
        .map(validate_session_key)
        .transpose()
        .map_err(ApiError::unprocessable)?;
    let value = json!({
        "enabled": body.enabled,
        "session_key": session_key
    });
    set_web_continuity(&state, &partner_id, &value)?;
    Ok(Json(value))
}

async fn partner_sessions(
    ScopedState(state): ScopedState,
    Path(partner_id): Path<String>,
) -> ApiResult<Json<Value>> {
    load_partner(&state, &partner_id)?;
    Ok(Json(Value::Array(public_sessions(&raw_partner_sessions(
        &state,
        &partner_id,
    )?))))
}

async fn archive_partner_session(
    ScopedState(state): ScopedState,
    Path(partner_id): Path<String>,
    Json(body): Json<SessionKeyRequest>,
) -> ApiResult<Json<Value>> {
    load_partner(&state, &partner_id)?;
    let session_key = validate_session_key(&body.session_key).map_err(ApiError::unprocessable)?;
    let session_id = partner_session_id(&partner_id, &session_key)?;
    if state.database.get_session_meta(&session_id)?.is_none() {
        return Err(ApiError::not_found("会话不存在"));
    }
    state
        .database
        .update_session_preferences(&session_id, &json!({"archived": true}))?;
    let active = move_continuity_session(&state, &partner_id)?;
    Ok(Json(json!({
        "partner_id": partner_id,
        "archived": true,
        "session_key": session_key,
        "active_session_key": active
    })))
}

async fn resume_partner_session(
    ScopedState(state): ScopedState,
    Path(partner_id): Path<String>,
    Json(body): Json<SessionKeyRequest>,
) -> ApiResult<Json<Value>> {
    load_partner(&state, &partner_id)?;
    let session_key = validate_session_key(&body.session_key).map_err(ApiError::unprocessable)?;
    let session_id = partner_session_id(&partner_id, &session_key)?;
    if state.database.get_session_meta(&session_id)?.is_none() {
        return Err(ApiError::not_found("会话不存在"));
    }
    state
        .database
        .update_session_preferences(&session_id, &json!({"archived": false}))?;
    if web_continuity(&state, &partner_id)?
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        set_web_continuity(
            &state,
            &partner_id,
            &json!({"enabled": true, "session_key": session_key}),
        )?;
    }
    let session = raw_partner_sessions(&state, &partner_id)?
        .into_iter()
        .find(|row| row.get("session_key").and_then(Value::as_str) == Some(session_key.as_str()))
        .map(|row| public_session(&row))
        .unwrap_or_else(|| json!({"session_key": session_key}));
    Ok(Json(json!({
        "partner_id": partner_id,
        "resumed": true,
        "session": session,
        "active_session_key": session_key
    })))
}

async fn delete_partner_session(
    ScopedState(state): ScopedState,
    Path(partner_id): Path<String>,
    Json(body): Json<SessionKeyRequest>,
) -> ApiResult<Json<Value>> {
    load_partner(&state, &partner_id)?;
    let session_key = validate_session_key(&body.session_key).map_err(ApiError::unprocessable)?;
    let session_id = partner_session_id(&partner_id, &session_key)?;
    if !state.database.soft_delete_session(&session_id)? {
        return Err(ApiError::not_found("会话不存在"));
    }
    let active = move_continuity_session(&state, &partner_id)?;
    Ok(Json(json!({
        "partner_id": partner_id,
        "deleted": true,
        "session_key": session_key,
        "active_session_key": active
    })))
}

async fn branch_partner_session(
    ScopedState(state): ScopedState,
    Path(partner_id): Path<String>,
    Json(body): Json<BranchSessionRequest>,
) -> ApiResult<Json<Value>> {
    load_partner(&state, &partner_id)?;
    let source_key = validate_session_key(&body.source_key).map_err(ApiError::unprocessable)?;
    let new_key = validate_session_key(&body.new_key).map_err(ApiError::unprocessable)?;
    if source_key == new_key {
        return Err(ApiError::bad_request("分支会话必须使用不同的会话标识"));
    }
    let source_id = partner_session_id(&partner_id, &source_key)?;
    let target_id = partner_session_id(&partner_id, &new_key)?;
    let messages = state.database.get_messages(&source_id)?;
    if messages.is_empty() {
        return Err(ApiError::bad_request("源会话没有可复制的消息"));
    }
    ensure_partner_session(&state, &partner_id, &new_key, "会话分支")?;
    for message in &messages {
        state.database.add_message(crate::storage::NewMessage {
            session_id: &target_id,
            role: message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("user"),
            content: message.get("content").and_then(Value::as_str).unwrap_or(""),
            capability: Some("chat"),
            events: message.get("events").cloned().unwrap_or_else(|| json!([])),
            attachments: message
                .get("attachments")
                .cloned()
                .unwrap_or_else(|| json!([])),
            metadata: message
                .get("metadata")
                .cloned()
                .unwrap_or_else(|| json!({})),
            parent_message_id: None,
        })?;
    }
    state
        .database
        .update_session_preferences(&source_id, &json!({"archived": true}))?;
    if web_continuity(&state, &partner_id)?
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        set_web_continuity(
            &state,
            &partner_id,
            &json!({"enabled": true, "session_key": new_key}),
        )?;
    }
    let session = raw_partner_sessions(&state, &partner_id)?
        .into_iter()
        .find(|row| row.get("session_key").and_then(Value::as_str) == Some(new_key.as_str()))
        .map(|row| public_session(&row))
        .unwrap_or_else(|| json!({"session_key": new_key}));
    Ok(Json(json!({
        "partner_id": partner_id,
        "branched": true,
        "session": session,
        "active_session_key": new_key
    })))
}

async fn list_links(
    ScopedState(state): ScopedState,
    Path(partner_id): Path<String>,
) -> ApiResult<Json<Value>> {
    load_partner(&state, &partner_id)?;
    let links = state
        .database
        .list_resources(LINK_DOMAIN)?
        .into_iter()
        .filter(|item| item.get("partner_id").and_then(Value::as_str) == Some(partner_id.as_str()))
        .map(|item| {
            json!({
                "key": item.get("key").cloned().unwrap_or(Value::Null),
                "channel": item.get("channel").cloned().unwrap_or_else(|| json!("")),
                "sender_id": item.get("sender_id").cloned().unwrap_or_else(|| json!("")),
                "linked_at": item.get("linked_at").cloned().unwrap_or(Value::Null)
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({"links": links})))
}

async fn create_link_code(
    ScopedState(state): ScopedState,
    Path(partner_id): Path<String>,
) -> ApiResult<Json<Value>> {
    load_partner(&state, &partner_id)?;
    let code = short_id().to_ascii_uppercase();
    let expires_at = iso_timestamp(now() + 600.0);
    let value = json!({
        "partner_id": partner_id,
        "code": code,
        "expires_at": expires_at,
        "used": false
    });
    state
        .database
        .put_resource(LINK_CODE_DOMAIN, &code, &value)?;
    Ok(Json(json!({
        "code": code,
        "expires_at": expires_at,
        "command": format!("/link {code}")
    })))
}

async fn delete_link(
    ScopedState(state): ScopedState,
    Path((partner_id, key)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    load_partner(&state, &partner_id)?;
    let resource_id = format!("{partner_id}:{key}");
    if !state.database.delete_resource(LINK_DOMAIN, &resource_id)? {
        return Err(ApiError::not_found("渠道账号链接不存在"));
    }
    Ok(Json(json!({"deleted": true, "key": key})))
}

async fn partner_chat(
    ScopedState(state): ScopedState,
    Path(partner_id): Path<String>,
    Json(body): Json<PartnerChatRequest>,
) -> ApiResult<Json<Value>> {
    let partner = load_partner(&state, &partner_id)?;
    let session_key = resolve_request_session_key(&body);
    let session_id = ensure_partner_session(&state, &partner_id, &session_key, &body.content)?;
    if let Some(active) = state.database.get_active_turn(&session_id)? {
        return Err(ApiError::conflict(format!(
            "当前会话已有运行中的回合：{}",
            active.get("turn_id").and_then(Value::as_str).unwrap_or("")
        )));
    }
    let request = partner_turn_request(&state, &partner, &session_id, &body)?;
    let receipt = engine::start_turn(state.clone(), request)
        .await
        .map_err(ApiError::bad_request)?;
    let turn_id = receipt
        .get("turn_id")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::internal("回合引擎未返回 turn_id"))?
        .to_string();
    let final_message = wait_for_turn(&state, &session_id, &turn_id).await?;
    Ok(Json(json!({
        "partner_id": partner_id,
        "session_id": session_id,
        "session_key": session_key,
        "turn_id": turn_id,
        "content": final_message.get("content").cloned().unwrap_or_else(|| json!("")),
        "message": final_message
    })))
}

async fn partner_chat_stream(
    ScopedState(state): ScopedState,
    Path(partner_id): Path<String>,
    Json(body): Json<PartnerChatRequest>,
) -> ApiResult<Response> {
    let partner = load_partner(&state, &partner_id)?;
    let session_key = resolve_request_session_key(&body);
    let session_id = ensure_partner_session(&state, &partner_id, &session_key, &body.content)?;
    if let Some(active) = state.database.get_active_turn(&session_id)? {
        return Err(ApiError::conflict(format!(
            "当前会话已有运行中的回合：{}",
            active.get("turn_id").and_then(Value::as_str).unwrap_or("")
        )));
    }
    let mut live = state.turn_events.subscribe();
    let request = partner_turn_request(&state, &partner, &session_id, &body)?;
    let receipt = engine::start_turn(state.clone(), request)
        .await
        .map_err(ApiError::bad_request)?;
    let turn_id = receipt
        .get("turn_id")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::internal("回合引擎未返回 turn_id"))?
        .to_string();
    let initial_session = format!(
        "event: session\ndata: {}\n\n",
        json!({"partner_id": partner_id, "session_id": session_id, "session_key": session_key})
    );
    let (sender, receiver) = tokio::sync::mpsc::channel::<Result<Bytes, Infallible>>(32);
    tokio::spawn(async move {
        if sender.send(Ok(Bytes::from(initial_session))).await.is_err() {
            return;
        }
        loop {
            let event = match live.recv().await {
                Ok(event) => event,
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            };
            if event.get("turn_id").and_then(Value::as_str) != Some(turn_id.as_str()) {
                continue;
            }
            let event_type = event.get("type").and_then(Value::as_str).unwrap_or("event");
            if event_type == "thinking" {
                let frame = sse_frame(
                    "thinking",
                    json!({"content": event.get("content").cloned().unwrap_or_else(|| json!(""))}),
                );
                if sender.send(Ok(Bytes::from(frame))).await.is_err() {
                    break;
                }
                continue;
            }
            if event_type != "done" {
                continue;
            }

            let status = event
                .pointer("/metadata/status")
                .and_then(Value::as_str)
                .unwrap_or("completed");
            let final_message = final_message_for_turn(&state, &session_id, &event);
            if status == "failed" {
                let detail = event
                    .pointer("/metadata/error")
                    .and_then(Value::as_str)
                    .or_else(|| {
                        final_message
                            .as_ref()
                            .and_then(|message| message.get("content"))
                            .and_then(Value::as_str)
                    })
                    .unwrap_or("伙伴回答失败");
                let frame = sse_frame("error", json!({"detail": detail}));
                let _ = sender.send(Ok(Bytes::from(frame))).await;
                break;
            }
            if let Some(message) = final_message.as_ref() {
                let frame = sse_frame(
                    "content",
                    json!({"content": message.get("content").cloned().unwrap_or_else(|| json!(""))}),
                );
                if sender.send(Ok(Bytes::from(frame))).await.is_err() {
                    break;
                }
            }
            if status == "cancelled" {
                let frame = sse_frame(
                    "stopped",
                    json!({"partner_id": partner_id, "session_id": session_id, "stopped": true}),
                );
                let _ = sender.send(Ok(Bytes::from(frame))).await;
            } else {
                let frame = sse_frame(
                    "done",
                    json!({"partner_id": partner_id, "session_id": session_id}),
                );
                let _ = sender.send(Ok(Bytes::from(frame))).await;
            }
            break;
        }
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(
            tokio_stream::wrappers::ReceiverStream::new(receiver),
        ))
        .map_err(|error| ApiError::internal(format!("无法构建 SSE 响应：{error}")))
}

async fn partner_websocket(
    ws: WebSocketUpgrade,
    ScopedState(state): ScopedState,
    Path(partner_id): Path<String>,
) -> impl IntoResponse {
    ws.max_message_size(64 * 1024 * 1024)
        .max_frame_size(16 * 1024 * 1024)
        .on_upgrade(move |socket| handle_partner_socket(socket, state, partner_id))
}

#[derive(Clone)]
struct WatchedTurn {
    session_key: String,
    session_id: String,
    turn_id: String,
    cursor: u64,
}

async fn handle_partner_socket(mut socket: WebSocket, state: AppState, partner_id: String) {
    if load_partner(&state, &partner_id).is_err() {
        let _ = socket.close().await;
        return;
    }
    let (mut sender, mut receiver) = socket.split();
    if send_json(
        &mut sender,
        json!({"type": "ready", "partner_id": partner_id}),
    )
    .await
    .is_err()
    {
        return;
    }
    let mut live = state.turn_events.subscribe();
    let mut watched = HashMap::<String, WatchedTurn>::new();
    loop {
        tokio::select! {
            incoming = receiver.next() => {
                let Some(incoming) = incoming else { break; };
                let message = match incoming {
                    Ok(message) => message,
                    Err(_) => break,
                };
                match message {
                    Message::Text(text) => {
                        if let Err(error) = handle_partner_frame(
                            &mut sender,
                            &state,
                            &partner_id,
                            &mut watched,
                            &text,
                        ).await {
                            tracing::debug!(%error, "伙伴 WebSocket 命令失败");
                            let _ = send_json(&mut sender, json!({"type": "error", "content": error})).await;
                        }
                    }
                    Message::Ping(value) => {
                        if sender.send(Message::Pong(value)).await.is_err() {
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    Message::Pong(_) | Message::Binary(_) => {}
                }
            }
            event = live.recv() => {
                match event {
                    Ok(event) => {
                        if deliver_watched_event(&mut sender, &state, &partner_id, &mut watched, event).await.is_err() {
                            break;
                        }
                    }
                    Err(RecvError::Lagged(_)) => {
                        if replay_watched(&mut sender, &state, &partner_id, &mut watched).await.is_err() {
                            break;
                        }
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        }
    }
}

async fn handle_partner_frame(
    sender: &mut SocketSender,
    state: &AppState,
    partner_id: &str,
    watched: &mut HashMap<String, WatchedTurn>,
    raw: &str,
) -> Result<(), String> {
    let frame: Value =
        serde_json::from_str(raw).map_err(|error| format!("无效的 JSON：{error}"))?;
    let action = frame.get("action").and_then(Value::as_str).unwrap_or("");
    let session_key = frame
        .get("session_key")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            frame
                .get("chat_id")
                .or_else(|| frame.get("session_id"))
                .and_then(Value::as_str)
                .unwrap_or("default")
                .to_string()
        });
    let session_key = validate_session_key(&session_key)?;
    let session_id = partner_session_id(partner_id, &session_key)?;

    match action {
        "attach" => {
            ensure_partner_session(state, partner_id, &session_key, "")
                .map_err(|error| error.to_string())?;
            if let Some(active) = state.database.get_active_turn(&session_id)? {
                let turn_id = active
                    .get("turn_id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if turn_id.is_empty() {
                    return send_json(
                        sender,
                        json!({"type": "attach_idle", "session_key": session_key}),
                    )
                    .await;
                }
                send_json(
                    sender,
                    json!({"type": "resuming", "session_key": session_key}),
                )
                .await?;
                if let Some(content) = latest_user_message(state, &session_id)? {
                    send_json(
                        sender,
                        json!({"type": "user_echo", "content": content, "session_key": session_key}),
                    )
                    .await?;
                }
                watched.insert(
                    turn_id.clone(),
                    WatchedTurn {
                        session_key,
                        session_id,
                        turn_id,
                        cursor: 0,
                    },
                );
                replay_watched(sender, state, partner_id, watched).await?;
            } else {
                send_json(
                    sender,
                    json!({"type": "attach_idle", "session_key": session_key}),
                )
                .await?;
            }
            Ok(())
        }
        "stop" => {
            if let Some(active) = state.database.get_active_turn(&session_id)? {
                if let Some(turn_id) = active.get("turn_id").and_then(Value::as_str) {
                    state.cancel_turn(turn_id);
                }
            }
            Ok(())
        }
        "" => {
            let content = frame
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            let attachments = frame
                .get("attachments")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if content.is_empty() && attachments.is_empty() {
                return Ok(());
            }
            ensure_partner_session(state, partner_id, &session_key, &content)
                .map_err(|error| error.to_string())?;
            if let Some(active) = state.database.get_active_turn(&session_id)? {
                return send_json(
                    sender,
                    json!({
                        "type": "turn_busy",
                        "content": "这个会话正在生成回答。",
                        "turn_id": active.get("turn_id").cloned().unwrap_or(Value::Null),
                        "session_key": session_key
                    }),
                )
                .await;
            }
            let partner = load_partner(state, partner_id).map_err(|error| error.to_string())?;
            let body = PartnerChatRequest {
                content: if content.is_empty() {
                    "请分析我上传的附件。".to_string()
                } else {
                    content
                },
                session_id: None,
                session_key: Some(session_key.clone()),
                chat_id: None,
                attachments,
                llm_selection_camel: None,
                llm_selection: None,
            };
            let request = partner_turn_request(state, &partner, &session_id, &body)
                .map_err(|error| error.to_string())?;
            let receipt = engine::start_turn(state.clone(), request).await?;
            let turn_id = receipt
                .get("turn_id")
                .and_then(Value::as_str)
                .ok_or_else(|| "回合引擎未返回 turn_id".to_string())?
                .to_string();
            send_json(
                sender,
                json!({"type": "accepted", "session_key": session_key, "turn_id": turn_id}),
            )
            .await?;
            watched.insert(
                turn_id.clone(),
                WatchedTurn {
                    session_key,
                    session_id,
                    turn_id: turn_id.clone(),
                    cursor: 0,
                },
            );
            replay_watched(sender, state, partner_id, watched).await
        }
        "consultation_activity" => Ok(()),
        _ => Ok(()),
    }
}

async fn deliver_watched_event(
    sender: &mut SocketSender,
    state: &AppState,
    partner_id: &str,
    watched: &mut HashMap<String, WatchedTurn>,
    event: Value,
) -> Result<(), String> {
    let Some(turn_id) = event
        .get("turn_id")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return Ok(());
    };
    let Some(watch) = watched.get_mut(&turn_id) else {
        return Ok(());
    };
    let seq = event.get("seq").and_then(Value::as_u64).unwrap_or(0);
    if seq <= watch.cursor {
        return Ok(());
    }
    watch.cursor = seq;
    let event_type = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    send_json(
        sender,
        json!({"type": "stream_event", "event": event.clone()}),
    )
    .await?;
    if event_type == "done" {
        finish_watched_turn(sender, state, partner_id, watched, &turn_id, &event).await?;
    }
    Ok(())
}

async fn replay_watched(
    sender: &mut SocketSender,
    state: &AppState,
    partner_id: &str,
    watched: &mut HashMap<String, WatchedTurn>,
) -> Result<(), String> {
    let turn_ids = watched.keys().cloned().collect::<Vec<_>>();
    for turn_id in turn_ids {
        let (session_id, cursor) = watched
            .get(&turn_id)
            .map(|watch| (watch.session_id.clone(), watch.cursor))
            .unwrap_or_default();
        let events = state
            .database
            .get_turn_events(&turn_id, cursor)
            .map_err(|error| format!("无法读取伙伴回合回放：{error}"))?;
        for event in events {
            let done = event.get("type").and_then(Value::as_str) == Some("done");
            deliver_watched_event(sender, state, partner_id, watched, event.clone()).await?;
            if done {
                break;
            }
        }
        let turn = state
            .database
            .get_turn(&turn_id)
            .map_err(|error| format!("无法读取伙伴回合状态：{error}"))?;
        if turn
            .as_ref()
            .and_then(|value| value.get("status"))
            .and_then(Value::as_str)
            .is_some_and(|status| matches!(status, "completed" | "failed" | "cancelled"))
            && watched.contains_key(&turn_id)
        {
            let terminal = json!({
                "type": "done",
                "metadata": {
                    "status": turn.as_ref().and_then(|value| value.get("status")).cloned().unwrap_or_else(|| json!("completed"))
                }
            });
            finish_watched_turn(sender, state, partner_id, watched, &turn_id, &terminal).await?;
        }
        let _ = session_id;
    }
    Ok(())
}

async fn finish_watched_turn(
    sender: &mut SocketSender,
    state: &AppState,
    _partner_id: &str,
    watched: &mut HashMap<String, WatchedTurn>,
    turn_id: &str,
    done_event: &Value,
) -> Result<(), String> {
    let Some(watch) = watched.remove(turn_id) else {
        return Ok(());
    };
    let status = done_event
        .pointer("/metadata/status")
        .and_then(Value::as_str)
        .unwrap_or("completed");
    if status == "cancelled" {
        return send_json(
            sender,
            json!({"type": "stopped", "session_key": watch.session_key}),
        )
        .await;
    }
    let message = if let Some(message_id) = done_event
        .pointer("/metadata/assistant_message_id")
        .and_then(Value::as_i64)
    {
        state
            .database
            .get_message(&watch.session_id, message_id)?
            .or_else(|| {
                latest_assistant_message(state, &watch.session_id)
                    .ok()
                    .flatten()
            })
    } else {
        latest_assistant_message(state, &watch.session_id)?
    };
    if let Some(message) = message {
        if status == "failed" {
            let content = message
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("伙伴回答失败。");
            send_json(sender, json!({"type": "content", "content": content})).await?;
        } else {
            let content = message.get("content").and_then(Value::as_str).unwrap_or("");
            send_json(sender, json!({"type": "content", "content": content})).await?;
        }
    }
    send_json(
        sender,
        json!({"type": "done", "session_key": watch.session_key}),
    )
    .await
}

fn partner_turn_request(
    state: &AppState,
    partner: &Value,
    session_id: &str,
    body: &PartnerChatRequest,
) -> ApiResult<Value> {
    let llm_selection = body
        .llm_selection
        .clone()
        .or_else(|| body.llm_selection_camel.clone())
        .or_else(|| partner.get("llm_selection").cloned())
        .filter(|value| value.is_object());
    let capability = "chat";
    let mut request = json!({
        "protocol_version": "2.0",
        "session_id": session_id,
        "content": body.content.trim(),
        "capability": capability,
        "language": partner.get("language").cloned().unwrap_or_else(|| json!("zh")),
        "partner_id": partner.get("partner_id").cloned().unwrap_or(Value::Null),
        "attachments": body.attachments,
        "client_submission_id": format!("partner-{}", Uuid::new_v4())
    });
    if let Some(selection) = llm_selection {
        request["llm_selection"] = selection;
    }
    if let Some(enabled) = partner
        .get("enabled_tools")
        .filter(|value| value.is_array())
    {
        request["enabled_tools"] = enabled.clone();
    }
    if let Some(partner_id) = partner.get("partner_id").and_then(Value::as_str) {
        if let Some(soul) = state
            .database
            .get_resource(PARTNER_SOUL_DOMAIN, partner_id)?
            .and_then(|value| {
                value
                    .get("content")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .filter(|value| !value.trim().is_empty())
        {
            request["system_prompt"] = Value::String(soul);
        }
    }
    if let Some(knowledge_bases) = partner.pointer("/assets/knowledge_bases") {
        request["knowledge_bases"] = knowledge_bases.clone();
    }
    Ok(request)
}

fn final_message_for_turn(state: &AppState, session_id: &str, done_event: &Value) -> Option<Value> {
    done_event
        .pointer("/metadata/assistant_message_id")
        .and_then(Value::as_i64)
        .and_then(|message_id| {
            state
                .database
                .get_message(session_id, message_id)
                .ok()
                .flatten()
        })
        .or_else(|| latest_assistant_message(state, session_id).ok().flatten())
}

fn sse_frame(event: &str, payload: Value) -> String {
    format!("event: {event}\ndata: {payload}\n\n")
}

async fn wait_for_turn(state: &AppState, session_id: &str, turn_id: &str) -> ApiResult<Value> {
    let started = Instant::now();
    loop {
        if let Some(turn) = state.database.get_turn(turn_id)? {
            let status = turn.get("status").and_then(Value::as_str).unwrap_or("");
            if matches!(status, "completed" | "failed" | "cancelled") {
                return latest_assistant_message(state, session_id)?
                    .ok_or_else(|| ApiError::internal("伙伴回合结束后没有保存回答"));
            }
        }
        if started.elapsed() > Duration::from_secs(600) {
            state.cancel_turn(turn_id);
            return Err(ApiError::provider("伙伴回答超时"));
        }
        tokio::time::sleep(Duration::from_millis(120)).await;
    }
}

fn latest_assistant_message(state: &AppState, session_id: &str) -> Result<Option<Value>, String> {
    Ok(state
        .database
        .get_messages(session_id)?
        .into_iter()
        .rev()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("assistant")))
}

fn latest_user_message(state: &AppState, session_id: &str) -> Result<Option<String>, String> {
    Ok(state
        .database
        .get_messages(session_id)?
        .into_iter()
        .rev()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        .and_then(|message| {
            message
                .get("content")
                .and_then(Value::as_str)
                .map(String::from)
        }))
}

fn resolve_request_session_key(body: &PartnerChatRequest) -> String {
    body.session_key
        .as_deref()
        .or(body.session_id.as_deref())
        .or(body.chat_id.as_deref())
        .unwrap_or("web")
        .to_string()
}

fn partner_session_id(partner_id: &str, session_key: &str) -> Result<String, String> {
    let key = validate_session_key(session_key)?;
    Ok(format!("partner:{partner_id}:{key}"))
}

fn validate_session_key(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > MAX_SESSION_KEY {
        return Err("会话标识长度必须为 1 至 120 个字符".to_string());
    }
    if !value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return Err("会话标识只能包含字母、数字、点、横线和下划线".to_string());
    }
    Ok(value.to_string())
}

fn ensure_partner_session(
    state: &AppState,
    partner_id: &str,
    session_key: &str,
    seed: &str,
) -> ApiResult<String> {
    let session_id = partner_session_id(partner_id, session_key)?;
    if state.database.get_session_meta(&session_id)?.is_some() {
        state.database.update_session_preferences(
            &session_id,
            &json!({"archived": false, "partner_id": partner_id, "session_key": session_key}),
        )?;
        return Ok(session_id);
    }
    let title = if seed.trim().is_empty() {
        "伙伴新对话".to_string()
    } else {
        seed.trim().chars().take(80).collect()
    };
    state.database.create_session(
        &session_id,
        &title,
        &json!({
            "capability": "chat",
            "partner_id": partner_id,
            "session_key": session_key,
            "channel": "web"
        }),
        "",
    )?;
    Ok(session_id)
}

fn raw_partner_sessions(state: &AppState, partner_id: &str) -> Result<Vec<Value>, String> {
    let prefix = format!("partner:{partner_id}:");
    state.database.with_connection(|connection| {
        let mut statement = connection
            .prepare(
                r#"
                SELECT id, title, created_at, updated_at, preferences_json, archived,
                       (SELECT COUNT(*) FROM messages m WHERE m.session_id=s.id),
                       COALESCE((SELECT content FROM messages lm
                           WHERE lm.session_id=s.id ORDER BY lm.id DESC LIMIT 1), '')
                FROM sessions s
                WHERE deleted_at IS NULL AND substr(id, 1, ?1) = ?2
                ORDER BY updated_at DESC
                "#,
            )
            .map_err(|error| format!("无法准备伙伴会话查询：{error}"))?;
        let rows = statement
            .query_map(
                rusqlite::params![prefix.chars().count() as i64, prefix],
                |row| {
                    let id: String = row.get(0)?;
                    let preferences: Value = serde_json::from_str(&row.get::<_, String>(4)?)
                        .unwrap_or_else(|_| json!({}));
                    let session_key = id.strip_prefix(&prefix).unwrap_or("default").to_string();
                    Ok(json!({
                        "_session_id": id,
                        "session_key": session_key,
                        "title": row.get::<_, String>(1)?,
                        "created_at": iso_timestamp(row.get::<_, f64>(2)?),
                        "updated_at": iso_timestamp(row.get::<_, f64>(3)?),
                        "updated_at_unix": row.get::<_, f64>(3)?,
                        "archived": row.get::<_, i64>(5)? != 0,
                        "message_count": row.get::<_, i64>(6)?,
                        "last_message": row.get::<_, String>(7)?,
                        "chat_id": preferences.get("chat_id").cloned().unwrap_or(Value::Null),
                        "scope": preferences.get("scope").cloned().unwrap_or(Value::Null)
                    }))
                },
            )
            .map_err(|error| format!("无法读取伙伴会话：{error}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("无法解析伙伴会话：{error}"))
    })
}

fn public_sessions(rows: &[Value]) -> Vec<Value> {
    rows.iter().map(public_session).collect()
}

fn public_session(row: &Value) -> Value {
    let mut value = row.clone();
    if let Some(object) = value.as_object_mut() {
        object.remove("_session_id");
        object.remove("updated_at_unix");
    }
    value
}

fn history_message(message: Value) -> Value {
    json!({
        "id": message.get("id").cloned().unwrap_or(Value::Null),
        "role": message.get("role").cloned().unwrap_or_else(|| json!("assistant")),
        "content": message.get("content").cloned().unwrap_or_else(|| json!("")),
        "timestamp": message.get("created_at").and_then(Value::as_f64).map(iso_timestamp),
        "channel": "web",
        "metadata": message.get("metadata").cloned().unwrap_or_else(|| json!({})),
        "attachments": message.get("attachments").cloned().unwrap_or_else(|| json!([])),
        "events": message.get("events").cloned().unwrap_or_else(|| json!([]))
    })
}

fn web_continuity(state: &AppState, partner_id: &str) -> ApiResult<Value> {
    let all = state
        .database
        .get_setting("partner_web_continuity")?
        .unwrap_or_else(|| json!({}));
    Ok(all
        .get(partner_id)
        .cloned()
        .unwrap_or_else(|| json!({"enabled": false, "session_key": Value::Null})))
}

fn set_web_continuity(state: &AppState, partner_id: &str, value: &Value) -> ApiResult<()> {
    let mut all = state
        .database
        .get_setting("partner_web_continuity")?
        .unwrap_or_else(|| json!({}));
    all[partner_id] = value.clone();
    state.database.set_setting("partner_web_continuity", &all)?;
    Ok(())
}

fn move_continuity_session(state: &AppState, partner_id: &str) -> ApiResult<Value> {
    let current = web_continuity(state, partner_id)?;
    if !current
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Ok(Value::Null);
    }
    let key = format!("web-{}", short_id());
    set_web_continuity(
        state,
        partner_id,
        &json!({"enabled": true, "session_key": key}),
    )?;
    Ok(Value::String(key))
}

fn cancel_partner_sessions(state: &AppState, partner_id: &str) {
    let prefix = format!("partner:{partner_id}:");
    if let Ok(turns) = state.database.with_connection(|connection| {
        let mut statement = connection
            .prepare(
                "SELECT t.id FROM turns t JOIN sessions s ON s.id=t.session_id \
                 WHERE t.status IN ('queued','running','waiting_input') \
                   AND substr(s.id, 1, ?1) = ?2",
            )
            .map_err(|error| format!("无法准备伙伴活动回合查询：{error}"))?;
        let rows = statement
            .query_map(
                rusqlite::params![prefix.chars().count() as i64, prefix],
                |row| row.get::<_, String>(0),
            )
            .map_err(|error| format!("无法读取伙伴活动回合：{error}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("无法解析伙伴活动回合：{error}"))
    }) {
        for turn_id in turns {
            state.cancel_turn(&turn_id);
        }
    }
}

fn load_partner(state: &AppState, partner_id: &str) -> ApiResult<Value> {
    state
        .database
        .get_resource(PARTNER_DOMAIN, partner_id)?
        .ok_or_else(|| ApiError::not_found("伙伴不存在"))
}

fn public_partner(value: &Value, detail: bool) -> Value {
    let mut partner = value.clone();
    let channel_names = partner
        .get("channels")
        .and_then(Value::as_object)
        .map(|channels| {
            channels
                .iter()
                .filter(|(_, config)| {
                    config
                        .get("enabled")
                        .and_then(Value::as_bool)
                        .unwrap_or(true)
                })
                .map(|(name, _)| Value::String(name.clone()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if !detail {
        partner["channels"] = Value::Array(channel_names);
    } else {
        partner["channels"] = mask_channel_secrets(
            partner
                .get("channels")
                .cloned()
                .unwrap_or_else(|| json!({})),
        );
    }
    partner["partner_id"] = partner
        .get("partner_id")
        .or_else(|| partner.get("id"))
        .cloned()
        .unwrap_or(Value::Null);
    partner["can_manage"] = Value::Bool(true);
    partner
}

fn mask_channel_secrets(value: Value) -> Value {
    fn is_secret_field(key: &str) -> bool {
        let key = key.to_ascii_lowercase();
        [
            "token",
            "secret",
            "password",
            "api_key",
            "apikey",
            "encrypt_key",
        ]
        .iter()
        .any(|hint| key.contains(hint))
    }

    fn walk(value: Value, key_hint: Option<&str>) -> Value {
        match value {
            Value::Object(object) => Value::Object(
                object
                    .into_iter()
                    .map(|(key, value)| {
                        let masked = walk(value, Some(&key));
                        (key, masked)
                    })
                    .collect(),
            ),
            Value::Array(values) => Value::Array(
                values
                    .into_iter()
                    .map(|value| walk(value, key_hint))
                    .collect(),
            ),
            Value::String(text)
                if key_hint.is_some_and(is_secret_field) && !text.trim().is_empty() =>
            {
                Value::String("***".to_string())
            }
            value => value,
        }
    }

    walk(value, None)
}

fn resolve_soul(state: &AppState, raw: Option<&Value>) -> ApiResult<(String, Value)> {
    let soul = raw
        .filter(|value| value.is_object())
        .cloned()
        .unwrap_or_else(|| json!({"source": "default"}));
    let source = soul
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("default");
    match source {
        "custom" => Ok((
            soul.get("content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            json!({"type": "custom"}),
        )),
        "library" => {
            let id = soul.get("id").and_then(Value::as_str).unwrap_or("");
            let template = state
                .database
                .get_resource(SOUL_DOMAIN, id)?
                .ok_or_else(|| ApiError::not_found("灵魂模板不存在"))?;
            Ok((
                template
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                json!({"type": "library", "id": id}),
            ))
        }
        "persona" => {
            let id = soul.get("id").and_then(Value::as_str).unwrap_or("");
            let persona = state
                .database
                .get_resource("persona", id)?
                .ok_or_else(|| ApiError::not_found("人格不存在"))?;
            let content = persona
                .get("content")
                .or_else(|| persona.get("system_prompt"))
                .or_else(|| persona.get("prompt"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Ok((content, json!({"type": "persona", "id": id})))
        }
        _ => Ok((
            "你是一位耐心、严谨、善于启发思考的学习伙伴。\
             优先帮助用户理解概念，必要时给出分步推理、例子和可执行的练习建议。"
                .to_string(),
            json!({"type": "default"}),
        )),
    }
}

fn normalize_assets(raw: Option<&Value>) -> ApiResult<Value> {
    let raw = raw
        .filter(|value| value.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
    Ok(json!({
        "knowledge_bases": string_array(raw.get("knowledge_bases")),
        "skills": string_array(raw.get("skills")),
        "notebooks": string_array(raw.get("notebooks"))
    }))
}

fn merge_asset_lists(current: &mut Value, additions: &Value) {
    for key in ["knowledge_bases", "skills", "notebooks"] {
        let mut values = current
            .get(key)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut seen = values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect::<std::collections::HashSet<_>>();
        for value in additions
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if seen.insert(value.to_string()) {
                values.push(Value::String(value.to_string()));
            }
        }
        current[key] = Value::Array(values);
    }
}

fn assets_value(state: &AppState, assets: Value) -> ApiResult<Value> {
    let knowledge_bases = assets
        .get("knowledge_bases")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|name| {
            let documents = state
                .database
                .get_resource("knowledge_base", name)
                .ok()
                .flatten()
                .and_then(|value| {
                    value
                        .get("documents")
                        .and_then(Value::as_array)
                        .map(Vec::len)
                })
                .unwrap_or(0);
            json!({"name": name, "documents": documents})
        })
        .collect::<Vec<_>>();
    let skills = assets
        .get("skills")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|name| json!({"name": name}))
        .collect::<Vec<_>>();
    let notebooks = assets
        .get("notebooks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|id| {
            let notebook = state.database.get_resource("notebook", id).ok().flatten();
            json!({
                "id": id,
                "name": notebook.as_ref().and_then(|value| value.get("name")).and_then(Value::as_str).unwrap_or(id),
                "record_count": notebook.as_ref().and_then(|value| value.get("records")).and_then(Value::as_array).map(Vec::len).unwrap_or(0)
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "knowledge_bases": knowledge_bases,
        "skills": skills,
        "notebooks": notebooks
    }))
}

fn channel_schema(display_name: &str, secrets: &[&str], defaults: Value) -> Value {
    channel_schema_with_secrets(display_name, secrets, defaults)
}

fn channel_schema_with_secrets(display_name: &str, secrets: &[&str], defaults: Value) -> Value {
    let properties = defaults
        .as_object()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|(key, value)| {
            let kind = match value {
                Value::Bool(_) => "boolean",
                Value::Number(_) => "number",
                _ => "string",
            };
            (key, json!({"type": kind}))
        })
        .collect::<Map<String, Value>>();
    json!({
        "name": display_name.to_ascii_lowercase().replace(' ', "_"),
        "display_name": display_name,
        "default_config": defaults,
        "secret_fields": secrets,
        "json_schema": {"type": "object", "properties": properties},
        "available": true
    })
}

fn safe_slug(value: &str) -> String {
    let mut output = String::new();
    let mut pending_separator = false;
    for ch in value.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            if pending_separator && !output.is_empty() {
                output.push('-');
            }
            output.push(ch);
            pending_separator = false;
        } else {
            pending_separator = true;
        }
        if output.len() >= 64 {
            break;
        }
    }
    output.trim_matches('-').to_string()
}

fn string_field(body: &Value, key: &str) -> String {
    body.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

fn string_field_or(body: &Value, key: &str, default: &str) -> String {
    body.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(default)
        .to_string()
}

fn required_text(body: &Value, key: &str, message: &str) -> ApiResult<String> {
    body.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| ApiError::bad_request(message))
}

fn string_array(value: Option<&Value>) -> Vec<Value> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| Value::String(value.to_string()))
        .collect()
}

fn short_id() -> String {
    Uuid::new_v4().simple().to_string()[..8].to_string()
}

fn now() -> f64 {
    chrono::Utc::now().timestamp_micros() as f64 / 1_000_000.0
}

fn iso_timestamp(timestamp: f64) -> String {
    chrono::DateTime::from_timestamp(timestamp as i64, 0)
        .map(|value| value.to_rfc3339())
        .unwrap_or_default()
}

fn default_history_limit() -> usize {
    200
}

fn default_page_limit() -> usize {
    60
}

async fn send_json(sender: &mut SocketSender, value: Value) -> Result<(), String> {
    let text = serde_json::to_string(&value)
        .map_err(|error| format!("无法序列化 WebSocket 消息：{error}"))?;
    sender
        .send(Message::Text(text.into()))
        .await
        .map_err(|error| format!("无法发送 WebSocket 消息：{error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_key_is_restricted_to_safe_characters() {
        assert!(validate_session_key("web-abc_123").is_ok());
        assert!(validate_session_key("../outside").is_err());
        assert!(validate_session_key("包含中文").is_err());
        assert!(validate_session_key("").is_err());
    }

    #[test]
    fn partner_ids_are_slugged_without_path_segments() {
        assert_eq!(safe_slug("My Partner / 01"), "my-partner-01");
        assert!(safe_slug("../../evil").chars().all(|ch| ch != '/'));
    }

    #[test]
    fn channel_schema_keeps_secret_fields() {
        let schema = channel_schema(
            "Telegram",
            &["token"],
            json!({"enabled": false, "token": ""}),
        );
        assert_eq!(schema["secret_fields"], json!(["token"]));
        assert_eq!(
            schema["json_schema"]["properties"]["enabled"]["type"],
            "boolean"
        );
    }
}
