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
    if trigram_enabled && all_tokens_at_least_3 {
        if let Some(fts_query) = build_fts_query(trimmed, &tokens, all_tokens_at_least_3) {
            if let Ok(hits) = run_fts_search(conn, &fts_query, drive_filter, limit) {
                if !hits.is_empty() {
                    return Ok(hits);
                }
            }
        }
    }

    // Parameterized token-AND LIKE search for queries with `<3` char tokens or unicode61 fallback.
    run_like_search(conn, &tokens, drive_filter, limit)
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
    tokens: &[&str],
    drive_filter: Option<&str>,
    limit: usize,
) -> Result<Vec<DriveSearchHit>> {
    if tokens.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let mut sql = format!("SELECT {SEARCH_COLS} FROM drive_items d WHERE ");
    let mut values: Vec<rusqlite::types::Value> = Vec::with_capacity(tokens.len() + 2);
    let mut clauses: Vec<String> = Vec::with_capacity(tokens.len() + 1);

    if let Some(did) = drive_filter {
        values.push(rusqlite::types::Value::Text(did.to_string()));
        clauses.push(format!("d.drive_id = ?{}", values.len()));
    }

    for token in tokens {
        let pattern = format!("%{}%", escape_like(&token.to_lowercase()));
        values.push(rusqlite::types::Value::Text(pattern));
        let p = values.len();
        clauses.push(format!(
            "(LOWER(d.name) LIKE ?{p} ESCAPE '\\' \
             OR LOWER(d.parent_path) LIKE ?{p} ESCAPE '\\' \
             OR LOWER(COALESCE(d.last_modified_by, '')) LIKE ?{p} ESCAPE '\\' \
             OR LOWER(COALESCE(d.created_by, '')) LIKE ?{p} ESCAPE '\\' \
             OR LOWER(COALESCE(d.mime_type, '')) LIKE ?{p} ESCAPE '\\')"
        ));
    }

    sql.push_str(&clauses.join(" AND "));
    values.push(rusqlite::types::Value::Integer(limit as i64));
    sql.push_str(&format!(
        " ORDER BY d.is_folder DESC, d.last_modified_at DESC, d.name COLLATE NOCASE ASC LIMIT ?{}",
        values.len()
    ));

    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(values.iter()), map_drive_item_row)?;
    let items: Vec<DriveItem> = rows.collect::<rusqlite::Result<Vec<_>>>()?;

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

#[cfg(test)]
mod tests {
    use crate::{models::DriveItem, storage::Storage};

    #[test]
    fn regression_sharepoint_sp_14_local_search_handles_mixed_short_tokens_and_like_escapes() {
        let storage = Storage::open_in_memory().unwrap();
        let seed = vec![
            DriveItem {
                id: "item-q4-report".into(),
                drive_id: "drv-a".into(),
                parent_path: String::new(),
                name: "Q4 2026 report.txt".into(),
                last_modified_at: Some("2026-11-20T10:00:00Z".into()),
                ..Default::default()
            },
            DriveItem {
                id: "item-q4-only".into(),
                drive_id: "drv-a".into(),
                parent_path: String::new(),
                name: "Q4 only.txt".into(),
                last_modified_at: Some("2026-11-20T09:00:00Z".into()),
                ..Default::default()
            },
            DriveItem {
                id: "item-report-only".into(),
                drive_id: "drv-a".into(),
                parent_path: String::new(),
                name: "2026 report.txt".into(),
                last_modified_at: Some("2026-11-20T08:00:00Z".into()),
                ..Default::default()
            },
            DriveItem {
                id: "item-ko-path".into(),
                drive_id: "drv-a".into(),
                parent_path: "/기획".into(),
                name: "2026년 보고서.pdf".into(),
                last_modified_at: Some("2026-11-20T07:00:00Z".into()),
                ..Default::default()
            },
            DriveItem {
                id: "item-ko-phrase".into(),
                drive_id: "drv-a".into(),
                parent_path: String::new(),
                name: "기획 보고서.md".into(),
                last_modified_at: Some("2026-11-20T06:00:00Z".into()),
                ..Default::default()
            },
            DriveItem {
                id: "item-ko-one-word".into(),
                drive_id: "drv-a".into(),
                parent_path: "/기획".into(),
                name: "메모.txt".into(),
                last_modified_at: Some("2026-11-20T05:00:00Z".into()),
                ..Default::default()
            },
            DriveItem {
                id: "item-like-literal".into(),
                drive_id: "drv-a".into(),
                parent_path: String::new(),
                name: "100%_budget.xlsx".into(),
                last_modified_at: Some("2026-11-20T04:00:00Z".into()),
                ..Default::default()
            },
            DriveItem {
                id: "item-like-false-pos".into(),
                drive_id: "drv-a".into(),
                parent_path: String::new(),
                name: "1000Xbudget.xlsx".into(),
                last_modified_at: Some("2026-11-20T03:00:00Z".into()),
                ..Default::default()
            },
            DriveItem {
                id: "item-other-drive".into(),
                drive_id: "drv-b".into(),
                parent_path: String::new(),
                name: "Q4 2026 report.txt".into(),
                last_modified_at: Some("2026-11-20T11:00:00Z".into()),
                ..Default::default()
            },
        ];
        storage.upsert_drive_items(&seed).unwrap();

        // Both token orders find Q4 2026 report.txt and exclude single-word matches
        for q in ["Q4 report", "report Q4"] {
            let hits = storage.search_items(q, Some("drv-a"), 20).unwrap();
            let ids: Vec<&str> = hits.iter().map(|h| h.item.id.as_str()).collect();
            assert_eq!(
                ids,
                vec!["item-q4-report"],
                "query '{q}' must match only Q4 2026 report.txt in drv-a"
            );
        }

        // Korean short token in parent_path + token in filename both match; single-word excluded
        let ko_hits = storage.search_items("기획 보고서", Some("drv-a"), 20).unwrap();
        let ko_ids: Vec<&str> = ko_hits.iter().map(|h| h.item.id.as_str()).collect();
        assert_eq!(
            ko_ids,
            vec!["item-ko-path", "item-ko-phrase"],
            "query '기획 보고서' must return both /기획/2026년 보고서.pdf and 기획 보고서.md, excluding single-word match"
        );

        // Literal % and _ escaping in short-token LIKE search
        let lit_hits = storage.search_items("% _", Some("drv-a"), 20).unwrap();
        let lit_ids: Vec<&str> = lit_hits.iter().map(|h| h.item.id.as_str()).collect();
        assert_eq!(
            lit_ids,
            vec!["item-like-literal"],
            "LIKE wildcards % and _ must be matched literally"
        );

        // Limit & drive scope respected
        let limited = storage.search_items("기획 보고서", Some("drv-a"), 1).unwrap();
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].item.id, "item-ko-path");

        let cross_drive = storage.search_items("Q4 report", None, 20).unwrap();
        assert_eq!(cross_drive.len(), 2);

        // Regression control: single short token and all-long-token FTS path preserved
        let single_short = storage.search_items("기획", Some("drv-a"), 20).unwrap();
        assert_eq!(single_short.len(), 3);

        let all_long = storage.search_items("2026 report", Some("drv-a"), 20).unwrap();
        let long_ids: Vec<&str> = all_long.iter().map(|h| h.item.id.as_str()).collect();
        assert_eq!(long_ids, vec!["item-q4-report", "item-report-only"]);
    }
}
