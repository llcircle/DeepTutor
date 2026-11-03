use rusqlite::Connection;

/// 独立的幂等迁移，不改变旧资料与用户配置。
pub(super) fn initialize(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch(
            r#"
        BEGIN IMMEDIATE;
        CREATE TABLE IF NOT EXISTS reading_schema_migrations (
            version INTEGER PRIMARY KEY, applied_at REAL NOT NULL
        );
        CREATE TABLE IF NOT EXISTS reading_revisions (
            material_id TEXT NOT NULL, revision INTEGER NOT NULL,
            data_json TEXT NOT NULL, created_at REAL NOT NULL,
            PRIMARY KEY(material_id, revision)
        );
        CREATE TABLE IF NOT EXISTS reading_revision_units (
            material_id TEXT NOT NULL, revision INTEGER NOT NULL, locator INTEGER NOT NULL,
            text TEXT NOT NULL, text_hash TEXT NOT NULL,
            PRIMARY KEY(material_id, revision, locator),
            FOREIGN KEY(material_id, revision)
                REFERENCES reading_revisions(material_id, revision) ON DELETE CASCADE
        );
        CREATE TABLE IF NOT EXISTS reading_ocr_corrections (
            id TEXT PRIMARY KEY, material_id TEXT NOT NULL, locator INTEGER NOT NULL,
            base_revision INTEGER NOT NULL, status TEXT NOT NULL,
            payload_json TEXT NOT NULL, created_at REAL NOT NULL, updated_at REAL NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_reading_corrections_material
            ON reading_ocr_corrections(material_id, status, locator);
        CREATE TABLE IF NOT EXISTS reading_index_state (
            workspace_id TEXT PRIMARY KEY, payload_json TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS reading_vectors (
            material_id TEXT NOT NULL, locator INTEGER NOT NULL, kind TEXT NOT NULL,
            ordinal INTEGER NOT NULL, signature TEXT NOT NULL, text_hash TEXT NOT NULL,
            start_offset INTEGER NOT NULL, end_offset INTEGER NOT NULL,
            dimension INTEGER NOT NULL, embedding BLOB NOT NULL,
            PRIMARY KEY(material_id, locator, kind, ordinal, signature)
        );
        CREATE INDEX IF NOT EXISTS idx_reading_vectors_signature
            ON reading_vectors(signature, material_id, locator);
        CREATE TABLE IF NOT EXISTS reading_citations (
            id TEXT PRIMARY KEY, material_id TEXT NOT NULL, revision INTEGER NOT NULL,
            locator INTEGER NOT NULL, payload_json TEXT NOT NULL
        );
        INSERT OR IGNORE INTO reading_schema_migrations(version, applied_at)
            VALUES(1, unixepoch('now'));
        COMMIT;
        "#,
        )
        .map_err(|error| format!("阅读数据迁移失败：{error}"))
}
