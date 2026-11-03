use super::*;
use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};

pub(super) const PROMPT_VERSION: &str = "reading-ocr-v1";

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/reading/materials/{material_id}/ocr-corrections",
            get(corrections),
        )
        .route(
            "/reading/materials/{material_id}/ocr-corrections/{id}/accept",
            post(accept),
        )
        .route(
            "/reading/materials/{material_id}/ocr-corrections/{id}/reject",
            post(reject),
        )
        .route(
            "/reading/materials/{material_id}/pages/{locator}/image",
            get(page_image),
        )
        .route(
            "/reading/materials/{material_id}/references/resolve",
            post(resolve_reference),
        )
}

pub(super) fn hash(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

fn sql(error: rusqlite::Error) -> ApiError {
    ApiError::internal(format!("阅读版本数据库操作失败：{error}"))
}

fn snapshot_data(state: &AppState, id: &str, revision: usize) -> ApiResult<Option<Value>> {
    let connection = state.database.connect()?;
    let raw: Option<String> = connection
        .query_row(
            "SELECT data_json FROM reading_revisions WHERE material_id=?1 AND revision=?2",
            params![id, revision],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql)?;
    let Some(raw) = raw else { return Ok(None) };
    let mut data: Value =
        serde_json::from_str(&raw).map_err(|e| ApiError::internal(e.to_string()))?;
    if data["unit_meta"]
        .as_array()
        .is_some_and(|rows| rows.iter().any(|row| row.get("original_text").is_none()))
    {
        let baseline: String = connection
            .query_row(
                "SELECT data_json FROM reading_revisions WHERE material_id=?1 AND revision<=?2 ORDER BY revision LIMIT 1",
                params![id, revision],
                |row| row.get(0),
            )
            .map_err(sql)?;
        let baseline: Value =
            serde_json::from_str(&baseline).map_err(|e| ApiError::internal(e.to_string()))?;
        for (index, meta) in data["unit_meta"]
            .as_array_mut()
            .into_iter()
            .flatten()
            .enumerate()
        {
            if meta.get("original_text").is_none() {
                meta["original_text"] = baseline["unit_meta"][index]["original_text"].clone();
            }
        }
    }
    let mut statement = connection
        .prepare(
            "SELECT text FROM reading_revision_units u WHERE material_id=?1 AND revision=(
            SELECT MAX(v.revision) FROM reading_revision_units v
            WHERE v.material_id=u.material_id AND v.locator=u.locator AND v.revision<=?2)
         AND locator<=?3 ORDER BY locator",
        )
        .map_err(sql)?;
    let count = data["unit_meta"]
        .as_array()
        .map(Vec::len)
        .unwrap_or(usize::MAX / 2);
    let texts = statement
        .query_map(params![id, revision, count], |row| row.get::<_, String>(0))
        .map_err(sql)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql)?;
    data["units"] = json!(texts);
    Ok(Some(data))
}

pub(super) fn current_data(state: &AppState, id: &str) -> ApiResult<Option<Value>> {
    let Some(record) = state.database.get_resource(MATERIAL_DOMAIN, id)? else {
        return Ok(None);
    };
    snapshot_data(state, id, record["revision"].as_u64().unwrap_or(1) as usize)
}

fn insert_snapshot(tx: &Transaction<'_>, id: &str, revision: usize, data: &Value) -> ApiResult<()> {
    let mut metadata = data.clone();
    metadata
        .as_object_mut()
        .ok_or_else(|| ApiError::internal("材料数据格式无效。"))?
        .remove("units");
    let has_baseline: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM reading_revisions WHERE material_id=?1 AND revision<?2)",
            params![id, revision],
            |row| row.get(0),
        )
        .map_err(sql)?;
    if has_baseline {
        // 原始 OCR 只保存在基线，后续版本继承，避免通过元数据重复保存整本正文。
        for meta in metadata["unit_meta"].as_array_mut().into_iter().flatten() {
            if let Some(object) = meta.as_object_mut() {
                object.remove("original_text");
            }
        }
    }
    let inserted = tx
        .execute(
            "INSERT OR IGNORE INTO reading_revisions VALUES(?1, ?2, ?3, ?4)",
            params![id, revision, metadata.to_string(), crate::storage::now()],
        )
        .map_err(sql)?;
    if inserted > 0 {
        let mut statement = tx
            .prepare("INSERT INTO reading_revision_units VALUES(?1, ?2, ?3, ?4, ?5)")
            .map_err(sql)?;
        for (index, text) in data["units"].as_array().into_iter().flatten().enumerate() {
            let text = text.as_str().unwrap_or("");
            let previous_hash: Option<String> = tx.query_row(
                "SELECT text_hash FROM reading_revision_units WHERE material_id=?1 AND locator=?2 AND revision<?3 ORDER BY revision DESC LIMIT 1",
                params![id,index+1,revision],|r|r.get(0)
            ).optional().map_err(sql)?;
            if previous_hash.as_deref() == Some(hash(text).as_str()) {
                continue;
            }
            statement
                .execute(params![id, revision, index + 1, text, hash(text)])
                .map_err(sql)?;
        }
    }
    Ok(())
}

pub(super) fn ensure_snapshot(state: &AppState, id: &str) -> ApiResult<()> {
    let record = material_record(state, id)?;
    let revision = record["revision"].as_u64().unwrap_or(1) as usize;
    let exists: bool = state
        .database
        .connect()?
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM reading_revisions WHERE material_id=?1 AND revision=?2)",
            params![id, revision],
            |r| r.get(0),
        )
        .map_err(sql)?;
    if exists {
        return Ok(());
    }
    let bytes = std::fs::read(material_data_path(state, id)?)
        .map_err(|e| ApiError::internal(format!("无法读取原始阅读数据：{e}")))?;
    let mut data: Value =
        serde_json::from_slice(&bytes).map_err(|e| ApiError::internal(e.to_string()))?;
    if !data["unit_meta"].is_array() {
        data["unit_meta"] = json!(data["units"].as_array().into_iter().flatten().map(|text| {
            json!({"source": if record["render_mode"] == "pdf" {"legacy_unknown"} else {"embedded"},
                "original_text": text, "text_hash": hash(text.as_str().unwrap_or(""))})
        }).collect::<Vec<_>>());
    }
    if record["render_mode"] == "pdf" {
        let directory = material_directory(state, id)?;
        for (i, meta) in data["unit_meta"]
            .as_array_mut()
            .into_iter()
            .flatten()
            .enumerate()
        {
            let relative = format!("media/page-{:04}.jpg", i + 1);
            meta["image_path"] = json!(relative);
            if let Ok(bytes) = std::fs::read(directory.join(&relative)) {
                meta["image_sha256"] = json!(hex::encode(Sha256::digest(bytes)));
            }
        }
    }
    let mut connection = state.database.connect()?;
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql)?;
    insert_snapshot(&tx, id, revision, &data)?;
    tx.commit().map_err(sql)
}

pub(super) fn list(state: &AppState, id: &str) -> ApiResult<Value> {
    ensure_snapshot(state, id)?;
    let connection = state.database.connect()?;
    let mut statement = connection
        .prepare(
            "SELECT revision, created_at, COALESCE(json_array_length(data_json,'$.unit_meta'),0)
         FROM reading_revisions r WHERE material_id=?1 ORDER BY revision DESC",
        )
        .map_err(sql)?;
    let rows = statement
        .query_map([id], |row| {
            Ok(json!({
                "material_id": id, "revision": row.get::<_, usize>(0)?,
                "created_at": row.get::<_, f64>(1)?, "unit_count": row.get::<_, usize>(2)?
            }))
        })
        .map_err(sql)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql)?;
    Ok(json!(rows))
}

pub(super) fn revision_unit(
    state: &AppState,
    id: &str,
    revision: usize,
    locator: usize,
) -> ApiResult<Value> {
    ensure_snapshot(state, id)?;
    let data = snapshot_data(state, id, revision)?
        .ok_or_else(|| ApiError::not_found("材料历史版本不存在。"))?;
    Ok(
        json!({"locator": locator, "revision": revision, "unit": data["unit"].as_str().unwrap_or("section"),
        "text": unit_text(&data, locator)?}),
    )
}

async fn corrections(
    ScopedState(state): ScopedState,
    AxumPath(id): AxumPath<String>,
) -> ApiResult<Json<Value>> {
    material_record(&state, &id)?;
    let connection = state.database.connect()?;
    let mut statement = connection.prepare(
        "SELECT payload_json FROM reading_ocr_corrections WHERE material_id=?1 ORDER BY created_at DESC"
    ).map_err(sql)?;
    let rows = statement
        .query_map([id], |row| row.get::<_, String>(0))
        .map_err(sql)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql)?
        .into_iter()
        .map(|raw| {
            serde_json::from_str::<Value>(&raw).map_err(|e| ApiError::internal(e.to_string()))
        })
        .collect::<ApiResult<Vec<_>>>()?;
    Ok(Json(
        json!({"pending_count": rows.iter().filter(|r| r["status"] == "pending").count(), "items": rows}),
    ))
}

pub(super) async fn ensure_page(state: &AppState, id: &str, locator: usize) -> ApiResult<Value> {
    let record = material_record(state, id)?;
    let data = load_material_data(state, id)?;
    let text = unit_text(&data, locator)?;
    if record["render_mode"] != "pdf" {
        return Err(ApiError::bad_request("按页图片仅支持 PDF。"));
    }
    let directory = material_directory(state, id)?;
    let name = format!("page-{locator:04}.jpg");
    let path = directory.join("media").join(&name);
    if !path.is_file() {
        let raw = safe_child(&directory, record["relative_path"].as_str().unwrap_or(""))?;
        super::super::pdf_text::render_page(raw, path.clone(), locator)
            .await
            .map_err(ApiError::bad_request)?;
    }
    let bytes =
        std::fs::read(&path).map_err(|e| ApiError::internal(format!("无法读取页面图片：{e}")))?;
    Ok(json!({"name": name, "locator": locator, "text": text,
        "sha256": hex::encode(Sha256::digest(&bytes)),
        "url": format!("/api/reading/materials/{id}/media/{name}"),
        "revision": record["revision"]}))
}

async fn page_image(
    ScopedState(state): ScopedState,
    AxumPath((id, locator)): AxumPath<(String, usize)>,
) -> ApiResult<Json<Value>> {
    ensure_page(&state, &id, locator).await.map(Json)
}

pub(super) fn propose(
    arguments: &Value,
    runtime: &super::super::tools::ToolRuntime<'_>,
) -> super::super::tools::ToolOutcome {
    match propose_inner(arguments, runtime) {
        Ok(row) => super::super::tools::ToolOutcome::success(
            format!(
                "已保存第 {} 页的 OCR 校正建议，等待用户逐页确认。当前检索文本尚未改变。",
                row["locator"]
            ),
            json!({"kind": "ocr_correction_proposed", "correction": row}),
        ),
        Err(error) => super::super::tools::ToolOutcome::failure(error.to_string()),
    }
}

fn propose_inner(
    arguments: &Value,
    runtime: &super::super::tools::ToolRuntime<'_>,
) -> ApiResult<Value> {
    let evidence_id = arguments["evidence_id"].as_str().unwrap_or("");
    let evidence = runtime
        .state
        .database
        .get_resource("reading_image_evidence", evidence_id)?
        .ok_or_else(|| {
            ApiError::bad_request("请先通过 view_figure 查看页面，并使用返回的 evidence_id。")
        })?;
    if evidence["session_id"].as_str() != Some(runtime.session_id)
        || evidence["turn_id"].as_str() != Some(runtime.turn_id)
    {
        return Err(ApiError::forbidden(
            "页面校正必须使用本次对话回合的看图依据。",
        ));
    }
    let id = evidence["material_id"].as_str().unwrap_or("");
    let material = load_turn_material(runtime.state, runtime.request.unwrap_or(&Value::Null))
        .map_err(ApiError::bad_request)?
        .ok_or_else(|| ApiError::bad_request("当前没有阅读材料。"))?;
    if material.material_id != id {
        return Err(ApiError::bad_request("请先切换到看图依据对应的材料。"));
    }
    if material.record["revision"] != evidence["revision"] {
        return Err(ApiError::conflict("看图后材料版本已改变，请重新查看页面。"));
    }
    let locator = evidence["locator"].as_u64().unwrap_or(0) as usize;
    ensure_snapshot(runtime.state, id)?;
    let data = load_material_data(runtime.state, id)?;
    let original = unit_text(&data, locator)?;
    let suggested = arguments["text"].as_str().unwrap_or("").trim();
    let confidence = arguments["confidence"]
        .as_f64()
        .filter(|v| v.is_finite() && (0.0..=1.0).contains(v))
        .ok_or_else(|| ApiError::bad_request("置信度必须为 0 到 1 之间的数值。"))?;
    if suggested.is_empty() || suggested == original.trim() || suggested.chars().count() > 200_000 {
        return Err(ApiError::bad_request(
            "校正建议必须是与当前文本不同的非空完整页文本，且不超过二十万字符。",
        ));
    }
    if material.record["render_mode"] != "pdf" {
        return Err(ApiError::bad_request("OCR 校正仅用于 PDF 页面。"));
    }
    let now = crate::storage::now();
    let correction_id = format!("ocr_{}", Uuid::new_v4());
    let row = json!({
        "id": correction_id, "material_id": id, "locator": locator,
        "base_revision": material.record["revision"], "status": "pending",
        "original_ocr": data["unit_meta"].get(locator - 1).and_then(|m| m.get("original_text")).cloned().unwrap_or_else(|| json!(original)),
        "previous_text": original, "suggested_text": suggested, "final_text": Value::Null,
        "confidence": confidence, "summary": arguments["summary"].as_str().unwrap_or("").chars().take(4000).collect::<String>(),
        "model": evidence["model"], "profile_id": evidence["profile_id"], "model_id": evidence["model_id"],
        "prompt_version": PROMPT_VERSION, "evidence_id": evidence_id, "image_sha256": evidence["image_sha256"],
        "created_at": now, "updated_at": now, "applied_revision": Value::Null
    });
    let connection = runtime.state.database.connect()?;
    // 同一基础版本与相同建议幂等，避免工具重试产生重复审核卡片。
    let existing: Option<String> = connection.query_row(
        "SELECT payload_json FROM reading_ocr_corrections WHERE material_id=?1 AND locator=?2 AND base_revision=?3 AND status='pending'
         AND json_extract(payload_json, '$.suggested_text')=?4",
        params![id, locator, material.record["revision"].as_u64().unwrap_or(1), suggested], |r| r.get(0)
    ).optional().map_err(sql)?;
    if let Some(raw) = existing {
        return serde_json::from_str(&raw).map_err(|e| ApiError::internal(e.to_string()));
    }
    connection
        .execute(
            "INSERT INTO reading_ocr_corrections VALUES(?1,?2,?3,?4,'pending',?5,?6,?6)",
            params![
                correction_id,
                id,
                locator,
                material.record["revision"].as_u64().unwrap_or(1),
                row.to_string(),
                now
            ],
        )
        .map_err(sql)?;
    Ok(row)
}

fn correction(tx: &Transaction<'_>, id: &str, correction_id: &str) -> ApiResult<Value> {
    let raw: Option<String> = tx
        .query_row(
            "SELECT payload_json FROM reading_ocr_corrections WHERE id=?1 AND material_id=?2",
            params![correction_id, id],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql)?;
    serde_json::from_str(&raw.ok_or_else(|| ApiError::not_found("校正建议不存在。"))?)
        .map_err(|e| ApiError::internal(e.to_string()))
}

fn save_correction(tx: &Transaction<'_>, row: &Value) -> ApiResult<()> {
    tx.execute(
        "UPDATE reading_ocr_corrections SET status=?1, payload_json=?2, updated_at=?3 WHERE id=?4",
        params![
            row["status"].as_str(),
            row.to_string(),
            crate::storage::now(),
            row["id"].as_str()
        ],
    )
    .map_err(sql)?;
    Ok(())
}

async fn accept(
    ScopedState(state): ScopedState,
    AxumPath((id, correction_id)): AxumPath<(String, String)>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let row = accept_inner(&state, &id, &correction_id, &body)?;
    index::invalidate(&state);
    index::schedule_for_material(&state, &id);
    Ok(Json(row))
}

fn accept_inner(state: &AppState, id: &str, correction_id: &str, body: &Value) -> ApiResult<Value> {
    ensure_snapshot(state, id)?;
    let expected = body["expected_revision"]
        .as_u64()
        .ok_or_else(|| ApiError::bad_request("确认时必须提供 expected_revision。"))?;
    let mut connection = state.database.connect()?;
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql)?;
    let mut row = correction(&tx, id, correction_id)?;
    if row["status"] != "pending" {
        return Err(ApiError::conflict("该建议已处理，请刷新审核列表。"));
    }
    let raw: String = tx
        .query_row(
            "SELECT payload_json FROM resources WHERE domain=?1 AND id=?2 AND deleted_at IS NULL",
            params![MATERIAL_DOMAIN, id],
            |r| r.get(0),
        )
        .map_err(sql)?;
    let mut record: Value =
        serde_json::from_str(&raw).map_err(|e| ApiError::internal(e.to_string()))?;
    let revision = record["revision"].as_u64().unwrap_or(1);
    if revision != expected {
        return Err(ApiError::conflict("材料版本已改变，请刷新后重新确认。"));
    }
    let locator = row["locator"].as_u64().unwrap_or(0) as usize;
    let mut data = snapshot_data(state, id, revision as usize)?
        .ok_or_else(|| ApiError::not_found("当前版本不存在。"))?;
    if row["previous_text"].as_str() != Some(unit_text(&data, locator)?.as_str()) {
        return Err(ApiError::conflict("该页已由其他校正修改，请重新生成建议。"));
    }
    let final_text = body
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_else(|| row["suggested_text"].as_str().unwrap_or(""))
        .trim()
        .to_string();
    if final_text.is_empty() || final_text.chars().count() > 200_000 {
        return Err(ApiError::bad_request(
            "确认文本不能为空且不能超过二十万字符。",
        ));
    }
    if row["previous_text"] == final_text {
        return Err(ApiError::bad_request("确认文本与当前文本相同。"));
    }
    data["units"][locator - 1] = json!(final_text);
    data["unit_meta"][locator - 1]["text_hash"] = json!(hash(&final_text));
    data["unit_meta"][locator - 1]["source"] = json!("user_confirmed");
    let next = revision + 1;
    insert_snapshot(&tx, id, next as usize, &data)?;
    record["revision"] = json!(next);
    record["char_count"] = json!(data["units"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|t| t.chars().count())
        .sum::<usize>());
    tx.execute(
        "UPDATE resources SET payload_json=?1, updated_at=?2 WHERE domain=?3 AND id=?4",
        params![
            record.to_string(),
            crate::storage::now(),
            MATERIAL_DOMAIN,
            id
        ],
    )
    .map_err(sql)?;
    // 版本快照和当前材料指针同一事务提交，units.json 继续保留初次提取内容。
    tx.execute(
        "DELETE FROM reading_vectors WHERE material_id=?1 AND locator=?2",
        params![id, locator],
    )
    .map_err(sql)?;
    revalidate_annotations(&tx, id, locator, revision, next, &final_text)?;
    revalidate_citations(&tx, id, locator, next, &final_text)?;
    row["status"] = json!("accepted");
    row["final_text"] = json!(final_text);
    row["applied_revision"] = json!(next);
    row["updated_at"] = json!(crate::storage::now());
    save_correction(&tx, &row)?;
    tx.commit().map_err(sql)?;
    Ok(row)
}

async fn reject(
    ScopedState(state): ScopedState,
    AxumPath((id, correction_id)): AxumPath<(String, String)>,
) -> ApiResult<Json<Value>> {
    material_record(&state, &id)?;
    let mut connection = state.database.connect()?;
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sql)?;
    let mut row = correction(&tx, &id, &correction_id)?;
    if row["status"] != "pending" {
        return Err(ApiError::conflict("该建议已经处理。"));
    }
    row["status"] = json!("rejected");
    row["updated_at"] = json!(crate::storage::now());
    save_correction(&tx, &row)?;
    tx.commit().map_err(sql)?;
    Ok(Json(row))
}

/// 用原文及前后文消除重复命中；位置采用 Unicode 字符偏移，不使用 UTF-8 字节下标。
pub(super) fn reanchor(text: &str, quote: &str, selectors: &Value) -> (String, Option<usize>) {
    let selector_rows = selectors.as_array().map(Vec::as_slice).unwrap_or(&[]);
    let selector = selector_rows
        .iter()
        .find(|s| s["type"] == "TextQuoteSelector");
    let exact = selector
        .and_then(|s| s["exact"].as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(quote);
    if exact.is_empty() {
        return ("stale".into(), None);
    }
    let mut matches = text
        .match_indices(exact)
        .map(|(offset, _)| offset)
        .collect::<Vec<_>>();
    if matches.len() > 1 {
        matches.retain(|offset| {
            let prefix = selector.and_then(|s| s["prefix"].as_str()).unwrap_or("");
            let suffix = selector.and_then(|s| s["suffix"].as_str()).unwrap_or("");
            text[..*offset].ends_with(prefix) && text[*offset + exact.len()..].starts_with(suffix)
        });
        if matches.len() > 1 {
            if let Some(position) = selector_rows
                .iter()
                .find(|s| s["type"] == "TextPositionSelector")
                .and_then(|s| s["start"].as_u64())
            {
                let at_position = matches
                    .iter()
                    .copied()
                    .filter(|offset| text[..*offset].chars().count() == position as usize)
                    .collect::<Vec<_>>();
                if at_position.len() == 1 {
                    matches = at_position;
                }
            }
        }
    }
    match matches.as_slice() {
        [offset] => ("resolved".into(), Some(text[..*offset].chars().count())),
        [] => ("unresolved".into(), None),
        _ => ("ambiguous".into(), None),
    }
}

fn revalidate_annotations(
    tx: &Transaction<'_>,
    id: &str,
    locator: usize,
    old: u64,
    next: u64,
    text: &str,
) -> ApiResult<()> {
    let domain = annotation_domain(id);
    let rows = {
        let mut s = tx
            .prepare(
                "SELECT id, payload_json FROM resources WHERE domain=?1 AND deleted_at IS NULL",
            )
            .map_err(sql)?;
        let result = s
            .query_map([&domain], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(sql)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql)?;
        result
    };
    for (annotation_id, raw) in rows {
        let mut mark: Value =
            serde_json::from_str(&raw).map_err(|e| ApiError::internal(e.to_string()))?;
        let previous = mark["material_revision"].as_u64().unwrap_or(old);
        if mark["locator"].as_u64() == Some(locator as u64) {
            let (resolution, offset) = reanchor(
                text,
                mark["quote"].as_str().unwrap_or(""),
                &mark["selectors"],
            );
            mark["previous_revision"] = json!(previous);
            mark["resolution"] = json!(resolution);
            // PDF 的旧矩形可能指向 OCR 错字，保留备份而不绘制错误位置。
            if mark.get("original_rects").is_none() {
                mark["original_rects"] = mark["rects"].clone();
            }
            mark["previous_rects"] = mark["rects"].clone();
            mark["rects"] = json!([]);
            if let Some(start) = offset {
                let len = mark["quote"].as_str().unwrap_or("").chars().count();
                mark["selectors"] = json!([
                    {"type": "TextQuoteSelector", "exact": mark["quote"]},
                    {"type": "TextPositionSelector", "start": start, "end": start + len}
                ]);
            }
        }
        mark["material_revision"] = json!(next);
        tx.execute(
            "UPDATE resources SET payload_json=?1, updated_at=?2 WHERE domain=?3 AND id=?4",
            params![
                mark.to_string(),
                crate::storage::now(),
                domain,
                annotation_id
            ],
        )
        .map_err(sql)?;
    }
    Ok(())
}

fn revalidate_citations(
    tx: &Transaction<'_>,
    id: &str,
    locator: usize,
    next: u64,
    text: &str,
) -> ApiResult<()> {
    let rows = {
        let mut s = tx.prepare("SELECT id, payload_json FROM reading_citations WHERE material_id=?1 AND locator=?2").map_err(sql)?;
        let rows = s
            .query_map(params![id, locator], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(sql)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql)?;
        rows
    };
    for (citation_id, raw) in rows {
        let mut row: Value =
            serde_json::from_str(&raw).map_err(|e| ApiError::internal(e.to_string()))?;
        let (resolution, offset) =
            reanchor(text, row["quote"].as_str().unwrap_or(""), &row["selectors"]);
        row["resolution"] = json!(resolution);
        row["current_revision"] = json!(next);
        row["current_offset"] = json!(offset);
        tx.execute(
            "UPDATE reading_citations SET payload_json=?1 WHERE id=?2",
            params![row.to_string(), citation_id],
        )
        .map_err(sql)?;
    }
    Ok(())
}

pub(super) fn remember_citation(
    state: &AppState,
    material: &TurnMaterial,
    locator: usize,
    quote: &str,
) -> ApiResult<()> {
    ensure_snapshot(state, &material.material_id)?;
    let revision = material.record["revision"].as_u64().unwrap_or(1);
    let id = hash(&format!(
        "{}:{revision}:{locator}:{quote}",
        material.material_id
    ));
    let payload = json!({"quote":quote,"selectors":[],"resolution":"resolved","current_revision":revision,
        "original_text_hash":hash(&unit_text(&material.data,locator)?)});
    state
        .database
        .connect()?
        .execute(
            "INSERT OR IGNORE INTO reading_citations VALUES(?1,?2,?3,?4,?5)",
            params![
                id,
                material.material_id,
                revision,
                locator,
                payload.to_string()
            ],
        )
        .map_err(sql)?;
    Ok(())
}

async fn resolve_reference(
    ScopedState(state): ScopedState,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let record = material_record(&state, &id)?;
    let revision = body["revision"]
        .as_u64()
        .ok_or_else(|| ApiError::bad_request("引用缺少 revision。"))? as usize;
    let locator = body["locator"]
        .as_u64()
        .ok_or_else(|| ApiError::bad_request("引用缺少 locator。"))? as usize;
    let old = revision_unit(&state, &id, revision, locator)?;
    let current = load_material_data(&state, &id)?;
    let text = unit_text(&current, locator)?;
    let quote = body["quote"].as_str().unwrap_or("");
    let (resolution, offset) = if quote.is_empty() && old["text"] == text {
        ("resolved".to_string(), None)
    } else {
        reanchor(&text, quote, &body["selectors"])
    };
    let payload = json!({"material_id": id, "revision": revision, "current_revision": record["revision"],
        "locator": locator, "resolution": resolution, "offset": offset,
        "old_text": old["text"], "current_text": text, "quote": quote, "selectors": body["selectors"]});
    let citation_id = hash(&format!("{id}:{revision}:{locator}:{quote}"));
    state
        .database
        .connect()?
        .execute(
            "INSERT INTO reading_citations VALUES(?1,?2,?3,?4,?5)
        ON CONFLICT(id) DO UPDATE SET payload_json=excluded.payload_json",
            params![citation_id, id, revision, locator, payload.to_string()],
        )
        .map_err(sql)?;
    Ok(Json(payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_reanchor_uses_context_and_does_not_guess() {
        assert_eq!(
            reanchor("这是错误，这里正确。", "正确", &json!([])),
            ("resolved".into(), Some(7))
        );
        assert_eq!(reanchor("重复 重复", "重复", &json!([])).0, "ambiguous");
        assert_eq!(reanchor("原文已修正", "错误", &json!([])).0, "unresolved");
        assert_eq!(
            reanchor(
                "甲重复乙 丙重复丁",
                "重复",
                &json!([
                    {"type":"TextQuoteSelector","exact":"重复","prefix":"丙","suffix":"丁"}
                ])
            ),
            ("resolved".into(), Some(6))
        );
    }

    #[tokio::test]
    async fn correction_requires_current_turn_evidence_and_explicit_user_acceptance() {
        let (state, _directory) = super::super::tests::pdf_state();
        let bytes = super::super::super::pdf_text::tests::test_pdf(&[
            "wr0ng text",
            "unchanged second page",
        ]);
        ingest_bytes(
            &state,
            "ocr-fixture",
            "ocr-fixture",
            "original.pdf",
            &bytes,
            "file",
            "",
        )
        .await
        .unwrap();
        let raw_before = std::fs::read(
            material_directory(&state, "ocr-fixture")
                .unwrap()
                .join("raw/original.pdf"),
        )
        .unwrap();
        let request =
            json!({"workspace_mode":"immersive_reading","reading_material_id":"ocr-fixture"});
        let runtime = super::super::super::tools::ToolRuntime {
            state: &state,
            request: Some(&request),
            llm: None,
            cancel: None,
            turn_id: "turn-1",
            session_id: "session-1",
            language: "zh",
            source_index: None,
        };
        let arguments = json!({"evidence_id":"evidence-test","text":"correct text","confidence":0.91,"summary":"修正字形识别错误"});
        assert!(propose_inner(&arguments, &runtime).is_err());
        state.database.put_resource("reading_image_evidence","evidence-test",&json!({
            "material_id":"ocr-fixture","locator":1,"revision":1,"session_id":"session-1","turn_id":"turn-1",
            "model":"vision-test","profile_id":"profile-test","image_sha256":"image-test"
        })).unwrap();
        let proposal = propose_inner(&arguments, &runtime).unwrap();
        let duplicate = propose_inner(&arguments, &runtime).unwrap();
        assert_eq!(proposal["id"], duplicate["id"]);
        assert!(
            unit_text(&load_material_data(&state, "ocr-fixture").unwrap(), 1)
                .unwrap()
                .contains("wr0ng")
        );
        let correction_id = proposal["id"].as_str().unwrap();
        assert!(accept_inner(
            &state,
            "ocr-fixture",
            correction_id,
            &json!({"expected_revision":2})
        )
        .is_err());
        state.database.put_resource(&annotation_domain("ocr-fixture"),"lost",&json!({
            "locator":1,"quote":"wr0ng","selectors":[],"rects":[[0,0,1,1]],"material_revision":1
        })).unwrap();
        let accepted = accept_inner(
            &state,
            "ocr-fixture",
            correction_id,
            &json!({"expected_revision":1,"text":"user corrected text"}),
        )
        .unwrap();
        assert_eq!(accepted["applied_revision"], 2);
        assert_eq!(accepted["suggested_text"], "correct text");
        assert_eq!(accepted["final_text"], "user corrected text");
        assert_eq!(
            unit_text(&load_material_data(&state, "ocr-fixture").unwrap(), 1).unwrap(),
            "user corrected text"
        );
        assert!(revision_unit(&state, "ocr-fixture", 1, 1).unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("wr0ng"));
        assert_eq!(
            list(&state, "ocr-fixture")
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert!(revision_unit(&state, "ocr-fixture", 99, 1).is_err());
        assert_eq!(list(&state, "ocr-fixture").unwrap()[0]["unit_count"], 2);
        assert!(revision_unit(&state, "ocr-fixture", 2, 2).unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("unchanged second page"));
        let stored_units: usize = state
            .database
            .connect()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM reading_revision_units WHERE material_id='ocr-fixture'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            stored_units, 3,
            "历史版本只追加被修改页面，避免重复保存整本书"
        );
        let repeated_originals: usize = state.database.connect().unwrap().query_row(
            "SELECT instr(data_json, 'unchanged second page') FROM reading_revisions WHERE material_id='ocr-fixture' AND revision=2",
            [],
            |row| row.get(0),
        ).unwrap();
        assert_eq!(
            repeated_originals, 0,
            "版本元数据也不得重复保存原始 OCR 全文"
        );
        assert_eq!(
            load_material_data(&state, "ocr-fixture").unwrap()["unit_meta"][0]["original_text"],
            proposal["original_ocr"],
            "读取新版本时仍能继承基线中的原始 OCR"
        );
        let mark = state
            .database
            .get_resource(&annotation_domain("ocr-fixture"), "lost")
            .unwrap()
            .unwrap();
        assert_eq!(mark["resolution"], "unresolved");
        assert_eq!(mark["material_revision"], 2);
        assert_eq!(mark["rects"], json!([]));
        assert_eq!(
            raw_before,
            std::fs::read(
                material_directory(&state, "ocr-fixture")
                    .unwrap()
                    .join("raw/original.pdf")
            )
            .unwrap()
        );
        assert!(accept_inner(
            &state,
            "ocr-fixture",
            correction_id,
            &json!({"expected_revision":2})
        )
        .is_err());
        state.database.initialize().unwrap();
        assert_eq!(
            unit_text(&load_material_data(&state, "ocr-fixture").unwrap(), 1).unwrap(),
            "user corrected text"
        );
    }
}
