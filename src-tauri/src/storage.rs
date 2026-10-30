use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Database {
    path: Arc<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct NewMessage<'a> {
    pub session_id: &'a str,
    pub role: &'a str,
    pub content: &'a str,
    pub capability: Option<&'a str>,
    pub events: Value,
    pub attachments: Value,
    pub metadata: Value,
    pub parent_message_id: Option<i64>,
}

pub struct NewAttachment<'a> {
    pub id: &'a str,
    pub session_id: &'a str,
    pub filename: &'a str,
    pub mime_type: &'a str,
    pub relative_path: &'a str,
    pub size_bytes: i64,
    pub sha256: &'a str,
    pub metadata: &'a Value,
}

impl Database {
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: Arc::new(path.as_ref().to_path_buf()),
        }
    }

    pub fn path(&self) -> &Path {
        self.path.as_ref()
    }

    pub fn initialize(&self) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("无法创建数据库目录：{error}"))?;
        }
        let connection = self.connect()?;
        connection
            .execute_batch(
                r#"
                PRAGMA journal_mode = WAL;
                PRAGMA synchronous = NORMAL;
                PRAGMA foreign_keys = ON;

                CREATE TABLE IF NOT EXISTS settings (
                    key TEXT PRIMARY KEY,
                    value_json TEXT NOT NULL,
                    updated_at REAL NOT NULL
                );

                CREATE TABLE IF NOT EXISTS sessions (
                    id TEXT PRIMARY KEY,
                    title TEXT NOT NULL,
                    created_at REAL NOT NULL,
                    updated_at REAL NOT NULL,
                    status TEXT NOT NULL DEFAULT 'idle',
                    preferences_json TEXT NOT NULL DEFAULT '{}',
                    workspace_id TEXT NOT NULL DEFAULT '',
                    archived INTEGER NOT NULL DEFAULT 0,
                    deleted_at REAL
                );
                CREATE INDEX IF NOT EXISTS idx_sessions_updated
                    ON sessions(deleted_at, archived, updated_at DESC);

                CREATE TABLE IF NOT EXISTS messages (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                    role TEXT NOT NULL,
                    content TEXT NOT NULL,
                    capability TEXT,
                    events_json TEXT NOT NULL DEFAULT '[]',
                    attachments_json TEXT NOT NULL DEFAULT '[]',
                    metadata_json TEXT NOT NULL DEFAULT '{}',
                    created_at REAL NOT NULL,
                    parent_message_id INTEGER
                );
                CREATE INDEX IF NOT EXISTS idx_messages_session
                    ON messages(session_id, id);

                CREATE TABLE IF NOT EXISTS turns (
                    id TEXT PRIMARY KEY,
                    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                    status TEXT NOT NULL,
                    capability TEXT NOT NULL,
                    error TEXT NOT NULL DEFAULT '',
                    error_code TEXT NOT NULL DEFAULT '',
                    retryable INTEGER NOT NULL DEFAULT 0,
                    created_at REAL NOT NULL,
                    updated_at REAL NOT NULL,
                    finished_at REAL,
                    last_seq INTEGER NOT NULL DEFAULT 0
                );
                CREATE INDEX IF NOT EXISTS idx_turns_session
                    ON turns(session_id, created_at DESC);
                CREATE INDEX IF NOT EXISTS idx_turns_active
                    ON turns(status, updated_at DESC);

                CREATE TABLE IF NOT EXISTS turn_submissions (
                    client_submission_id TEXT PRIMARY KEY,
                    workspace_id TEXT NOT NULL DEFAULT '',
                    request_digest TEXT NOT NULL,
                    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                    turn_id TEXT REFERENCES turns(id) ON DELETE SET NULL,
                    superseded_by TEXT,
                    created_at REAL NOT NULL,
                    updated_at REAL NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_turn_submissions_session
                    ON turn_submissions(session_id, created_at);
                CREATE INDEX IF NOT EXISTS idx_turn_submissions_turn
                    ON turn_submissions(turn_id);

                CREATE TABLE IF NOT EXISTS turn_events (
                    turn_id TEXT NOT NULL REFERENCES turns(id) ON DELETE CASCADE,
                    seq INTEGER NOT NULL,
                    event_json TEXT NOT NULL,
                    PRIMARY KEY(turn_id, seq)
                );

                CREATE TABLE IF NOT EXISTS resources (
                    domain TEXT NOT NULL,
                    id TEXT NOT NULL,
                    payload_json TEXT NOT NULL,
                    created_at REAL NOT NULL,
                    updated_at REAL NOT NULL,
                    deleted_at REAL,
                    PRIMARY KEY(domain, id)
                );
                CREATE INDEX IF NOT EXISTS idx_resources_domain_updated
                    ON resources(domain, deleted_at, updated_at DESC);

                CREATE TABLE IF NOT EXISTS attachments (
                    id TEXT PRIMARY KEY,
                    session_id TEXT NOT NULL,
                    filename TEXT NOT NULL,
                    mime_type TEXT NOT NULL,
                    relative_path TEXT NOT NULL,
                    size_bytes INTEGER NOT NULL,
                    sha256 TEXT NOT NULL,
                    metadata_json TEXT NOT NULL DEFAULT '{}',
                    created_at REAL NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_attachments_session
                    ON attachments(session_id, created_at);

                CREATE TABLE IF NOT EXISTS auth_users (
                    username TEXT PRIMARY KEY,
                    password_hash TEXT NOT NULL,
                    role TEXT NOT NULL DEFAULT 'admin',
                    profile_json TEXT NOT NULL DEFAULT '{}',
                    created_at REAL NOT NULL
                );

                CREATE TABLE IF NOT EXISTS auth_accounts (
                    id TEXT PRIMARY KEY,
                    username TEXT NOT NULL UNIQUE COLLATE NOCASE,
                    password_hash TEXT NOT NULL,
                    role TEXT NOT NULL DEFAULT 'user',
                    disabled INTEGER NOT NULL DEFAULT 0,
                    avatar TEXT NOT NULL DEFAULT '',
                    preset TEXT NOT NULL DEFAULT 'standard',
                    learner_profile_json TEXT NOT NULL DEFAULT '{}',
                    book_permission_json TEXT NOT NULL DEFAULT '{}',
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_auth_accounts_role
                    ON auth_accounts(role, created_at);

                CREATE TABLE IF NOT EXISTS auth_sessions (
                    token_hash TEXT PRIMARY KEY,
                    user_id TEXT NOT NULL REFERENCES auth_accounts(id) ON DELETE CASCADE,
                    created_at TEXT NOT NULL,
                    expires_at INTEGER NOT NULL,
                    last_seen_at TEXT NOT NULL,
                    device_credential_id TEXT NOT NULL DEFAULT '',
                    device_session_nonce_hash TEXT NOT NULL DEFAULT ''
                );
                CREATE INDEX IF NOT EXISTS idx_auth_sessions_user
                    ON auth_sessions(user_id, expires_at);

                CREATE TABLE IF NOT EXISTS auth_device_credentials (
                    id TEXT PRIMARY KEY,
                    user_id TEXT NOT NULL REFERENCES auth_accounts(id) ON DELETE CASCADE,
                    device_name TEXT NOT NULL,
                    pairing_code_hash TEXT NOT NULL UNIQUE,
                    pin_hash TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    expires_at TEXT NOT NULL,
                    daily_limit_minutes INTEGER NOT NULL,
                    last_login_at TEXT NOT NULL DEFAULT '',
                    last_heartbeat_at TEXT NOT NULL DEFAULT '',
                    usage_day TEXT NOT NULL DEFAULT '',
                    used_seconds INTEGER NOT NULL DEFAULT 0,
                    session_nonce_hash TEXT NOT NULL DEFAULT '',
                    failed_pin_attempts INTEGER NOT NULL DEFAULT 0,
                    pin_locked_until TEXT NOT NULL DEFAULT '',
                    revoked_at TEXT,
                    revoked_by TEXT NOT NULL DEFAULT ''
                );
                CREATE INDEX IF NOT EXISTS idx_auth_devices_user
                    ON auth_device_credentials(user_id, revoked_at, created_at);

                CREATE TABLE IF NOT EXISTS guardian_relationships (
                    id TEXT PRIMARY KEY,
                    guardian_user_id TEXT NOT NULL REFERENCES auth_accounts(id) ON DELETE CASCADE,
                    learner_user_id TEXT NOT NULL REFERENCES auth_accounts(id) ON DELETE CASCADE,
                    permissions_json TEXT NOT NULL DEFAULT '[]',
                    granted_at TEXT NOT NULL,
                    revoked_at TEXT,
                    revoked_by TEXT NOT NULL DEFAULT '',
                    revocation_reason TEXT NOT NULL DEFAULT ''
                );
                CREATE INDEX IF NOT EXISTS idx_guardian_relationships_guardian
                    ON guardian_relationships(guardian_user_id, revoked_at);
                CREATE INDEX IF NOT EXISTS idx_guardian_relationships_learner
                    ON guardian_relationships(learner_user_id, revoked_at);

                CREATE TABLE IF NOT EXISTS user_grants (
                    user_id TEXT PRIMARY KEY REFERENCES auth_accounts(id) ON DELETE CASCADE,
                    payload_json TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS auth_handoffs (
                    id TEXT PRIMARY KEY,
                    code_hash TEXT NOT NULL UNIQUE,
                    ticket_hash TEXT NOT NULL UNIQUE,
                    user_id TEXT NOT NULL REFERENCES auth_accounts(id) ON DELETE CASCADE,
                    public_host TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    expires_at INTEGER NOT NULL,
                    exchanged_at TEXT,
                    completed_at TEXT
                );
                CREATE INDEX IF NOT EXISTS idx_auth_handoffs_expiry
                    ON auth_handoffs(expires_at, completed_at);

                CREATE TABLE IF NOT EXISTS partner_groups (
                    group_id TEXT PRIMARY KEY,
                    owner_id TEXT NOT NULL,
                    name TEXT NOT NULL,
                    description TEXT NOT NULL DEFAULT '',
                    member_ids_json TEXT NOT NULL DEFAULT '[]',
                    discussion_mode TEXT NOT NULL DEFAULT 'panel_parallel',
                    shared_memory TEXT NOT NULL DEFAULT 'whiteboard',
                    emoji TEXT NOT NULL DEFAULT '👥',
                    color TEXT NOT NULL DEFAULT '#6366f1',
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    version INTEGER NOT NULL DEFAULT 1
                );
                CREATE INDEX IF NOT EXISTS idx_partner_groups_updated
                    ON partner_groups(updated_at DESC);

                CREATE TABLE IF NOT EXISTS partner_group_sessions (
                    group_id TEXT NOT NULL REFERENCES partner_groups(group_id) ON DELETE CASCADE,
                    session_key TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    PRIMARY KEY(group_id, session_key)
                );

                CREATE TABLE IF NOT EXISTS partner_group_messages (
                    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                    event_id TEXT NOT NULL UNIQUE,
                    group_id TEXT NOT NULL REFERENCES partner_groups(group_id) ON DELETE CASCADE,
                    turn_id TEXT NOT NULL,
                    session_key TEXT NOT NULL,
                    role TEXT NOT NULL,
                    content TEXT NOT NULL,
                    author_id TEXT NOT NULL,
                    author_name TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    mentions_json TEXT NOT NULL DEFAULT '[]',
                    error INTEGER NOT NULL DEFAULT 0,
                    kind TEXT NOT NULL DEFAULT 'message',
                    events_json TEXT NOT NULL DEFAULT '[]',
                    invocation_id TEXT NOT NULL DEFAULT '',
                    invocation_json TEXT
                );
                CREATE INDEX IF NOT EXISTS idx_partner_group_messages_session
                    ON partner_group_messages(group_id, session_key, sequence);
                CREATE INDEX IF NOT EXISTS idx_partner_group_messages_turn
                    ON partner_group_messages(group_id, session_key, turn_id, sequence);

                CREATE TABLE IF NOT EXISTS partner_group_invocations (
                    invocation_id TEXT PRIMARY KEY,
                    group_id TEXT NOT NULL REFERENCES partner_groups(group_id) ON DELETE CASCADE,
                    session_key TEXT NOT NULL,
                    parent_turn_id TEXT NOT NULL DEFAULT '',
                    requester_partner_id TEXT NOT NULL,
                    requester_partner_name TEXT NOT NULL,
                    target_partner_id TEXT NOT NULL,
                    target_partner_name TEXT NOT NULL,
                    question TEXT NOT NULL,
                    status TEXT NOT NULL DEFAULT 'pending',
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    question_event_id TEXT NOT NULL DEFAULT '',
                    reply_event_id TEXT NOT NULL DEFAULT '',
                    error TEXT NOT NULL DEFAULT ''
                );
                CREATE INDEX IF NOT EXISTS idx_partner_group_invocations_session
                    ON partner_group_invocations(group_id, session_key, created_at);

                CREATE TABLE IF NOT EXISTS partner_group_whiteboard (
                    group_id TEXT NOT NULL REFERENCES partner_groups(group_id) ON DELETE CASCADE,
                    event_id TEXT NOT NULL,
                    turn_id TEXT NOT NULL,
                    session_key TEXT NOT NULL,
                    author_id TEXT NOT NULL,
                    author_name TEXT NOT NULL,
                    content TEXT NOT NULL,
                    mentions_json TEXT NOT NULL DEFAULT '[]',
                    created_at TEXT NOT NULL,
                    pinned_at TEXT NOT NULL,
                    PRIMARY KEY(group_id, event_id)
                );
                CREATE INDEX IF NOT EXISTS idx_partner_group_whiteboard_order
                    ON partner_group_whiteboard(group_id, pinned_at);
                "#,
            )
            .map_err(|error| format!("无法初始化 SQLite 数据库：{error}"))?;
        self.seed_defaults(&connection)?;
        Ok(())
    }

    fn connect(&self) -> Result<Connection, String> {
        let connection = Connection::open(self.path.as_ref())
            .map_err(|error| format!("无法打开 SQLite 数据库：{error}"))?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(|error| format!("无法设置 SQLite 忙等待：{error}"))?;
        connection
            .execute_batch("PRAGMA foreign_keys = ON;")
            .map_err(|error| format!("无法启用 SQLite 外键：{error}"))?;
        Ok(connection)
    }

    fn seed_defaults(&self, connection: &Connection) -> Result<(), String> {
        let now = now();
        let defaults = [
            (
                "ui",
                json!({
                    "theme": "snow",
                    "language": "zh",
                    "response_language": "zh",
                    "code_block_theme": "github-dark",
                    "code_block_show_line_numbers": true,
                    "code_block_wrap_long_lines": false
                }),
            ),
            (
                "catalog",
                json!({
                    "version": 1,
                    "connections": [],
                    "services": {
                        "llm": {"active_profile_id": null, "active_model_id": null, "profiles": []},
                        "task": {"active_profile_id": null, "active_model_id": null, "profiles": []},
                        "embedding": {"active_profile_id": null, "active_model_id": null, "profiles": []},
                        "search": {"active_profile_id": null, "profiles": []},
                        "tts": {"active_profile_id": null, "active_model_id": null, "profiles": []},
                        "stt": {"active_profile_id": null, "active_model_id": null, "profiles": []},
                        "imagegen": {"active_profile_id": null, "active_model_id": null, "profiles": []},
                        "videogen": {"active_profile_id": null, "active_model_id": null, "profiles": []}
                    }
                }),
            ),
            (
                "enabled_tools",
                json!([
                    "brainstorm",
                    "web_search",
                    "paper_search",
                    "zotero_search",
                    "reason",
                    "geogebra_analysis",
                    "imagegen",
                    "videogen"
                ]),
            ),
            ("capability_settings", json!({})),
            (
                "memory_settings",
                json!({"enabled": true, "max_entries": 200}),
            ),
            (
                "auth",
                json!({
                    "enabled": false,
                    "cookie_secure": false,
                    "token_expire_hours": 168,
                    "session_handoff_enabled": false
                }),
            ),
            ("network", json!({"proxy": "", "disable_ssl_verify": false})),
        ];
        for (key, value) in defaults {
            connection
                .execute(
                    "INSERT OR IGNORE INTO settings(key, value_json, updated_at) VALUES(?1, ?2, ?3)",
                    params![key, value.to_string(), now],
                )
                .map_err(|error| format!("无法写入默认设置：{error}"))?;
        }
        Ok(())
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<Value>, String> {
        let connection = self.connect()?;
        let raw = connection
            .query_row(
                "SELECT value_json FROM settings WHERE key = ?1",
                params![key],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| format!("无法读取设置 {key}：{error}"))?;
        raw.map(|value| {
            serde_json::from_str(&value)
                .map_err(|error| format!("设置 {key} 的 JSON 已损坏：{error}"))
        })
        .transpose()
    }

    pub fn set_setting(&self, key: &str, value: &Value) -> Result<(), String> {
        let connection = self.connect()?;
        connection
            .execute(
                r#"
                INSERT INTO settings(key, value_json, updated_at) VALUES(?1, ?2, ?3)
                ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json,
                    updated_at = excluded.updated_at
                "#,
                params![key, value.to_string(), now()],
            )
            .map_err(|error| format!("无法保存设置 {key}：{error}"))?;
        Ok(())
    }

    pub fn merge_setting(&self, key: &str, patch: &Value) -> Result<Value, String> {
        let mut current = self
            .get_setting(key)?
            .unwrap_or_else(|| Value::Object(Default::default()));
        merge_json(&mut current, patch);
        self.set_setting(key, &current)?;
        Ok(current)
    }

    /// 在同一个连接上执行领域操作。
    ///
    /// 题库、复习队列等模块需要事务和组合查询；集中暴露连接入口可以避免
    /// 每个领域重复解析数据库路径，同时仍由本模块统一维护连接参数。
    pub fn with_connection<T>(
        &self,
        operation: impl FnOnce(&mut Connection) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut connection = self.connect()?;
        operation(&mut connection)
    }

    pub fn create_session(
        &self,
        id: &str,
        title: &str,
        preferences: &Value,
        workspace_id: &str,
    ) -> Result<Value, String> {
        let connection = self.connect()?;
        let timestamp = now();
        connection
            .execute(
                r#"
                INSERT INTO sessions(
                    id, title, created_at, updated_at, status, preferences_json,
                    workspace_id, archived
                ) VALUES(?1, ?2, ?3, ?3, 'idle', ?4, ?5, 0)
                "#,
                params![
                    id,
                    normalize_title(title),
                    timestamp,
                    preferences.to_string(),
                    workspace_id
                ],
            )
            .map_err(|error| format!("无法创建会话：{error}"))?;
        self.get_session(id)?
            .ok_or_else(|| "会话创建后无法读取".to_string())
    }

    pub fn get_session(&self, id: &str) -> Result<Option<Value>, String> {
        let connection = self.connect()?;
        let row = connection
            .query_row(
                &session_select_sql("WHERE s.id = ?1 AND s.deleted_at IS NULL"),
                params![id],
                session_from_row,
            )
            .optional()
            .map_err(|error| format!("无法读取会话 {id}：{error}"))?;
        let Some(mut session) = row else {
            return Ok(None);
        };
        session["messages"] = Value::Array(self.get_messages(id)?);
        session["active_turns"] = Value::Array(self.list_active_turns(id)?);
        Ok(Some(session))
    }

    pub fn get_session_meta(&self, id: &str) -> Result<Option<Value>, String> {
        let connection = self.connect()?;
        let session = connection
            .query_row(
                &session_select_sql("WHERE s.id = ?1 AND s.deleted_at IS NULL"),
                params![id],
                session_from_row,
            )
            .optional()
            .map_err(|error| format!("无法读取会话 {id}：{error}"))?;
        Ok(session)
    }

    pub fn list_sessions(&self, limit: usize, offset: usize) -> Result<Vec<Value>, String> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare(&session_select_sql(
                "WHERE s.deleted_at IS NULL AND s.archived = 0 \
                 ORDER BY s.updated_at DESC LIMIT ?1 OFFSET ?2",
            ))
            .map_err(|error| format!("无法查询会话列表：{error}"))?;
        let rows = statement
            .query_map(params![limit as i64, offset as i64], session_from_row)
            .map_err(|error| format!("无法读取会话列表：{error}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("无法解析会话列表：{error}"))
    }

    pub fn list_recycle_bin(&self, limit: usize, offset: usize) -> Result<Vec<Value>, String> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare(&session_select_sql(
                "WHERE s.deleted_at IS NOT NULL ORDER BY s.deleted_at DESC LIMIT ?1 OFFSET ?2",
            ))
            .map_err(|error| format!("无法查询回收站：{error}"))?;
        let rows = statement
            .query_map(params![limit as i64, offset as i64], session_from_row)
            .map_err(|error| format!("无法读取回收站：{error}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("无法解析回收站：{error}"))
    }

    pub fn search_sessions(
        &self,
        query: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Value, String> {
        let connection = self.connect()?;
        let pattern = format!("%{query}%");
        let total: i64 = connection
            .query_row(
                r#"
                SELECT COUNT(DISTINCT s.id) FROM sessions s
                LEFT JOIN messages m ON m.session_id = s.id
                WHERE s.deleted_at IS NULL
                  AND (s.title LIKE ?1 OR m.content LIKE ?1)
                "#,
                params![pattern],
                |row| row.get(0),
            )
            .map_err(|error| format!("无法统计会话搜索结果：{error}"))?;
        let mut statement = connection
            .prepare(
                r#"
                SELECT s.id, s.title, s.created_at, s.updated_at, s.status,
                       s.preferences_json, s.workspace_id, s.archived,
                       (SELECT COUNT(*) FROM messages mc WHERE mc.session_id = s.id),
                       COALESCE((SELECT content FROM messages ml
                           WHERE ml.session_id = s.id ORDER BY ml.id DESC LIMIT 1), ''),
                       COALESCE((SELECT content FROM messages mm
                           WHERE mm.session_id = s.id AND mm.content LIKE ?1
                           ORDER BY mm.id DESC LIMIT 1), ''),
                       COALESCE((SELECT role FROM messages mr
                           WHERE mr.session_id = s.id AND mr.content LIKE ?1
                           ORDER BY mr.id DESC LIMIT 1), ''),
                       (SELECT id FROM messages mi
                           WHERE mi.session_id = s.id AND mi.content LIKE ?1
                           ORDER BY mi.id DESC LIMIT 1),
                       (SELECT created_at FROM messages mt
                           WHERE mt.session_id = s.id AND mt.content LIKE ?1
                           ORDER BY mt.id DESC LIMIT 1)
                FROM sessions s
                WHERE s.deleted_at IS NULL AND (
                    s.title LIKE ?1 OR EXISTS(
                        SELECT 1 FROM messages m WHERE m.session_id = s.id AND m.content LIKE ?1
                    )
                )
                ORDER BY s.updated_at DESC LIMIT ?2 OFFSET ?3
                "#,
            )
            .map_err(|error| format!("无法准备会话搜索：{error}"))?;
        let rows = statement
            .query_map(params![pattern, limit as i64, offset as i64], |row| {
                let mut session = summary_from_row(row)?;
                session["match_excerpt"] = Value::String(row.get::<_, String>(10)?);
                session["match_role"] = Value::String(row.get::<_, String>(11)?);
                session["match_message_id"] = Value::from(row.get::<_, Option<i64>>(12)?);
                session["match_created_at"] = Value::from(row.get::<_, Option<f64>>(13)?);
                Ok(session)
            })
            .map_err(|error| format!("无法执行会话搜索：{error}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("无法解析会话搜索：{error}"))?;
        Ok(json!({
            "sessions": rows,
            "total": total,
            "limit": limit,
            "offset": offset
        }))
    }

    pub fn update_session_title(&self, id: &str, title: &str) -> Result<bool, String> {
        let connection = self.connect()?;
        let changed = connection
            .execute(
                "UPDATE sessions SET title = ?2, updated_at = ?3 WHERE id = ?1 AND deleted_at IS NULL",
                params![id, normalize_title(title), now()],
            )
            .map_err(|error| format!("无法重命名会话：{error}"))?;
        Ok(changed > 0)
    }

    pub fn update_session_preferences(
        &self,
        id: &str,
        patch: &Value,
    ) -> Result<Option<Value>, String> {
        let connection = self.connect()?;
        let existing: Option<String> = connection
            .query_row(
                "SELECT preferences_json FROM sessions WHERE id = ?1 AND deleted_at IS NULL",
                params![id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| format!("无法读取会话偏好：{error}"))?;
        let Some(existing) = existing else {
            return Ok(None);
        };
        let mut preferences: Value = serde_json::from_str(&existing).unwrap_or_else(|_| json!({}));
        merge_json(&mut preferences, patch);
        let workspace_id = preferences
            .get("workspace_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        let archived = preferences
            .get("archived")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        connection
            .execute(
                r#"
                UPDATE sessions SET preferences_json = ?2, workspace_id = ?3,
                    archived = ?4, updated_at = ?5
                WHERE id = ?1 AND deleted_at IS NULL
                "#,
                params![
                    id,
                    preferences.to_string(),
                    workspace_id,
                    archived as i64,
                    now()
                ],
            )
            .map_err(|error| format!("无法更新会话偏好：{error}"))?;
        self.get_session_meta(id)
    }

    pub fn set_session_status(
        &self,
        id: &str,
        status: &str,
        active_turn_id: Option<&str>,
    ) -> Result<(), String> {
        let connection = self.connect()?;
        connection
            .execute(
                "UPDATE sessions SET status = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, status, now()],
            )
            .map_err(|error| format!("无法更新会话状态：{error}"))?;
        let mut patch = json!({"status": status});
        if let Some(turn_id) = active_turn_id {
            patch["active_turn_id"] = Value::String(turn_id.to_string());
        } else {
            patch["active_turn_id"] = Value::Null;
        }
        self.update_session_preferences(id, &patch)?;
        Ok(())
    }

    pub fn soft_delete_session(&self, id: &str) -> Result<bool, String> {
        let connection = self.connect()?;
        let changed = connection
            .execute(
                "UPDATE sessions SET deleted_at = ?2, updated_at = ?2 WHERE id = ?1 AND deleted_at IS NULL",
                params![id, now()],
            )
            .map_err(|error| format!("无法删除会话：{error}"))?;
        Ok(changed > 0)
    }

    pub fn restore_session(&self, id: &str) -> Result<bool, String> {
        let connection = self.connect()?;
        let changed = connection
            .execute(
                "UPDATE sessions SET deleted_at = NULL, updated_at = ?2 WHERE id = ?1 AND deleted_at IS NOT NULL",
                params![id, now()],
            )
            .map_err(|error| format!("无法恢复会话：{error}"))?;
        Ok(changed > 0)
    }

    pub fn purge_session(&self, id: &str) -> Result<bool, String> {
        let connection = self.connect()?;
        let changed = connection
            .execute(
                "DELETE FROM sessions WHERE id = ?1 AND deleted_at IS NOT NULL",
                params![id],
            )
            .map_err(|error| format!("无法彻底删除会话：{error}"))?;
        Ok(changed > 0)
    }

    pub fn add_message(&self, message: NewMessage<'_>) -> Result<i64, String> {
        let connection = self.connect()?;
        let timestamp = now();
        connection
            .execute(
                r#"
                INSERT INTO messages(
                    session_id, role, content, capability, events_json,
                    attachments_json, metadata_json, created_at, parent_message_id
                ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                "#,
                params![
                    message.session_id,
                    message.role,
                    message.content,
                    message.capability,
                    message.events.to_string(),
                    message.attachments.to_string(),
                    message.metadata.to_string(),
                    timestamp,
                    message.parent_message_id
                ],
            )
            .map_err(|error| format!("无法保存消息：{error}"))?;
        let id = connection.last_insert_rowid();
        connection
            .execute(
                "UPDATE sessions SET updated_at = ?2 WHERE id = ?1",
                params![message.session_id, timestamp],
            )
            .map_err(|error| format!("无法更新会话时间：{error}"))?;
        Ok(id)
    }

    pub fn set_message_events(&self, message_id: i64, events: &Value) -> Result<(), String> {
        let connection = self.connect()?;
        connection
            .execute(
                "UPDATE messages SET events_json = ?2 WHERE id = ?1",
                params![message_id, events.to_string()],
            )
            .map_err(|error| format!("无法保存消息轨迹：{error}"))?;
        Ok(())
    }

    pub fn get_messages(&self, session_id: &str) -> Result<Vec<Value>, String> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare(
                r#"
                SELECT id, session_id, role, content, capability, events_json,
                       attachments_json, metadata_json, created_at, parent_message_id
                FROM messages WHERE session_id = ?1 ORDER BY id ASC
                "#,
            )
            .map_err(|error| format!("无法准备消息查询：{error}"))?;
        let rows = statement
            .query_map(params![session_id], |row| {
                Ok(json!({
                    "id": row.get::<_, i64>(0)?,
                    "session_id": row.get::<_, String>(1)?,
                    "role": row.get::<_, String>(2)?,
                    "content": row.get::<_, String>(3)?,
                    "capability": row.get::<_, Option<String>>(4)?,
                    "events": parse_sql_json(row.get::<_, String>(5)?, json!([])),
                    "attachments": parse_sql_json(row.get::<_, String>(6)?, json!([])),
                    "metadata": parse_sql_json(row.get::<_, String>(7)?, json!({})),
                    "created_at": row.get::<_, f64>(8)?,
                    "parent_message_id": row.get::<_, Option<i64>>(9)?
                }))
            })
            .map_err(|error| format!("无法查询消息：{error}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("无法解析消息：{error}"))
    }

    pub fn get_message(&self, session_id: &str, message_id: i64) -> Result<Option<Value>, String> {
        Ok(self
            .get_messages(session_id)?
            .into_iter()
            .find(|message| message["id"].as_i64() == Some(message_id)))
    }

    pub fn add_attachment(&self, attachment: NewAttachment<'_>) -> Result<Value, String> {
        let connection = self.connect()?;
        let timestamp = now();
        connection
            .execute(
                r#"
                INSERT INTO attachments(
                    id, session_id, filename, mime_type, relative_path,
                    size_bytes, sha256, metadata_json, created_at
                ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                ON CONFLICT(id) DO UPDATE SET
                    filename = excluded.filename,
                    mime_type = excluded.mime_type,
                    relative_path = excluded.relative_path,
                    size_bytes = excluded.size_bytes,
                    sha256 = excluded.sha256,
                    metadata_json = excluded.metadata_json
                "#,
                params![
                    attachment.id,
                    attachment.session_id,
                    attachment.filename,
                    attachment.mime_type,
                    attachment.relative_path,
                    attachment.size_bytes,
                    attachment.sha256,
                    attachment.metadata.to_string(),
                    timestamp
                ],
            )
            .map_err(|error| format!("无法保存聊天附件：{error}"))?;
        self.get_attachment(attachment.session_id, attachment.id)?
            .ok_or_else(|| "聊天附件保存后无法读取".to_string())
    }

    pub fn get_attachment(
        &self,
        session_id: &str,
        attachment_id: &str,
    ) -> Result<Option<Value>, String> {
        let connection = self.connect()?;
        connection
            .query_row(
                r#"
                SELECT id, session_id, filename, mime_type, relative_path,
                       size_bytes, sha256, metadata_json, created_at
                FROM attachments WHERE session_id = ?1 AND id = ?2
                "#,
                params![session_id, attachment_id],
                attachment_from_row,
            )
            .optional()
            .map_err(|error| format!("无法读取聊天附件：{error}"))
    }

    pub fn list_attachments(&self, session_id: &str) -> Result<Vec<Value>, String> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare(
                r#"
                SELECT id, session_id, filename, mime_type, relative_path,
                       size_bytes, sha256, metadata_json, created_at
                FROM attachments WHERE session_id = ?1 ORDER BY created_at ASC
                "#,
            )
            .map_err(|error| format!("无法准备聊天附件查询：{error}"))?;
        let rows = statement
            .query_map(params![session_id], attachment_from_row)
            .map_err(|error| format!("无法查询聊天附件：{error}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("无法解析聊天附件：{error}"))
    }

    pub fn delete_message(&self, session_id: &str, message_id: i64) -> Result<bool, String> {
        let connection = self.connect()?;
        let changed = connection
            .execute(
                "DELETE FROM messages WHERE id = ?1 AND session_id = ?2",
                params![message_id, session_id],
            )
            .map_err(|error| format!("无法删除消息：{error}"))?;
        Ok(changed > 0)
    }

    /// 原子保留一次客户端提交，并在需要时一并创建会话。
    ///
    /// 返回值中的 `status` 为 `reserved` 或 `replay`。前者表示调用方仍需
    /// 创建回合；后者表示同一提交已绑定到某个回合，可直接复用回执。
    pub fn reserve_submission(
        &self,
        submission_id: &str,
        digest: &str,
        workspace_id: &str,
        requested_session_id: Option<&str>,
        title: &str,
        preferences: &Value,
    ) -> Result<Value, String> {
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| format!("无法开始提交保留事务：{error}"))?;
        let timestamp = now();
        let prior = transaction
            .query_row(
                r#"
                SELECT workspace_id, request_digest, session_id, turn_id, superseded_by
                FROM turn_submissions WHERE client_submission_id = ?1
                "#,
                params![submission_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| format!("无法读取提交保留记录：{error}"))?;

        if let Some((prior_workspace, prior_digest, session_id, turn_id, superseded_by)) = prior {
            if superseded_by.is_some() {
                return Err("该提交已被编辑后的消息取代，请刷新会话后重试。".to_string());
            }
            if prior_workspace != workspace_id || prior_digest != digest {
                return Err("该提交 ID 已绑定到不同请求或工作区。".to_string());
            }
            if requested_session_id.is_some_and(|requested| requested != session_id) {
                return Err("该提交 ID 已绑定到另一个会话。".to_string());
            }
            ensure_session_available(&transaction, &session_id, workspace_id)?;
            if let Some(turn_id) = turn_id {
                let turn = transaction
                    .query_row(
                        r#"
                        SELECT id, session_id, status, capability, error, error_code,
                               retryable, created_at, updated_at, finished_at, last_seq
                        FROM turns WHERE id = ?1
                        "#,
                        params![turn_id],
                        turn_from_row,
                    )
                    .optional()
                    .map_err(|error| format!("无法读取提交对应回合：{error}"))?;
                if let Some(turn) = turn {
                    transaction
                        .commit()
                        .map_err(|error| format!("无法提交保留事务：{error}"))?;
                    return Ok(json!({
                        "status": "replay",
                        "submission_id": submission_id,
                        "session_id": session_id,
                        "session_created": false,
                        "turn": turn
                    }));
                }
            }
            transaction
                .commit()
                .map_err(|error| format!("无法提交保留事务：{error}"))?;
            return Ok(json!({
                "status": "reserved",
                "submission_id": submission_id,
                "session_id": session_id,
                "session_created": false
            }));
        }

        let (session_id, session_created) = if let Some(session_id) = requested_session_id {
            ensure_session_available(&transaction, session_id, workspace_id)?;
            (session_id.to_string(), false)
        } else {
            let session_id = format!("session-{}", uuid::Uuid::new_v4());
            transaction
                .execute(
                    r#"
                    INSERT INTO sessions(
                        id, title, created_at, updated_at, status, preferences_json,
                        workspace_id, archived
                    ) VALUES(?1, ?2, ?3, ?3, 'idle', ?4, ?5, 0)
                    "#,
                    params![
                        session_id,
                        normalize_title(title),
                        timestamp,
                        preferences.to_string(),
                        workspace_id
                    ],
                )
                .map_err(|error| format!("无法创建提交所属会话：{error}"))?;
            (session_id, true)
        };
        transaction
            .execute(
                r#"
                INSERT INTO turn_submissions(
                    client_submission_id, workspace_id, request_digest, session_id,
                    turn_id, superseded_by, created_at, updated_at
                ) VALUES(?1, ?2, ?3, ?4, NULL, NULL, ?5, ?5)
                "#,
                params![submission_id, workspace_id, digest, session_id, timestamp],
            )
            .map_err(|error| format!("无法保留客户端提交：{error}"))?;
        transaction
            .commit()
            .map_err(|error| format!("无法提交保留事务：{error}"))?;
        Ok(json!({
            "status": "reserved",
            "submission_id": submission_id,
            "session_id": session_id,
            "session_created": session_created
        }))
    }

    pub fn bind_submission_turn(&self, submission_id: &str, turn_id: &str) -> Result<(), String> {
        let connection = self.connect()?;
        let changed = connection
            .execute(
                r#"
                UPDATE turn_submissions
                SET turn_id = ?2, updated_at = ?3
                WHERE client_submission_id = ?1 AND superseded_by IS NULL
                "#,
                params![submission_id, turn_id, now()],
            )
            .map_err(|error| format!("无法绑定提交与回合：{error}"))?;
        if changed == 0 {
            return Err("提交保留记录不存在或已被编辑取代。".to_string());
        }
        Ok(())
    }

    /// 将原始提交原子地标记为已被替换，并保留新的客户端提交。
    ///
    /// 若旧提交对应的回合仍在运行，返回 `requires_cancel`；调用方必须先
    /// 取消并等待回合结束，再重试替换，避免两个回合并行写入同一分支。
    pub fn replace_submission(
        &self,
        original_id: &str,
        replacement_id: &str,
        digest: &str,
        workspace_id: &str,
        requested_session_id: Option<&str>,
        title: &str,
        preferences: &Value,
    ) -> Result<Value, String> {
        if original_id == replacement_id {
            return Err("编辑消息必须使用新的提交 ID。".to_string());
        }
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| format!("无法开始提交替换事务：{error}"))?;
        let timestamp = now();
        let old = submission_row(&transaction, original_id)?;
        let session_id = old
            .as_ref()
            .map(|row| row.session_id.clone())
            .or_else(|| requested_session_id.map(str::to_string));
        if let (Some(old), Some(requested)) = (old.as_ref(), requested_session_id) {
            if old.session_id != requested {
                return Err("原始提交属于另一个会话。".to_string());
            }
        }
        if let Some(old) = old.as_ref() {
            if old.workspace_id != workspace_id {
                return Err("原始提交属于另一个工作区。".to_string());
            }
        }
        let (session_id, session_created) = if let Some(session_id) = session_id {
            ensure_session_available(&transaction, &session_id, workspace_id)?;
            (session_id, false)
        } else {
            let session_id = format!("session-{}", uuid::Uuid::new_v4());
            transaction
                .execute(
                    r#"
                    INSERT INTO sessions(
                        id, title, created_at, updated_at, status, preferences_json,
                        workspace_id, archived
                    ) VALUES(?1, ?2, ?3, ?3, 'idle', ?4, ?5, 0)
                    "#,
                    params![
                        session_id,
                        normalize_title(title),
                        timestamp,
                        preferences.to_string(),
                        workspace_id
                    ],
                )
                .map_err(|error| format!("无法创建替换提交所属会话：{error}"))?;
            (session_id, true)
        };

        let original_user_message = if old.is_some() {
            transaction
                .query_row(
                    r#"
                    SELECT id, parent_message_id FROM messages
                    WHERE session_id = ?1 AND role = 'user'
                      AND json_valid(metadata_json)
                      AND json_extract(metadata_json, '$.client_submission_id') = ?2
                    ORDER BY id ASC LIMIT 1
                    "#,
                    params![session_id, original_id],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?)),
                )
                .optional()
                .map_err(|error| format!("无法读取原始用户消息：{error}"))?
        } else {
            None
        };
        let original_parent_message_id = original_user_message
            .as_ref()
            .and_then(|(_, parent)| *parent);

        // 先确认替换 ID 没有冲突，再根据旧回合状态判断是否需要取消。
        // 这样无效的编辑请求不会触发正在运行回合的副作用。
        if let Some(replacement) = submission_row(&transaction, replacement_id)? {
            if replacement.request_digest != digest
                || replacement.workspace_id != workspace_id
                || replacement.session_id != session_id
                || replacement.superseded_by.is_some()
            {
                return Err("替换提交 ID 已被其他请求使用。".to_string());
            }
        }

        if let Some(old) = old.as_ref() {
            if let Some(turn_id) = old.turn_id.as_deref() {
                let active: Option<String> = transaction
                    .query_row(
                        r#"
                        SELECT id FROM turns
                        WHERE id = ?1 AND status IN ('queued', 'running', 'waiting_input')
                        "#,
                        params![turn_id],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(|error| format!("无法检查原始回合状态：{error}"))?;
                if let Some(active_turn_id) = active {
                    transaction
                        .commit()
                        .map_err(|error| format!("无法提交替换检查事务：{error}"))?;
                    return Ok(json!({
                        "status": "requires_cancel",
                        "session_id": session_id,
                        "turn_id": active_turn_id,
                        "user_message_id": original_user_message.as_ref().map(|(id, _)| *id),
                        "parent_message_id": original_parent_message_id
                    }));
                }
            }
            if let Some(superseded_by) = old.superseded_by.as_deref() {
                if superseded_by != replacement_id {
                    return Err("该提交已被另一条编辑操作取代。".to_string());
                }
                let replacement = submission_row(&transaction, replacement_id)?
                    .ok_or_else(|| "替换提交记录缺失。".to_string())?;
                if replacement.request_digest != digest
                    || replacement.workspace_id != workspace_id
                    || replacement.session_id != session_id
                    || replacement.superseded_by.is_some()
                {
                    return Err("替换提交 ID 已绑定到不同请求。".to_string());
                }
                transaction
                    .commit()
                    .map_err(|error| format!("无法提交替换事务：{error}"))?;
                return Ok(json!({
                    "status": "reserved",
                    "session_id": session_id,
                    "submission_id": replacement_id,
                    "session_created": false,
                    "parent_message_id": original_parent_message_id
                }));
            }
        }

        if submission_row(&transaction, replacement_id)?.is_none() {
            transaction
                .execute(
                    r#"
                    INSERT INTO turn_submissions(
                        client_submission_id, workspace_id, request_digest, session_id,
                        turn_id, superseded_by, created_at, updated_at
                    ) VALUES(?1, ?2, ?3, ?4, NULL, NULL, ?5, ?5)
                    "#,
                    params![replacement_id, workspace_id, digest, session_id, timestamp],
                )
                .map_err(|error| format!("无法保留替换提交：{error}"))?;
        }

        if let Some(old) = old.as_ref() {
            transaction
                .execute(
                    r#"
                    UPDATE turn_submissions
                    SET superseded_by = ?2, updated_at = ?3
                    WHERE client_submission_id = ?1
                    "#,
                    params![original_id, replacement_id, timestamp],
                )
                .map_err(|error| format!("无法标记原始提交：{error}"))?;
            let _ = old;
        } else {
            // 原始请求可能仍在传输队列中，插入墓碑记录可阻止迟到重放。
            transaction
                .execute(
                    r#"
                    INSERT INTO turn_submissions(
                        client_submission_id, workspace_id, request_digest, session_id,
                        turn_id, superseded_by, created_at, updated_at
                    ) VALUES(?1, ?2, '', ?3, NULL, ?4, ?5, ?5)
                    "#,
                    params![
                        original_id,
                        workspace_id,
                        session_id,
                        replacement_id,
                        timestamp
                    ],
                )
                .map_err(|error| format!("无法登记原始提交墓碑：{error}"))?;
        }
        transaction
            .commit()
            .map_err(|error| format!("无法提交替换事务：{error}"))?;
        Ok(json!({
            "status": "reserved",
            "session_id": session_id,
            "submission_id": replacement_id,
            "session_created": session_created,
            "parent_message_id": original_parent_message_id
        }))
    }

    pub fn create_turn(
        &self,
        id: &str,
        session_id: &str,
        capability: &str,
    ) -> Result<Value, String> {
        let connection = self.connect()?;
        let timestamp = now();
        connection
            .execute(
                r#"
                INSERT INTO turns(
                    id, session_id, status, capability, created_at, updated_at, last_seq
                ) VALUES(?1, ?2, 'queued', ?3, ?4, ?4, 0)
                "#,
                params![id, session_id, capability, timestamp],
            )
            .map_err(|error| format!("无法创建回合：{error}"))?;
        connection
            .execute(
                "UPDATE sessions SET status = 'running', updated_at = ?2 WHERE id = ?1",
                params![session_id, timestamp],
            )
            .map_err(|error| format!("无法更新会话运行状态：{error}"))?;
        Ok(json!({
            "id": id,
            "turn_id": id,
            "session_id": session_id,
            "capability": capability,
            "status": "queued",
            "error": "",
            "created_at": timestamp,
            "updated_at": timestamp,
            "last_seq": 0
        }))
    }

    pub fn update_turn_status(
        &self,
        id: &str,
        status: &str,
        error: &str,
        error_code: &str,
        retryable: bool,
    ) -> Result<(), String> {
        let connection = self.connect()?;
        let timestamp = now();
        let terminal = matches!(status, "completed" | "failed" | "cancelled");
        connection
            .execute(
                r#"
                UPDATE turns SET status = ?2, error = ?3, error_code = ?4,
                    retryable = ?5, updated_at = ?6,
                    finished_at = CASE WHEN ?7 THEN ?6 ELSE finished_at END
                WHERE id = ?1
                "#,
                params![
                    id,
                    status,
                    error,
                    error_code,
                    retryable as i64,
                    timestamp,
                    terminal as i64
                ],
            )
            .map_err(|error| format!("无法更新回合状态：{error}"))?;
        let session_id: Option<String> = connection
            .query_row(
                "SELECT session_id FROM turns WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| format!("无法读取回合所属会话：{error}"))?;
        if let Some(session_id) = session_id {
            let session_status = if terminal { status } else { "running" };
            connection
                .execute(
                    "UPDATE sessions SET status = ?2, updated_at = ?3 WHERE id = ?1",
                    params![session_id, session_status, timestamp],
                )
                .map_err(|error| format!("无法更新会话状态：{error}"))?;
        }
        Ok(())
    }

    pub fn append_turn_event(&self, turn_id: &str, event: &Value) -> Result<u64, String> {
        let connection = self.connect()?;
        let transaction = connection
            .unchecked_transaction()
            .map_err(|error| format!("无法开始事件事务：{error}"))?;
        let seq: u64 = transaction
            .query_row(
                "SELECT COALESCE(MAX(last_seq), 0) + 1 FROM turns WHERE id = ?1",
                params![turn_id],
                |row| row.get(0),
            )
            .map_err(|error| format!("无法分配事件序号：{error}"))?;
        let mut stored_event = event.clone();
        if let Some(object) = stored_event.as_object_mut() {
            object.insert("seq".into(), Value::from(seq));
            object.insert("turn_id".into(), Value::String(turn_id.to_string()));
        }
        transaction
            .execute(
                "INSERT INTO turn_events(turn_id, seq, event_json) VALUES(?1, ?2, ?3)",
                params![turn_id, seq as i64, stored_event.to_string()],
            )
            .map_err(|error| format!("无法持久化流事件：{error}"))?;
        transaction
            .execute(
                "UPDATE turns SET last_seq = ?2, updated_at = ?3 WHERE id = ?1",
                params![turn_id, seq as i64, now()],
            )
            .map_err(|error| format!("无法更新回合事件序号：{error}"))?;
        transaction
            .commit()
            .map_err(|error| format!("无法提交事件事务：{error}"))?;
        Ok(seq)
    }

    pub fn get_turn_events(&self, turn_id: &str, after_seq: u64) -> Result<Vec<Value>, String> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare(
                "SELECT event_json FROM turn_events WHERE turn_id = ?1 AND seq > ?2 ORDER BY seq",
            )
            .map_err(|error| format!("无法准备回合回放查询：{error}"))?;
        let rows = statement
            .query_map(params![turn_id, after_seq as i64], |row| {
                let raw: String = row.get(0)?;
                Ok(parse_sql_json(raw, json!({})))
            })
            .map_err(|error| format!("无法读取回合回放：{error}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("无法解析回合回放：{error}"))
    }

    pub fn get_session_turn_events(
        &self,
        session_id: &str,
        after_seq: u64,
    ) -> Result<Vec<Value>, String> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare(
                r#"
                SELECT e.event_json
                FROM turn_events e
                JOIN turns t ON t.id = e.turn_id
                WHERE t.session_id = ?1 AND e.seq > ?2
                ORDER BY t.created_at ASC, e.seq ASC
                "#,
            )
            .map_err(|error| format!("无法准备会话回放查询：{error}"))?;
        let rows = statement
            .query_map(params![session_id, after_seq as i64], |row| {
                let raw: String = row.get(0)?;
                Ok(parse_sql_json(raw, json!({})))
            })
            .map_err(|error| format!("无法读取会话回放：{error}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("无法解析会话回放：{error}"))
    }

    pub fn get_turn(&self, id: &str) -> Result<Option<Value>, String> {
        let connection = self.connect()?;
        connection
            .query_row(
                r#"
                SELECT id, session_id, status, capability, error, error_code,
                       retryable, created_at, updated_at, finished_at, last_seq
                FROM turns WHERE id = ?1
                "#,
                params![id],
                turn_from_row,
            )
            .optional()
            .map_err(|error| format!("无法读取回合：{error}"))
    }

    pub fn get_active_turn(&self, session_id: &str) -> Result<Option<Value>, String> {
        let connection = self.connect()?;
        connection
            .query_row(
                r#"
                SELECT id, session_id, status, capability, error, error_code,
                       retryable, created_at, updated_at, finished_at, last_seq
                FROM turns
                WHERE session_id = ?1 AND status IN ('queued', 'running', 'waiting_input')
                ORDER BY created_at DESC LIMIT 1
                "#,
                params![session_id],
                turn_from_row,
            )
            .optional()
            .map_err(|error| format!("无法读取活动回合：{error}"))
    }

    pub fn list_active_turns(&self, session_id: &str) -> Result<Vec<Value>, String> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare(
                r#"
                SELECT id, session_id, status, capability, error, error_code,
                       retryable, created_at, updated_at, finished_at, last_seq
                FROM turns
                WHERE session_id = ?1 AND status IN ('queued', 'running', 'waiting_input')
                ORDER BY created_at DESC
                "#,
            )
            .map_err(|error| format!("无法准备活动回合查询：{error}"))?;
        let rows = statement
            .query_map(params![session_id], turn_from_row)
            .map_err(|error| format!("无法读取活动回合列表：{error}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("无法解析活动回合列表：{error}"))
    }

    pub fn has_active_turns(&self) -> Result<bool, String> {
        let connection = self.connect()?;
        let count = connection
            .query_row(
                "SELECT COUNT(*) FROM turns WHERE status IN ('queued', 'running', 'waiting_input')",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|error| format!("无法检查运行中的对话：{error}"))?;
        Ok(count > 0)
    }

    pub fn list_resources(&self, domain: &str) -> Result<Vec<Value>, String> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare(
                r#"
                SELECT id, payload_json, created_at, updated_at FROM resources
                WHERE domain = ?1 AND deleted_at IS NULL ORDER BY updated_at DESC
                "#,
            )
            .map_err(|error| format!("无法准备资源列表：{error}"))?;
        let rows = statement
            .query_map(params![domain], |row| {
                let id: String = row.get(0)?;
                let mut payload = parse_sql_json(row.get::<_, String>(1)?, json!({}));
                if payload.get("id").is_none() {
                    payload["id"] = Value::String(id);
                }
                payload["created_at"] = Value::from(row.get::<_, f64>(2)?);
                payload["updated_at"] = Value::from(row.get::<_, f64>(3)?);
                Ok(payload)
            })
            .map_err(|error| format!("无法读取资源列表：{error}"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("无法解析资源列表：{error}"))
    }

    pub fn get_resource(&self, domain: &str, id: &str) -> Result<Option<Value>, String> {
        let connection = self.connect()?;
        let raw = connection
            .query_row(
                r#"
                SELECT payload_json, created_at, updated_at FROM resources
                WHERE domain = ?1 AND id = ?2 AND deleted_at IS NULL
                "#,
                params![domain, id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, f64>(1)?,
                        row.get::<_, f64>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| format!("无法读取资源：{error}"))?;
        Ok(raw.map(|(value, created_at, updated_at)| {
            let mut payload = parse_sql_json(value, json!({}));
            payload["id"] = Value::String(id.to_string());
            payload["created_at"] = Value::from(created_at);
            payload["updated_at"] = Value::from(updated_at);
            payload
        }))
    }

    pub fn put_resource(&self, domain: &str, id: &str, payload: &Value) -> Result<Value, String> {
        let connection = self.connect()?;
        let timestamp = now();
        connection
            .execute(
                r#"
                INSERT INTO resources(
                    domain, id, payload_json, created_at, updated_at, deleted_at
                ) VALUES(?1, ?2, ?3, ?4, ?4, NULL)
                ON CONFLICT(domain, id) DO UPDATE SET
                    payload_json = excluded.payload_json,
                    updated_at = excluded.updated_at,
                    deleted_at = NULL
                "#,
                params![domain, id, payload.to_string(), timestamp],
            )
            .map_err(|error| format!("无法保存资源：{error}"))?;
        self.get_resource(domain, id)?
            .ok_or_else(|| "资源保存后无法读取".to_string())
    }

    pub fn delete_resource(&self, domain: &str, id: &str) -> Result<bool, String> {
        let connection = self.connect()?;
        let changed = connection
            .execute(
                r#"
                UPDATE resources SET deleted_at = ?3, updated_at = ?3
                WHERE domain = ?1 AND id = ?2 AND deleted_at IS NULL
                "#,
                params![domain, id, now()],
            )
            .map_err(|error| format!("无法删除资源：{error}"))?;
        Ok(changed > 0)
    }
}

fn session_select_sql(where_clause: &str) -> String {
    format!(
        r#"
        SELECT s.id, s.title, s.created_at, s.updated_at, s.status,
               s.preferences_json, s.workspace_id, s.archived,
               (SELECT COUNT(*) FROM messages m WHERE m.session_id = s.id),
               COALESCE((SELECT content FROM messages lm
                   WHERE lm.session_id = s.id ORDER BY lm.id DESC LIMIT 1), '')
        FROM sessions s {where_clause}
        "#
    )
}

fn session_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    summary_from_row(row)
}

fn summary_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let id: String = row.get(0)?;
    let preferences = parse_sql_json(row.get::<_, String>(5)?, json!({}));
    Ok(json!({
        "id": id,
        "session_id": id,
        "title": row.get::<_, String>(1)?,
        "created_at": row.get::<_, f64>(2)?,
        "updated_at": row.get::<_, f64>(3)?,
        "status": row.get::<_, String>(4)?,
        "preferences": preferences,
        "content_workspace_id": row.get::<_, String>(6)?,
        "archived": row.get::<_, i64>(7)? != 0,
        "message_count": row.get::<_, i64>(8)?,
        "last_message": row.get::<_, String>(9)?
    }))
}

fn turn_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let id: String = row.get(0)?;
    Ok(json!({
        "id": id,
        "turn_id": id,
        "session_id": row.get::<_, String>(1)?,
        "status": row.get::<_, String>(2)?,
        "capability": row.get::<_, String>(3)?,
        "error": row.get::<_, String>(4)?,
        "failure_code": row.get::<_, String>(5)?,
        "error_code": row.get::<_, String>(5)?,
        "retryable": row.get::<_, i64>(6)? != 0,
        "created_at": row.get::<_, f64>(7)?,
        "updated_at": row.get::<_, f64>(8)?,
        "finished_at": row.get::<_, Option<f64>>(9)?,
        "last_seq": row.get::<_, i64>(10)?
    }))
}

#[derive(Debug, Clone)]
struct StoredSubmission {
    workspace_id: String,
    request_digest: String,
    session_id: String,
    turn_id: Option<String>,
    superseded_by: Option<String>,
}

fn submission_row(
    connection: &Connection,
    submission_id: &str,
) -> Result<Option<StoredSubmission>, String> {
    connection
        .query_row(
            r#"
            SELECT workspace_id, request_digest, session_id, turn_id, superseded_by
            FROM turn_submissions WHERE client_submission_id = ?1
            "#,
            params![submission_id],
            |row| {
                Ok(StoredSubmission {
                    workspace_id: row.get(0)?,
                    request_digest: row.get(1)?,
                    session_id: row.get(2)?,
                    turn_id: row.get(3)?,
                    superseded_by: row.get(4)?,
                })
            },
        )
        .optional()
        .map_err(|error| format!("无法读取提交记录：{error}"))
}

fn ensure_session_available(
    connection: &Connection,
    session_id: &str,
    workspace_id: &str,
) -> Result<(), String> {
    let row = connection
        .query_row(
            "SELECT workspace_id, deleted_at FROM sessions WHERE id = ?1",
            params![session_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<f64>>(1)?)),
        )
        .optional()
        .map_err(|error| format!("无法检查会话状态：{error}"))?;
    let Some((session_workspace, deleted_at)) = row else {
        return Err("提交所属会话不存在。".to_string());
    };
    if deleted_at.is_some() {
        return Err("请先恢复原始会话，再发送或编辑这条消息。".to_string());
    }
    if session_workspace != workspace_id {
        return Err("该会话属于另一个工作区。".to_string());
    }
    Ok(())
}

fn parse_sql_json(raw: String, fallback: Value) -> Value {
    serde_json::from_str(&raw).unwrap_or(fallback)
}

fn attachment_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let id: String = row.get(0)?;
    let session_id: String = row.get(1)?;
    let filename: String = row.get(2)?;
    let mime_type: String = row.get(3)?;
    let relative_path: String = row.get(4)?;
    let metadata = parse_sql_json(row.get::<_, String>(7)?, json!({}));
    Ok(json!({
        "id": id,
        "session_id": session_id,
        "filename": filename,
        "mime_type": mime_type,
        "relative_path": relative_path,
        "url": format!("/files/attachments/{session_id}/{}/{filename}", metadata.get("attachment_id").and_then(Value::as_str).unwrap_or("unknown")),
        "size_bytes": row.get::<_, i64>(5)?,
        "sha256": row.get::<_, String>(6)?,
        "metadata": metadata,
        "created_at": row.get::<_, f64>(8)?
    }))
}

pub fn merge_json(target: &mut Value, patch: &Value) {
    match (target, patch) {
        (Value::Object(target), Value::Object(patch)) => {
            for (key, value) in patch {
                if value.is_null() {
                    target.insert(key.clone(), Value::Null);
                } else {
                    merge_json(
                        target
                            .entry(key.clone())
                            .or_insert_with(|| Value::Object(Default::default())),
                        value,
                    );
                }
            }
        }
        (target, patch) => *target = patch.clone(),
    }
}

pub fn now() -> f64 {
    chrono::Utc::now().timestamp_micros() as f64 / 1_000_000.0
}

pub fn normalize_title(title: &str) -> String {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        "新对话".to_string()
    } else {
        trimmed.chars().take(160).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_lifecycle_and_events_survive_reopen() {
        let directory = tempfile::tempdir().expect("临时目录");
        let path = directory.path().join("test.sqlite3");
        let database = Database::new(&path);
        database.initialize().expect("初始化");
        let session = database
            .create_session("s1", "测试", &json!({}), "")
            .expect("创建会话");
        assert_eq!(session["id"], "s1");
        database.create_turn("t1", "s1", "chat").expect("创建回合");
        database
            .append_turn_event("t1", &json!({"type": "content", "content": "你好"}))
            .expect("写入事件");
        database
            .update_turn_status("t1", "completed", "", "", false)
            .expect("结束回合");

        let reopened = Database::new(&path);
        reopened.initialize().expect("重新初始化");
        let events = reopened.get_turn_events("t1", 0).expect("读取事件");
        assert_eq!(events.len(), 1);
        assert_eq!(reopened.list_sessions(10, 0).expect("会话列表").len(), 1);
    }
}
