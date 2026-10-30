use super::auth::{
    account_by_id, hash_password, learner_grant, list_accounts, load_grant_value,
    normalize_book_permission, normalize_grant, require_admin, save_grant, update_account_fields,
    validate_grant, validate_password, Account, Principal,
};
use super::error::{ApiError, ApiResult};
use super::AppState;
use axum::extract::{Extension, Path, Query, State};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use rusqlite::{params, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::{BTreeSet, HashMap};
use uuid::Uuid;

const GUARDIAN_PERMISSIONS: [&str; 4] = [
    "assign_materials",
    "manage_restrictions",
    "view_reports",
    "reset_credentials",
];

const READING_EXTENSIONS: [(&str, &str, &str); 5] = [
    ("read_aloud", "朗读", "1.0.0"),
    ("translation", "翻译", "1.0.0"),
    ("vocabulary", "词汇帮助", "1.0.0"),
    ("guided_learning", "学习引导", "1.0.0"),
    ("quiz", "阅读测验", "1.0.0"),
];

const BUILTIN_TOOLS: [(&str, &str); 8] = [
    ("brainstorm", "生成有依据的多方向想法"),
    ("web_search", "搜索互联网并附带来源"),
    ("paper_search", "检索 arXiv 论文"),
    ("zotero_search", "搜索本地 Zotero 文库"),
    ("reason", "使用专用推理提示词"),
    ("geogebra_analysis", "分析数学图像"),
    ("imagegen", "按描述生成图像"),
    ("videogen", "按描述生成视频"),
];

#[derive(Debug, Deserialize)]
struct GuardianListQuery {
    #[serde(default)]
    include_revoked: bool,
}

fn all_guardian_permissions() -> Vec<String> {
    GUARDIAN_PERMISSIONS
        .into_iter()
        .map(str::to_string)
        .collect()
}

#[derive(Debug, Deserialize)]
struct GuardianAuthorizationPayload {
    guardian_user_id: String,
    learner_user_id: String,
    #[serde(default = "all_guardian_permissions")]
    permissions: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct GuardianMaterialsPayload {
    book_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct GuardianRestrictionsPayload {
    age_band: String,
    allow_upload: bool,
    allowed_surfaces: Vec<String>,
    #[serde(default)]
    extensions: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct GuardianCredentialResetPayload {
    new_password: String,
}

#[derive(Debug, Deserialize)]
struct GrantPayload {
    grant: Value,
}

#[derive(Debug, Deserialize)]
struct BookPermissionPayload {
    #[serde(default = "default_create_permission")]
    create: bool,
    #[serde(default)]
    default: String,
    #[serde(default)]
    books: HashMap<String, String>,
}

fn default_create_permission() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct SkillInstallPayload {
    #[serde(rename = "ref")]
    reference: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    force: bool,
    #[serde(default)]
    allow_unverified: bool,
}

#[derive(Clone, Debug)]
struct GuardianRelationship {
    id: String,
    guardian_user_id: String,
    learner_user_id: String,
    permissions: Vec<String>,
    granted_at: String,
    revoked_at: Option<String>,
    revoked_by: String,
    revocation_reason: String,
}

impl GuardianRelationship {
    fn public(&self, accounts: &HashMap<String, Account>) -> Value {
        let guardian = accounts.get(&self.guardian_user_id);
        let learner = accounts.get(&self.learner_user_id);
        json!({
            "id": self.id,
            "guardian_user_id": self.guardian_user_id,
            "guardian_username": guardian.map(|account| account.username.as_str()).unwrap_or(""),
            "learner_user_id": self.learner_user_id,
            "learner_username": learner.map(|account| account.username.as_str()).unwrap_or(""),
            "permissions": self.permissions,
            "granted_at": self.granted_at,
            "revoked_at": self.revoked_at,
            "revoked_by": self.revoked_by,
            "revocation_reason": self.revocation_reason
        })
    }
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/multi-user/admin/resources", get(admin_resources))
        .route("/multi-user/admin/books", get(admin_books))
        .route(
            "/multi-user/admin/skills/install",
            post(admin_install_skill),
        )
        .route("/multi-user/guardians", get(list_guardians))
        .route(
            "/multi-user/guardians",
            axum::routing::post(authorize_guardian),
        )
        .route(
            "/multi-user/guardians/{relationship_id}",
            delete(revoke_guardian_relationship),
        )
        .route("/multi-user/me/guardianships", get(my_guardianships))
        .route(
            "/multi-user/me/guardianships/{relationship_id}",
            delete(revoke_my_guardianship),
        )
        .route(
            "/multi-user/learners/{learner_user_id}/guardian-report",
            get(guardian_report),
        )
        .route(
            "/multi-user/learners/{learner_user_id}/materials",
            get(guardian_materials).put(assign_guardian_materials),
        )
        .route(
            "/multi-user/learners/{learner_user_id}/restrictions",
            get(get_guardian_restrictions).put(put_guardian_restrictions),
        )
        .route(
            "/multi-user/learners/{learner_user_id}/credentials/reset",
            post(reset_learner_credentials),
        )
        .route("/multi-user/users", get(multi_user_list_users))
        .route(
            "/multi-user/users/{user_id}/grants",
            get(get_user_grants).put(put_user_grants),
        )
        .route(
            "/multi-user/users/{user_id}/book-permission",
            get(get_user_book_permission).put(put_user_book_permission),
        )
}

async fn admin_resources(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
) -> ApiResult<Json<Value>> {
    require_admin(&principal)?;
    let catalog = state
        .database
        .get_setting("catalog")?
        .unwrap_or_else(|| json!({}));
    let knowledge_bases = state
        .database
        .list_resources("knowledge_base")?
        .into_iter()
        .filter_map(|item| {
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .or_else(|| item.get("id").and_then(Value::as_str))?
                .to_string();
            Some(json!({
                "resource_id": format!("admin:kb:{name}"),
                "name": name,
                "source": "admin"
            }))
        })
        .collect::<Vec<_>>();
    let skills = state
        .database
        .list_resources("skill")?
        .into_iter()
        .map(|item| {
            json!({
                "name": item.get("name").or_else(|| item.get("id")).cloned().unwrap_or(Value::Null),
                "description": item.get("description").cloned().unwrap_or(Value::Null),
                "tags": item.get("tags").cloned().unwrap_or_else(|| json!([]))
            })
        })
        .collect::<Vec<_>>();
    let partners = state
        .database
        .list_resources("partner")?
        .into_iter()
        .filter(|item| {
            item.get("owner_id")
                .and_then(Value::as_str)
                .is_none_or(|owner| owner.is_empty() || owner == "local-admin")
        })
        .map(|item| {
            json!({
                "partner_id": item.get("partner_id").or_else(|| item.get("id")).cloned().unwrap_or(Value::Null),
                "name": item.get("name").cloned().unwrap_or(Value::Null),
                "description": item.get("description").cloned().unwrap_or(Value::Null),
                "emoji": item.get("emoji").cloned().unwrap_or(Value::Null)
            })
        })
        .collect::<Vec<_>>();
    let reading_materials = state
        .database
        .list_resources("reading_material")?
        .into_iter()
        .map(|item| {
            json!({
                "material_id": item.get("material_id").or_else(|| item.get("id")).cloned().unwrap_or(Value::Null),
                "title": item.get("title").cloned().unwrap_or(Value::Null),
                "filename": item.get("filename").cloned().unwrap_or(Value::Null),
                "render_mode": item.get("render_mode").cloned().unwrap_or_else(|| json!("text"))
            })
        })
        .collect::<Vec<_>>();
    let mcp_tools = mcp_tool_options(&state)?;
    Ok(Json(json!({
        "models": {"llm": model_profile_summaries(&catalog)},
        "knowledge_bases": knowledge_bases,
        "skills": skills,
        "partners": partners,
        "reading_materials": reading_materials,
        "reading_extensions": reading_extension_summaries(&state)?,
        "tools": builtin_tool_options(),
        "mcp_tools": mcp_tools
    })))
}

fn model_profile_summaries(catalog: &Value) -> Vec<Value> {
    let Some(profiles) = catalog
        .pointer("/services/llm/profiles")
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    profiles
        .iter()
        .filter(|profile| {
            !profile
                .get("owner_bound")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .filter_map(|profile| {
            let profile_id = profile.get("id").and_then(Value::as_str)?.to_string();
            let models = profile
                .get("models")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|model| {
                    !model
                        .get("owner_bound")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                })
                .map(|model| {
                    json!({
                        "model_id": model.get("id").cloned().unwrap_or(Value::Null),
                        "name": model
                            .get("name")
                            .or_else(|| model.get("model"))
                            .cloned()
                            .unwrap_or(Value::Null),
                        "model": model.get("model").cloned().unwrap_or(Value::Null)
                    })
                })
                .collect::<Vec<_>>();
            Some(json!({
                "profile_id": profile_id,
                "name": profile
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty())
                    .unwrap_or(&profile_id),
                "models": models
            }))
        })
        .collect()
}

fn builtin_tool_options() -> Vec<Value> {
    BUILTIN_TOOLS
        .into_iter()
        .map(|(name, description)| {
            json!({
                "name": name,
                "description": description,
                "kind": "builtin"
            })
        })
        .collect()
}

fn reading_extension_summaries(state: &AppState) -> ApiResult<Vec<Value>> {
    let mut summaries = READING_EXTENSIONS
        .into_iter()
        .map(|(id, name, version)| {
            json!({
                "id": id,
                "name": name,
                "version": version,
                "kind": "builtin"
            })
        })
        .collect::<Vec<_>>();
    for extension in super::plugins::plugin_reading_extensions(state).map_err(ApiError::internal)? {
        let id = extension
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::internal("插件阅读扩展缺少 ID"))?;
        summaries.push(json!({
            "id": id,
            "name": extension.get("name").cloned().unwrap_or_else(|| Value::String(id.to_string())),
            "version": extension.get("version").cloned().unwrap_or_else(|| json!("1.0.0")),
            "kind": "plugin"
        }));
    }
    Ok(summaries)
}

fn allowed_reading_extension_ids(state: &AppState) -> ApiResult<BTreeSet<String>> {
    Ok(reading_extension_summaries(state)?
        .into_iter()
        .filter_map(|extension| {
            extension
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect())
}

fn mcp_tool_options(state: &AppState) -> ApiResult<Vec<Value>> {
    let Some(config) = state.database.get_setting("mcp")? else {
        return Ok(Vec::new());
    };
    let mut output = Vec::new();
    let servers = config
        .get("servers")
        .and_then(Value::as_object)
        .or_else(|| config.as_object());
    if let Some(servers) = servers {
        for (provider_id, server) in servers {
            let Some(tools) = server.get("tools").and_then(Value::as_array) else {
                continue;
            };
            for tool in tools {
                let Some(name) = tool
                    .get("name")
                    .or_else(|| tool.get("id"))
                    .and_then(Value::as_str)
                else {
                    continue;
                };
                output.push(json!({
                    "name": name,
                    "provider_id": provider_id,
                    "server": provider_id,
                    "kind": "mcp",
                    "description": tool.get("description").cloned().unwrap_or(Value::Null)
                }));
            }
        }
    }
    output.sort_by(|left, right| {
        left.get("name")
            .and_then(Value::as_str)
            .cmp(&right.get("name").and_then(Value::as_str))
    });
    Ok(output)
}

async fn admin_books(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
) -> ApiResult<Json<Value>> {
    require_admin(&principal)?;
    Ok(Json(json!({"books": admin_book_catalog(&state)?})))
}

fn admin_book_catalog(state: &AppState) -> ApiResult<Vec<Value>> {
    let mut books = state
        .database
        .list_resources("book")?
        .into_iter()
        .map(|book| {
            json!({
                "book_id": book.get("id").cloned().unwrap_or(Value::Null),
                "title": book.get("title").cloned().unwrap_or(Value::Null),
                "status": book.get("status").cloned().unwrap_or_else(|| json!("draft")),
                "updated_at": book.get("updated_at").cloned().unwrap_or(Value::Null)
            })
        })
        .collect::<Vec<_>>();
    books.sort_by(|left, right| {
        right
            .get("updated_at")
            .and_then(Value::as_f64)
            .partial_cmp(&left.get("updated_at").and_then(Value::as_f64))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(books)
}

async fn list_guardians(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Query(query): Query<GuardianListQuery>,
) -> ApiResult<Json<Value>> {
    require_admin(&principal)?;
    let relationships = guardian_relationships(&state, None, None, query.include_revoked)?;
    let accounts = account_map(&state)?;
    Ok(Json(json!({
        "relationships": relationships
            .iter()
            .map(|relationship| relationship.public(&accounts))
            .collect::<Vec<_>>()
    })))
}

async fn authorize_guardian(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Json(payload): Json<GuardianAuthorizationPayload>,
) -> ApiResult<Json<Value>> {
    require_admin(&principal)?;
    let guardian = assignable_account(&state, &payload.guardian_user_id)?;
    let learner = assignable_account(&state, &payload.learner_user_id)?;
    if guardian.id == learner.id {
        return Err(ApiError::bad_request("用户不能监护自己的账户"));
    }
    if guardian.preset == "learner" {
        return Err(ApiError::bad_request("学习者账户不能作为守护人"));
    }
    if learner.preset != "learner" {
        return Err(ApiError::bad_request("守护关系只能绑定学习者账户"));
    }
    let permissions = normalized_permissions(payload.permissions)?;
    let relationship_id = format!("ga_{}", Uuid::new_v4().simple());
    let granted_at = chrono::Utc::now().to_rfc3339();
    state
        .database
        .with_connection(|connection| {
            let transaction = connection
                .transaction()
                .map_err(|error| format!("无法开始守护关系事务：{error}"))?;
            let duplicate: i64 = transaction
                .query_row(
                    r#"
                    SELECT COUNT(*)
                      FROM guardian_relationships
                     WHERE guardian_user_id = ?1
                       AND learner_user_id = ?2
                       AND revoked_at IS NULL
                    "#,
                    params![guardian.id, learner.id],
                    |row| row.get(0),
                )
                .map_err(|error| format!("无法检查守护关系：{error}"))?;
            if duplicate > 0 {
                return Err("该守护人已经获得此学习者的授权".to_string());
            }
            let inverse: i64 = transaction
                .query_row(
                    r#"
                    SELECT COUNT(*)
                      FROM guardian_relationships
                     WHERE guardian_user_id = ?1
                       AND learner_user_id = ?2
                       AND revoked_at IS NULL
                    "#,
                    params![learner.id, guardian.id],
                    |row| row.get(0),
                )
                .map_err(|error| format!("无法检查反向守护关系：{error}"))?;
            if inverse > 0 {
                return Err("学习者不能监护当前有效的守护人".to_string());
            }
            transaction
                .execute(
                    r#"
                    INSERT INTO guardian_relationships(
                        id, guardian_user_id, learner_user_id, permissions_json,
                        granted_at, revoked_at, revoked_by, revocation_reason
                    ) VALUES(?1, ?2, ?3, ?4, ?5, NULL, '', '')
                    "#,
                    params![
                        relationship_id,
                        guardian.id,
                        learner.id,
                        Value::Array(permissions.iter().cloned().map(Value::String).collect())
                            .to_string(),
                        granted_at
                    ],
                )
                .map_err(|error| format!("无法创建守护关系：{error}"))?;
            transaction
                .commit()
                .map_err(|error| format!("无法提交守护关系：{error}"))?;
            Ok(())
        })
        .map_err(|error| {
            if error.contains("已经获得") || error.contains("不能监护") {
                ApiError::conflict(error)
            } else {
                ApiError::internal(error)
            }
        })?;
    let relationship = guardian_relationship_by_id(&state, &relationship_id)?
        .ok_or_else(|| ApiError::internal("守护关系创建后无法读取"))?;
    Ok(Json(json!({
        "relationship": relationship.public(&account_map(&state)?),
        "actor_username": principal.username
    })))
}

async fn revoke_guardian_relationship(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(relationship_id): Path<String>,
) -> ApiResult<Json<Value>> {
    require_admin(&principal)?;
    let relationship = revoke_relationship(&state, &relationship_id, &principal.user_id, "")?
        .ok_or_else(|| ApiError::not_found("守护关系不存在"))?;
    Ok(Json(json!({
        "relationship": relationship.public(&account_map(&state)?),
        "ok": true
    })))
}

async fn my_guardianships(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
) -> ApiResult<Json<Value>> {
    let guardian = assignable_account(&state, &principal.user_id)?;
    if guardian.preset == "learner" {
        return Err(ApiError::forbidden("学习者账户不能作为守护人"));
    }
    let relationships = guardian_relationships(&state, Some(&guardian.id), None, false)?;
    let accounts = account_map(&state)?;
    Ok(Json(json!({
        "relationships": relationships
            .iter()
            .map(|relationship| relationship.public(&accounts))
            .collect::<Vec<_>>()
    })))
}

async fn revoke_my_guardianship(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(relationship_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let guardian = assignable_account(&state, &principal.user_id)?;
    if guardian.preset == "learner" {
        return Err(ApiError::forbidden("学习者账户不能作为守护人"));
    }
    let existing = guardian_relationship_by_id(&state, &relationship_id)?
        .filter(|relationship| {
            relationship.guardian_user_id == guardian.id && relationship.revoked_at.is_none()
        })
        .ok_or_else(|| ApiError::not_found("有效守护关系不存在"))?;
    let relationship = revoke_relationship(&state, &existing.id, &guardian.id, "self_revoked")?
        .ok_or_else(|| ApiError::not_found("有效守护关系不存在"))?;
    Ok(Json(json!({
        "relationship": relationship.public(&account_map(&state)?),
        "ok": true
    })))
}

async fn guardian_report(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(learner_user_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let learner = guardian_access(&state, &principal, &learner_user_id, "view_reports")?;
    let permission = normalize_book_permission(&learner.book_permission);
    let assigned_materials = admin_book_catalog(&state)?
        .into_iter()
        .filter_map(|book| {
            let book_id = book.get("book_id").and_then(Value::as_str)?;
            let level = book_permission_level(&permission, book_id);
            (level != "none").then(|| {
                let mut book = book;
                book["permission"] = Value::String(level.to_string());
                book
            })
        })
        .collect::<Vec<_>>();
    let grant = load_grant_value(&state, &learner.id)?;
    Ok(Json(json!({
        "learner": {
            "id": learner.id,
            "username": learner.username,
            "disabled": learner.disabled
        },
        "book_permission": permission,
        "assigned_materials": assigned_materials,
        "grant_summary": {
            "model_count": grant.pointer("/models/llm").and_then(Value::as_array).map(Vec::len).unwrap_or(0),
            "knowledge_base_count": grant.get("knowledge_bases").and_then(Value::as_array).map(Vec::len).unwrap_or(0),
            "skill_count": grant.get("skills").and_then(Value::as_array).map(Vec::len).unwrap_or(0),
            "enabled_tools": grant.get("enabled_tools").cloned().unwrap_or(Value::Null)
        }
    })))
}

async fn guardian_materials(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(learner_user_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let learner = guardian_access(&state, &principal, &learner_user_id, "assign_materials")?;
    let permission = normalize_book_permission(&learner.book_permission);
    let materials = admin_book_catalog(&state)?
        .into_iter()
        .map(|mut book| {
            let level = book
                .get("book_id")
                .and_then(Value::as_str)
                .map(|book_id| book_permission_level(&permission, book_id))
                .unwrap_or("none");
            book["assigned"] = Value::Bool(level != "none");
            book["permission"] = Value::String(level.to_string());
            book
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({"materials": materials})))
}

async fn assign_guardian_materials(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(learner_user_id): Path<String>,
    Json(payload): Json<GuardianMaterialsPayload>,
) -> ApiResult<Json<Value>> {
    let learner = guardian_access(&state, &principal, &learner_user_id, "assign_materials")?;
    let catalog = admin_book_catalog(&state)?;
    let catalog_ids = catalog
        .iter()
        .filter_map(|book| book.get("book_id").and_then(Value::as_str))
        .collect::<BTreeSet<_>>();
    let selected_ids = payload
        .book_ids
        .into_iter()
        .map(|book_id| book_id.trim().to_string())
        .filter(|book_id| !book_id.is_empty())
        .collect::<BTreeSet<_>>();
    if let Some(unknown) = selected_ids
        .iter()
        .find(|book_id| !catalog_ids.contains(book_id.as_str()))
    {
        return Err(ApiError::bad_request(format!(
            "未知的共享书籍标识：{unknown}"
        )));
    }
    let existing = normalize_book_permission(&learner.book_permission);
    let default = existing
        .get("default")
        .and_then(Value::as_str)
        .unwrap_or("none");
    let mut books = Map::new();
    for book_id in catalog_ids {
        let selected = selected_ids.contains(book_id);
        match (default, selected) {
            ("read", false) => {
                books.insert(book_id.to_string(), Value::String("none".into()));
            }
            ("none", true) => {
                books.insert(book_id.to_string(), Value::String("read".into()));
            }
            _ => {}
        }
    }
    let permission = normalize_book_permission(&json!({
        "create": existing
            .get("create")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        "default": default,
        "books": books
    }));
    update_account_fields(
        &state,
        &learner.id,
        &[(
            "book_permission_json",
            Value::String(permission.to_string()),
        )],
    )?;
    Ok(Json(json!({"book_permission": permission})))
}

async fn get_guardian_restrictions(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(learner_user_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let learner = guardian_access(&state, &principal, &learner_user_id, "manage_restrictions")?;
    let grant = restriction_grant(&state, &learner)?;
    Ok(Json(json!({
        "restrictions": guardian_restrictions(&grant)?,
        "available_extensions": reading_extension_summaries(&state)?
    })))
}

async fn put_guardian_restrictions(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(learner_user_id): Path<String>,
    Json(payload): Json<GuardianRestrictionsPayload>,
) -> ApiResult<Json<Value>> {
    let learner = guardian_access(&state, &principal, &learner_user_id, "manage_restrictions")?;
    validate_restriction_payload(&state, &payload)?;
    let mut grant = restriction_grant(&state, &learner)?;
    if !grant.get("learning_policy").is_some_and(Value::is_object) {
        grant = learner_grant(&learner.id);
    }
    let policy = grant
        .get_mut("learning_policy")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| ApiError::conflict("学习者账户没有学习策略"))?;
    policy.insert("age_band".into(), Value::String(payload.age_band.clone()));
    policy.insert(
        "allowed_surfaces".into(),
        Value::Array(
            payload
                .allowed_surfaces
                .iter()
                .cloned()
                .map(Value::String)
                .collect(),
        ),
    );
    let reading = policy
        .entry("reading")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| ApiError::unprocessable("阅读策略必须是对象"))?;
    reading.insert("allow_upload".into(), Value::Bool(payload.allow_upload));
    reading.insert(
        "extensions".into(),
        Value::Array(
            payload
                .extensions
                .iter()
                .cloned()
                .map(Value::String)
                .collect(),
        ),
    );
    let grant = normalize_grant(&learner.id, &grant);
    validate_grant(&grant)?;
    save_grant_with_profiles(&state, &learner, &grant, Some("learner"))?;
    Ok(Json(json!({
        "restrictions": guardian_restrictions(&grant)?
    })))
}

async fn reset_learner_credentials(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(learner_user_id): Path<String>,
    Json(payload): Json<GuardianCredentialResetPayload>,
) -> ApiResult<Json<Value>> {
    let learner = guardian_access(&state, &principal, &learner_user_id, "reset_credentials")?;
    validate_password(&payload.new_password)?;
    let password_hash = hash_password(&payload.new_password).await?;
    let revoked_devices = state
        .database
        .with_connection(|connection| {
            let transaction = connection
                .transaction()
                .map_err(|error| format!("无法开始密码重置事务：{error}"))?;
            let revoked: i64 = transaction
                .query_row(
                    r#"
                    SELECT COUNT(*)
                      FROM auth_device_credentials
                     WHERE user_id = ?1 AND revoked_at IS NULL
                    "#,
                    params![learner.id],
                    |row| row.get(0),
                )
                .map_err(|error| format!("无法统计设备凭据：{error}"))?;
            transaction
                .execute(
                    "UPDATE auth_accounts SET password_hash = ?1, updated_at = ?2 WHERE id = ?3",
                    params![password_hash, now_iso(), learner.id],
                )
                .map_err(|error| format!("无法更新学习者密码：{error}"))?;
            transaction
                .execute(
                    "DELETE FROM auth_sessions WHERE user_id = ?1",
                    params![learner.id],
                )
                .map_err(|error| format!("无法撤销学习者会话：{error}"))?;
            transaction
                .execute(
                    r#"
                    UPDATE auth_device_credentials
                       SET revoked_at = ?1, revoked_by = ?2,
                           session_nonce_hash = '', pin_locked_until = ''
                     WHERE user_id = ?3 AND revoked_at IS NULL
                    "#,
                    params![now_iso(), principal.user_id, learner.id],
                )
                .map_err(|error| format!("无法撤销学习者设备凭据：{error}"))?;
            transaction
                .commit()
                .map_err(|error| format!("无法提交密码重置：{error}"))?;
            Ok(revoked.max(0) as usize)
        })
        .map_err(ApiError::internal)?;
    Ok(Json(json!({
        "ok": true,
        "device_credentials_revoked": revoked_devices
    })))
}

async fn multi_user_list_users(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
) -> ApiResult<Json<Value>> {
    require_admin(&principal)?;
    Ok(Json(json!({
        "users": list_accounts(&state)?
            .iter()
            .map(Account::public)
            .collect::<Vec<_>>()
    })))
}

async fn get_user_grants(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(user_id): Path<String>,
) -> ApiResult<Json<Value>> {
    require_admin(&principal)?;
    assignable_account(&state, &user_id)?;
    Ok(Json(json!({
        "grant": load_grant_value(&state, &user_id)?
    })))
}

async fn put_user_grants(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(user_id): Path<String>,
    Json(payload): Json<GrantPayload>,
) -> ApiResult<Json<Value>> {
    require_admin(&principal)?;
    let account = assignable_account(&state, &user_id)?;
    let grant = normalize_grant(&user_id, &payload.grant);
    if account.preset == "learner" && grant.get("learning_policy").is_none_or(Value::is_null) {
        return Err(ApiError::unprocessable("学习者账户必须保留学习策略"));
    }
    validate_grant(&grant)?;
    validate_grant_reading_extensions(&state, &grant)?;
    save_grant(&state, &user_id, &grant)?;
    Ok(Json(json!({"grant": grant})))
}

async fn get_user_book_permission(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(user_id): Path<String>,
) -> ApiResult<Json<Value>> {
    require_admin(&principal)?;
    let account = assignable_account(&state, &user_id)?;
    Ok(Json(json!({
        "permission": normalize_book_permission(&account.book_permission)
    })))
}

async fn put_user_book_permission(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(user_id): Path<String>,
    Json(payload): Json<BookPermissionPayload>,
) -> ApiResult<Json<Value>> {
    require_admin(&principal)?;
    let account = assignable_account(&state, &user_id)?;
    let catalog_ids = admin_book_catalog(&state)?
        .into_iter()
        .filter_map(|book| {
            book.get("book_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect::<BTreeSet<_>>();
    if let Some(unknown) = payload
        .books
        .keys()
        .find(|book_id| !catalog_ids.contains(book_id.as_str()))
    {
        return Err(ApiError::bad_request(format!(
            "未知共享书籍标识：{unknown}"
        )));
    }
    let permission = normalize_book_permission(&json!({
        "create": payload.create,
        "default": payload.default,
        "books": payload.books
    }));
    update_account_fields(
        &state,
        &account.id,
        &[(
            "book_permission_json",
            Value::String(permission.to_string()),
        )],
    )?;
    Ok(Json(json!({"permission": permission})))
}

async fn admin_install_skill(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Json(payload): Json<SkillInstallPayload>,
) -> ApiResult<Json<Value>> {
    require_admin(&principal)?;
    let reference = payload.reference.trim();
    if reference.is_empty() {
        return Err(ApiError::bad_request("技能引用不能为空"));
    }
    if !reference.starts_with("https://") && !reference.starts_with("http://") {
        return Err(ApiError::bad_request(
            "当前纯 Rust 版本仅支持通过 SKILL.md 的 HTTP(S) 地址安装技能",
        ));
    }
    let response = state
        .client
        .get(reference)
        .send()
        .await
        .map_err(|error| ApiError::provider(format!("无法下载技能：{error}")))?;
    if !response.status().is_success() {
        return Err(ApiError::provider(format!(
            "技能下载失败，远端返回 HTTP {}",
            response.status()
        )));
    }
    if response
        .content_length()
        .is_some_and(|length| length > 2 * 1024 * 1024)
    {
        return Err(ApiError::bad_request("技能文件不能超过 2 MiB"));
    }
    let body = response
        .text()
        .await
        .map_err(|error| ApiError::provider(format!("无法读取技能内容：{error}")))?;
    if body.len() > 2 * 1024 * 1024 {
        return Err(ApiError::bad_request("技能文件不能超过 2 MiB"));
    }
    let name = payload
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .or_else(|| frontmatter_name(&body))
        .or_else(|| {
            reference
                .rsplit('/')
                .next()
                .map(|name| name.trim_end_matches(".md"))
                .filter(|name| !name.is_empty())
                .map(str::to_string)
        })
        .ok_or_else(|| ApiError::bad_request("无法确定技能名称"))?;
    let slug = safe_skill_name(&name)?;
    let existing = state.database.get_resource("skill", &slug)?;
    if existing.is_some() && !payload.force {
        return Err(ApiError::conflict(format!("技能已存在：{slug}")));
    }
    let root = state.paths.data.join("skills");
    tokio::fs::create_dir_all(&root)
        .await
        .map_err(|error| ApiError::internal(format!("无法创建技能目录：{error}")))?;
    let target = root.join(&slug);
    tokio::fs::create_dir_all(&target)
        .await
        .map_err(|error| ApiError::internal(format!("无法创建技能文件夹：{error}")))?;
    tokio::fs::write(target.join("SKILL.md"), body.as_bytes())
        .await
        .map_err(|error| ApiError::internal(format!("无法保存技能文件：{error}")))?;
    let description = frontmatter_description(&body).unwrap_or_default();
    let skill = state.database.put_resource(
        "skill",
        &slug,
        &json!({
            "name": name,
            "description": description,
            "tags": [],
            "source": reference,
            "verified": payload.allow_unverified,
            "installed_at": now_iso(),
            "relative_path": format!("{slug}/SKILL.md")
        }),
    )?;
    Ok(Json(json!({
        "skill": skill,
        "verdict": {
            "status": if payload.allow_unverified {"unverified"} else {"accepted"},
            "detail": "已由本地 Rust 服务安装"
        },
        "version": Value::Null,
        "skipped": []
    })))
}

fn frontmatter_name(body: &str) -> Option<String> {
    frontmatter_value(body, "name")
}

fn frontmatter_description(body: &str) -> Option<String> {
    frontmatter_value(body, "description")
}

fn frontmatter_value(body: &str, key: &str) -> Option<String> {
    let rest = body.strip_prefix("---")?;
    let frontmatter = rest.split_once("\n---")?.0;
    frontmatter.lines().find_map(|line| {
        let (candidate, value) = line.split_once(':')?;
        (candidate.trim() == key)
            .then(|| value.trim().trim_matches(['"', '\'']).to_string())
            .filter(|value| !value.is_empty())
    })
}

fn safe_skill_name(value: &str) -> ApiResult<String> {
    let slug = value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    if slug.is_empty() || slug.len() > 64 {
        return Err(ApiError::bad_request("技能名称无法转换为有效标识"));
    }
    Ok(slug)
}

fn assignable_account(state: &AppState, user_id: &str) -> ApiResult<Account> {
    let account =
        account_by_id(state, user_id)?.ok_or_else(|| ApiError::not_found("用户不存在"))?;
    if account.role == "admin" {
        return Err(ApiError::forbidden(
            "管理员使用主工作区，不能接收用户级分配",
        ));
    }
    Ok(account)
}

fn account_map(state: &AppState) -> ApiResult<HashMap<String, Account>> {
    Ok(list_accounts(state)?
        .into_iter()
        .map(|account| (account.id.clone(), account))
        .collect())
}

fn normalized_permissions(values: Vec<String>) -> ApiResult<Vec<String>> {
    let permissions = values
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect::<BTreeSet<_>>();
    if permissions.is_empty()
        || permissions
            .iter()
            .any(|value| !GUARDIAN_PERMISSIONS.contains(&value.as_str()))
    {
        return Err(ApiError::unprocessable("守护权限为空或包含未知值"));
    }
    Ok(permissions.into_iter().collect())
}

fn guardian_relationships(
    state: &AppState,
    guardian_user_id: Option<&str>,
    learner_user_id: Option<&str>,
    include_revoked: bool,
) -> ApiResult<Vec<GuardianRelationship>> {
    state
        .database
        .with_connection(|connection| {
            let mut statement = connection
                .prepare(
                    r#"
                    SELECT id, guardian_user_id, learner_user_id, permissions_json,
                           granted_at, revoked_at, revoked_by, revocation_reason
                      FROM guardian_relationships
                     ORDER BY granted_at DESC, id
                    "#,
                )
                .map_err(|error| format!("无法查询守护关系：{error}"))?;
            let rows = statement
                .query_map([], relationship_from_row)
                .map_err(|error| format!("无法查询守护关系：{error}"))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|error| format!("无法读取守护关系：{error}"))
        })
        .map(|relationships| {
            relationships
                .into_iter()
                .filter(|relationship| {
                    (include_revoked || relationship.revoked_at.is_none())
                        && guardian_user_id
                            .is_none_or(|user_id| relationship.guardian_user_id == user_id)
                        && learner_user_id
                            .is_none_or(|user_id| relationship.learner_user_id == user_id)
                })
                .collect()
        })
        .map_err(ApiError::internal)
}

fn guardian_relationship_by_id(
    state: &AppState,
    relationship_id: &str,
) -> ApiResult<Option<GuardianRelationship>> {
    state
        .database
        .with_connection(|connection| {
            connection
                .query_row(
                    r#"
                    SELECT id, guardian_user_id, learner_user_id, permissions_json,
                           granted_at, revoked_at, revoked_by, revocation_reason
                      FROM guardian_relationships
                     WHERE id = ?1
                    "#,
                    params![relationship_id],
                    relationship_from_row,
                )
                .optional()
                .map_err(|error| format!("无法读取守护关系：{error}"))
        })
        .map_err(ApiError::internal)
}

fn relationship_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<GuardianRelationship> {
    let permissions_raw: String = row.get(3)?;
    let permissions = serde_json::from_str::<Value>(&permissions_raw)
        .ok()
        .and_then(|value| {
            value.as_array().map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|value| GUARDIAN_PERMISSIONS.contains(value))
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
        })
        .unwrap_or_default();
    Ok(GuardianRelationship {
        id: row.get(0)?,
        guardian_user_id: row.get(1)?,
        learner_user_id: row.get(2)?,
        permissions,
        granted_at: row.get(4)?,
        revoked_at: row.get(5)?,
        revoked_by: row.get(6)?,
        revocation_reason: row.get(7)?,
    })
}

fn revoke_relationship(
    state: &AppState,
    relationship_id: &str,
    revoked_by: &str,
    reason: &str,
) -> ApiResult<Option<GuardianRelationship>> {
    let existing = guardian_relationship_by_id(state, relationship_id)?;
    let Some(existing) = existing else {
        return Ok(None);
    };
    if existing.revoked_at.is_some() {
        return Ok(Some(existing));
    }
    state
        .database
        .with_connection(|connection| {
            connection
                .execute(
                    r#"
                    UPDATE guardian_relationships
                       SET revoked_at = ?1, revoked_by = ?2, revocation_reason = ?3
                     WHERE id = ?4 AND revoked_at IS NULL
                    "#,
                    params![now_iso(), revoked_by, reason, relationship_id],
                )
                .map_err(|error| format!("无法撤销守护关系：{error}"))?;
            Ok(())
        })
        .map_err(ApiError::internal)?;
    guardian_relationship_by_id(state, relationship_id)
}

fn guardian_access(
    state: &AppState,
    principal: &Principal,
    learner_user_id: &str,
    permission: &str,
) -> ApiResult<Account> {
    let learner = assignable_account(state, learner_user_id)?;
    if learner.preset != "learner" {
        return Err(ApiError::bad_request("目标账户不是学习者"));
    }
    if principal.is_admin() {
        return Ok(learner);
    }
    let guardian = assignable_account(state, &principal.user_id)?;
    if guardian.preset == "learner" {
        return Err(ApiError::forbidden("需要守护人授权"));
    }
    let allowed = guardian_relationships(state, Some(&guardian.id), Some(&learner.id), false)?
        .into_iter()
        .any(|relationship| {
            relationship
                .permissions
                .iter()
                .any(|candidate| candidate == permission)
        });
    if !allowed {
        return Err(ApiError::forbidden("需要守护人授权"));
    }
    Ok(learner)
}

fn book_permission_level<'a>(permission: &'a Value, book_id: &str) -> &'a str {
    permission
        .pointer(&format!("/books/{}", escape_json_pointer(book_id)))
        .and_then(Value::as_str)
        .or_else(|| permission.get("default").and_then(Value::as_str))
        .filter(|level| matches!(*level, "none" | "read" | "edit"))
        .unwrap_or("none")
}

fn escape_json_pointer(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

fn restriction_grant(state: &AppState, learner: &Account) -> ApiResult<Value> {
    let grant = load_grant_value(state, &learner.id)?;
    if grant.get("learning_policy").is_some_and(Value::is_object) {
        Ok(grant)
    } else if learner.preset == "learner" {
        Ok(learner_grant(&learner.id))
    } else {
        Ok(grant)
    }
}

fn guardian_restrictions(grant: &Value) -> ApiResult<Value> {
    let policy = grant
        .get("learning_policy")
        .and_then(Value::as_object)
        .ok_or_else(|| ApiError::conflict("学习者账户没有学习策略"))?;
    let reading = policy
        .get("reading")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    Ok(json!({
        "age_band": policy.get("age_band").cloned().unwrap_or(Value::Null),
        "allow_upload": reading
            .get("allow_upload")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "allowed_surfaces": policy
            .get("allowed_surfaces")
            .cloned()
            .unwrap_or_else(|| json!(["chat", "reading"])),
        "extensions": reading
            .get("extensions")
            .cloned()
            .unwrap_or_else(|| json!([]))
    }))
}

fn validate_restriction_payload(
    state: &AppState,
    payload: &GuardianRestrictionsPayload,
) -> ApiResult<()> {
    if !matches!(payload.age_band.as_str(), "6-8" | "9-12" | "13-15") {
        return Err(ApiError::unprocessable("学习策略年龄区间无效"));
    }
    if payload.allowed_surfaces.is_empty()
        || payload
            .allowed_surfaces
            .iter()
            .any(|surface| !matches!(surface.as_str(), "books" | "chat" | "reading"))
    {
        return Err(ApiError::unprocessable("学习入口为空或包含未知值"));
    }
    let allowed_extensions = allowed_reading_extension_ids(state)?;
    if let Some(unknown) = payload
        .extensions
        .iter()
        .find(|extension| !allowed_extensions.contains(extension.as_str()))
    {
        return Err(ApiError::bad_request(format!("未知阅读扩展：{unknown}")));
    }
    Ok(())
}

fn validate_grant_reading_extensions(state: &AppState, grant: &Value) -> ApiResult<()> {
    let Some(extensions) = grant.pointer("/learning_policy/reading/extensions") else {
        return Ok(());
    };
    let allowed_extensions = allowed_reading_extension_ids(state)?;
    if let Some(unknown) = extensions
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .find(|extension| !allowed_extensions.contains(*extension))
    {
        return Err(ApiError::bad_request(format!("未知阅读扩展：{unknown}")));
    }
    Ok(())
}

fn save_grant_with_profiles(
    state: &AppState,
    account: &Account,
    grant: &Value,
    preset: Option<&str>,
) -> ApiResult<()> {
    let Some(preset) = preset else {
        return save_grant(state, &account.id, grant);
    };
    validate_grant(grant)?;
    state
        .database
        .with_connection(|connection| {
            let transaction = connection
                .transaction()
                .map_err(|error| format!("无法开始学习者策略事务：{error}"))?;
            transaction
                .execute(
                    r#"
                    INSERT INTO user_grants(user_id, payload_json, updated_at)
                    VALUES(?1, ?2, ?3)
                    ON CONFLICT(user_id) DO UPDATE SET
                        payload_json = excluded.payload_json,
                        updated_at = excluded.updated_at
                    "#,
                    params![account.id, grant.to_string(), now_iso()],
                )
                .map_err(|error| format!("无法保存学习者授予：{error}"))?;
            transaction
                .execute(
                    "UPDATE auth_accounts SET preset = ?1, updated_at = ?2 WHERE id = ?3",
                    params![preset, now_iso(), account.id],
                )
                .map_err(|error| format!("无法更新学习者预设：{error}"))?;
            transaction
                .commit()
                .map_err(|error| format!("无法提交学习者策略：{error}"))?;
            Ok(())
        })
        .map_err(ApiError::internal)
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guardian_permissions_are_deduplicated_and_sorted() {
        let values = vec![
            "view_reports".to_string(),
            "assign_materials".to_string(),
            "view_reports".to_string(),
        ];
        assert_eq!(
            normalized_permissions(values).expect("权限有效"),
            vec!["assign_materials", "view_reports"]
        );
    }

    #[test]
    fn book_permission_level_uses_default_and_explicit_override() {
        let permission = normalize_book_permission(&json!({
            "create": true,
            "default": "none",
            "books": {"book/a": "edit"}
        }));
        assert_eq!(book_permission_level(&permission, "book/a"), "edit");
        assert_eq!(book_permission_level(&permission, "book-b"), "none");
    }

    #[test]
    fn skill_names_cannot_escape_the_skill_directory() {
        assert_eq!(safe_skill_name("A Safe Skill").unwrap(), "a-safe-skill");
        assert!(safe_skill_name("../../").is_err());
    }

    #[test]
    fn frontmatter_is_read_without_external_parser() {
        let body = "---\nname: demo-skill\ndescription: 示例技能\n---\n\n# 正文\n";
        assert_eq!(frontmatter_name(body).as_deref(), Some("demo-skill"));
        assert_eq!(frontmatter_description(body).as_deref(), Some("示例技能"));
    }
}
