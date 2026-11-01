use crate::storage::Database;
use rusqlite::{params, OptionalExtension, Transaction};
use serde_json::{json, Value};

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn resource(
    transaction: &Transaction<'_>,
    domain: &str,
    id: &str,
) -> Result<Option<Value>, String> {
    let raw = transaction
        .query_row(
            "SELECT payload_json FROM resources WHERE domain = ?1 AND id = ?2 AND deleted_at IS NULL",
            params![domain, id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| format!("无法读取阅读会话关联：{error}"))?;
    Ok(raw.and_then(|raw| serde_json::from_str(&raw).ok()))
}

fn binding(value: &Value) -> Option<(String, String)> {
    let workspace =
        text(value, "reading_workspace_id").or_else(|| text(value, "readingWorkspaceId"))?;
    let material =
        text(value, "reading_material_id").or_else(|| text(value, "readingMaterialId"))?;
    Some((workspace.to_string(), material.to_string()))
}

/// 从旧回合的明确关联补齐会话，保留原有消息、状态、排序和用户选择。
pub(super) fn restore_legacy_bindings(database: &Database) -> Result<(), String> {
    database.with_connection(|connection| {
        let transaction = connection
            .transaction()
            .map_err(|error| format!("无法开始阅读会话升级：{error}"))?;
        let schema_ready = transaction
            .query_row(
                "SELECT COUNT(*) = 2 FROM sqlite_master \
                 WHERE type = 'table' AND name IN ('sessions', 'resources')",
                [],
                |row| row.get::<_, bool>(0),
            )
            .map_err(|error| format!("无法检查阅读会话数据库：{error}"))?;
        if !schema_ready {
            return transaction
                .commit()
                .map_err(|error| format!("无法结束阅读会话升级：{error}"));
        }
        let sessions = {
            let mut query = transaction
                .prepare(
                    "SELECT id, title, preferences_json, created_at, updated_at FROM sessions \
                     WHERE deleted_at IS NULL AND preferences_json LIKE '%immersive_reading%'",
                )
                .map_err(|error| format!("无法查询旧阅读会话：{error}"))?;
            let rows = query
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, f64>(3)?,
                        row.get::<_, f64>(4)?,
                    ))
                })
                .map_err(|error| format!("无法读取旧阅读会话：{error}"))?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|error| format!("无法解析旧阅读会话：{error}"))?
        };
        for (id, title, raw_preferences, created_at, updated_at) in sessions {
            let Ok(mut preferences) = serde_json::from_str::<Value>(&raw_preferences) else {
                continue;
            };
            if text(&preferences, "workspace_mode") != Some("immersive_reading") {
                continue;
            }
            let saved = resource(&transaction, "reading_session", &id)?;
            let saved_binding = saved.as_ref().and_then(|session| {
                Some((
                    text(session, "workspace_id")?.to_string(),
                    text(session, "active_material_id")?.to_string(),
                ))
            });
            if binding(&preferences).is_some_and(|saved| Some(saved) == saved_binding) {
                continue;
            }
            let mut candidates = Vec::new();
            if let Some(candidate) = binding(&preferences) {
                candidates.push(candidate);
            }
            {
                let mut query = transaction
                    .prepare("SELECT payload_json FROM resources \
                              WHERE domain = 'pending_input' AND deleted_at IS NULL \
                              ORDER BY updated_at DESC")
                    .map_err(|error| format!("无法查询旧阅读回合：{error}"))?;
                let rows = query
                    .query_map([], |row| row.get::<_, String>(0))
                    .map_err(|error| format!("无法读取旧阅读回合：{error}"))?;
                for raw in rows {
                    let raw = raw.map_err(|error| format!("无法解析旧阅读回合：{error}"))?;
                    let Ok(pending) = serde_json::from_str::<Value>(&raw) else {
                        continue;
                    };
                    let pending_session_id = text(&pending, "session_id");
                    if pending_session_id.is_some_and(|session_id| session_id != id) {
                        continue;
                    }
                    let Some(request) = pending.get("request") else {
                        continue;
                    };
                    let request_session_id = text(request, "session_id");
                    if request_session_id.is_some_and(|session_id| session_id != id)
                        || (pending_session_id != Some(id.as_str())
                            && request_session_id != Some(id.as_str()))
                    {
                        continue;
                    }
                    if let Some(candidate) = binding(request) {
                        candidates.push(candidate);
                        break;
                    }
                }
            }
            {
                let mut query = transaction
                    .prepare(
                        "SELECT metadata_json FROM messages \
                         WHERE session_id = ?1 AND role = 'user' ORDER BY id DESC",
                    )
                    .map_err(|error| format!("无法查询阅读消息快照：{error}"))?;
                let rows = query
                    .query_map(params![id], |row| row.get::<_, String>(0))
                    .map_err(|error| format!("无法读取阅读消息快照：{error}"))?;
                for raw in rows {
                    let raw = raw.map_err(|error| format!("无法解析阅读消息快照：{error}"))?;
                    let Ok(metadata) = serde_json::from_str::<Value>(&raw) else {
                        continue;
                    };
                    if let Some(snapshot) = metadata
                        .get("request_snapshot")
                        .or_else(|| metadata.get("requestSnapshot"))
                    {
                        if let Some(candidate) = binding(snapshot).or_else(|| {
                            snapshot.get("readingTurnFields").and_then(binding)
                        }) {
                            candidates.push(candidate);
                            break;
                        }
                    }
                }
            }
            if let Some(saved) = &saved {
                if let (Some(workspace), Some(material)) =
                    (text(saved, "workspace_id"), text(saved, "active_material_id"))
                {
                    candidates.push((workspace.to_string(), material.to_string()));
                }
            }
            for (workspace_id, material_id) in candidates {
                if text(&preferences, "reading_workspace_id")
                    .is_some_and(|known| known != workspace_id)
                    || text(&preferences, "reading_material_id")
                        .is_some_and(|known| known != material_id)
                {
                    continue;
                }
                let Some(workspace) = resource(&transaction, "reading_workspace", &workspace_id)?
                else {
                    continue;
                };
                let belongs = workspace
                    .get("tabs")
                    .and_then(Value::as_array)
                    .is_some_and(|tabs| {
                        tabs.iter()
                            .any(|tab| text(tab, "material_id") == Some(material_id.as_str()))
                    });
                if !belongs
                    || resource(&transaction, "reading_material", &material_id)?.is_none()
                {
                    continue;
                }
                preferences["reading_workspace_id"] = json!(workspace_id);
                preferences["reading_material_id"] = json!(material_id);
                transaction
                    .execute(
                        "UPDATE sessions SET preferences_json = ?2 WHERE id = ?1",
                        params![id, preferences.to_string()],
                    )
                    .map_err(|error| format!("无法恢复阅读会话偏好：{error}"))?;
                let mut reading_session = saved.clone().unwrap_or_else(|| {
                    json!({
                        "session_id": id,
                        "title": title,
                        "created_at": created_at,
                        "updated_at": updated_at,
                        "linked_session_ids": []
                    })
                });
                reading_session["workspace_id"] = json!(workspace_id);
                reading_session["active_material_id"] = json!(material_id);
                transaction
                    .execute(
                        "INSERT INTO resources(domain, id, payload_json, created_at, updated_at) \
                         VALUES('reading_session', ?1, ?2, ?3, ?4) \
                         ON CONFLICT(domain, id) DO UPDATE SET payload_json = excluded.payload_json \
                         WHERE resources.deleted_at IS NULL",
                        params![id, reading_session.to_string(), created_at, updated_at],
                    )
                    .map_err(|error| format!("无法恢复阅读会话材料关联：{error}"))?;
                break;
            }
        }
        transaction
            .commit()
            .map_err(|error| format!("无法保存阅读会话升级：{error}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn database() -> (Database, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let database = Database::new(directory.path().join("test.sqlite3"));
        database.initialize().unwrap();
        database
            .put_resource(
                "reading_workspace",
                "collection",
                &json!({
                    "workspace_id": "collection",
                    "active_material_id": "material-a",
                    "tabs": [{"material_id": "material-a"}, {"material_id": "material-b"}]
                }),
            )
            .unwrap();
        for id in ["material-a", "material-b", "outside"] {
            database
                .put_resource("reading_material", id, &json!({"material_id": id}))
                .unwrap();
        }
        (database, directory)
    }

    #[test]
    fn 旧会话从等待回合恢复准确资料并保持历史排序() {
        let (database, _directory) = database();
        database
            .create_session(
                "legacy",
                "旧阅读记录",
                &json!({"workspace_mode": "immersive_reading", "capability": "chat"}),
                "",
            )
            .unwrap();
        database
            .put_resource(
                "pending_input",
                "turn",
                &json!({
                    "session_id": "legacy",
                    "request": {
                        "session_id": "legacy",
                        "reading_workspace_id": "collection",
                        "reading_material_id": "material-b"
                    }
                }),
            )
            .unwrap();
        let before = database.get_session("legacy").unwrap().unwrap();
        restore_legacy_bindings(&database).unwrap();
        let after = database.get_session("legacy").unwrap().unwrap();
        assert_eq!(after["preferences"]["reading_workspace_id"], "collection");
        assert_eq!(after["preferences"]["reading_material_id"], "material-b");
        for field in ["created_at", "updated_at", "status", "messages", "title"] {
            assert_eq!(after[field], before[field], "{field}");
        }
        let saved = database
            .get_resource("reading_session", "legacy")
            .unwrap()
            .unwrap();
        assert_eq!(saved["active_material_id"], "material-b");
        restore_legacy_bindings(&database).unwrap();
        assert_eq!(database.get_session("legacy").unwrap().unwrap(), after);
    }

    #[test]
    fn 恢复读取最新消息快照并支持两种字段命名() {
        let (database, _directory) = database();
        for (id, snapshot) in [
            (
                "camel",
                json!({"readingWorkspaceId": "collection", "readingMaterialId": "material-b"}),
            ),
            (
                "snake",
                json!({"readingTurnFields": {
                    "reading_workspace_id": "collection", "reading_material_id": "material-b"
                }}),
            ),
        ] {
            database
                .create_session(
                    id,
                    "历史阅读",
                    &json!({"workspace_mode": "immersive_reading"}),
                    "",
                )
                .unwrap();
            database
                .add_message(crate::storage::NewMessage {
                    session_id: id,
                    role: "user",
                    content: "问题",
                    capability: Some("chat"),
                    events: json!([]),
                    attachments: json!([]),
                    metadata: json!({"request_snapshot": snapshot}),
                    parent_message_id: None,
                })
                .unwrap();
        }
        restore_legacy_bindings(&database).unwrap();
        for id in ["camel", "snake"] {
            let session = database.get_session_meta(id).unwrap().unwrap();
            assert_eq!(session["preferences"]["reading_material_id"], "material-b");
        }
    }

    #[test]
    fn 最新等待回合优先于旧消息中的材料关联() {
        let (database, _directory) = database();
        database
            .create_session(
                "legacy",
                "历史阅读",
                &json!({"workspace_mode": "immersive_reading"}),
                "",
            )
            .unwrap();
        database
            .add_message(crate::storage::NewMessage {
                session_id: "legacy",
                role: "user",
                content: "旧书上的问题",
                capability: Some("chat"),
                events: json!([]),
                attachments: json!([]),
                metadata: json!({"request_snapshot": {
                    "reading_workspace_id": "collection",
                    "reading_material_id": "material-a"
                }}),
                parent_message_id: None,
            })
            .unwrap();
        database
            .put_resource(
                "pending_input",
                "latest-turn",
                &json!({
                    "session_id": "legacy",
                    "request": {
                        "session_id": "legacy",
                        "reading_workspace_id": "collection",
                        "reading_material_id": "material-b"
                    }
                }),
            )
            .unwrap();

        restore_legacy_bindings(&database).unwrap();

        let session = database.get_session_meta("legacy").unwrap().unwrap();
        assert_eq!(session["preferences"]["reading_material_id"], "material-b");
    }

    #[test]
    fn 会话偏好与阅读资源不一致时同步为明确的会话材料() {
        let (database, _directory) = database();
        database
            .create_session(
                "legacy",
                "历史阅读",
                &json!({
                    "workspace_mode": "immersive_reading",
                    "reading_workspace_id": "collection",
                    "reading_material_id": "material-b"
                }),
                "",
            )
            .unwrap();
        database
            .put_resource(
                "reading_session",
                "legacy",
                &json!({
                    "session_id": "legacy",
                    "workspace_id": "collection",
                    "active_material_id": "material-a"
                }),
            )
            .unwrap();

        restore_legacy_bindings(&database).unwrap();

        let saved = database
            .get_resource("reading_session", "legacy")
            .unwrap()
            .unwrap();
        assert_eq!(saved["active_material_id"], "material-b");
    }

    #[test]
    fn 不猜测缺失关联不跨资料集且不覆盖退出阅读的选择() {
        let (database, _directory) = database();
        for (id, mode, material) in [
            ("missing", "immersive_reading", ""),
            ("outside", "immersive_reading", "outside"),
            ("chat", "", "material-b"),
            ("other", "mastery_path", "material-b"),
        ] {
            database
                .create_session(id, "历史记录", &json!({"workspace_mode": mode}), "")
                .unwrap();
            database
                .put_resource(
                    "pending_input",
                    id,
                    &json!({
                        "session_id": id,
                        "request": {
                            "reading_workspace_id": "collection",
                            "reading_material_id": material
                        }
                    }),
                )
                .unwrap();
        }
        restore_legacy_bindings(&database).unwrap();
        for id in ["missing", "outside", "chat", "other"] {
            let session = database.get_session_meta(id).unwrap().unwrap();
            assert!(session["preferences"].get("reading_workspace_id").is_none());
            assert!(database
                .get_resource("reading_session", id)
                .unwrap()
                .is_none());
        }
    }
}
