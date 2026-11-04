use super::*;
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use std::sync::Arc;
use std::time::Duration;

#[derive(Default)]
pub(crate) struct IndexRuntime {
    gate: tokio::sync::Mutex<()>,
    jobs: parking_lot::Mutex<HashMap<String, (String, CancellationToken)>>,
    cache: parking_lot::Mutex<HashMap<String, Arc<Vec<VectorRow>>>>,
    dirty: parking_lot::Mutex<HashSet<String>>,
}

#[derive(Clone)]
struct VectorRow {
    material_id: String,
    locator: usize,
    kind: String,
    start: usize,
    end: usize,
    text_hash: String,
    vector: Vec<f32>,
}

#[derive(Clone)]
struct EmbeddingConfig {
    profile: Value,
    model: String,
    endpoint: String,
    binding: String,
    signature: String,
}

#[derive(Clone, Debug)]
struct Chunk {
    kind: &'static str,
    ordinal: usize,
    start: usize,
    end: usize,
    text: String,
}

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/reading/retrieval/settings",
            get(get_settings).put(put_settings),
        )
        .route(
            "/reading/workspaces/{workspace_id}/index",
            get(get_status).post(rebuild).delete(delete_index),
        )
        .route(
            "/reading/workspaces/{workspace_id}/search",
            post(search_api),
        )
}

fn db_error(error: rusqlite::Error) -> ApiError {
    ApiError::internal(format!("阅读检索数据库操作失败：{error}"))
}

fn settings(state: &AppState) -> ApiResult<Value> {
    Ok(state
        .database
        .get_setting("reading_retrieval")?
        .unwrap_or_else(|| json!({"enabled": false, "consented": false})))
}

fn enabled(state: &AppState) -> bool {
    settings(state).is_ok_and(|s| s["enabled"] == true && s["consented"] == true)
}

async fn get_settings(ScopedState(state): ScopedState) -> ApiResult<Json<Value>> {
    let mut value = settings(&state)?;
    match config(&state) {
        Ok(config) => {
            value["model_available"] = json!(true);
            value["model"] = json!(config.model);
            value["model_error"] = Value::Null;
        }
        Err(error) => {
            value["model_available"] = json!(false);
            value["model"] = Value::Null;
            value["model_error"] = json!(error);
        }
    }
    value["strategy"] = json!("page_paragraph_keyword_rrf");
    Ok(Json(value))
}

async fn put_settings(
    ScopedState(state): ScopedState,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let previous = settings(&state)?;
    let on = body["enabled"]
        .as_bool()
        .ok_or_else(|| ApiError::bad_request("enabled 必须为布尔值。"))?;
    if on && body["consented"] != true && previous["consented"] != true {
        return Err(ApiError::bad_request(
            "首次开启前请确认：材料文本会发送到您配置的向量服务。",
        ));
    }
    let value = json!({"enabled": on, "consented": body["consented"] == true || previous["consented"] == true});
    state.database.set_setting("reading_retrieval", &value)?;
    if !on {
        for (_, token) in state.reading_index.jobs.lock().values() {
            token.cancel();
        }
    } else if let Some(id) = body["workspace_id"].as_str().filter(|id| !id.is_empty()) {
        workspace_record(&state, id)?;
        schedule(&state, id);
    }
    get_settings(ScopedState(state)).await
}

fn config(state: &AppState) -> Result<EmbeddingConfig, String> {
    let catalog = state
        .database
        .get_setting("catalog")?
        .unwrap_or_else(|| json!({}));
    config_from_catalog(&catalog)
}

fn config_from_catalog(catalog: &Value) -> Result<EmbeddingConfig, String> {
    let service = &catalog["services"]["embedding"];
    let selection = |field: &str| {
        service[field]
            .as_str()
            .map(str::trim)
            .filter(|id| !id.is_empty())
    };
    let profile_id = selection("active_profile_id");
    let model_id = selection("active_model_id");
    let profiles = service["profiles"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|profile| profile["provider_only"] != true)
        .filter(|profile| profile_id.is_none_or(|id| profile["id"] == id))
        .collect::<Vec<_>>();
    if profile_id.is_some() && profiles.is_empty() {
        return Err("默认向量模型的配置已不存在，请在设置 → 向量模型重新设为默认。".into());
    }
    let candidates = profiles
        .iter()
        .flat_map(|profile| {
            profile["models"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|model| {
                    model["model"]
                        .as_str()
                        .is_some_and(|name| !name.trim().is_empty())
                        && model_id.is_none_or(|id| model["id"] == id)
                })
                .map(move |model| (*profile, model))
        })
        .collect::<Vec<_>>();
    let (profile, model) = match candidates.as_slice() {
        [selected] => *selected,
        [] if model_id.is_some() || profile_id.is_some() => {
            return Err(
                "默认向量模型已不存在或模型标识为空，请在设置 → 向量模型检查并重新设为默认。"
                    .into(),
            )
        }
        [] => return Err("未配置向量模型，继续使用关键词检索。".into()),
        _ => return Err("已配置多个向量模型，请在设置 → 向量模型将其中一个设为默认。".into()),
    };
    // 设置页允许只保存模型；没有默认选择时仅采用唯一模型，不猜测多个账户。
    // 已指定的默认项失效或提供方引用错误时保留明确错误，不切换到其他服务。
    let profile = super::super::catalog::resolve_profile_provider(
        catalog,
        "embedding",
        profile,
        Some(model),
    )?;
    let name = model["model"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "向量模型标识为空。".to_string())?
        .trim()
        .to_string();
    let base = profile["base_url"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "向量服务地址为空，请检查提供方关联。".to_string())?;
    let binding = profile["binding"].as_str().unwrap_or("openai").to_string();
    let endpoint = embedding_endpoint(base, &binding)?;
    let signature = revisions::hash(&format!(
        "{endpoint}|{name}|{binding}|{}|{}",
        profile["id"], model["id"]
    ));
    Ok(EmbeddingConfig {
        profile,
        model: name,
        endpoint,
        binding,
        signature,
    })
}

fn embedding_endpoint(base: &str, binding: &str) -> Result<String, String> {
    let suffix = match binding {
        "ollama" => "/api/embed",
        "cohere" => "/v2/embed",
        _ => return llm::endpoint(base, &["/embeddings"]),
    };
    // 原生协议复用提供方地址解析，避免裸域名被通用 LLM 逻辑补上 /v1。
    let endpoint = super::super::catalog::provider_endpoint(base, "", "embedding", binding);
    llm::endpoint(&endpoint, &[suffix])
}

fn decode_vectors(payload: &Value, expected: usize) -> Result<Vec<Vec<f32>>, String> {
    let arrays = payload["embeddings"]["float"]
        .as_array()
        .or_else(|| payload["data"].as_array())
        .or_else(|| payload["embeddings"].as_array())
        .or_else(|| {
            payload
                .pointer("/output/embeddings")
                .and_then(Value::as_array)
        })
        .ok_or_else(|| "向量响应缺少批量向量。".to_string())?;
    if arrays.len() != expected {
        return Err("向量响应数量与输入不一致。".into());
    }
    let mut rows = arrays
        .iter()
        .enumerate()
        .map(|(position, row)| {
            let ordinal = row
                .get("index")
                .and_then(Value::as_u64)
                .map(|i| i as usize)
                .unwrap_or(position);
            let vector = row
                .as_array()
                .or_else(|| row["embedding"].as_array())
                .or_else(|| row["vector"].as_array())
                .ok_or_else(|| "向量响应格式无效。".to_string())?
                .iter()
                .map(|n| {
                    n.as_f64()
                        .filter(|v| v.is_finite())
                        .map(|v| v as f32)
                        .ok_or_else(|| "向量含非有限数值。".to_string())
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok((ordinal, normalize(vector)?))
        })
        .collect::<Result<Vec<_>, String>>()?;
    rows.sort_by_key(|r| r.0);
    if rows.iter().enumerate().any(|(i, row)| i != row.0) {
        return Err("向量响应序号重复或缺失。".into());
    }
    let dimension = rows.first().map(|r| r.1.len()).unwrap_or(0);
    if dimension == 0 || dimension > 65_536 || rows.iter().any(|r| r.1.len() != dimension) {
        return Err("向量维度为空、过大或不一致。".into());
    }
    Ok(rows.into_iter().map(|r| r.1).collect())
}

fn normalize(mut vector: Vec<f32>) -> Result<Vec<f32>, String> {
    let norm = vector
        .iter()
        .map(|v| (*v as f64).powi(2))
        .sum::<f64>()
        .sqrt();
    if !norm.is_finite() || norm <= 0.0 {
        return Err("向量必须包含有限的非零数值。".into());
    }
    for n in &mut vector {
        *n = (*n as f64 / norm) as f32;
    }
    Ok(vector)
}

async fn embed(
    state: &AppState,
    config: &EmbeddingConfig,
    texts: &[String],
    query: bool,
    cancel: &CancellationToken,
) -> Result<Vec<Vec<f32>>, String> {
    let mut all = Vec::new();
    let mut dimension = None;
    for batch in texts.chunks(32) {
        let body = if config.binding == "cohere" {
            json!({"model":config.model,"texts":batch,"input_type":if query {"search_query"} else {"search_document"},"embedding_types":["float"]})
        } else {
            json!({"model":config.model,"input":batch})
        };
        let mut attempt = 0u32;
        let payload = loop {
            if cancel.is_cancelled() {
                return Err("向量任务已取消。".into());
            }
            let mut request = state
                .client
                .post(&config.endpoint)
                .timeout(Duration::from_secs(60))
                .json(&body);
            if let Some(key) = config.profile["api_key"]
                .as_str()
                .filter(|s| !s.is_empty() && *s != "********")
            {
                request = request.bearer_auth(key);
            }
            for (name, value) in super::super::catalog::extra_headers(&config.profile) {
                request = request.header(name, value);
            }
            let response = tokio::select! {
                _ = cancel.cancelled() => return Err("向量任务已取消。".into()),
                response = request.send() => response.map_err(|e| format!("无法连接配置的向量服务：{e}"))?
            };
            if (response.status().as_u16() == 429 || response.status().is_server_error())
                && attempt < 3
            {
                attempt += 1;
                tokio::select! {
                    _ = cancel.cancelled() => return Err("向量任务已取消。".into()),
                    _ = tokio::time::sleep(Duration::from_millis(250 * (1 << attempt))) => {}
                }
                continue;
            }
            if !response.status().is_success() {
                return Err(format!(
                    "向量服务返回 HTTP {}，已保留关键词检索。",
                    response.status()
                ));
            }
            if response.content_length().unwrap_or(0) > 32 * 1024 * 1024 {
                return Err("向量响应超过 32 MB 限制。".into());
            }
            let bytes = tokio::select! {
                _ = cancel.cancelled() => return Err("向量任务已取消。".into()),
                bytes = response.bytes() => bytes.map_err(|e| format!("无法读取向量响应：{e}"))?
            };
            if bytes.len() > 32 * 1024 * 1024 {
                return Err("向量响应超过 32 MB 限制。".into());
            }
            break serde_json::from_slice::<Value>(&bytes)
                .map_err(|_| "向量服务返回无效 JSON。".to_string())?;
        };
        let vectors = decode_vectors(&payload, batch.len())?;
        let size = vectors.first().map(Vec::len).unwrap_or(0);
        if dimension.is_some_and(|d| d != size) {
            return Err("向量服务在不同批次返回了不同维度。".into());
        }
        dimension = Some(size);
        all.extend(vectors);
    }
    Ok(all)
}

fn chunks(text: &str) -> Vec<Chunk> {
    let chars = text.chars().collect::<Vec<_>>();
    if text.trim().is_empty() {
        return Vec::new();
    }
    let mut rows = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let max_end = (start + 1600).min(chars.len());
        let target = (start + 1200).min(max_end);
        let min = (start + 800).min(target);
        let end = if target == chars.len() {
            target
        } else {
            (min..target)
                .rev()
                .find(|&i| chars[i] == '\n' && i > 0 && chars[i - 1] == '\n')
                .or_else(|| {
                    (min..max_end)
                        .rev()
                        .find(|&i| matches!(chars[i], '。' | '！' | '？' | '\n' | '.' | '!' | '?'))
                })
                .map(|i| i + 1)
                .unwrap_or(target)
        };
        rows.push(Chunk {
            kind: "paragraph",
            ordinal: rows.len(),
            start,
            end,
            text: chars[start..end].iter().collect(),
        });
        if end == chars.len() {
            break;
        }
        start = end.saturating_sub(120).max(start + 1);
    }
    // 长页的页向量使用所有段落向量的均值，不截去页尾内容。
    if chars.len() <= 8000 {
        rows.push(Chunk {
            kind: "page",
            ordinal: 0,
            start: 0,
            end: chars.len(),
            text: text.to_string(),
        });
    }
    rows
}

fn retrieval_text(text: &str) -> &str {
    if let Some((body, marker)) = text.rsplit_once("\n[图片 ") {
        if marker.ends_with(']') && !marker.contains('\n') && marker.contains(": page-") {
            return body;
        }
    }
    text
}

fn member_ids(state: &AppState, id: &str) -> ApiResult<Vec<String>> {
    let workspace = workspace_record(state, id)?;
    Ok(workspace["tabs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["material_id"].as_str().map(str::to_string))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect())
}

fn corpus_hash(state: &AppState, id: &str) -> ApiResult<String> {
    let mut rows = Vec::new();
    for id in member_ids(state, id)? {
        let record = material_record(state, &id)?;
        rows.push(json!([
            id,
            record["revision"],
            record["unit_count"],
            record["status"]
        ]));
    }
    Ok(revisions::hash(&json!(rows).to_string()))
}

fn save_status(state: &AppState, id: &str, payload: &Value) -> ApiResult<()> {
    state.database.connect()?.execute("INSERT INTO reading_index_state VALUES(?1,?2) ON CONFLICT(workspace_id) DO UPDATE SET payload_json=excluded.payload_json",
        params![id, payload.to_string()]).map_err(db_error)?;
    Ok(())
}

fn status(state: &AppState, id: &str) -> ApiResult<Value> {
    workspace_record(state, id)?;
    let raw: Option<String> = state
        .database
        .connect()?
        .query_row(
            "SELECT payload_json FROM reading_index_state WHERE workspace_id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()
        .map_err(db_error)?;
    let mut value = raw
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or_else(|| json!({"status":"not_indexed","completed_pages":0,"total_pages":0}));
    value["enabled"] = json!(enabled(state));
    value["model_available"] = json!(config(state).is_ok());
    let has_job = state.reading_index.jobs.lock().contains_key(id);
    if has_job && value["status"] != "indexing" {
        value["status"] = json!("queued");
        // 重试已经排队时，不再把上一次未配置模型的错误显示为当前结果。
        value.as_object_mut().unwrap().remove("error");
    } else if value["status"] == "indexing" && !has_job {
        value["status"] = json!("interrupted");
    }
    if let Ok(config) = config(state) {
        if !has_job
            && value["signature"]
                .as_str()
                .is_some_and(|s| s != config.signature)
        {
            value["status"] = json!("stale");
        }
    }
    Ok(value)
}

async fn get_status(
    ScopedState(state): ScopedState,
    AxumPath(id): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    status(&state, &id).map(Json)
}

pub(super) fn invalidate(state: &AppState) {
    state.reading_index.cache.lock().clear();
}

pub(super) fn schedule_for_material(state: &AppState, material_id: &str) {
    if let Ok(workspaces) = state.database.list_resources(WORKSPACE_DOMAIN) {
        for workspace in workspaces {
            if workspace["tabs"]
                .as_array()
                .is_some_and(|tabs| tabs.iter().any(|t| t["material_id"] == material_id))
            {
                if let Some(id) = workspace["workspace_id"].as_str() {
                    state.reading_index.dirty.lock().insert(id.to_string());
                    schedule(state, id);
                }
            }
        }
    }
}

pub(super) fn schedule(state: &AppState, id: &str) {
    if !enabled(state) {
        return;
    }
    let dirty = state.reading_index.dirty.lock().contains(id);
    if !dirty {
        if let (Ok(config), Ok(corpus), Ok(status)) =
            (config(state), corpus_hash(state, id), status(state, id))
        {
            if status["status"] == "ready"
                && status["signature"] == config.signature
                && status["corpus_hash"] == corpus
            {
                return;
            }
        }
    }
    let mut jobs = state.reading_index.jobs.lock();
    if jobs.contains_key(id) {
        return;
    }
    let cancel = CancellationToken::new();
    let job_id = Uuid::new_v4().to_string();
    jobs.insert(id.to_string(), (job_id.clone(), cancel.clone()));
    state.reading_index.dirty.lock().remove(id);
    let state = state.clone();
    let id = id.to_string();
    tokio::spawn(async move {
        let _guard = state.reading_index.gate.lock().await;
        if let Err(error) = build(&state, &id, &cancel).await {
            let _ = save_status(
                &state,
                &id,
                &json!({"status":if cancel.is_cancelled() {"cancelled"} else {"failed"},
                "error":error.to_string(), "updated_at":crate::storage::now()}),
            );
        }
        let mut jobs = state.reading_index.jobs.lock();
        if jobs.get(&id).is_some_and(|(current, _)| current == &job_id) {
            jobs.remove(&id);
        }
        let rerun = state.reading_index.dirty.lock().contains(&id);
        drop(jobs);
        drop(_guard);
        if rerun && !cancel.is_cancelled() {
            schedule(&state, &id);
        }
    });
}

async fn rebuild(
    ScopedState(state): ScopedState,
    AxumPath(id): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    workspace_record(&state, &id)?;
    if !enabled(&state) {
        return Err(ApiError::bad_request(
            "请先在阅读检索设置中开启并确认文本发送提示。",
        ));
    }
    config(&state).map_err(ApiError::bad_request)?;
    {
        let _guard = state.reading_index.gate.lock().await;
        let connection = state.database.connect()?;
        for material in member_ids(&state, &id)? {
            connection
                .execute(
                    "DELETE FROM reading_vectors WHERE material_id=?1",
                    [material],
                )
                .map_err(db_error)?;
        }
        invalidate(&state);
    }
    state.reading_index.dirty.lock().insert(id.clone());
    schedule(&state, &id);
    Ok(Json(json!({"status":"queued"})))
}

async fn delete_index(
    ScopedState(state): ScopedState,
    AxumPath(id): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    let ids = member_ids(&state, &id)?;
    if let Some((_, token)) = state.reading_index.jobs.lock().remove(&id) {
        token.cancel();
    }
    state.reading_index.dirty.lock().remove(&id);
    // 等待写入任务退出，避免删除后旧任务把索引写回。
    let _guard = state.reading_index.gate.lock().await;
    let connection = state.database.connect()?;
    for material in ids {
        connection
            .execute(
                "DELETE FROM reading_vectors WHERE material_id=?1",
                [&material],
            )
            .map_err(db_error)?;
    }
    save_status(
        &state,
        &id,
        &json!({"status":"not_indexed","completed_pages":0,"total_pages":0}),
    )?;
    invalidate(&state);
    Ok(Json(json!({"status":"deleted"})))
}

async fn build(state: &AppState, id: &str, cancel: &CancellationToken) -> ApiResult<()> {
    let config = config(state).map_err(ApiError::bad_request)?;
    let ids = member_ids(state, id)?;
    let corpus = corpus_hash(state, id)?;
    let mut total = 0usize;
    let mut completed = 0usize;
    for material in &ids {
        if let Ok(record) = material_record(state, material) {
            if record["status"] == "ready" {
                total += record["unit_count"].as_u64().unwrap_or(0) as usize;
            }
        }
    }
    let mut progress = json!({"status":"indexing","completed_pages":0,"total_pages":total,
        "signature":config.signature,"model":config.model,"corpus_hash":corpus,"updated_at":crate::storage::now()});
    save_status(state, id, &progress)?;
    for material in &ids {
        let record = material_record(state, material)?;
        if record["status"] != "ready" {
            continue;
        }
        revisions::ensure_snapshot(state, material)?;
        let data = load_material_data(state, material)?;
        for (i, value) in data["units"].as_array().into_iter().flatten().enumerate() {
            if cancel.is_cancelled() || !enabled(state) {
                return Err(ApiError::bad_request("索引任务已取消。"));
            }
            let raw = value.as_str().unwrap_or("");
            let text = retrieval_text(raw);
            let text_hash = revisions::hash(raw);
            let existing: bool = state.database.connect()?.query_row(
                "SELECT EXISTS(SELECT 1 FROM reading_vectors WHERE material_id=?1 AND locator=?2 AND signature=?3 AND text_hash=?4 AND kind='page')",
                params![material,i+1,config.signature,text_hash], |r| r.get(0)).map_err(db_error)?;
            if !existing && !text.trim().is_empty() {
                let mut parts = chunks(text);
                let inputs = parts.iter().map(|p| p.text.clone()).collect::<Vec<_>>();
                let mut vectors = embed(state, &config, &inputs, false, cancel)
                    .await
                    .map_err(ApiError::provider)?;
                if !parts.iter().any(|p| p.kind == "page") {
                    let dim = vectors[0].len();
                    let mut average = vec![0f32; dim];
                    for vector in &vectors {
                        for (a, v) in average.iter_mut().zip(vector) {
                            *a += v;
                        }
                    }
                    vectors.push(normalize(average).map_err(ApiError::provider)?);
                    parts.push(Chunk {
                        kind: "page",
                        ordinal: 0,
                        start: 0,
                        end: text.chars().count(),
                        text: String::new(),
                    });
                }
                if cancel.is_cancelled() {
                    return Err(ApiError::bad_request("索引任务已取消。"));
                }
                store_page(
                    state,
                    material,
                    i + 1,
                    &text_hash,
                    &config.signature,
                    &parts,
                    &vectors,
                )?;
                invalidate(state);
            }
            completed += 1;
            progress["completed_pages"] = json!(completed);
            progress["updated_at"] = json!(crate::storage::now());
            save_status(state, id, &progress)?;
        }
    }
    progress["status"] = json!("ready");
    if corpus_hash(state, id)? != corpus {
        progress["status"] = json!("stale");
        state.reading_index.dirty.lock().insert(id.to_string());
    }
    save_status(state, id, &progress)?;
    Ok(())
}

fn store_page(
    state: &AppState,
    id: &str,
    locator: usize,
    text_hash: &str,
    signature: &str,
    chunks: &[Chunk],
    vectors: &[Vec<f32>],
) -> ApiResult<()> {
    let mut connection = state.database.connect()?;
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(db_error)?;
    // 网络请求期间页面可能被用户校正，过期向量不能进入当前检索。
    let current = load_material_data(state, id)?;
    if revisions::hash(&unit_text(&current, locator)?) != text_hash {
        return Ok(());
    }
    tx.execute(
        "DELETE FROM reading_vectors WHERE material_id=?1 AND locator=?2",
        params![id, locator],
    )
    .map_err(db_error)?;
    for (chunk, vector) in chunks.iter().zip(vectors) {
        let blob = vector
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect::<Vec<_>>();
        tx.execute(
            "INSERT INTO reading_vectors VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                id,
                locator,
                chunk.kind,
                chunk.ordinal,
                signature,
                text_hash,
                chunk.start,
                chunk.end,
                vector.len(),
                blob
            ],
        )
        .map_err(db_error)?;
    }
    tx.commit().map_err(db_error)
}

fn cached_vectors(
    state: &AppState,
    signature: &str,
    ids: &[String],
) -> ApiResult<Arc<Vec<VectorRow>>> {
    let cache_key = revisions::hash(&format!("{signature}:{}", json!(ids)));
    if let Some(rows) = state.reading_index.cache.lock().get(&cache_key).cloned() {
        return Ok(rows);
    }
    let connection = state.database.connect()?;
    let mut statement = connection.prepare(
        "SELECT material_id,locator,kind,start_offset,end_offset,text_hash,embedding,dimension FROM reading_vectors WHERE signature=?1 AND material_id=?2"
    ).map_err(db_error)?;
    let mut rows = Vec::new();
    for id in ids {
        rows.extend(
            statement
                .query_map(params![signature, id], |row| {
                    let blob: Vec<u8> = row.get(6)?;
                    let dimension: usize = row.get(7)?;
                    let vector = if blob.len() == dimension * 4 {
                        blob.chunks_exact(4)
                            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                            .collect()
                    } else {
                        Vec::new()
                    };
                    Ok(VectorRow {
                        material_id: row.get(0)?,
                        locator: row.get(1)?,
                        kind: row.get(2)?,
                        start: row.get(3)?,
                        end: row.get(4)?,
                        text_hash: row.get(5)?,
                        vector,
                    })
                })
                .map_err(db_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(db_error)?,
        );
    }
    let rows = Arc::new(rows);
    let mut cache = state.reading_index.cache.lock();
    cache.clear();
    cache.insert(cache_key, rows.clone());
    Ok(rows)
}

#[derive(Clone)]
struct Candidate {
    material_id: String,
    locator: usize,
    start: usize,
    end: usize,
    score: f64,
    methods: BTreeSet<String>,
}

pub(super) async fn search(
    state: &AppState,
    workspace: &str,
    material_filter: Option<&str>,
    query: &str,
    limit: usize,
    cancel: &CancellationToken,
) -> ApiResult<Value> {
    let query = query.trim();
    if query.trim().is_empty() || query.chars().count() > 4000 {
        return Err(ApiError::bad_request(
            "检索问题不能为空且不能超过 4000 字符。",
        ));
    }
    let mut ids = member_ids(state, workspace)?;
    if let Some(id) = material_filter {
        if !ids.iter().any(|s| s == id) {
            return Err(ApiError::forbidden("检索材料不属于当前资料集。"));
        }
        ids.retain(|s| s == id);
    }
    let mut materials = HashMap::new();
    let mut page_hashes = HashMap::new();
    let mut keywords = Vec::new();
    let target = query.to_lowercase();
    let normalized_query = normalize_search_text(query);
    let terms = search_terms(query);
    for id in &ids {
        let record = material_record(state, id)?;
        if record["status"] != "ready" {
            continue;
        }
        let data = load_material_data(state, id)?;
        let units = data["units"].as_array().cloned().unwrap_or_default();
        for (i, value) in units.iter().enumerate() {
            let raw = value.as_str().unwrap_or("");
            let text = retrieval_text(raw);
            page_hashes.insert((id.clone(), i + 1), revisions::hash(raw));
            let lower = text.to_lowercase();
            let count = terms
                .iter()
                .filter(|term| lower.contains(term.as_str()))
                .count();
            let (relevance, offset) = if let Some(position) = lower.find(&target) {
                (
                    10_000 + count,
                    text.get(..position).unwrap_or("").chars().count(),
                )
            } else if !normalized_query.is_empty()
                && normalize_search_text(text).contains(&normalized_query)
            {
                (5_000 + count, first_term_offset(text, query))
            } else if count > 0 {
                (count, first_term_offset(text, query))
            } else {
                continue;
            };
            keywords.push((relevance, id.clone(), i + 1, offset));
        }
        materials.insert(id.clone(), (record, data));
    }
    keywords.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
    });
    let mut ranked = HashMap::<(String, usize), Candidate>::new();
    for (rank, (_, id, locator, start)) in keywords.into_iter().take(32).enumerate() {
        add_candidate(
            &mut ranked,
            &id,
            locator,
            start,
            start + 360,
            0.9 / (61.0 + rank as f64),
            "keyword",
        );
    }
    let mut mode = "keyword";
    let mut warning = String::new();
    if enabled(state) {
        schedule(state, workspace);
        match config(state) {
            Ok(config) => {
                let vectors = cached_vectors(state, &config.signature, &ids)?;
                let valid = vectors
                    .iter()
                    .filter(|v| {
                        page_hashes.get(&(v.material_id.clone(), v.locator)) == Some(&v.text_hash)
                    })
                    .collect::<Vec<_>>();
                if !valid.is_empty() {
                    match embed(state, &config, &[query.to_string()], true, cancel).await {
                        Ok(query_vectors) => {
                            let q = &query_vectors[0];
                            for (kind, count, weight) in [("page", 12, 1.0), ("paragraph", 32, 1.2)]
                            {
                                let mut hits = valid
                                    .iter()
                                    .filter(|v| v.kind == kind && v.vector.len() == q.len())
                                    .map(|v| {
                                        (
                                            v.vector
                                                .iter()
                                                .zip(q)
                                                .map(|(a, b)| *a as f64 * *b as f64)
                                                .sum::<f64>(),
                                            *v,
                                        )
                                    })
                                    .filter(|(s, _)| s.is_finite())
                                    .collect::<Vec<_>>();
                                hits.sort_by(|a, b| b.0.total_cmp(&a.0));
                                for (rank, (_, v)) in hits.into_iter().take(count).enumerate() {
                                    add_candidate(
                                        &mut ranked,
                                        &v.material_id,
                                        v.locator,
                                        v.start,
                                        v.end,
                                        weight / (61.0 + rank as f64),
                                        kind,
                                    );
                                }
                            }
                            mode = "hybrid";
                            if valid.iter().all(|v| v.vector.len() != q.len()) {
                                mode = "keyword";
                                warning = "向量维度不匹配，请重建当前资料集索引。".into();
                            }
                        }
                        Err(error) => warning = error,
                    }
                } else {
                    warning =
                        "当前资料集尚无有效向量，正在后台建立索引，本次使用关键词检索。".into();
                }
            }
            Err(error) => warning = error,
        }
    }
    let mut rows = ranked.into_values().collect::<Vec<_>>();
    rows.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.material_id.cmp(&b.material_id))
            .then_with(|| a.locator.cmp(&b.locator))
    });
    let total = rows.len();
    let hits = rows.into_iter().take(limit.clamp(1,20)).filter_map(|row| {
        let (record,data)=materials.get(&row.material_id)?;
        let text=unit_text(data,row.locator).ok()?;
        let chars=text.chars().collect::<Vec<_>>();
        let start=row.start.min(chars.len());
        let end=row.end.min(chars.len()).min(start+1200);
        Some(json!({"material_id":row.material_id,"material_revision":record["revision"],"locator":row.locator,
            "title":record["title"],"snippet":chars[start..end].iter().collect::<String>(),
            "offset":start,"end_offset":end,"score":row.score,"methods":row.methods,
            "type":"reading","page":row.locator}))
    }).collect::<Vec<_>>();
    Ok(json!({"mode":mode,"warning":warning,"hits":hits,"truncated":total>limit}))
}

fn add_candidate(
    rows: &mut HashMap<(String, usize), Candidate>,
    id: &str,
    locator: usize,
    start: usize,
    end: usize,
    score: f64,
    method: &str,
) {
    let row = rows
        .entry((id.to_string(), locator))
        .or_insert_with(|| Candidate {
            material_id: id.to_string(),
            locator,
            start,
            end,
            score: 0.0,
            methods: BTreeSet::new(),
        });
    // 同一路径的多个段落只能投一次票，避免长页面凭段落数量霸榜。
    if row.methods.insert(method.to_string()) {
        row.score += score;
        if method == "paragraph" {
            row.start = start;
            row.end = end;
        }
    }
}

async fn search_api(
    ScopedState(state): ScopedState,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    search(
        &state,
        &id,
        body["material_id"].as_str(),
        body["query"].as_str().unwrap_or(""),
        body["limit"].as_u64().unwrap_or(8) as usize,
        &CancellationToken::new(),
    )
    .await
    .map(Json)
}

pub(super) async fn search_tool(
    arguments: &Value,
    runtime: &super::super::tools::ToolRuntime<'_>,
    current_only: bool,
) -> super::super::tools::ToolOutcome {
    let request = runtime.request.unwrap_or(&Value::Null);
    let workspace = request["reading_workspace_id"].as_str().unwrap_or("");
    let material = if current_only {
        request["reading_material_id"].as_str()
    } else {
        None
    };
    if workspace.is_empty() {
        return super::super::tools::ToolOutcome::failure("当前回合没有阅读资料集。");
    }
    let cancel = runtime.cancel.cloned().unwrap_or_default();
    match search(
        runtime.state,
        workspace,
        material,
        arguments["query"].as_str().unwrap_or(""),
        arguments["limit"].as_u64().unwrap_or(8) as usize,
        &cancel,
    )
    .await
    {
        Ok(result) => {
            let content = format!(
                "检索模式：{}。{}\n{}\n引用前请切换到对应材料并使用 read_material 核对原文。",
                result["mode"].as_str().unwrap_or("keyword"),
                result["warning"].as_str().unwrap_or(""),
                result["hits"]
            );
            let sources = result["hits"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|hit| {
                    let mut source = hit.clone();
                    source["content"] = source["snippet"].clone();
                    source
                })
                .collect::<Vec<_>>();
            let mut metadata = json!({"kind":"search_reading_workspace",
                "mode":result["mode"],"hits":result["hits"],"sources":sources,"warning":result["warning"]});
            if let Some(id) = material {
                metadata["material_id"] = json!(id);
                metadata["material_revision"] = material_record(runtime.state, id)
                    .ok()
                    .and_then(|r| r.get("revision").cloned())
                    .unwrap_or(json!(1));
            }
            super::super::tools::ToolOutcome::success(content, metadata)
        }
        Err(error) => super::super::tools::ToolOutcome::failure(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn referenced_catalog() -> Value {
        serde_json::from_str(include_str!("../../tests/fixtures/referenced-catalog.json"))
            .expect("读取脱敏模型配置")
    }

    #[test]
    fn 唯一向量模型无需先设为默认且不修改保存的配置() {
        for defaults in [Value::Null, json!(""), json!("   ")] {
            let mut catalog = referenced_catalog();
            catalog["services"]["embedding"]["active_profile_id"] = defaults.clone();
            catalog["services"]["embedding"]["active_model_id"] = defaults;
            let original = catalog.clone();
            let config = config_from_catalog(&catalog).expect("识别唯一向量模型");
            assert_eq!(config.model, "bge-m3");
            assert_eq!(config.endpoint, "https://gateway.example/api/v3/embeddings");
            assert_eq!(config.profile["api_key"], "fixture-key");
            assert_eq!(catalog, original);
        }
        let mut catalog = referenced_catalog();
        let service = catalog["services"]["embedding"].as_object_mut().unwrap();
        service.remove("active_profile_id");
        service.remove("active_model_id");
        assert_eq!(config_from_catalog(&catalog).unwrap().model, "bge-m3");
    }

    #[test]
    fn 多个模型必须明确选择且已有默认优先() {
        let mut catalog = referenced_catalog();
        let second = json!({"id":"other-model","model":"other-embedding"});
        catalog["services"]["embedding"]["profiles"][0]["models"]
            .as_array_mut()
            .unwrap()
            .push(second);
        assert!(config_from_catalog(&catalog)
            .err()
            .unwrap()
            .contains("多个向量模型"));
        catalog["services"]["embedding"]["active_profile_id"] = json!("embedding-profile");
        assert!(config_from_catalog(&catalog).is_err());
        catalog["services"]["embedding"]["active_model_id"] = json!("other-model");
        assert_eq!(
            config_from_catalog(&catalog).unwrap().model,
            "other-embedding"
        );
        catalog["services"]["embedding"]["active_profile_id"] = Value::Null;
        assert_eq!(
            config_from_catalog(&catalog).unwrap().model,
            "other-embedding"
        );

        let mut other = catalog["services"]["embedding"]["profiles"][0].clone();
        other["id"] = json!("other-profile");
        other["models"] = json!([{"id":"third-model","model":"third-embedding"}]);
        catalog["services"]["embedding"]["profiles"]
            .as_array_mut()
            .unwrap()
            .push(other);
        catalog["services"]["embedding"]["active_profile_id"] = json!("other-profile");
        catalog["services"]["embedding"]["active_model_id"] = Value::Null;
        assert_eq!(
            config_from_catalog(&catalog).unwrap().model,
            "third-embedding"
        );
    }

    #[test]
    fn 默认项失效不擅自回退到其他模型() {
        for (profile, model) in [
            (json!("deleted-profile"), Value::Null),
            (Value::Null, json!("deleted-model")),
            (json!("embedding-profile"), json!("deleted-model")),
        ] {
            let mut catalog = referenced_catalog();
            catalog["services"]["embedding"]["active_profile_id"] = profile;
            catalog["services"]["embedding"]["active_model_id"] = model;
            assert!(config_from_catalog(&catalog)
                .err()
                .unwrap()
                .contains("已不存在"));
        }
    }

    #[test]
    fn 缺失或无效提供方返回真实配置错误() {
        let mut catalog = referenced_catalog();
        catalog["connections"] = json!([]);
        let error = config_from_catalog(&catalog).err().unwrap();
        assert!(error.contains("提供方已不存在"));
        assert!(!error.contains("未配置向量模型"));

        let mut catalog = referenced_catalog();
        catalog["services"]["embedding"]["profiles"][0]["models"][0]["provider_ref"] =
            json!("invalid");
        assert!(config_from_catalog(&catalog)
            .err()
            .unwrap()
            .contains("提供方引用无效"));

        let mut catalog = referenced_catalog();
        catalog["connections"][0]["base_url"] = json!("");
        assert!(config_from_catalog(&catalog)
            .err()
            .unwrap()
            .contains("地址为空"));
    }

    #[test]
    fn 空模型和仅提供方配置不参与唯一模型判断() {
        let mut catalog = referenced_catalog();
        catalog["services"]["embedding"]["profiles"][0]["models"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id":"empty-model","model":"  "}));
        catalog["services"]["embedding"]["profiles"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id":"provider-only","provider_only":true,
                "models":[{"id":"unused","model":"unused"}]}));
        assert_eq!(config_from_catalog(&catalog).unwrap().model, "bge-m3");
        catalog["services"]["embedding"]["profiles"][0]["models"] = json!([]);
        assert!(config_from_catalog(&catalog)
            .err()
            .unwrap()
            .contains("未配置向量模型"));
    }

    #[tokio::test]
    async fn 阅读设置返回实际模型和可操作的配置错误() {
        let (state, _directory) = super::super::tests::pdf_state();
        let mut catalog = referenced_catalog();
        state.database.set_setting("catalog", &catalog).unwrap();
        let result = get_settings(ScopedState(state.clone())).await.unwrap().0;
        assert_eq!(result["model_available"], true);
        assert_eq!(result["model"], "bge-m3");
        assert!(result["model_error"].is_null());
        assert_eq!(
            state.database.get_setting("catalog").unwrap().unwrap(),
            catalog
        );

        catalog["services"]["embedding"]["profiles"][0]["models"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id":"other","model":"other-embedding"}));
        state.database.set_setting("catalog", &catalog).unwrap();
        let result = get_settings(ScopedState(state)).await.unwrap().0;
        assert_eq!(result["model_available"], false);
        assert!(result["model"].is_null());
        assert!(result["model_error"]
            .as_str()
            .unwrap()
            .contains("多个向量模型"));
    }

    #[test]
    fn 失败后的索引重试排队不继续显示旧错误() {
        let (state, _directory) = super::super::tests::pdf_state();
        let workspace = create_workspace_record(&state, "索引重试测试", "", "", &[]).unwrap();
        let id = workspace["workspace_id"].as_str().unwrap();
        save_status(
            &state,
            id,
            &json!({"status":"failed","error":"未配置向量模型，继续使用关键词检索。"}),
        )
        .unwrap();
        state.reading_index.jobs.lock().insert(
            id.to_string(),
            ("retry".to_string(), CancellationToken::new()),
        );
        let result = status(&state, id).unwrap();
        assert_eq!(result["status"], "queued");
        assert!(result.get("error").is_none());
        state.reading_index.jobs.lock().remove(id);
        assert_eq!(status(&state, id).unwrap()["status"], "failed");
    }

    #[test]
    fn native_embedding_endpoints_keep_gateway_prefixes_without_llm_version() {
        for (base, binding, expected) in [
            (
                "http://localhost:11434",
                "ollama",
                "http://localhost:11434/api/embed",
            ),
            (
                "http://localhost:11434/api",
                "ollama",
                "http://localhost:11434/api/embed",
            ),
            (
                "http://localhost:11434/v1",
                "ollama",
                "http://localhost:11434/api/embed",
            ),
            (
                "https://gateway.test/custom/api/embed",
                "ollama",
                "https://gateway.test/custom/api/embed",
            ),
            (
                "https://api.cohere.com",
                "cohere",
                "https://api.cohere.com/v2/embed",
            ),
            (
                "https://gateway.test/custom/v2",
                "cohere",
                "https://gateway.test/custom/v2/embed",
            ),
            (
                "https://gateway.test/custom/v2/embed?account=test",
                "cohere",
                "https://gateway.test/custom/v2/embed?account=test",
            ),
            (
                "https://gateway.test/custom/v3",
                "openai",
                "https://gateway.test/custom/v3/embeddings",
            ),
        ] {
            assert_eq!(embedding_endpoint(base, binding).unwrap(), expected);
        }
        assert!(embedding_endpoint("file:///private", "ollama").is_err());
    }

    #[test]
    fn embedding_response_is_ordered_validated_and_normalized() {
        let vectors = decode_vectors(
            &json!({"data":[{"index":1,"embedding":[0,2]},{"index":0,"embedding":[3,0]}]}),
            2,
        )
        .unwrap();
        assert_eq!(vectors, vec![vec![1.0, 0.0], vec![0.0, 1.0]]);
        assert!(decode_vectors(&json!({"data":[{"index":0,"embedding":[0,0]}]}), 1).is_err());
        assert!(decode_vectors(
            &json!({"data":[{"index":0,"embedding":[1,0]},{"index":0,"embedding":[0,1]}]}),
            2
        )
        .is_err());
        assert!(decode_vectors(&json!({"embeddings":[[1],[1,2]]}), 2).is_err());
    }
    #[test]
    fn chunks_preserve_unicode_offsets_overlap_and_page_boundaries() {
        let text = "中文段落。\n\n".repeat(700);
        let rows = chunks(&text);
        for row in rows.iter().filter(|r| r.kind == "paragraph") {
            assert!(row.end - row.start <= 1600);
            assert_eq!(
                row.text,
                text.chars()
                    .skip(row.start)
                    .take(row.end - row.start)
                    .collect::<String>()
            );
        }
        assert_eq!(
            rows.iter().find(|r| r.kind == "paragraph").unwrap().start,
            0
        );
        assert_eq!(
            rows.iter()
                .filter(|r| r.kind == "paragraph")
                .last()
                .unwrap()
                .end,
            text.chars().count()
        );
        assert!(chunks("").is_empty());
    }

    #[tokio::test]
    async fn hybrid_retrieval_uses_configured_gateway_and_reembeds_only_modified_page() {
        use axum::extract::State;
        let (state, _directory) = super::super::tests::pdf_state();
        let requests = Arc::new(parking_lot::Mutex::new(Vec::<Value>::new()));
        let capture = requests.clone();
        let app = Router::new()
            .route(
                "/custom/v3/embeddings",
                post(
                    |State(requests): State<Arc<parking_lot::Mutex<Vec<Value>>>>,
                     headers: HeaderMap,
                     Json(body): Json<Value>| async move {
                        assert_eq!(
                            headers.get("authorization").unwrap(),
                            "Bearer local-fixture"
                        );
                        assert!(body.get("dimensions").is_none());
                        let rows = body["input"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .enumerate()
                            .map(|(i, text)| {
                                let text = text.as_str().unwrap();
                                let vector =
                                    if text.contains("quantum") || text.contains("microscopic") {
                                        vec![1.0, 0.05]
                                    } else {
                                        vec![0.05, 1.0]
                                    };
                                json!({"index":i,"embedding":vector})
                            })
                            .rev()
                            .collect::<Vec<_>>();
                        requests.lock().push(body);
                        Json(json!({"data":rows}))
                    },
                ),
            )
            .with_state(capture);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        state.database.set_setting("catalog",&json!({
            "connections":[{"id":"local","base_url":format!("http://{address}/custom/v3"),"api_key":"local-fixture"}],
            "services":{"embedding":{"active_profile_id":null,"active_model_id":null,"profiles":[{
                "id":"profile","connection_id":"local","binding":"openai","base_url":"https://api.openai.com/v1",
                "models":[{"id":"model","model":"fixture-embedding"}]
            }]}}
        })).unwrap();
        let bytes = super::super::super::pdf_text::tests::test_pdf(&[
            "quantum mechanics evidence",
            "gardening and soil",
        ]);
        ingest_bytes(
            &state,
            "indexed-pdf",
            "indexed-pdf",
            "sample.pdf",
            &bytes,
            "file",
            "",
        )
        .await
        .unwrap();
        let workspace =
            create_workspace_record(&state, "测试资料集", "", "", &["indexed-pdf".into()]).unwrap();
        let id = workspace["workspace_id"].as_str().unwrap();
        let before = search(&state, id, None, "quantum", 5, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(before["mode"], "keyword");
        assert!(!before["hits"].as_array().unwrap().is_empty());
        assert!(requests.lock().is_empty(), "未开启向量时不得发送文档");
        state
            .database
            .set_setting(
                "reading_retrieval",
                &json!({"enabled":true,"consented":true}),
            )
            .unwrap();
        build(&state, id, &CancellationToken::new()).await.unwrap();
        assert_eq!(requests.lock().len(), 2, "每页一个批次");
        let result = search(
            &state,
            id,
            None,
            "microscopic particles",
            5,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result["mode"], "hybrid");
        assert_eq!(result["hits"][0]["locator"], 1);
        assert!(result["hits"][0]["methods"]
            .as_array()
            .unwrap()
            .contains(&json!("paragraph")));
        // 模拟当前页被更新后仅失效该页向量，其他页面不重复发送。
        let connection = state.database.connect().unwrap();
        connection
            .execute(
                "DELETE FROM reading_vectors WHERE material_id='indexed-pdf' AND locator=1",
                [],
            )
            .unwrap();
        let count = requests.lock().len();
        build(&state, id, &CancellationToken::new()).await.unwrap();
        assert_eq!(requests.lock().len(), count + 1);
        let last = requests.lock().last().cloned().unwrap();
        assert!(last["input"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| !s.as_str().unwrap().contains("gardening")));
        assert!(search(
            &state,
            id,
            Some("outside-material"),
            "quantum",
            5,
            &CancellationToken::new()
        )
        .await
        .is_err());
        server.abort();
    }
}
