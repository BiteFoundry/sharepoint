//! SharePoint & OneDrive SQLite schema initialization, WAL pragmas, and FTS5 setup.

use anyhow::{Context, Result};
use rusqlite::Connection;

const SCHEMA_USER_VERSION_UNICODE61: i32 = 72;
const SCHEMA_USER_VERSION_TRIGRAM: i32 = 73;

/// Initializes SQLite PRAGMAs, tables, indexes, and FTS5 virtual table.
/// Returns `true` if `drive_items_fts` uses the `trigram` tokenizer, `false` if using `unicode61`.
pub fn init_connection(conn: &Connection, is_memory: bool) -> Result<bool> {
    conn.set_prepared_statement_cache_capacity(64);
    if !is_memory {
        let _ = conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA journal_size_limit = 67108864;
            ",
        );
    }
    conn.execute_batch(
        "
        PRAGMA synchronous = NORMAL;
        PRAGMA foreign_keys = ON;
        PRAGMA busy_timeout = 5000;
        PRAGMA temp_store = MEMORY;
        PRAGMA cache_size = -8000;
        ",
    )
    .context("Failed to configure SQLite PRAGMAs")?;

    if !is_memory {
        let _ = conn.execute_batch("PRAGMA mmap_size = 67108864;");
        let user_version: i32 = conn
            .query_row("PRAGMA user_version;", [], |row| row.get(0))
            .unwrap_or(0);
        if user_version == SCHEMA_USER_VERSION_TRIGRAM
            || user_version == SCHEMA_USER_VERSION_UNICODE61
        {
            let intact: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='drive_items_fts')
                     AND EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='drive_items')
                     AND EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='sites')
                     AND EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='drives')
                     AND EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='local_download_baselines')",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(false);
            if intact {
                return Ok(user_version == SCHEMA_USER_VERSION_TRIGRAM);
            }
        }
    }

    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS sites (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            display_name TEXT NOT NULL,
            description TEXT,
            web_url TEXT,
            is_followed INTEGER NOT NULL DEFAULT 0,
            created_at TEXT,
            last_modified_at TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_sites_followed_name
            ON sites(is_followed DESC, display_name COLLATE NOCASE ASC);

        CREATE TABLE IF NOT EXISTS drives (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            drive_type TEXT NOT NULL,
            site_id TEXT,
            site_name TEXT,
            web_url TEXT,
            owner_name TEXT,
            quota_total INTEGER,
            quota_used INTEGER,
            quota_remaining INTEGER,
            quota_state TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_drives_site
            ON drives(site_id, name COLLATE NOCASE ASC);

        CREATE TABLE IF NOT EXISTS drive_items (
            drive_id TEXT NOT NULL,
            id TEXT NOT NULL,
            parent_id TEXT,
            parent_path TEXT NOT NULL DEFAULT '',
            name TEXT NOT NULL,
            size INTEGER NOT NULL DEFAULT 0,
            is_folder INTEGER NOT NULL DEFAULT 0,
            child_count INTEGER,
            mime_type TEXT,
            web_url TEXT,
            download_url TEXT,
            e_tag TEXT,
            c_tag TEXT,
            created_at TEXT,
            last_modified_at TEXT,
            created_by TEXT,
            last_modified_by TEXT,
            shared_scope TEXT,
            remote_drive_id TEXT,
            remote_item_id TEXT,
            PRIMARY KEY (drive_id, id)
        );

        CREATE INDEX IF NOT EXISTS idx_drive_items_parent
            ON drive_items(drive_id, parent_id, is_folder DESC, name COLLATE NOCASE ASC);

        CREATE INDEX IF NOT EXISTS idx_drive_items_path
            ON drive_items(drive_id, parent_path, name COLLATE NOCASE ASC);

        CREATE INDEX IF NOT EXISTS idx_drive_items_modified
            ON drive_items(last_modified_at DESC, name COLLATE NOCASE ASC);

        CREATE TABLE IF NOT EXISTS drive_item_lists (
            list_kind TEXT NOT NULL,
            position INTEGER NOT NULL,
            drive_id TEXT NOT NULL,
            item_id TEXT NOT NULL,
            PRIMARY KEY (list_kind, drive_id, item_id)
        );

        CREATE INDEX IF NOT EXISTS idx_drive_item_lists_order
            ON drive_item_lists(list_kind, position ASC);

        CREATE TABLE IF NOT EXISTS pinned_targets (
            key TEXT PRIMARY KEY,
            kind TEXT NOT NULL,
            drive_id TEXT,
            item_id TEXT,
            site_id TEXT,
            label TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS sync_cursors (
            scope_key TEXT PRIMARY KEY,
            delta_link TEXT,
            history_next_link TEXT,
            last_synced_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS local_download_baselines (
            local_path TEXT PRIMARY KEY,
            drive_id TEXT NOT NULL,
            item_id TEXT NOT NULL,
            parent_id TEXT,
            parent_path TEXT NOT NULL DEFAULT '',
            remote_name TEXT NOT NULL,
            e_tag TEXT,
            c_tag TEXT,
            last_modified_at TEXT,
            downloaded_at TEXT NOT NULL
        );
        ",
    )
    .context("Failed to create SharePoint/OneDrive SQLite schema")?;

    let trigram = ensure_fts5_table(
        conn,
        "drive_items_fts",
        "
        CREATE VIRTUAL TABLE IF NOT EXISTS drive_items_fts USING fts5(
            drive_id UNINDEXED,
            item_id UNINDEXED,
            name,
            parent_path,
            author,
            mime_type,
            tokenize='trigram'
        );
        ",
        "
        CREATE VIRTUAL TABLE IF NOT EXISTS drive_items_fts USING fts5(
            drive_id UNINDEXED,
            item_id UNINDEXED,
            name,
            parent_path,
            author,
            mime_type,
            tokenize='unicode61'
        );
        ",
    )?;

    if !is_memory {
        let version = if trigram {
            SCHEMA_USER_VERSION_TRIGRAM
        } else {
            SCHEMA_USER_VERSION_UNICODE61
        };
        conn.execute_batch(&format!("PRAGMA user_version = {version};"))
            .context("Failed to set SQLite user_version")?;
    }

    Ok(trigram)
}

/// Configures a read-only WAL reader connection for concurrent UI reads.
pub fn init_reader_connection(conn: &Connection) -> Result<()> {
    conn.set_prepared_statement_cache_capacity(64);
    conn.execute_batch(
        "
        PRAGMA query_only = ON;
        PRAGMA busy_timeout = 5000;
        PRAGMA temp_store = MEMORY;
        PRAGMA cache_size = -4000;
        ",
    )
    .context("Failed to configure SQLite reader PRAGMAs")?;
    let _ = conn.execute_batch("PRAGMA mmap_size = 67108864;");
    Ok(())
}

/// Releases unused SQLite page-cache memory back to the OS (`PRAGMA shrink_memory`).
pub fn shrink_connection_memory(conn: &Connection) {
    let _ = conn.execute_batch("PRAGMA shrink_memory;");
}

fn ensure_fts5_table(
    conn: &Connection,
    table_name: &str,
    trigram_sql: &str,
    unicode61_sql: &str,
) -> Result<bool> {
    if conn.execute_batch(trigram_sql).is_err() {
        conn.execute_batch(unicode61_sql).with_context(|| {
            format!("Failed to create FTS5 virtual table ({table_name}, unicode61)")
        })?;
        return Ok(false);
    }

    let sql_def: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table_name],
            |row| row.get(0),
        )
        .ok();
    let uses_trigram = sql_def
        .map(|s| s.to_ascii_lowercase().contains("trigram"))
        .unwrap_or(true);
    Ok(uses_trigram)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_sharepoint_schema_and_user_version_fast_path() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("sharepoint_fastpath.db");

        let first_trigram = {
            let conn = Connection::open(&db_path).unwrap();
            let trigram = init_connection(&conn, false).unwrap();
            let ver: i32 = conn
                .query_row("PRAGMA user_version;", [], |row| row.get(0))
                .unwrap();
            assert_eq!(
                ver,
                if trigram {
                    SCHEMA_USER_VERSION_TRIGRAM
                } else {
                    SCHEMA_USER_VERSION_UNICODE61
                }
            );
            trigram
        };

        let conn2 = Connection::open(&db_path).unwrap();
        assert_eq!(init_connection(&conn2, false).unwrap(), first_trigram);
    }
}
