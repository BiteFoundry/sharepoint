//! Hybrid FTS5 (`trigram` / `unicode61`) + Korean `<3` char `LIKE` search across cached drive items.

use anyhow::Result;
use rusqlite::{params, Connection};

use crate::{
    models::{DriveItem, DriveSearchHit},
    storage::drives::{escape_like, map_drive_item_row},
};

const SEARCH_COLS: &str = "
    d.drive_id, d.id, d.parent_id, d.parent_path, d.name, d.size, d.is_folder, d.child_count,
    d.mime_type, d.web_url, d.download_url, d.e_tag, d.c_tag, d.created_at, d.last_modified_at,
    d.created_by, d.last_modified_by, d.shared_scope, d.remote_drive_id, d.remote_item_id
";

pub fn search_items(
    conn: &Connection,
    query: &str,
    drive_id: Option<&str>,
    limit: usize,
    trigram_enabled: bool,
) -> Result<Vec<DriveSearchHit>> {
    let trimmed = query.trim();
    if trimmed.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let drive_filter = drive_id.map(str::trim).filter(|s| !s.is_empty());

    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    let all_tokens_at_least_3 = tokens.iter().all(|t| t.chars().count() >= 3);
    if trigram_enabled && trimmed.chars().count() >= 3 {
        if let Some(fts_query) = build_fts_query(trimmed, &tokens, all_tokens_at_least_3) {
            if let Ok(hits) = run_fts_search(conn, &fts_query, drive_filter, limit) {
                if !hits.is_empty() {
                    return Ok(hits);
                }
            }
        }
    }

    // Fallback for Korean/CJK `<3` character queries or unicode61 partial substring matches.
    run_like_search(conn, trimmed, drive_filter, limit)
}

fn build_fts_query(trimmed: &str, tokens: &[&str], all_tokens_at_least_3: bool) -> Option<String> {
    if tokens.is_empty() {
        return None;
    }
    if tokens.len() > 1 && all_tokens_at_least_3 {
        let quoted: Vec<String> = tokens
            .iter()
            .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
            .collect();
        Some(quoted.join(" "))
    } else {
        Some(format!("\"{}\"", trimmed.replace('"', "\"\"")))
    }
}

fn run_fts_search(
    conn: &Connection,
    fts_query: &str,
    drive_filter: Option<&str>,
    limit: usize,
) -> Result<Vec<DriveSearchHit>> {
    let items: Vec<DriveItem> = if let Some(did) = drive_filter {
        let sql = format!(
            "
            SELECT {SEARCH_COLS}
            FROM drive_items_fts f
            JOIN drive_items d ON d.rowid = f.rowid
            WHERE drive_items_fts MATCH ?1
              AND d.drive_id = ?2
            ORDER BY d.is_folder DESC, d.last_modified_at DESC, d.name COLLATE NOCASE ASC
            LIMIT ?3
            "
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params![fts_query, did, limit as i64], map_drive_item_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    } else {
        let sql = format!(
            "
            SELECT {SEARCH_COLS}
            FROM drive_items_fts f
            JOIN drive_items d ON d.rowid = f.rowid
            WHERE drive_items_fts MATCH ?1
            ORDER BY d.is_folder DESC, d.last_modified_at DESC, d.name COLLATE NOCASE ASC
            LIMIT ?2
            "
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params![fts_query, limit as i64], map_drive_item_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    Ok(items.into_iter().map(into_search_hit).collect())
}

fn run_like_search(
    conn: &Connection,
    raw_query: &str,
    drive_filter: Option<&str>,
    limit: usize,
) -> Result<Vec<DriveSearchHit>> {
    let pattern = format!("%{}%", escape_like(&raw_query.to_lowercase()));
    let items: Vec<DriveItem> = if let Some(did) = drive_filter {
        let sql = format!(
            "
            SELECT {SEARCH_COLS}
            FROM drive_items d
            WHERE d.drive_id = ?1
              AND (
                  LOWER(d.name) LIKE ?2 ESCAPE '\\'
                  OR LOWER(d.parent_path) LIKE ?2 ESCAPE '\\'
                  OR LOWER(COALESCE(d.last_modified_by, '')) LIKE ?2 ESCAPE '\\'
                  OR LOWER(COALESCE(d.created_by, '')) LIKE ?2 ESCAPE '\\'
                  OR LOWER(COALESCE(d.mime_type, '')) LIKE ?2 ESCAPE '\\'
              )
            ORDER BY d.is_folder DESC, d.last_modified_at DESC, d.name COLLATE NOCASE ASC
            LIMIT ?3
            "
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params![did, pattern, limit as i64], map_drive_item_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    } else {
        let sql = format!(
            "
            SELECT {SEARCH_COLS}
            FROM drive_items d
            WHERE (
                LOWER(d.name) LIKE ?1 ESCAPE '\\'
                OR LOWER(d.parent_path) LIKE ?1 ESCAPE '\\'
                OR LOWER(COALESCE(d.last_modified_by, '')) LIKE ?1 ESCAPE '\\'
                OR LOWER(COALESCE(d.created_by, '')) LIKE ?1 ESCAPE '\\'
                OR LOWER(COALESCE(d.mime_type, '')) LIKE ?1 ESCAPE '\\'
            )
            ORDER BY d.is_folder DESC, d.last_modified_at DESC, d.name COLLATE NOCASE ASC
            LIMIT ?2
            "
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params![pattern, limit as i64], map_drive_item_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    Ok(items.into_iter().map(into_search_hit).collect())
}

fn into_search_hit(item: DriveItem) -> DriveSearchHit {
    let path = item.display_path();
    let author = item
        .last_modified_by
        .as_deref()
        .or(item.created_by.as_deref())
        .unwrap_or("");
    let snippet = if author.is_empty() {
        path
    } else {
        format!("{path} · {author}")
    };
    DriveSearchHit { item, snippet }
}
