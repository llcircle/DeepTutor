use super::error::{ApiError, ApiResult};
use super::{AppState, ScopedState};
use axum::extract::{Path, Query};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashSet};
use std::io::{Cursor, Read};
use std::path::{Component, Path as FilePath, PathBuf};
use uuid::Uuid;

const SKILL_DOMAIN: &str = "skill";
const BUILTIN_SOURCE: &str = "builtin";
const USER_SOURCE: &str = "user";
const MAX_SKILL_BYTES: u64 = 1_000_000;
const MAX_PACKAGE_BYTES: u64 = 40_000_000;
const MAX_PACKAGE_ENTRIES: usize = 600;

const BUILTIN_SKILLS: [(&str, &str); 5] = [
    (
        "docx",
        include_str!("../../../deeptutor/skills/builtin/docx/SKILL.md"),
    ),
    (
        "pdf",
        include_str!("../../../deeptutor/skills/builtin/pdf/SKILL.md"),
    ),
    (
        "pptx",
        include_str!("../../../deeptutor/skills/builtin/pptx/SKILL.md"),
    ),
    (
        "skill-creator",
        include_str!("../../../deeptutor/skills/builtin/skill-creator/SKILL.md"),
    ),
    (
        "xlsx",
        include_str!("../../../deeptutor/skills/builtin/xlsx/SKILL.md"),
    ),
];

#[derive(Debug, Deserialize)]
struct CreateSkillPayload {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct UpdateSkillPayload {
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    rename_to: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct InstallSkillPayload {
    #[serde(rename = "ref")]
    reference: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    force: bool,
    #[serde(default)]
    allow_unverified: bool,
}

#[derive(Debug, Deserialize)]
struct TagPayload {
    #[serde(default)]
    name: String,
    #[serde(default)]
    rename_to: String,
}

#[derive(Debug, Default, Deserialize)]
struct HubCatalogQuery {
    #[serde(default = "default_hub")]
    hub: String,
    #[serde(default)]
    q: String,
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct HubDetailQuery {
    slug: String,
    #[serde(default = "default_hub")]
    hub: String,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/skills/list", get(list_skills))
        .route("/skills/tags/list", get(list_tags))
        .route("/skills/tags/create", post(create_tag))
        .route("/skills/tags/{tag}", put(rename_tag).delete(delete_tag))
        .route("/skills/create", post(create_skill))
        .route("/skills/install", post(install_skill))
        .route("/skills/hub/catalog", get(hub_catalog))
        .route("/skills/hub/detail", get(hub_detail))
        .route(
            "/skills/{name}",
            get(get_skill).put(update_skill).delete(delete_skill),
        )
}

fn default_hub() -> String {
    "eduhub".to_string()
}

fn skills_root(state: &AppState) -> PathBuf {
    state.paths.data.join("skills")
}

fn skill_dir(state: &AppState, name: &str) -> ApiResult<PathBuf> {
    Ok(skills_root(state).join(validate_skill_name(name)?))
}

fn validate_skill_name(value: &str) -> ApiResult<String> {
    let candidate = value.trim().to_ascii_lowercase();
    if candidate.is_empty()
        || candidate.len() > 64
        || !candidate
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_alphanumeric())
        || !candidate
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
    {
        return Err(ApiError::bad_request(
            "技能名称必须匹配 ^[a-z0-9][a-z0-9-]{0,63}$",
        ));
    }
    Ok(candidate)
}

fn normalize_tag(value: &str) -> ApiResult<String> {
    let candidate = value.trim().to_ascii_lowercase();
    if candidate.is_empty()
        || candidate.len() > 32
        || !candidate
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_alphanumeric())
        || !candidate.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | ' ')
        })
    {
        return Err(ApiError::bad_request(
            "标签必须匹配 ^[a-z0-9][a-z0-9\\-_ ]{0,31}$",
        ));
    }
    Ok(candidate)
}

fn normalized_tags<I, S>(values: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut seen = HashSet::new();
    values
        .into_iter()
        .filter_map(|value| normalize_tag(value.as_ref()).ok())
        .filter(|tag| seen.insert(tag.clone()))
        .collect()
}

fn frontmatter(content: &str) -> Option<&str> {
    let rest = content.strip_prefix("---")?;
    let (head, _) = rest.split_once("\n---")?;
    Some(head)
}

fn frontmatter_value(content: &str, key: &str) -> Option<String> {
    frontmatter(content)?.lines().find_map(|line| {
        let (candidate, value) = line.split_once(':')?;
        (candidate.trim() == key)
            .then(|| value.trim().trim_matches(['"', '\'']).to_string())
            .filter(|value| !value.is_empty())
    })
}

fn frontmatter_tags(content: &str) -> Vec<String> {
    let Some(frontmatter) = frontmatter(content) else {
        return Vec::new();
    };
    let mut tags = Vec::new();
    let mut collecting = false;
    for line in frontmatter.lines() {
        let trimmed = line.trim();
        if let Some(raw) = trimmed.strip_prefix("tags:") {
            collecting = true;
            let raw = raw.trim();
            if raw.starts_with('[') && raw.ends_with(']') {
                tags.extend(
                    raw.trim_matches(['[', ']'])
                        .split(',')
                        .map(|value| value.trim().trim_matches(['"', '\'']))
                        .filter(|value| !value.is_empty())
                        .map(str::to_string),
                );
                collecting = false;
            }
            continue;
        }
        if collecting {
            if let Some(item) = trimmed.strip_prefix("- ") {
                tags.push(item.trim().trim_matches(['"', '\'']).to_string());
            } else if !trimmed.is_empty() && !line.starts_with(' ') {
                collecting = false;
            }
        }
    }
    normalized_tags(tags)
}

fn render_skill(name: &str, description: &str, content: &str, tags: &[String]) -> String {
    let body = content
        .strip_prefix("---")
        .and_then(|rest| rest.split_once("\n---"))
        .map(|(_, body)| body.trim_start_matches('-').trim_start())
        .unwrap_or_else(|| content.trim());
    let tag_line = if tags.is_empty() {
        "[]".to_string()
    } else {
        format!(
            "[{}]",
            tags.iter()
                .map(|tag| format!("\"{tag}\""))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    format!(
        "---\nname: {name}\ndescription: {description}\ntags: {tag_line}\n---\n\n{}\n",
        body.trim()
    )
}

fn skill_info(name: &str, content: &str, source: &str, tags: &[String]) -> Value {
    json!({
        "name": name,
        "description": frontmatter_value(content, "description").unwrap_or_default(),
        "tags": tags,
        "source": source,
        "read_only": source != USER_SOURCE
    })
}

fn atomic_write(path: &FilePath, bytes: &[u8]) -> ApiResult<()> {
    let parent = path
        .parent()
        .ok_or_else(|| ApiError::internal("技能文件路径缺少父目录"))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| ApiError::internal(format!("无法创建技能目录：{error}")))?;
    let temporary = parent.join(format!(".write-{}.tmp", Uuid::new_v4().simple()));
    std::fs::write(&temporary, bytes)
        .map_err(|error| ApiError::internal(format!("无法写入技能临时文件：{error}")))?;
    if path.exists() {
        std::fs::remove_file(path)
            .map_err(|error| ApiError::internal(format!("无法替换旧技能文件：{error}")))?;
    }
    std::fs::rename(&temporary, path)
        .map_err(|error| ApiError::internal(format!("无法提交技能文件：{error}")))
}

fn read_skill_metadata(state: &AppState, name: &str) -> ApiResult<Option<Value>> {
    state
        .database
        .get_resource(SKILL_DOMAIN, name)
        .map_err(ApiError::internal)
}

fn metadata_tags(metadata: &Value) -> Vec<String> {
    normalized_tags(
        metadata
            .get("tags")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str),
    )
}

fn write_metadata(
    state: &AppState,
    name: &str,
    description: &str,
    tags: &[String],
    source: &str,
) -> ApiResult<Value> {
    state
        .database
        .put_resource(
            SKILL_DOMAIN,
            name,
            &json!({
                "name": name,
                "description": description,
                "tags": tags,
                "source": source,
                "read_only": false,
                "updated_at": chrono::Utc::now().to_rfc3339()
            }),
        )
        .map_err(ApiError::internal)
}

async fn list_skills(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    let root = skills_root(&state);
    std::fs::create_dir_all(&root)
        .map_err(|error| ApiError::internal(format!("无法创建技能目录：{error}")))?;
    let mut skills = Vec::new();
    let mut names = BTreeSet::new();
    let mut entries = std::fs::read_dir(&root)
        .map_err(|error| ApiError::internal(format!("无法读取技能目录：{error}")))?
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        if !entry.path().is_dir() {
            continue;
        }
        let skill_file = entry.path().join("SKILL.md");
        let Ok(content) = std::fs::read_to_string(&skill_file) else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().to_string();
        if validate_skill_name(&name).is_err() {
            continue;
        }
        let stored = read_skill_metadata(&state, &name)?;
        let tags = stored
            .as_ref()
            .map(metadata_tags)
            .filter(|tags| !tags.is_empty())
            .unwrap_or_else(|| frontmatter_tags(&content));
        skills.push(skill_info(&name, &content, USER_SOURCE, &tags));
        names.insert(name);
    }
    for (name, content) in BUILTIN_SKILLS {
        if names.contains(name) {
            continue;
        }
        skills.push(skill_info(
            name,
            content,
            BUILTIN_SOURCE,
            &frontmatter_tags(content),
        ));
    }
    skills.sort_by(|left, right| {
        left.get("name")
            .and_then(Value::as_str)
            .cmp(&right.get("name").and_then(Value::as_str))
    });
    Ok(Json(json!({"skills": skills})))
}

/// 内建技能始终随二进制提供，因此不存在技能目录时仍可挂载读取工具。
pub fn has_skills(state: &AppState) -> Result<bool, String> {
    if !BUILTIN_SKILLS.is_empty() {
        return Ok(true);
    }
    let root = skills_root(state);
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("无法读取技能目录：{error}")),
    };
    Ok(entries
        .filter_map(Result::ok)
        .any(|entry| entry.path().is_dir() && entry.path().join("SKILL.md").is_file()))
}

async fn get_skill(
    ScopedState(state): ScopedState,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    let name = validate_skill_name(&name)?;
    let path = skill_dir(&state, &name)?.join("SKILL.md");
    if let Ok(content) = std::fs::read_to_string(&path) {
        let metadata = read_skill_metadata(&state, &name)?;
        let tags = metadata
            .as_ref()
            .map(metadata_tags)
            .filter(|tags| !tags.is_empty())
            .unwrap_or_else(|| frontmatter_tags(&content));
        let mut payload = skill_info(&name, &content, USER_SOURCE, &tags);
        payload["content"] = Value::String(content);
        return Ok(Json(payload));
    }
    if let Some((_, content)) = BUILTIN_SKILLS.iter().find(|(id, _)| *id == name) {
        let mut payload = skill_info(&name, content, BUILTIN_SOURCE, &frontmatter_tags(content));
        payload["content"] = Value::String((*content).to_string());
        return Ok(Json(payload));
    }
    Err(ApiError::not_found(format!("技能不存在：{name}")))
}

async fn create_skill(
    ScopedState(state): ScopedState,
    Json(payload): Json<CreateSkillPayload>,
) -> ApiResult<Json<Value>> {
    let name = validate_skill_name(&payload.name)?;
    let directory = skill_dir(&state, &name)?;
    if directory.exists()
        || BUILTIN_SKILLS.iter().any(|(builtin, _)| *builtin == name)
        || read_skill_metadata(&state, &name)?.is_some()
    {
        return Err(ApiError::conflict(format!("技能已存在：{name}")));
    }
    let tags = normalized_tags(payload.tags);
    let rendered = render_skill(&name, payload.description.trim(), &payload.content, &tags);
    atomic_write(&directory.join("SKILL.md"), rendered.as_bytes())?;
    let metadata = write_metadata(
        &state,
        &name,
        payload.description.trim(),
        &tags,
        USER_SOURCE,
    )?;
    Ok(Json(metadata))
}

async fn update_skill(
    ScopedState(state): ScopedState,
    Path(name): Path<String>,
    Json(payload): Json<UpdateSkillPayload>,
) -> ApiResult<Json<Value>> {
    let name = validate_skill_name(&name)?;
    let directory = skill_dir(&state, &name)?;
    let path = directory.join("SKILL.md");
    let current = std::fs::read_to_string(&path)
        .map_err(|_| ApiError::not_found(format!("技能不存在：{name}")))?;
    let mut target_name = name.clone();
    if let Some(rename_to) = payload.rename_to.as_deref() {
        target_name = validate_skill_name(rename_to)?;
        if target_name != name {
            let target = skill_dir(&state, &target_name)?;
            if target.exists() {
                return Err(ApiError::conflict(format!("技能已存在：{target_name}")));
            }
        }
    }
    let description = payload
        .description
        .unwrap_or_else(|| frontmatter_value(&current, "description").unwrap_or_default());
    let tags = payload.tags.map(normalized_tags).unwrap_or_else(|| {
        read_skill_metadata(&state, &name)
            .ok()
            .flatten()
            .map(|value| metadata_tags(&value))
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| frontmatter_tags(&current))
    });
    let content = payload.content.unwrap_or(current);
    let rendered = render_skill(&target_name, description.trim(), &content, &tags);
    if target_name != name {
        std::fs::rename(&directory, skill_dir(&state, &target_name)?)
            .map_err(|error| ApiError::internal(format!("无法重命名技能目录：{error}")))?;
        let _ = state.database.delete_resource(SKILL_DOMAIN, &name)?;
    }
    atomic_write(
        &skill_dir(&state, &target_name)?.join("SKILL.md"),
        rendered.as_bytes(),
    )?;
    let metadata = write_metadata(&state, &target_name, description.trim(), &tags, USER_SOURCE)?;
    Ok(Json(metadata))
}

async fn delete_skill(
    ScopedState(state): ScopedState,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    let name = validate_skill_name(&name)?;
    let directory = skill_dir(&state, &name)?;
    if !directory.exists() {
        if BUILTIN_SKILLS.iter().any(|(builtin, _)| *builtin == name) {
            return Err(ApiError::forbidden("内置技能为只读，不能删除"));
        }
        return Err(ApiError::not_found(format!("技能不存在：{name}")));
    }
    std::fs::remove_dir_all(&directory)
        .map_err(|error| ApiError::internal(format!("无法删除技能目录：{error}")))?;
    let _ = state.database.delete_resource(SKILL_DOMAIN, &name)?;
    Ok(Json(json!({"status": "deleted", "name": name})))
}

fn tags_path(state: &AppState) -> PathBuf {
    skills_root(state).join(".tags.json")
}

fn load_tag_vocabulary(state: &AppState) -> Vec<String> {
    let raw = std::fs::read_to_string(tags_path(state)).unwrap_or_default();
    serde_json::from_str::<Value>(&raw)
        .ok()
        .and_then(|value| value.get("tags").and_then(Value::as_array).cloned())
        .map(|values| normalized_tags(values.iter().filter_map(Value::as_str)))
        .unwrap_or_else(|| vec!["style".to_string(), "tool".to_string()])
}

fn save_tag_vocabulary(state: &AppState, tags: &[String]) -> ApiResult<()> {
    let path = tags_path(state);
    atomic_write(
        &path,
        serde_json::to_vec_pretty(&json!({"tags": normalized_tags(tags)}))
            .map_err(|error| ApiError::internal(format!("无法序列化标签：{error}")))?
            .as_slice(),
    )
}

async fn list_tags(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    let mut tags = load_tag_vocabulary(&state);
    for item in state.database.list_resources(SKILL_DOMAIN)? {
        for tag in metadata_tags(&item) {
            if !tags.contains(&tag) {
                tags.push(tag);
            }
        }
    }
    Ok(Json(json!({"tags": tags})))
}

async fn create_tag(
    ScopedState(state): ScopedState,
    Json(payload): Json<TagPayload>,
) -> ApiResult<Json<Value>> {
    let tag = normalize_tag(&payload.name)?;
    let mut tags = load_tag_vocabulary(&state);
    if tags.contains(&tag) {
        return Err(ApiError::conflict(format!("标签已存在：{tag}")));
    }
    tags.push(tag.clone());
    save_tag_vocabulary(&state, &tags)?;
    Ok(Json(json!({"name": tag})))
}

async fn rename_tag(
    ScopedState(state): ScopedState,
    Path(tag): Path<String>,
    Json(payload): Json<TagPayload>,
) -> ApiResult<Json<Value>> {
    let old = normalize_tag(&tag)?;
    let new = normalize_tag(&payload.rename_to)?;
    let mut tags = load_tag_vocabulary(&state);
    if !tags.contains(&old) {
        return Err(ApiError::not_found(format!("标签不存在：{old}")));
    }
    if old != new && tags.contains(&new) {
        return Err(ApiError::conflict(format!("标签已存在：{new}")));
    }
    for tag in &mut tags {
        if *tag == old {
            *tag = new.clone();
        }
    }
    tags = normalized_tags(tags);
    save_tag_vocabulary(&state, &tags)?;
    for mut item in state.database.list_resources(SKILL_DOMAIN)? {
        let mut changed = false;
        if let Some(values) = item.get_mut("tags").and_then(Value::as_array_mut) {
            for value in values {
                if value.as_str() == Some(old.as_str()) {
                    *value = Value::String(new.clone());
                    changed = true;
                }
            }
        }
        if changed {
            if let Some(id) = item.get("name").and_then(Value::as_str) {
                state.database.put_resource(SKILL_DOMAIN, id, &item)?;
            }
        }
    }
    Ok(Json(json!({"name": new})))
}

async fn delete_tag(
    ScopedState(state): ScopedState,
    Path(tag): Path<String>,
) -> ApiResult<Json<Value>> {
    let tag = normalize_tag(&tag)?;
    let mut tags = load_tag_vocabulary(&state);
    if !tags.contains(&tag) {
        return Err(ApiError::not_found(format!("标签不存在：{tag}")));
    }
    tags.retain(|value| value != &tag);
    save_tag_vocabulary(&state, &tags)?;
    for mut item in state.database.list_resources(SKILL_DOMAIN)? {
        let mut changed = false;
        if let Some(values) = item.get_mut("tags").and_then(Value::as_array_mut) {
            let before = values.len();
            values.retain(|value| value.as_str() != Some(tag.as_str()));
            changed = values.len() != before;
        }
        if changed {
            if let Some(id) = item.get("name").and_then(Value::as_str) {
                state.database.put_resource(SKILL_DOMAIN, id, &item)?;
            }
        }
    }
    Ok(Json(json!({"status": "deleted", "name": tag})))
}

fn hub_base_url(hub: &str, state: &AppState) -> ApiResult<String> {
    if let Some(config) = state.database.get_setting("skill_hubs")? {
        if let Some(value) = config
            .pointer(&format!(
                "/hubs/{}/base_url",
                hub.replace('~', "~0").replace('/', "~1")
            ))
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
        {
            return Ok(value.trim_end_matches('/').to_string());
        }
    }
    match hub {
        "eduhub" => Ok("https://eduhub.deeptutor.info/api/v1".to_string()),
        "clawhub" => Ok("https://clawhub.ai/api/v1".to_string()),
        _ => Err(ApiError::bad_request(format!("未知技能中心：{hub}"))),
    }
}

fn hub_web_origin(base: &str) -> String {
    base.find("/api/")
        .map(|index| base[..index].to_string())
        .unwrap_or_else(|| base.trim_end_matches("/api/v1").to_string())
}

async fn hub_catalog(
    ScopedState(state): ScopedState,
    Query(query): Query<HubCatalogQuery>,
) -> ApiResult<Json<Value>> {
    let hub = query.hub.trim().to_ascii_lowercase();
    let base = hub_base_url(&hub, &state)?;
    let limit = query.limit.unwrap_or(50).clamp(1, 100);
    let endpoint = if query.q.trim().is_empty() {
        format!("{base}/skills")
    } else {
        format!("{base}/search")
    };
    let mut request = state
        .client
        .get(endpoint)
        .query(&[("limit", limit.to_string())]);
    if !query.q.trim().is_empty() {
        request = request.query(&[("q", query.q.trim())]);
    } else {
        request = request.query(&[("sort", "createdAt")]);
    }
    let response = request
        .send()
        .await
        .map_err(|error| ApiError::provider(format!("技能中心请求失败：{error}")))?;
    if !response.status().is_success() {
        return Err(ApiError::provider(format!(
            "技能中心返回 HTTP {}",
            response.status()
        )));
    }
    let payload = response
        .json::<Value>()
        .await
        .map_err(|error| ApiError::provider(format!("技能中心返回无效 JSON：{error}")))?;
    let rows = payload
        .as_array()
        .cloned()
        .or_else(|| {
            ["results", "skills", "items"]
                .iter()
                .find_map(|key| payload.get(*key).and_then(Value::as_array).cloned())
        })
        .unwrap_or_default();
    let skills = rows
        .into_iter()
        .filter_map(|row| {
            let slug = row.get("slug").and_then(Value::as_str)?.trim();
            if slug.is_empty() {
                return None;
            }
            let stats = row.get("stats").cloned().unwrap_or_else(|| json!({}));
            let owner = row.get("owner").cloned().unwrap_or_else(|| json!({}));
            Some(json!({
                "slug": slug,
                "owner_handle": row.get("ownerHandle").or_else(|| owner.get("handle")).cloned().unwrap_or_else(|| json!("")),
                "name": row.get("displayName").or_else(|| row.get("name")).cloned().unwrap_or_else(|| json!(slug)),
                "summary": row.get("summary").or_else(|| row.get("description")).cloned().unwrap_or_else(|| json!("")),
                "version": row.get("version").cloned().unwrap_or_else(|| json!("")),
                "downloads": stats.get("downloads").cloned().unwrap_or_else(|| json!(0)),
                "stars": stats.get("stars").cloned().unwrap_or_else(|| json!(0)),
                "owner": owner.get("displayName").or_else(|| row.get("ownerHandle")).cloned().unwrap_or_else(|| json!("")),
                "owner_url": owner.get("htmlUrl").cloned().unwrap_or_else(|| json!("")),
                "updated_at": row.get("updatedAt").cloned().unwrap_or(Value::Null)
            }))
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({
        "hub": hub,
        "web_url": hub_web_origin(&base),
        "skills": skills
    })))
}

async fn hub_detail(
    ScopedState(state): ScopedState,
    Query(query): Query<HubDetailQuery>,
) -> ApiResult<Json<Value>> {
    let hub = query.hub.trim().to_ascii_lowercase();
    let base = hub_base_url(&hub, &state)?;
    let (owner, slug) = query.slug.split_once('/').unwrap_or(("", &query.slug));
    let response = state
        .client
        .get(format!("{base}/skills/{slug}"))
        .query(&if owner.is_empty() {
            Vec::new()
        } else {
            vec![("ownerHandle", owner)]
        })
        .send()
        .await
        .map_err(|error| ApiError::provider(format!("技能详情请求失败：{error}")))?;
    if !response.status().is_success() {
        return Err(ApiError::provider(format!(
            "技能中心返回 HTTP {}",
            response.status()
        )));
    }
    let payload = response
        .json::<Value>()
        .await
        .map_err(|error| ApiError::provider(format!("技能详情 JSON 无效：{error}")))?;
    let skill = payload.get("skill").unwrap_or(&payload);
    let latest = payload
        .pointer("/distTags/latest")
        .or_else(|| skill.get("latestVersion"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let keywords = skill
        .get("keywords")
        .and_then(Value::as_array)
        .or_else(|| skill.get("tags").and_then(Value::as_array))
        .cloned()
        .unwrap_or_default();
    Ok(Json(json!({
        "slug": slug,
        "name": skill.get("displayName").or_else(|| skill.get("name")).cloned().unwrap_or_else(|| json!(slug)),
        "summary": skill.get("summary").or_else(|| skill.get("description")).cloned().unwrap_or_else(|| json!("")),
        "version": latest,
        "downloads": skill.pointer("/stats/downloads").cloned().unwrap_or_else(|| json!(0)),
        "stars": skill.pointer("/stats/stars").cloned().unwrap_or_else(|| json!(0)),
        "owner": payload.pointer("/owner/displayName").or_else(|| skill.pointer("/owner/displayName")).cloned().unwrap_or_else(|| json!("")),
        "owner_url": payload.pointer("/owner/htmlUrl").or_else(|| skill.pointer("/owner/htmlUrl")).cloned().unwrap_or_else(|| json!("")),
        "content": skill.get("description").cloned().unwrap_or_else(|| json!("")),
        "tags": normalized_tags(keywords.iter().filter_map(Value::as_str)),
        "web_url": format!("{}/skills/{slug}", hub_web_origin(&base))
    })))
}

fn parse_hub_reference(reference: &str) -> ApiResult<(String, String, Option<String>)> {
    let reference = reference.trim();
    if reference.is_empty() || reference.len() > 256 {
        return Err(ApiError::bad_request("技能引用不能为空且不能超过 256 字符"));
    }
    let (hub, rest) = reference
        .split_once(':')
        .filter(|(hub, _)| {
            !hub.is_empty()
                && hub.chars().all(|character| {
                    character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
                })
        })
        .unwrap_or(("eduhub", reference));
    let (slug, version) = rest
        .rsplit_once('@')
        .filter(|(_, version)| !version.is_empty())
        .map(|(slug, version)| (slug, Some(version.to_string())))
        .unwrap_or((rest, None));
    let valid = slug.split('/').all(|part| {
        !part.is_empty()
            && part.len() <= 128
            && part.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
            })
    });
    if !valid {
        return Err(ApiError::bad_request("技能引用格式无效"));
    }
    Ok((hub.to_string(), slug.to_string(), version))
}

fn allowed_support_file(path: &FilePath) -> bool {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(
        extension.as_str(),
        "" | "md"
            | "markdown"
            | "txt"
            | "rst"
            | "py"
            | "js"
            | "mjs"
            | "ts"
            | "sh"
            | "rb"
            | "lua"
            | "r"
            | "json"
            | "jsonl"
            | "yaml"
            | "yml"
            | "toml"
            | "ini"
            | "cfg"
            | "csv"
            | "tsv"
            | "html"
            | "htm"
            | "css"
            | "xml"
            | "sql"
            | "jinja"
            | "j2"
            | "tmpl"
            | "template"
    )
}

fn safe_member_path(raw: &str) -> Option<PathBuf> {
    let normalized = raw.replace('\\', "/");
    let path = FilePath::new(&normalized);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return None;
    }
    let path = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value),
            Component::CurDir => None,
            _ => None,
        })
        .collect::<PathBuf>();
    if path.as_os_str().is_empty()
        || path.components().any(|component| {
            component
                .as_os_str()
                .to_string_lossy()
                .starts_with("__MACOSX")
        })
    {
        None
    } else {
        Some(path)
    }
}

fn extract_skill_archive(bytes: &[u8], destination: &FilePath) -> ApiResult<PathBuf> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|error| ApiError::bad_request(format!("技能包不是有效 ZIP：{error}")))?;
    if archive.len() > MAX_PACKAGE_ENTRIES {
        return Err(ApiError::bad_request("技能包文件数量超过限制"));
    }
    std::fs::create_dir_all(destination)
        .map_err(|error| ApiError::internal(format!("无法创建技能解压目录：{error}")))?;
    let mut total = 0_u64;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| ApiError::bad_request(format!("无法读取技能包条目：{error}")))?;
        if entry.is_dir() {
            continue;
        }
        let Some(relative) = safe_member_path(entry.name()) else {
            return Err(ApiError::bad_request("技能包包含不安全路径"));
        };
        let size = entry.size();
        total = total.saturating_add(size);
        if total > MAX_PACKAGE_BYTES {
            return Err(ApiError::bad_request("技能包解压后超过 40 MiB"));
        }
        if size > MAX_SKILL_BYTES {
            return Err(ApiError::bad_request(format!(
                "技能包条目过大：{}",
                relative.display()
            )));
        }
        let target = destination.join(&relative);
        if !target.starts_with(destination) {
            return Err(ApiError::bad_request("技能包路径逃逸"));
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| ApiError::internal(format!("无法创建技能目录：{error}")))?;
        }
        let mut data = Vec::with_capacity(size as usize);
        entry
            .read_to_end(&mut data)
            .map_err(|error| ApiError::bad_request(format!("无法解压技能文件：{error}")))?;
        std::fs::write(&target, data)
            .map_err(|error| ApiError::internal(format!("无法保存技能文件：{error}")))?;
    }
    if (destination.join("SKILL.md")).is_file() {
        return Ok(destination.to_path_buf());
    }
    let directories = std::fs::read_dir(destination)
        .map_err(|error| ApiError::internal(format!("无法读取技能包目录：{error}")))?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .collect::<Vec<_>>();
    if directories.len() == 1 && directories[0].path().join("SKILL.md").is_file() {
        return Ok(directories[0].path());
    }
    Err(ApiError::bad_request("技能包缺少 SKILL.md"))
}

fn copy_skill_tree(source: &FilePath, destination: &FilePath) -> ApiResult<Vec<Value>> {
    let mut skipped = Vec::new();
    let mut count = 0usize;
    let mut total = 0u64;
    for item in walkdir::WalkDir::new(source).follow_links(false) {
        let item = item.map_err(|error| ApiError::internal(format!("无法读取技能树：{error}")))?;
        if !item.file_type().is_file() {
            continue;
        }
        let relative = item
            .path()
            .strip_prefix(source)
            .map_err(|_| ApiError::internal("技能路径无法映射"))?;
        if relative.starts_with("__MACOSX")
            || relative
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with('.'))
        {
            continue;
        }
        if relative != FilePath::new("SKILL.md") && !allowed_support_file(relative) {
            skipped.push(json!({
                "path": relative.to_string_lossy(),
                "reason": "不支持的文件类型"
            }));
            continue;
        }
        let size = item.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        total = total.saturating_add(size);
        count += 1;
        if size > MAX_SKILL_BYTES || total > 20_000_000 || count > 500 {
            return Err(ApiError::bad_request("技能包内容超过导入限制"));
        }
        let target = destination.join(relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| ApiError::internal(format!("无法创建技能子目录：{error}")))?;
        }
        std::fs::copy(item.path(), &target)
            .map_err(|error| ApiError::internal(format!("无法复制技能文件：{error}")))?;
    }
    Ok(skipped)
}

async fn install_skill(
    ScopedState(state): ScopedState,
    Json(payload): Json<InstallSkillPayload>,
) -> ApiResult<Json<Value>> {
    let (hub, slug, version) = parse_hub_reference(&payload.reference)?;
    let base = hub_base_url(&hub, &state)?;
    let (owner, plain_slug) = slug.split_once('/').unwrap_or(("", slug.as_str()));
    let verdict = match state
        .client
        .get(format!("{base}/skills/{plain_slug}/verify"))
        .query(&[
            ("tag", if version.is_some() { "" } else { "latest" }),
            ("version", version.as_deref().unwrap_or("")),
            ("ownerHandle", owner),
        ])
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => {
            let value = response.json::<Value>().await.unwrap_or_else(|_| json!({}));
            if value.get("ok").and_then(Value::as_bool) == Some(true) {
                json!({"status": "ok", "detail": value.get("decision").and_then(Value::as_str).unwrap_or("")})
            } else {
                json!({"status": "suspicious", "detail": value.get("decision").and_then(Value::as_str).unwrap_or("技能中心标记该包可疑")})
            }
        }
        Ok(response) => {
            json!({"status": "unknown", "detail": format!("校验接口返回 HTTP {}", response.status())})
        }
        Err(error) => json!({"status": "unknown", "detail": error.to_string()}),
    };
    if verdict.get("status").and_then(Value::as_str) == Some("suspicious")
        && !payload.allow_unverified
    {
        return Err(ApiError::bad_request(format!(
            "技能中心将 {slug} 标记为可疑；如确认可信，请允许未验证安装。{}",
            verdict
                .get("detail")
                .and_then(Value::as_str)
                .map(|detail| format!("（{detail}）"))
                .unwrap_or_default()
        )));
    }
    let response = state
        .client
        .get(format!("{base}/download"))
        .query(&[
            ("slug", plain_slug),
            ("version", version.as_deref().unwrap_or("")),
            ("tag", if version.is_some() { "" } else { "latest" }),
            ("ownerHandle", owner),
        ])
        .send()
        .await
        .map_err(|error| ApiError::provider(format!("技能下载失败：{error}")))?;
    if !response.status().is_success() {
        return Err(ApiError::provider(format!(
            "技能下载失败，远端返回 HTTP {}",
            response.status()
        )));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PACKAGE_BYTES)
    {
        return Err(ApiError::bad_request("技能包超过 40 MiB"));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|error| ApiError::provider(format!("读取技能包失败：{error}")))?;
    let work = state
        .paths
        .temp
        .join(format!("skill-import-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&work)
        .map_err(|error| ApiError::internal(format!("无法创建技能临时目录：{error}")))?;
    let result = (|| -> ApiResult<(PathBuf, String)> {
        let extracted = work.join("extracted");
        let package_root = if bytes.starts_with(b"PK\x03\x04") {
            extract_skill_archive(&bytes, &extracted)?
        } else {
            std::fs::create_dir_all(&extracted)
                .map_err(|error| ApiError::internal(format!("无法创建技能临时目录：{error}")))?;
            std::fs::write(extracted.join("SKILL.md"), &bytes)
                .map_err(|error| ApiError::internal(format!("无法保存技能文件：{error}")))?;
            extracted.clone()
        };
        let content = std::fs::read_to_string(package_root.join("SKILL.md"))
            .map_err(|error| ApiError::bad_request(format!("无法读取 SKILL.md：{error}")))?;
        if content.len() as u64 > MAX_SKILL_BYTES {
            return Err(ApiError::bad_request("SKILL.md 超过 1 MiB"));
        }
        let fallback = plain_slug.rsplit('/').next().unwrap_or(plain_slug);
        let name = payload
            .name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .or_else(|| frontmatter_value(&content, "name"))
            .unwrap_or_else(|| fallback.to_string());
        let name = validate_skill_name(&name)?;
        let destination = skill_dir(&state, &name)?;
        if destination.exists() && !payload.force {
            return Err(ApiError::conflict(format!("技能已存在：{name}")));
        }
        if destination.exists() {
            std::fs::remove_dir_all(&destination)
                .map_err(|error| ApiError::internal(format!("无法替换旧技能：{error}")))?;
        }
        let skipped = copy_skill_tree(&package_root, &destination)?;
        let mut tags = frontmatter_tags(&content);
        if !tags.contains(&hub) {
            tags.push(hub.clone());
        }
        let description = frontmatter_value(&content, "description").unwrap_or_default();
        write_metadata(&state, &name, &description, &tags, USER_SOURCE)?;
        let lock_path = skills_root(&state).join(".hub-lock.json");
        let mut lock = std::fs::read_to_string(&lock_path)
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .unwrap_or_else(|| json!({"skills": {}}));
        if !lock.get("skills").is_some_and(Value::is_object) {
            lock["skills"] = json!({});
        }
        lock["skills"][name.as_str()] = json!({
            "hub": hub,
            "slug": plain_slug,
            "owner_handle": owner,
            "version": version.clone().unwrap_or_default(),
            "verdict": verdict.get("status").cloned().unwrap_or_else(|| json!("unknown")),
            "installed_at": chrono::Utc::now().to_rfc3339(),
            "skipped": skipped
        });
        atomic_write(
            &lock_path,
            serde_json::to_vec_pretty(&lock)
                .map_err(|error| ApiError::internal(format!("无法序列化技能来源：{error}")))?
                .as_slice(),
        )?;
        Ok((destination, name))
    })();
    let _ = std::fs::remove_dir_all(&work);
    let (_, name) = result?;
    Ok(Json(json!({
        "skill": {
            "name": name,
            "description": read_skill_metadata(&state, &name)?
                .and_then(|value| value.get("description").cloned())
                .unwrap_or_else(|| json!("")),
            "tags": read_skill_metadata(&state, &name)?
                .map(|value| metadata_tags(&value))
                .unwrap_or_default(),
            "source": USER_SOURCE,
            "read_only": false
        },
        "verdict": verdict,
        "version": version.unwrap_or_default(),
        "skipped": []
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_name_validation_rejects_path_escape() {
        assert_eq!(validate_skill_name("Safe-Skill").unwrap(), "safe-skill");
        assert!(validate_skill_name("../escape").is_err());
        assert!(validate_skill_name("bad/name").is_err());
    }

    #[test]
    fn frontmatter_tags_support_inline_and_list_shapes() {
        assert_eq!(
            frontmatter_tags("---\nname: demo\ntags: [style, tool]\n---\n"),
            vec!["style", "tool"]
        );
        assert_eq!(
            frontmatter_tags("---\ntags:\n  - style\n  - \"tool\"\n---\n"),
            vec!["style", "tool"]
        );
    }

    #[test]
    fn archive_paths_reject_traversal() {
        assert!(safe_member_path("../secret").is_none());
        assert!(safe_member_path("/absolute").is_none());
        assert_eq!(
            safe_member_path("references/example.md").unwrap(),
            PathBuf::from("references/example.md")
        );
    }
}
