//! SQLite persistence for SharePoint sites, drives, drive items (with FTS5 sync),
//! recent/shared lists, pinned sites/folders, and sync cursors.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::models::{DriveInfo, DriveItem, PinnedTarget, SharePointSite, SyncCursor};

pub(crate) fn escape_like(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\\' | '%' | '_' => {
                out.push('\\');
                out.push(ch);
            }
            other => out.push(other),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Sites
// ---------------------------------------------------------------------------

pub fn upsert_sites(conn: &mut Connection, sites: &[SharePointSite]) -> Result<()> {
    if sites.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare_cached(
            "
            INSERT INTO sites (id, name, display_name, description, web_url, is_followed, created_at, last_modified_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                display_name = excluded.display_name,
                description = COALESCE(excluded.description, sites.description),
                web_url = COALESCE(excluded.web_url, sites.web_url),
                is_followed = MAX(sites.is_followed, excluded.is_followed),
                created_at = COALESCE(excluded.created_at, sites.created_at),
                last_modified_at = COALESCE(excluded.last_modified_at, sites.last_modified_at)
            ",
        )?;
        for s in sites {
            stmt.execute(params![
                s.id,
                s.name,
                s.display_name,
                s.description,
                s.web_url,
                if s.is_followed { 1i32 } else { 0i32 },
                s.created_at,
                s.last_modified_at,
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn list_sites(conn: &Connection) -> Result<Vec<SharePointSite>> {
    let mut stmt = conn.prepare_cached(
        "
        SELECT id, name, display_name, description, web_url, is_followed, created_at, last_modified_at
        FROM sites
        ORDER BY is_followed DESC, display_name COLLATE NOCASE ASC
        ",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(SharePointSite {
            id: row.get(0)?,
            name: row.get(1)?,
            display_name: row.get(2)?,
            description: row.get(3)?,
            web_url: row.get(4)?,
            is_followed: row.get::<_, i32>(5)? != 0,
            created_at: row.get(6)?,
            last_modified_at: row.get(7)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

// ---------------------------------------------------------------------------
// Drives
// ---------------------------------------------------------------------------

pub fn upsert_drives(conn: &mut Connection, drives: &[DriveInfo]) -> Result<()> {
    if drives.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare_cached(
            "
            INSERT INTO drives (id, name, drive_type, site_id, site_name, web_url, owner_name, quota_total, quota_used, quota_remaining, quota_state)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
            ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                drive_type = excluded.drive_type,
                site_id = COALESCE(excluded.site_id, drives.site_id),
                site_name = COALESCE(excluded.site_name, drives.site_name),
                web_url = COALESCE(excluded.web_url, drives.web_url),
                owner_name = COALESCE(excluded.owner_name, drives.owner_name),
                quota_total = COALESCE(excluded.quota_total, drives.quota_total),
                quota_used = COALESCE(excluded.quota_used, drives.quota_used),
                quota_remaining = COALESCE(excluded.quota_remaining, drives.quota_remaining),
                quota_state = COALESCE(excluded.quota_state, drives.quota_state)
            ",
        )?;
        for d in drives {
            stmt.execute(params![
                d.id,
                d.name,
                d.drive_type,
                d.site_id,
                d.site_name,
                d.web_url,
                d.owner_name,
                d.quota_total.map(|v| v as i64),
                d.quota_used.map(|v| v as i64),
                d.quota_remaining.map(|v| v as i64),
                d.quota_state,
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn list_drives(conn: &Connection, site_id: Option<&str>) -> Result<Vec<DriveInfo>> {
    let map_row = |row: &Row<'_>| -> rusqlite::Result<DriveInfo> {
        Ok(DriveInfo {
            id: row.get(0)?,
            name: row.get(1)?,
            drive_type: row.get(2)?,
            site_id: row.get(3)?,
            site_name: row.get(4)?,
            web_url: row.get(5)?,
            owner_name: row.get(6)?,
            quota_total: row.get::<_, Option<i64>>(7)?.map(|v| v.max(0) as u64),
            quota_used: row.get::<_, Option<i64>>(8)?.map(|v| v.max(0) as u64),
            quota_remaining: row.get::<_, Option<i64>>(9)?.map(|v| v.max(0) as u64),
            quota_state: row.get(10)?,
        })
    };

    if let Some(sid) = site_id.map(str::trim).filter(|s| !s.is_empty()) {
        let mut stmt = conn.prepare_cached(
            "
            SELECT id, name, drive_type, site_id, site_name, web_url, owner_name,
                   quota_total, quota_used, quota_remaining, quota_state
            FROM drives
            WHERE site_id = ?1
            ORDER BY name COLLATE NOCASE ASC
            ",
        )?;
        let rows = stmt.query_map([sid], map_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    } else {
        let mut stmt = conn.prepare_cached(
            "
            SELECT id, name, drive_type, site_id, site_name, web_url, owner_name,
                   quota_total, quota_used, quota_remaining, quota_state
            FROM drives
            ORDER BY (site_id IS NOT NULL) ASC, site_name COLLATE NOCASE ASC, name COLLATE NOCASE ASC
            ",
        )?;
        let rows = stmt.query_map([], map_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }
}

// ---------------------------------------------------------------------------
// Drive Items + FTS5
// ---------------------------------------------------------------------------

const DRIVE_ITEM_COLS: &str = "
    drive_id, id, parent_id, parent_path, name, size, is_folder, child_count,
    mime_type, web_url, download_url, e_tag, c_tag, created_at, last_modified_at,
    created_by, last_modified_by, shared_scope, remote_drive_id, remote_item_id
";

pub(crate) fn map_drive_item_row(row: &Row<'_>) -> rusqlite::Result<DriveItem> {
    Ok(DriveItem {
        drive_id: row.get(0)?,
        id: row.get(1)?,
        parent_id: row.get(2)?,
        parent_path: row.get(3)?,
        name: row.get(4)?,
        size: row.get::<_, i64>(5)?.max(0) as u64,
        is_folder: row.get::<_, i32>(6)? != 0,
        child_count: row.get::<_, Option<i64>>(7)?.map(|v| v.max(0) as u32),
        mime_type: row.get(8)?,
        web_url: row.get(9)?,
        download_url: row.get(10)?,
        e_tag: row.get(11)?,
        c_tag: row.get(12)?,
        created_at: row.get(13)?,
        last_modified_at: row.get(14)?,
        created_by: row.get(15)?,
        last_modified_by: row.get(16)?,
        shared_scope: row.get(17)?,
        remote_drive_id: row.get(18)?,
        remote_item_id: row.get(19)?,
    })
}

fn author_text(item: &DriveItem) -> String {
    match (
        item.last_modified_by.as_deref().map(str::trim).filter(|s| !s.is_empty()),
        item.created_by.as_deref().map(str::trim).filter(|s| !s.is_empty()),
    ) {
        (Some(m), Some(c)) if !m.eq_ignore_ascii_case(c) => format!("{m} {c}"),
        (Some(m), _) => m.to_string(),
        (None, Some(c)) => c.to_string(),
        (None, None) => String::new(),
    }
}

pub fn upsert_drive_items(conn: &mut Connection, items: &[DriveItem]) -> Result<usize> {
    if items.is_empty() {
        return Ok(0);
    }
    let tx = conn.transaction()?;
    let mut written = 0usize;
    {
        let mut upsert_stmt = tx.prepare_cached(
            "
            INSERT INTO drive_items (
                drive_id, id, parent_id, parent_path, name, size, is_folder, child_count,
                mime_type, web_url, download_url, e_tag, c_tag, created_at, last_modified_at,
                created_by, last_modified_by, shared_scope, remote_drive_id, remote_item_id
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
                ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                ?16, ?17, ?18, ?19, ?20
            )
            ON CONFLICT(drive_id, id) DO UPDATE SET
                parent_id = COALESCE(excluded.parent_id, drive_items.parent_id),
                parent_path = CASE WHEN excluded.parent_path != '' THEN excluded.parent_path ELSE drive_items.parent_path END,
                name = excluded.name,
                size = excluded.size,
                is_folder = excluded.is_folder,
                child_count = COALESCE(excluded.child_count, drive_items.child_count),
                mime_type = COALESCE(excluded.mime_type, drive_items.mime_type),
                web_url = COALESCE(excluded.web_url, drive_items.web_url),
                download_url = COALESCE(excluded.download_url, drive_items.download_url),
                e_tag = COALESCE(excluded.e_tag, drive_items.e_tag),
                c_tag = COALESCE(excluded.c_tag, drive_items.c_tag),
                created_at = COALESCE(excluded.created_at, drive_items.created_at),
                last_modified_at = COALESCE(excluded.last_modified_at, drive_items.last_modified_at),
                created_by = COALESCE(excluded.created_by, drive_items.created_by),
                last_modified_by = COALESCE(excluded.last_modified_by, drive_items.last_modified_by),
                shared_scope = COALESCE(excluded.shared_scope, drive_items.shared_scope),
                remote_drive_id = COALESCE(excluded.remote_drive_id, drive_items.remote_drive_id),
                remote_item_id = COALESCE(excluded.remote_item_id, drive_items.remote_item_id)
            ",
        )?;
        let mut get_rowid_stmt = tx.prepare_cached(
            "SELECT rowid FROM drive_items WHERE drive_id = ?1 AND id = ?2",
        )?;
        let mut del_fts_stmt =
            tx.prepare_cached("DELETE FROM drive_items_fts WHERE rowid = ?1")?;
        let mut ins_fts_stmt = tx.prepare_cached(
            "
            INSERT INTO drive_items_fts (rowid, drive_id, item_id, name, parent_path, author, mime_type)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ",
        )?;

        for item in items {
            upsert_stmt.execute(params![
                item.drive_id,
                item.id,
                item.parent_id,
                item.parent_path,
                item.name,
                item.size as i64,
                if item.is_folder { 1i32 } else { 0i32 },
                item.child_count.map(|c| c as i64),
                item.mime_type,
                item.web_url,
                item.download_url,
                item.e_tag,
                item.c_tag,
                item.created_at,
                item.last_modified_at,
                item.created_by,
                item.last_modified_by,
                item.shared_scope,
                item.remote_drive_id,
                item.remote_item_id,
            ])?;
            let rowid: i64 =
                get_rowid_stmt.query_row(params![item.drive_id, item.id], |r| r.get(0))?;
            del_fts_stmt.execute([rowid])?;
            ins_fts_stmt.execute(params![
                rowid,
                item.drive_id,
                item.id,
                item.name,
                item.parent_path,
                author_text(item),
                item.mime_type.as_deref().unwrap_or(""),
            ])?;
            written += 1;
        }
    }
    tx.commit()?;
    Ok(written)
}

/// Replaces cached children for a specific `(drive_id, parent_id)` folder view so deleted items vanish cleanly.
pub fn replace_folder_children(
    conn: &mut Connection,
    drive_id: &str,
    parent_id: Option<&str>,
    items: &[DriveItem],
) -> Result<()> {
    let pid = parent_id.map(str::trim).filter(|s| !s.is_empty() && *s != "root");
    let tx = conn.transaction()?;
    {
        let existing_ids: Vec<(i64, String)> = if let Some(p) = pid {
            let mut stmt = tx.prepare_cached(
                "SELECT rowid, id FROM drive_items WHERE drive_id = ?1 AND parent_id = ?2",
            )?;
            let mapped = stmt.query_map(params![drive_id, p], |r| Ok((r.get(0)?, r.get(1)?)))?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            let mut stmt = tx.prepare_cached(
                "SELECT rowid, id FROM drive_items WHERE drive_id = ?1 AND (parent_path = '' OR parent_path = '/')",
            )?;
            let mapped = stmt.query_map([drive_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()?
        };

        let incoming: std::collections::HashSet<&str> =
            items.iter().map(|i| i.id.as_str()).collect();
        let mut del_item =
            tx.prepare_cached("DELETE FROM drive_items WHERE drive_id = ?1 AND id = ?2")?;
        let mut del_fts = tx.prepare_cached("DELETE FROM drive_items_fts WHERE rowid = ?1")?;
        for (rowid, id) in existing_ids {
            if !incoming.contains(id.as_str()) {
                del_fts.execute([rowid])?;
                del_item.execute(params![drive_id, id])?;
            }
        }
    }
    tx.commit()?;
    upsert_drive_items(conn, items)?;
    Ok(())
}

pub fn list_folder_children(
    conn: &Connection,
    drive_id: &str,
    parent_id: Option<&str>,
    parent_path: Option<&str>,
    limit: usize,
) -> Result<Vec<DriveItem>> {
    let lim = limit.max(1) as i64;
    if let Some(pid) = parent_id.map(str::trim).filter(|s| !s.is_empty() && *s != "root") {
        let sql = format!(
            "SELECT {DRIVE_ITEM_COLS} FROM drive_items
             WHERE drive_id = ?1 AND parent_id = ?2
             ORDER BY is_folder DESC, name COLLATE NOCASE ASC
             LIMIT ?3"
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params![drive_id, pid, lim], map_drive_item_row)?;
        return rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into);
    }

    let path = parent_path.unwrap_or("").trim().trim_end_matches('/');
    let norm_path = if path.is_empty() || path == "/" {
        ""
    } else {
        path
    };
    let sql = format!(
        "SELECT {DRIVE_ITEM_COLS} FROM drive_items
         WHERE drive_id = ?1 AND parent_path = ?2
         ORDER BY is_folder DESC, name COLLATE NOCASE ASC
         LIMIT ?3"
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map(params![drive_id, norm_path, lim], map_drive_item_row)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub fn get_drive_item(
    conn: &Connection,
    drive_id: &str,
    item_id: &str,
) -> Result<Option<DriveItem>> {
    let sql = format!(
        "SELECT {DRIVE_ITEM_COLS} FROM drive_items WHERE drive_id = ?1 AND id = ?2 LIMIT 1"
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    stmt.query_row(params![drive_id, item_id], map_drive_item_row)
        .optional()
        .map_err(Into::into)
}

pub fn get_drive_item_by_id_any_drive(conn: &Connection, item_id: &str) -> Result<Option<DriveItem>> {
    let sql = format!("SELECT {DRIVE_ITEM_COLS} FROM drive_items WHERE id = ?1 LIMIT 1");
    let mut stmt = conn.prepare_cached(&sql)?;
    stmt.query_row([item_id], map_drive_item_row)
        .optional()
        .map_err(Into::into)
}

pub fn delete_drive_item(conn: &mut Connection, drive_id: &str, item_id: &str) -> Result<()> {
    let tx = conn.transaction()?;
    {
        let rowid: Option<i64> = tx
            .query_row(
                "SELECT rowid FROM drive_items WHERE drive_id = ?1 AND id = ?2",
                params![drive_id, item_id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(rid) = rowid {
            tx.execute("DELETE FROM drive_items_fts WHERE rowid = ?1", [rid])?;
        }
        tx.execute(
            "DELETE FROM drive_items WHERE drive_id = ?1 AND id = ?2",
            params![drive_id, item_id],
        )?;
        tx.execute(
            "DELETE FROM drive_item_lists WHERE drive_id = ?1 AND item_id = ?2",
            params![drive_id, item_id],
        )?;
    }
    tx.commit()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Recent & Shared Item Lists
// ---------------------------------------------------------------------------

pub fn replace_named_item_list(
    conn: &mut Connection,
    list_kind: &str,
    items: &[DriveItem],
) -> Result<()> {
    upsert_drive_items(conn, items)?;
    let tx = conn.transaction()?;
    {
        tx.execute(
            "DELETE FROM drive_item_lists WHERE list_kind = ?1",
            [list_kind],
        )?;
        let mut stmt = tx.prepare_cached(
            "INSERT OR REPLACE INTO drive_item_lists (list_kind, position, drive_id, item_id) VALUES (?1, ?2, ?3, ?4)",
        )?;
        for (idx, item) in items.iter().enumerate() {
            stmt.execute(params![list_kind, idx as i64, item.drive_id, item.id])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn list_named_items(conn: &Connection, list_kind: &str, limit: usize) -> Result<Vec<DriveItem>> {
    let sql = format!(
        "
        SELECT d.drive_id, d.id, d.parent_id, d.parent_path, d.name, d.size, d.is_folder, d.child_count,
               d.mime_type, d.web_url, d.download_url, d.e_tag, d.c_tag, d.created_at, d.last_modified_at,
               d.created_by, d.last_modified_by, d.shared_scope, d.remote_drive_id, d.remote_item_id
        FROM drive_item_lists l
        JOIN drive_items d ON d.drive_id = l.drive_id AND d.id = l.item_id
        WHERE l.list_kind = ?1
        ORDER BY l.position ASC
        LIMIT ?2
        "
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map(params![list_kind, limit.max(1) as i64], map_drive_item_row)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

// ---------------------------------------------------------------------------
// Pinned Sites & Folders
// ---------------------------------------------------------------------------

pub fn upsert_pin(conn: &Connection, pin: &PinnedTarget) -> Result<()> {
    conn.execute(
        "
        INSERT INTO pinned_targets (key, kind, drive_id, item_id, site_id, label)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6)
        ON CONFLICT(key) DO UPDATE SET
            kind = excluded.kind,
            drive_id = excluded.drive_id,
            item_id = excluded.item_id,
            site_id = excluded.site_id,
            label = excluded.label
        ",
        params![
            pin.key,
            pin.kind,
            pin.drive_id,
            pin.item_id,
            pin.site_id,
            pin.label
        ],
    )?;
    Ok(())
}

pub fn remove_pin(conn: &Connection, key: &str) -> Result<()> {
    conn.execute("DELETE FROM pinned_targets WHERE key = ?1", [key])?;
    Ok(())
}

pub fn list_pins(conn: &Connection) -> Result<Vec<PinnedTarget>> {
    let mut stmt = conn.prepare_cached(
        "SELECT key, kind, drive_id, item_id, site_id, label FROM pinned_targets ORDER BY kind ASC, label COLLATE NOCASE ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(PinnedTarget {
            key: row.get(0)?,
            kind: row.get(1)?,
            drive_id: row.get(2)?,
            item_id: row.get(3)?,
            site_id: row.get(4)?,
            label: row.get(5)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

// ---------------------------------------------------------------------------
// Sync Cursors
// ---------------------------------------------------------------------------

pub fn get_sync_cursor(conn: &Connection, scope_key: &str) -> Result<Option<SyncCursor>> {
    let mut stmt = conn.prepare_cached(
        "SELECT scope_key, delta_link, history_next_link, last_synced_at FROM sync_cursors WHERE scope_key = ?1",
    )?;
    stmt.query_row([scope_key], |row| {
        Ok(SyncCursor {
            scope_key: row.get(0)?,
            delta_link: row.get(1)?,
            history_next_link: row.get(2)?,
            last_synced_at: row.get(3)?,
        })
    })
    .optional()
    .map_err(Into::into)
}

pub fn upsert_sync_cursor(conn: &Connection, cursor: &SyncCursor) -> Result<()> {
    conn.execute(
        "
        INSERT INTO sync_cursors (scope_key, delta_link, history_next_link, last_synced_at)
        VALUES (?1, ?2, ?3, ?4)
        ON CONFLICT(scope_key) DO UPDATE SET
            delta_link = excluded.delta_link,
            history_next_link = excluded.history_next_link,
            last_synced_at = excluded.last_synced_at
        ",
        params![
            cursor.scope_key,
            cursor.delta_link,
            cursor.history_next_link,
            cursor.last_synced_at,
        ],
    )?;
    Ok(())
}
