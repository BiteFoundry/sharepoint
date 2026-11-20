//! SQLite persistence for SharePoint sites, drives, drive items (with FTS5 sync),
//! recent/shared lists, pinned sites/folders, and sync cursors.

use std::{
    collections::{HashSet, VecDeque},
    path::Path,
};

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::{
    graph::drives::DriveDeltaChange,
    models::{
        DownloadBaseline, DriveInfo, DriveItem, PinnedTarget, SharePointSite, SyncCursor,
    },
};

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
    upsert_sites_inner(conn, sites, false)
}

/// Upserts an authoritative site list with exact follow state (reflecting site unfollows).
pub fn upsert_sites_with_follow_state(
    conn: &mut Connection,
    sites: &[SharePointSite],
) -> Result<()> {
    upsert_sites_inner(conn, sites, true)
}

fn upsert_sites_inner(
    conn: &mut Connection,
    sites: &[SharePointSite],
    authoritative_follow_state: bool,
) -> Result<()> {
    let tx = conn.transaction()?;
    {
        if authoritative_follow_state {
            tx.execute("UPDATE sites SET is_followed = 0 WHERE is_followed != 0", [])?;
        }
        if !sites.is_empty() {
            let follow_expr = if authoritative_follow_state {
                "excluded.is_followed"
            } else {
                "MAX(sites.is_followed, excluded.is_followed)"
            };
            let sql = format!(
                "
                INSERT INTO sites (id, name, display_name, description, web_url, is_followed, created_at, last_modified_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                ON CONFLICT(id) DO UPDATE SET
                    name = excluded.name,
                    display_name = excluded.display_name,
                    description = COALESCE(excluded.description, sites.description),
                    web_url = COALESCE(excluded.web_url, sites.web_url),
                    is_followed = {follow_expr},
                    created_at = COALESCE(excluded.created_at, sites.created_at),
                    last_modified_at = COALESCE(excluded.last_modified_at, sites.last_modified_at)
                "
            );
            let mut stmt = tx.prepare_cached(&sql)?;
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

fn normalize_parent_path(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() || trimmed == "/" {
        String::new()
    } else if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    }
}

fn item_display_path(parent_path: &str, name: &str) -> String {
    let p = normalize_parent_path(parent_path);
    if p.is_empty() {
        format!("/{name}")
    } else {
        format!("{p}/{name}")
    }
}

fn upsert_drive_items_tx(tx: &rusqlite::Transaction<'_>, items: &[DriveItem]) -> Result<usize> {
    if items.is_empty() {
        return Ok(0);
    }
    let mut written = 0usize;
    let mut moved_folders: Vec<(String, String, String)> = Vec::new();
    {
        let mut get_prev_folder_stmt = tx.prepare_cached(
            "SELECT is_folder, parent_path, name FROM drive_items WHERE drive_id = ?1 AND id = ?2",
        )?;
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
                parent_path = excluded.parent_path,
                name = excluded.name,
                size = excluded.size,
                is_folder = excluded.is_folder,
                child_count = COALESCE(excluded.child_count, drive_items.child_count),
                mime_type = COALESCE(excluded.mime_type, drive_items.mime_type),
                web_url = COALESCE(excluded.web_url, drive_items.web_url),
                download_url = COALESCE(excluded.download_url, drive_items.download_url),
                e_tag = COALESCE(excluded.e_tag, drive_items.e_tag),
                c_tag = CASE
                    WHEN excluded.c_tag IS NOT NULL THEN excluded.c_tag
                    WHEN excluded.e_tag IS NOT NULL AND drive_items.e_tag IS NOT NULL AND excluded.e_tag != drive_items.e_tag THEN NULL
                    WHEN excluded.size != drive_items.size THEN NULL
                    WHEN excluded.last_modified_at IS NOT NULL AND drive_items.last_modified_at IS NOT NULL AND excluded.last_modified_at != drive_items.last_modified_at THEN NULL
                    ELSE drive_items.c_tag
                END,
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
            let norm_parent_path = normalize_parent_path(&item.parent_path);
            if item.is_folder {
                let prev: Option<(i32, String, String)> = get_prev_folder_stmt
                    .query_row(params![item.drive_id, item.id], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                    })
                    .optional()?;
                if let Some((was_folder, old_pp, old_name)) = prev {
                    if was_folder != 0 {
                        let old_full = item_display_path(&old_pp, &old_name);
                        let new_full = item_display_path(&norm_parent_path, &item.name);
                        if old_full != new_full {
                            moved_folders.push((item.drive_id.clone(), old_full, new_full));
                        }
                    }
                }
            }
            upsert_stmt.execute(params![
                item.drive_id,
                item.id,
                item.parent_id,
                norm_parent_path,
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
                norm_parent_path,
                author_text(item),
                item.mime_type.as_deref().unwrap_or(""),
            ])?;
            written += 1;
        }
    }

    for (drive_id, old_prefix, new_prefix) in moved_folders {
        let like_pat = format!("{}/%", escape_like(&old_prefix));
        let descendants: Vec<(i64, String, String, String, Option<String>, Option<String>, Option<String>)> = {
            let mut stmt = tx.prepare_cached(
                "SELECT rowid, id, parent_path, name, last_modified_by, created_by, mime_type
                 FROM drive_items
                 WHERE drive_id = ?1
                   AND (parent_path = ?2 COLLATE NOCASE OR parent_path LIKE ?3 ESCAPE '\\' COLLATE NOCASE)",
            )?;
            let mapped = stmt.query_map(params![drive_id, old_prefix, like_pat], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            })?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut upd_path = tx.prepare_cached(
            "UPDATE drive_items SET parent_path = ?1 WHERE drive_id = ?2 AND id = ?3",
        )?;
        let mut del_fts = tx.prepare_cached("DELETE FROM drive_items_fts WHERE rowid = ?1")?;
        let mut ins_fts = tx.prepare_cached(
            "INSERT INTO drive_items_fts (rowid, drive_id, item_id, name, parent_path, author, mime_type)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        for (rowid, id, pp, name, mod_by, cr_by, mime) in descendants {
            let suffix = pp.get(old_prefix.len()..).unwrap_or("");
            let updated_pp = format!("{new_prefix}{suffix}");
            upd_path.execute(params![updated_pp, drive_id, id])?;
            del_fts.execute([rowid])?;
            let dummy = DriveItem {
                last_modified_by: mod_by,
                created_by: cr_by,
                ..Default::default()
            };
            ins_fts.execute(params![
                rowid,
                drive_id,
                id,
                name,
                updated_pp,
                author_text(&dummy),
                mime.as_deref().unwrap_or(""),
            ])?;
        }
    }

    Ok(written)
}

pub fn upsert_drive_items(conn: &mut Connection, items: &[DriveItem]) -> Result<usize> {
    if items.is_empty() {
        return Ok(0);
    }
    let tx = conn.transaction()?;
    let written = upsert_drive_items_tx(&tx, items)?;
    tx.commit()?;
    Ok(written)
}

/// Deletes an item and, if it is a cached folder, recursively deletes all cached descendants
/// from `drive_items`, `drive_items_fts`, and `drive_item_lists`.
fn delete_item_and_descendants_tx(
    tx: &rusqlite::Transaction<'_>,
    drive_id: &str,
    item_id: &str,
) -> Result<()> {
    let mut visited_ids: HashSet<String> = HashSet::new();
    let mut to_delete: Vec<(i64, String)> = Vec::new();
    let mut folder_queue: VecDeque<(String, String)> = VecDeque::new();

    let root_row: Option<(i64, i32, String, String)> = tx
        .query_row(
            "SELECT rowid, is_folder, parent_path, name FROM drive_items WHERE drive_id = ?1 AND id = ?2",
            params![drive_id, item_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;

    if let Some((rid, is_folder, pp, name)) = root_row {
        visited_ids.insert(item_id.to_string());
        to_delete.push((rid, item_id.to_string()));
        if is_folder != 0 {
            folder_queue.push_back((item_id.to_string(), item_display_path(&pp, &name)));
        }
    } else {
        tx.execute(
            "DELETE FROM drive_item_lists WHERE drive_id = ?1 AND item_id = ?2",
            params![drive_id, item_id],
        )?;
        return Ok(());
    }

    while let Some((folder_id, folder_path)) = folder_queue.pop_front() {
        let like_pat = format!("{}/%", escape_like(&folder_path));
        let children: Vec<(i64, String, i32, String, String)> = {
            let mut stmt = tx.prepare_cached(
                "SELECT rowid, id, is_folder, parent_path, name
                 FROM drive_items
                 WHERE drive_id = ?1
                   AND (
                       parent_id = ?2
                       OR parent_path = ?3 COLLATE NOCASE
                       OR parent_path LIKE ?4 ESCAPE '\\' COLLATE NOCASE
                   )",
            )?;
            let mapped = stmt.query_map(params![drive_id, folder_id, folder_path, like_pat], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (rid, cid, is_folder, pp, name) in children {
            if visited_ids.insert(cid.clone()) {
                to_delete.push((rid, cid.clone()));
                if is_folder != 0 {
                    folder_queue.push_back((cid, item_display_path(&pp, &name)));
                }
            }
        }
    }

    let mut del_fts = tx.prepare_cached("DELETE FROM drive_items_fts WHERE rowid = ?1")?;
    let mut del_item =
        tx.prepare_cached("DELETE FROM drive_items WHERE drive_id = ?1 AND id = ?2")?;
    let mut del_list =
        tx.prepare_cached("DELETE FROM drive_item_lists WHERE drive_id = ?1 AND item_id = ?2")?;
    for (rid, id) in to_delete {
        del_fts.execute([rid])?;
        del_item.execute(params![drive_id, id])?;
        del_list.execute(params![drive_id, id])?;
    }
    Ok(())
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
        let existing_ids: Vec<String> = if let Some(p) = pid {
            let mut stmt = tx.prepare_cached(
                "SELECT id FROM drive_items WHERE drive_id = ?1 AND parent_id = ?2",
            )?;
            let mapped = stmt.query_map(params![drive_id, p], |r| r.get(0))?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            let mut stmt = tx.prepare_cached(
                "SELECT id FROM drive_items WHERE drive_id = ?1 AND (parent_path = '' OR parent_path = '/')",
            )?;
            let mapped = stmt.query_map([drive_id], |r| r.get(0))?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()?
        };

        let incoming: HashSet<&str> = items.iter().map(|i| i.id.as_str()).collect();
        for id in existing_ids {
            if !incoming.contains(id.as_str()) {
                delete_item_and_descendants_tx(&tx, drive_id, &id)?;
            }
        }
        upsert_drive_items_tx(&tx, items)?;
    }
    tx.commit()?;
    Ok(())
}

/// Replaces cached children for a folder addressed by logical path (e.g. `"/Projects"` or `"Projects"`).
pub fn replace_folder_children_by_path(
    conn: &mut Connection,
    drive_id: &str,
    folder_path: &str,
    items: &[DriveItem],
) -> Result<()> {
    let norm_path = normalize_parent_path(folder_path);
    if norm_path.is_empty() {
        return replace_folder_children(conn, drive_id, None, items);
    }

    let tx = conn.transaction()?;
    {
        let folder_id = get_drive_item_by_path_conn(&tx, drive_id, &norm_path)?
            .filter(|it| it.is_folder)
            .map(|it| it.id);
        let existing_ids: Vec<String> = if let Some(ref fid) = folder_id {
            let mut stmt = tx.prepare_cached(
                "SELECT id FROM drive_items
                 WHERE drive_id = ?1
                   AND (parent_path = ?2 COLLATE NOCASE OR parent_id = ?3)",
            )?;
            let mapped = stmt.query_map(params![drive_id, norm_path, fid], |r| r.get(0))?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            let mut stmt = tx.prepare_cached(
                "SELECT id FROM drive_items
                 WHERE drive_id = ?1 AND parent_path = ?2 COLLATE NOCASE",
            )?;
            let mapped = stmt.query_map(params![drive_id, norm_path], |r| r.get(0))?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()?
        };

        let incoming: HashSet<&str> = items.iter().map(|i| i.id.as_str()).collect();
        for id in existing_ids {
            if !incoming.contains(id.as_str()) {
                delete_item_and_descendants_tx(&tx, drive_id, &id)?;
            }
        }
        upsert_drive_items_tx(&tx, items)?;
    }
    tx.commit()?;
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

    let norm_path = normalize_parent_path(parent_path.unwrap_or(""));
    if norm_path.is_empty() {
        let sql = format!(
            "SELECT {DRIVE_ITEM_COLS} FROM drive_items
             WHERE drive_id = ?1 AND (parent_path = '' OR parent_path = '/')
             ORDER BY is_folder DESC, name COLLATE NOCASE ASC
             LIMIT ?2"
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params![drive_id, lim], map_drive_item_row)?;
        return rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into);
    }

    let sql = format!(
        "SELECT {DRIVE_ITEM_COLS} FROM drive_items
         WHERE drive_id = ?1 AND parent_path = ?2 COLLATE NOCASE
         ORDER BY is_folder DESC, name COLLATE NOCASE ASC
         LIMIT ?3"
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map(params![drive_id, norm_path, lim], map_drive_item_row)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

fn get_drive_item_by_path_conn(
    conn: &Connection,
    drive_id: &str,
    path: &str,
) -> Result<Option<DriveItem>> {
    let trimmed = path.trim().trim_matches('/');
    if trimmed.is_empty() {
        return Ok(None);
    }
    let (parent_path, name) = match trimmed.rsplit_once('/') {
        Some((parent, leaf)) => (format!("/{parent}"), leaf),
        None => (String::new(), trimmed),
    };
    if parent_path.is_empty() {
        let sql = format!(
            "SELECT {DRIVE_ITEM_COLS} FROM drive_items
             WHERE drive_id = ?1
               AND (parent_path = '' OR parent_path = '/')
               AND name = ?2 COLLATE NOCASE
             LIMIT 1"
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        stmt.query_row(params![drive_id, name], map_drive_item_row)
            .optional()
            .map_err(Into::into)
    } else {
        let sql = format!(
            "SELECT {DRIVE_ITEM_COLS} FROM drive_items
             WHERE drive_id = ?1
               AND parent_path = ?2 COLLATE NOCASE
               AND name = ?3 COLLATE NOCASE
             LIMIT 1"
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        stmt.query_row(params![drive_id, parent_path, name], map_drive_item_row)
            .optional()
            .map_err(Into::into)
    }
}

pub fn get_drive_item_by_path(
    conn: &Connection,
    drive_id: &str,
    path: &str,
) -> Result<Option<DriveItem>> {
    get_drive_item_by_path_conn(conn, drive_id, path)
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
    delete_item_and_descendants_tx(&tx, drive_id, item_id)?;
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

pub fn delete_sync_cursor(conn: &Connection, scope_key: &str) -> Result<()> {
    conn.execute("DELETE FROM sync_cursors WHERE scope_key = ?1", [scope_key])?;
    Ok(())
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DeltaApplyOutcome {
    pub upserted_count: usize,
    pub deleted_count: usize,
    pub affected_root: bool,
    pub affected_parent_ids: HashSet<String>,
    pub detected_root_id: Option<String>,
}

fn is_root_parent_id(pid: Option<&str>, known_root_id: Option<&str>) -> bool {
    match pid.map(str::trim).filter(|s| !s.is_empty()) {
        None => true,
        Some("root") => true,
        Some(p) => known_root_id.is_some_and(|r| r == p),
    }
}

fn resolve_delta_item_parent_path(
    tx: &rusqlite::Transaction<'_>,
    drive_id: &str,
    item: &DriveItem,
    has_explicit_parent_path: bool,
    known_root_id: Option<&str>,
    existing: Option<&(Option<String>, String)>,
) -> Result<String> {
    if has_explicit_parent_path {
        return Ok(normalize_parent_path(&item.parent_path));
    }
    if is_root_parent_id(item.parent_id.as_deref(), known_root_id) {
        return Ok(String::new());
    }
    if let Some(pid) = item
        .parent_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let parent_row: Option<(String, String)> = tx
            .query_row(
                "SELECT parent_path, name FROM drive_items WHERE drive_id = ?1 AND id = ?2 LIMIT 1",
                params![drive_id, pid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((pp, name)) = parent_row {
            return Ok(item_display_path(&pp, &name));
        }
        if let Some((old_pid, old_pp)) = existing {
            if old_pid.as_deref() == Some(pid) && !old_pp.is_empty() {
                return Ok(old_pp.clone());
            }
        }
        // Keep subfolder items out of root listing (`parent_path = ''`) when parent folder is uncached.
        return Ok(format!("/__parent/{pid}"));
    }
    Ok(String::new())
}

/// Applies a single `driveItem: delta` page in one SQLite transaction.
/// Only advances `sync_cursors` when `commit_cursor` is `Some(...)` (i.e. final page).
pub fn apply_delta_page(
    conn: &mut Connection,
    drive_id: &str,
    changes: &[DriveDeltaChange],
    known_root_id: Option<&str>,
    commit_cursor: Option<&SyncCursor>,
) -> Result<DeltaApplyOutcome> {
    let tx = conn.transaction()?;
    let mut outcome = DeltaApplyOutcome::default();
    let mut effective_root_id = known_root_id.map(str::to_string);

    for ch in changes {
        if let DriveDeltaChange::Root { id } = ch {
            if !id.trim().is_empty() {
                effective_root_id = Some(id.clone());
                outcome.detected_root_id = Some(id.clone());
            }
        }
    }

    for ch in changes {
        match ch {
            DriveDeltaChange::Root { .. } => {}
            DriveDeltaChange::Upserted {
                item,
                has_explicit_parent_path,
            } => {
                let existing: Option<(Option<String>, String)> = tx
                    .query_row(
                        "SELECT parent_id, parent_path FROM drive_items WHERE drive_id = ?1 AND id = ?2 LIMIT 1",
                        params![drive_id, item.id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;

                if let Some((ref old_pid, ref old_pp)) = existing {
                    if old_pp.is_empty()
                        || old_pp == "/"
                        || is_root_parent_id(old_pid.as_deref(), effective_root_id.as_deref())
                    {
                        outcome.affected_root = true;
                    }
                    if let Some(pid) = old_pid.as_deref().filter(|s| !s.is_empty()) {
                        outcome.affected_parent_ids.insert(pid.to_string());
                    }
                }

                let mut resolved = item.clone();
                if resolved.drive_id.trim().is_empty() {
                    resolved.drive_id = drive_id.to_string();
                }
                resolved.parent_path = resolve_delta_item_parent_path(
                    &tx,
                    drive_id,
                    &resolved,
                    *has_explicit_parent_path,
                    effective_root_id.as_deref(),
                    existing.as_ref(),
                )?;

                if resolved.parent_path.is_empty()
                    || is_root_parent_id(
                        resolved.parent_id.as_deref(),
                        effective_root_id.as_deref(),
                    )
                {
                    outcome.affected_root = true;
                }
                if let Some(pid) = resolved.parent_id.as_deref().filter(|s| !s.is_empty()) {
                    outcome.affected_parent_ids.insert(pid.to_string());
                }

                upsert_drive_items_tx(&tx, std::slice::from_ref(&resolved))?;
                outcome.upserted_count += 1;
            }
            DriveDeltaChange::Deleted { id } => {
                let existing: Option<(Option<String>, String, i32)> = tx
                    .query_row(
                        "SELECT parent_id, parent_path, is_folder FROM drive_items WHERE drive_id = ?1 AND id = ?2 LIMIT 1",
                        params![drive_id, id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()?;
                if let Some((old_pid, old_pp, is_folder)) = existing {
                    if old_pp.is_empty()
                        || old_pp == "/"
                        || is_root_parent_id(old_pid.as_deref(), effective_root_id.as_deref())
                    {
                        outcome.affected_root = true;
                    }
                    if let Some(pid) = old_pid.as_deref().filter(|s| !s.is_empty()) {
                        outcome.affected_parent_ids.insert(pid.to_string());
                    }
                    if is_folder != 0 {
                        outcome.affected_parent_ids.insert(id.clone());
                    }
                } else {
                    // Deleted item may not have been cached yet, or could be an uncached root entry.
                    outcome.affected_root = true;
                }
                delete_item_and_descendants_tx(&tx, drive_id, id)?;
                outcome.deleted_count += 1;
            }
        }
    }

    if let Some(cursor) = commit_cursor {
        upsert_sync_cursor(&tx, cursor)?;
    }
    tx.commit()?;
    Ok(outcome)
}

/// Atomically reconciles a bounded full-drive resync snapshot (e.g. after HTTP `410 Gone`),
/// pruning stale items missing from the snapshot and advancing `sync_cursors` in one transaction.
pub fn replace_drive_items_from_resync(
    conn: &mut Connection,
    drive_id: &str,
    changes: &[DriveDeltaChange],
    known_root_id: Option<&str>,
    commit_cursor: &SyncCursor,
) -> Result<DeltaApplyOutcome> {
    let tx = conn.transaction()?;
    let mut outcome = DeltaApplyOutcome {
        affected_root: true,
        ..Default::default()
    };
    let mut effective_root_id = known_root_id.map(str::to_string);
    let mut live_ids: HashSet<String> = HashSet::new();

    // Delta can return the same ID more than once. Its last state wins,
    // including a tombstone that follows an earlier live item.
    let mut seen: HashSet<&str> = HashSet::new();
    let mut latest: Vec<&DriveDeltaChange> = changes
        .iter()
        .rev()
        .filter(|change| {
            let id = match change {
                DriveDeltaChange::Root { id } | DriveDeltaChange::Deleted { id } => id,
                DriveDeltaChange::Upserted { item, .. } => &item.id,
            };
            seen.insert(id.as_str())
        })
        .collect();
    latest.reverse();

    for ch in latest.iter().copied() {
        if let DriveDeltaChange::Root { id } = ch {
            if !id.trim().is_empty() {
                effective_root_id = Some(id.clone());
                outcome.detected_root_id = Some(id.clone());
            }
        }
    }

    // Apply live items in order (folders first where possible, then remaining items)
    for ch in latest.iter().copied() {
        if let DriveDeltaChange::Upserted {
            item,
            has_explicit_parent_path,
        } = ch
        {
            if item.is_folder {
                let mut resolved = item.clone();
                if resolved.drive_id.trim().is_empty() {
                    resolved.drive_id = drive_id.to_string();
                }
                resolved.parent_path = resolve_delta_item_parent_path(
                    &tx,
                    drive_id,
                    &resolved,
                    *has_explicit_parent_path,
                    effective_root_id.as_deref(),
                    None,
                )?;
                if let Some(pid) = resolved.parent_id.as_deref().filter(|s| !s.is_empty()) {
                    outcome.affected_parent_ids.insert(pid.to_string());
                }
                live_ids.insert(resolved.id.clone());
                upsert_drive_items_tx(&tx, std::slice::from_ref(&resolved))?;
                outcome.upserted_count += 1;
            }
        }
    }
    for ch in latest.iter().copied() {
        if let DriveDeltaChange::Upserted {
            item,
            has_explicit_parent_path,
        } = ch
        {
            if !item.is_folder {
                let mut resolved = item.clone();
                if resolved.drive_id.trim().is_empty() {
                    resolved.drive_id = drive_id.to_string();
                }
                resolved.parent_path = resolve_delta_item_parent_path(
                    &tx,
                    drive_id,
                    &resolved,
                    *has_explicit_parent_path,
                    effective_root_id.as_deref(),
                    None,
                )?;
                if let Some(pid) = resolved.parent_id.as_deref().filter(|s| !s.is_empty()) {
                    outcome.affected_parent_ids.insert(pid.to_string());
                }
                live_ids.insert(resolved.id.clone());
                upsert_drive_items_tx(&tx, std::slice::from_ref(&resolved))?;
                outcome.upserted_count += 1;
            }
        }
    }

    // Prune any previously cached items in this drive missing from the authoritative snapshot
    let existing_ids: Vec<(i64, String, Option<String>)> = {
        let mut stmt =
            tx.prepare_cached("SELECT rowid, id, parent_id FROM drive_items WHERE drive_id = ?1")?;
        let mapped = stmt.query_map([drive_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (rowid, id, pid) in existing_ids {
        if !live_ids.contains(&id) {
            if let Some(p) = pid.filter(|s| !s.is_empty()) {
                outcome.affected_parent_ids.insert(p);
            }
            tx.execute("DELETE FROM drive_items_fts WHERE rowid = ?1", [rowid])?;
            tx.execute("DELETE FROM drive_items WHERE drive_id = ?1 AND id = ?2", params![drive_id, id])?;
            tx.execute("DELETE FROM drive_item_lists WHERE drive_id = ?1 AND item_id = ?2", params![drive_id, id])?;
            outcome.deleted_count += 1;
        }
    }

    upsert_sync_cursor(&tx, commit_cursor)?;
    tx.commit()?;
    Ok(outcome)
}

// ---------------------------------------------------------------------------
// Local Download Baselines (for conditional re-upload with If-Match)
// ---------------------------------------------------------------------------

pub fn normalize_local_baseline_path(path: &Path) -> String {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let s = resolved.to_string_lossy().to_string();
    #[cfg(windows)]
    {
        let stripped = s.strip_prefix(r"\\?\").unwrap_or(&s);
        stripped.replace('/', "\\").to_ascii_lowercase()
    }
    #[cfg(not(windows))]
    {
        s
    }
}

pub fn upsert_download_baseline(conn: &Connection, baseline: &DownloadBaseline) -> Result<()> {
    conn.execute(
        "
        INSERT INTO local_download_baselines (
            local_path, drive_id, item_id, parent_id, parent_path, remote_name,
            e_tag, c_tag, last_modified_at, downloaded_at
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
        ON CONFLICT(local_path) DO UPDATE SET
            drive_id = excluded.drive_id,
            item_id = excluded.item_id,
            parent_id = excluded.parent_id,
            parent_path = excluded.parent_path,
            remote_name = excluded.remote_name,
            e_tag = excluded.e_tag,
            c_tag = excluded.c_tag,
            last_modified_at = excluded.last_modified_at,
            downloaded_at = excluded.downloaded_at
        ",
        params![
            baseline.local_path,
            baseline.drive_id,
            baseline.item_id,
            baseline.parent_id,
            normalize_parent_path(&baseline.parent_path),
            baseline.remote_name,
            baseline.e_tag,
            baseline.c_tag,
            baseline.last_modified_at,
            baseline.downloaded_at,
        ],
    )?;
    Ok(())
}

pub fn get_download_baseline(
    conn: &Connection,
    local_path: &Path,
) -> Result<Option<DownloadBaseline>> {
    let key = normalize_local_baseline_path(local_path);
    let mut stmt = conn.prepare_cached(
        "
        SELECT local_path, drive_id, item_id, parent_id, parent_path, remote_name,
               e_tag, c_tag, last_modified_at, downloaded_at
        FROM local_download_baselines
        WHERE local_path = ?1
        LIMIT 1
        ",
    )?;
    stmt.query_row([key], |row| {
        Ok(DownloadBaseline {
            local_path: row.get(0)?,
            drive_id: row.get(1)?,
            item_id: row.get(2)?,
            parent_id: row.get(3)?,
            parent_path: row.get(4)?,
            remote_name: row.get(5)?,
            e_tag: row.get(6)?,
            c_tag: row.get(7)?,
            last_modified_at: row.get(8)?,
            downloaded_at: row.get(9)?,
        })
    })
    .optional()
    .map_err(Into::into)
}
