//! Smart entity resolver for CLI, HTTP API, and MCP.
//!
//! Resolves SharePoint sites, drives/document libraries (including `onedrive` / `me` / `personal` aliases),
//! and drive items (by exact ID, path like `/Projects/Q4/plan.md`, or unique name).

use anyhow::{bail, Result};

use crate::{
    config::Persona,
    graph::GraphClient,
    models::{DriveInfo, DriveItem, SharePointSite},
    storage::Storage,
};

#[derive(Debug, Clone)]
pub struct AmbiguousMatch {
    pub query: String,
    pub candidates: Vec<(String, String)>,
    message: String,
}

impl std::fmt::Display for AmbiguousMatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AmbiguousMatch {}

fn ambiguous_site_error(query: &str, message: String, candidates: &[&SharePointSite]) -> anyhow::Error {
    anyhow::Error::new(AmbiguousMatch {
        query: query.to_string(),
        candidates: candidates
            .iter()
            .map(|s| (s.effective_name().to_string(), s.id.clone()))
            .collect(),
        message,
    })
}

fn ambiguous_drive_error(query: &str, message: String, candidates: &[&DriveInfo]) -> anyhow::Error {
    anyhow::Error::new(AmbiguousMatch {
        query: query.to_string(),
        candidates: candidates
            .iter()
            .map(|d| (d.display_label(), d.id.clone()))
            .collect(),
        message,
    })
}

pub fn is_personal_drive_alias(q: &str) -> bool {
    matches!(
        q.trim().to_ascii_lowercase().as_str(),
        "me" | "onedrive" | "od" | "personal" | "my" | "my-files" | "myfiles"
    )
}

pub fn resolve_site_from_slice(sites: &[SharePointSite], query: &str) -> Result<SharePointSite> {
    let q = query.trim();
    if q.is_empty() {
        bail!("Site name or ID is empty.");
    }
    if let Some(exact) = sites.iter().find(|s| s.id == q) {
        return Ok(exact.clone());
    }
    let q_lower = q.to_lowercase();
    let exact_name: Vec<&SharePointSite> = sites
        .iter()
        .filter(|s| {
            s.display_name.trim().to_lowercase() == q_lower
                || s.name.trim().to_lowercase() == q_lower
        })
        .collect();
    if exact_name.len() == 1 {
        return Ok(exact_name[0].clone());
    } else if exact_name.len() > 1 {
        return Err(ambiguous_site_error(
            q,
            format!(
                "Ambiguous SharePoint site '{q}' matches multiple sites:\n{}",
                format_site_candidates(&exact_name)
            ),
            &exact_name,
        ));
    }

    let prefix: Vec<&SharePointSite> = sites
        .iter()
        .filter(|s| {
            s.display_name.trim().to_lowercase().starts_with(&q_lower)
                || s.name.trim().to_lowercase().starts_with(&q_lower)
        })
        .collect();
    if prefix.len() == 1 {
        return Ok(prefix[0].clone());
    } else if prefix.len() > 1 {
        return Err(ambiguous_site_error(
            q,
            format!(
                "Ambiguous SharePoint site prefix '{q}' matches multiple sites:\n{}",
                format_site_candidates(&prefix)
            ),
            &prefix,
        ));
    }

    let sub: Vec<&SharePointSite> = sites
        .iter()
        .filter(|s| {
            s.display_name.to_lowercase().contains(&q_lower)
                || s.name.to_lowercase().contains(&q_lower)
                || s.web_url
                    .as_deref()
                    .is_some_and(|u| u.to_lowercase().contains(&q_lower))
        })
        .collect();
    match sub.len() {
        1 => Ok(sub[0].clone()),
        n if n > 1 => Err(ambiguous_site_error(
            q,
            format!(
                "Ambiguous SharePoint site '{q}' matches {n} sites:\n{}",
                format_site_candidates(&sub)
            ),
            &sub,
        )),
        _ => bail!("SharePoint site not found: {q}"),
    }
}

pub async fn resolve_site(
    graph: &GraphClient,
    storage: &Storage,
    query: &str,
    prefer_cache: bool,
) -> Result<SharePointSite> {
    let q = query.trim();
    if q.is_empty() {
        bail!("Site name or ID is empty.");
    }
    let cached = storage.list_sites()?;
    if !cached.is_empty() {
        match resolve_site_from_slice(&cached, q) {
            Ok(s) => return Ok(s),
            Err(err) if prefer_cache => return Err(err),
            Err(_) => {}
        }
    }

    // Followed sites + a Graph search for the query itself in parallel. Searching for the
    // query (not `*`) is what finds a site in a tenant with more sites than one result page.
    let (followed_res, search_res) = tokio::join!(
        graph.list_followed_sites(),
        graph.list_sites(Some(q), SITE_RESOLVE_SEARCH_LIMIT),
    );
    let mut combined = followed_res.unwrap_or_default();
    let mut search_error: Option<anyhow::Error> = None;
    match search_res {
        Ok(found) => merge_sites(&mut combined, found),
        // Kept: an auth or throttling failure must not be reported as "not found".
        Err(err) => search_error = Some(err),
    }
    if !combined.is_empty() {
        let _ = storage.upsert_sites(&combined);
        match resolve_site_from_slice(&combined, q) {
            Ok(found) => return Ok(found),
            Err(err) if err.downcast_ref::<AmbiguousMatch>().is_some() => return Err(err),
            Err(_) => {}
        }
    }

    // Second chance: Graph site search is keyword-based, while the local matcher also
    // accepts substrings and URL fragments, so list broadly and match locally once more.
    let before = combined.len();
    let wildcard = graph
        .list_sites(Some("*"), SITE_RESOLVE_SEARCH_LIMIT)
        .await
        .unwrap_or_default();
    merge_sites(&mut combined, wildcard);
    if combined.len() > before {
        let _ = storage.upsert_sites(&combined[before..]);
        match resolve_site_from_slice(&combined, q) {
            Ok(found) => return Ok(found),
            Err(err) if err.downcast_ref::<AmbiguousMatch>().is_some() => return Err(err),
            Err(_) => {}
        }
    }

    // Direct lookup fallback when user passes an exact site ID
    match graph.get_site(q).await {
        Ok(direct) => {
            let _ = storage.upsert_sites(std::slice::from_ref(&direct));
            Ok(direct)
        }
        // `/sites/{display name}` answers 400 (malformed ID) or 404: the site was not found.
        Err(err) if matches!(crate::graph::graph_error_status(&err), Some(400 | 404)) => {
            // The search itself failed for a reason other than "no such thing": report that.
            if let Some(search_err) = search_error {
                if !matches!(
                    crate::graph::graph_error_status(&search_err),
                    Some(400 | 404)
                ) {
                    return Err(search_err);
                }
            }
            bail!(
                "SharePoint site not found: {q}. Run 'sharepoint sites {q}' to search, or pass the site ID or URL."
            )
        }
        Err(err) => Err(err),
    }
}

/// Result cap of the Graph site searches `resolve_site` runs.
const SITE_RESOLVE_SEARCH_LIMIT: usize = 100;

fn merge_sites(combined: &mut Vec<SharePointSite>, more: Vec<SharePointSite>) {
    for s in more {
        if !combined.iter().any(|existing| existing.id == s.id) {
            combined.push(s);
        }
    }
}

pub fn resolve_drive_from_slice(drives: &[DriveInfo], query: &str) -> Result<DriveInfo> {
    resolve_drive_from_slice_inner(drives, query, true)
}

fn resolve_drive_from_slice_inner(
    drives: &[DriveInfo],
    query: &str,
    allow_personal_alias: bool,
) -> Result<DriveInfo> {
    let q = query.trim();
    if q.is_empty() {
        bail!("Drive name or ID is empty.");
    }
    if allow_personal_alias && is_personal_drive_alias(q) {
        if let Some(personal) = drives.iter().find(|d| d.is_personal_onedrive()) {
            return Ok(personal.clone());
        }
    }
    if let Some(exact) = drives.iter().find(|d| d.id == q) {
        return Ok(exact.clone());
    }
    let q_lower = q.to_lowercase();
    let exact_name: Vec<&DriveInfo> = drives
        .iter()
        .filter(|d| {
            d.name.trim().to_lowercase() == q_lower
                || d.display_label().to_lowercase() == q_lower
        })
        .collect();
    if exact_name.len() == 1 {
        return Ok(exact_name[0].clone());
    } else if exact_name.len() > 1 {
        return Err(ambiguous_drive_error(
            q,
            format!(
                "Ambiguous drive '{q}' matches {} drives:\n{}",
                exact_name.len(),
                format_drive_candidates(&exact_name)
            ),
            &exact_name,
        ));
    }

    let prefix: Vec<&DriveInfo> = drives
        .iter()
        .filter(|d| {
            d.name.trim().to_lowercase().starts_with(&q_lower)
                || d.display_label().to_lowercase().starts_with(&q_lower)
        })
        .collect();
    if prefix.len() == 1 {
        return Ok(prefix[0].clone());
    } else if prefix.len() > 1 {
        return Err(ambiguous_drive_error(
            q,
            format!(
                "Ambiguous drive prefix '{q}' matches {} drives:\n{}",
                prefix.len(),
                format_drive_candidates(&prefix)
            ),
            &prefix,
        ));
    }

    let sub: Vec<&DriveInfo> = drives
        .iter()
        .filter(|d| {
            d.name.to_lowercase().contains(&q_lower)
                || d.display_label().to_lowercase().contains(&q_lower)
        })
        .collect();
    match sub.len() {
        1 => Ok(sub[0].clone()),
        n if n > 1 => Err(ambiguous_drive_error(
            q,
            format!(
                "Ambiguous drive '{q}' matches {n} drives:\n{}",
                format_drive_candidates(&sub)
            ),
            &sub,
        )),
        _ => bail!("Drive not found: {q}"),
    }
}

/// Resolves the target drive given optional `--drive`, optional `--site`, and active persona.
///
/// - If `drive_query` is `"me"`/`"onedrive"`/`"personal"`, resolves `/me/drive`.
/// - If `site_query` is given without `drive_query`, picks the site's default/first document library.
/// - If neither is given:
///   - `Persona::OneDrive` -> `/me/drive`
///   - `Persona::SharePoint` -> first cached/followed SharePoint library if any, else `/me/drive`.
pub async fn resolve_target_drive(
    graph: &GraphClient,
    storage: &Storage,
    drive_query: Option<&str>,
    site_query: Option<&str>,
    persona: Persona,
    offline: bool,
) -> Result<DriveInfo> {
    let dq = drive_query.map(str::trim).filter(|s| !s.is_empty());
    let sq = site_query.map(str::trim).filter(|s| !s.is_empty());

    if let Some(s_str) = sq {
        let site = if offline {
            let cached = storage.list_sites()?;
            resolve_site_from_slice(&cached, s_str)?
        } else {
            resolve_site(graph, storage, s_str, false).await?
        };
        let cached_drives = storage.list_drives(Some(&site.id))?;
        if !cached_drives.is_empty() {
            if let Some(d_str) = dq {
                match resolve_drive_from_slice_inner(&cached_drives, d_str, false) {
                    Ok(found) => return Ok(found),
                    Err(err) if err.downcast_ref::<AmbiguousMatch>().is_some() => return Err(err),
                    Err(_) if !offline => {}
                    Err(err) => return Err(err),
                }
            } else if let Some(first) = cached_drives.first() {
                return Ok(first.clone());
            }
        }
        let drives = if offline {
            cached_drives
        } else {
            let fetched = graph
                .list_site_drives(&site.id, Some(site.effective_name()))
                .await?;
            let _ = storage.upsert_drives(&fetched);
            fetched
        };
        if let Some(d_str) = dq {
            return resolve_drive_from_slice_inner(&drives, d_str, false);
        }
        return drives
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("No document libraries found for site '{}'", site.effective_name()));
    }

    if let Some(d_str) = dq {
        if is_personal_drive_alias(d_str) {
            return resolve_personal_drive(graph, storage, offline).await;
        }
    }

    if let Some(d_str) = dq {
        let cached = storage.list_drives(None)?;
        match resolve_drive_from_slice(&cached, d_str) {
            Ok(found) => return Ok(found),
            Err(err) if err.downcast_ref::<AmbiguousMatch>().is_some() => return Err(err),
            Err(_) => {}
        }
        if offline {
            bail!("Drive '{d_str}' not found in local cache.");
        }
        // Try direct drive ID fetch from Graph
        if let Ok(direct) = graph.get_drive(d_str).await {
            let _ = storage.upsert_drives(std::slice::from_ref(&direct));
            return Ok(direct);
        }
        // Fall back to checking personal drive
        if let Ok(me) = graph.get_my_drive().await {
            let _ = storage.upsert_drives(std::slice::from_ref(&me));
            if me.id == d_str || me.name.eq_ignore_ascii_case(d_str) {
                return Ok(me);
            }
        }
        bail!("Drive not found: {d_str}. Library names are matched against the local cache; pass --site <NAME_OR_ID> or run 'sharepoint drives' first.");
    }

    // Default drive selection based on persona
    if persona == Persona::OneDrive {
        return resolve_personal_drive(graph, storage, offline).await;
    }

    // SharePoint persona: prefer cached SharePoint library, else fetch followed site's library, else OneDrive
    let cached_drives = storage.list_drives(None)?;
    if let Some(sp_drive) = cached_drives.iter().find(|d| d.site_id.is_some()) {
        return Ok(sp_drive.clone());
    }
    if !offline {
        if let Ok(sites) = graph.list_followed_sites().await {
            if !sites.is_empty() {
                let _ = storage.upsert_sites(&sites);
                for site in &sites {
                    if let Ok(drives) = graph
                        .list_site_drives(&site.id, Some(site.effective_name()))
                        .await
                    {
                        if !drives.is_empty() {
                            let _ = storage.upsert_drives(&drives);
                            return Ok(drives[0].clone());
                        }
                    }
                }
            }
        }
    }
    resolve_personal_drive(graph, storage, offline).await
}

pub async fn resolve_personal_drive(
    graph: &GraphClient,
    storage: &Storage,
    offline: bool,
) -> Result<DriveInfo> {
    let cached = storage.list_drives(None)?;
    if let Some(personal) = cached.into_iter().find(|d| d.is_personal_onedrive()) {
        if offline {
            return Ok(personal);
        }
        return Ok(personal);
    }
    if offline {
        bail!("Personal OneDrive is not in local cache. Run online first.");
    }
    let drive = graph.get_my_drive().await?;
    let _ = storage.upsert_drives(std::slice::from_ref(&drive));
    Ok(drive)
}

/// Resolves a drive item strictly within `drive_id` by exact item ID or exact path
/// (`/Projects/plan.md` or `plan.md` at drive root).
///
/// Online lookups (`offline == false`) never fall back to local cache search or bare-filename
/// matching across other folders. Offline lookups (`offline == true`) match only exact ID or
/// exact path within `drive_id`.
pub async fn resolve_item(
    graph: &GraphClient,
    storage: &Storage,
    drive_id: &str,
    item_or_path: &str,
    offline: bool,
) -> Result<DriveItem> {
    let raw = item_or_path.trim();
    if raw.is_empty() {
        bail!("Item ID or path is empty.");
    }
    let looks_like_path = raw.starts_with('/') || raw.contains('/');

    if offline {
        if !looks_like_path {
            if let Some(cached) = storage.get_drive_item(drive_id, raw)? {
                return Ok(cached);
            }
        }
        if let Some(exact) = storage.get_drive_item_by_path(drive_id, raw)? {
            return Ok(exact);
        }
        bail!("Drive item not found in local cache for drive '{drive_id}': {raw}")
    }

    // Use SQLite solely as a lookup-order hint for bare root filenames (still always
    // verifying live on Graph) so `info report.md` / `rm report.md` skips a guaranteed
    // 404 on `/items/report.md` when the cache already knows `report.md` is a root path.
    let hint_root_path = !looks_like_path
        && storage.get_drive_item(drive_id, raw)?.is_none()
        && storage
            .get_drive_item_by_path(drive_id, raw)?
            .is_some_and(|cached| cached.id != raw);

    if hint_root_path {
        match graph.get_item_by_path(drive_id, raw).await {
            Ok(item) => {
                if item.drive_id == drive_id || item.effective_drive_id() == drive_id {
                    let _ = storage.upsert_drive_items(std::slice::from_ref(&item));
                    return Ok(item);
                }
            }
            Err(err) if crate::graph::is_graph_not_found_error(&err) => {}
            Err(err) => return Err(err),
        }
        match graph.get_item(drive_id, raw).await {
            Ok(item) => {
                if item.drive_id == drive_id || item.effective_drive_id() == drive_id {
                    let _ = storage.upsert_drive_items(std::slice::from_ref(&item));
                    return Ok(item);
                }
                bail!("Drive item not found in drive '{drive_id}': {raw}")
            }
            Err(err) if crate::graph::is_graph_not_found_error(&err) => {
                bail!("Drive item not found in drive '{drive_id}': {raw}")
            }
            Err(err) => return Err(err),
        }
    }

    if !looks_like_path {
        match graph.get_item(drive_id, raw).await {
            Ok(item) => {
                if item.drive_id == drive_id || item.effective_drive_id() == drive_id {
                    let _ = storage.upsert_drive_items(std::slice::from_ref(&item));
                    return Ok(item);
                }
            }
            Err(err) if crate::graph::is_graph_not_found_error(&err) => {}
            Err(err) => return Err(err),
        }
    }

    match graph.get_item_by_path(drive_id, raw).await {
        Ok(item) => {
            if item.drive_id == drive_id || item.effective_drive_id() == drive_id {
                let _ = storage.upsert_drive_items(std::slice::from_ref(&item));
                return Ok(item);
            }
            bail!("Drive item not found in drive '{drive_id}': {raw}")
        }
        Err(err) if crate::graph::is_graph_not_found_error(&err) => {
            bail!("Drive item not found in drive '{drive_id}': {raw}")
        }
        Err(err) => Err(err),
    }
}

/// Resolves a drive item for mutating operations (delete, move/rename, target folder).
/// Requires a confirmed live match by exact ID or exact path inside `drive_id`.
pub async fn resolve_item_for_write(
    graph: &GraphClient,
    storage: &Storage,
    drive_id: &str,
    item_or_path: &str,
) -> Result<DriveItem> {
    let raw = item_or_path.trim();
    if raw.is_empty() || raw == "/" || raw == "root" {
        bail!("Refusing write target '{raw}': specify an exact item ID or path inside drive '{drive_id}'.");
    }
    let item = resolve_item(graph, storage, drive_id, raw, false).await?;
    if item.drive_id != drive_id {
        bail!(
            "Resolved item '{}' belongs to drive '{}' rather than requested drive '{drive_id}'.",
            item.id,
            item.drive_id
        );
    }
    Ok(item)
}

fn format_site_candidates(candidates: &[&SharePointSite]) -> String {
    candidates
        .iter()
        .take(8)
        .map(|s| format!("  - {} ({})", s.effective_name(), s.id))
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_drive_candidates(candidates: &[&DriveInfo]) -> String {
    candidates
        .iter()
        .take(8)
        .map(|d| format!("  - {} ({})", d.display_label(), d.id))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_sites_and_drives_by_alias_id_and_prefix() {
        let sites = vec![
            SharePointSite {
                id: "s-eng".into(),
                name: "engineering".into(),
                display_name: "Engineering Hub".into(),
                ..Default::default()
            },
            SharePointSite {
                id: "s-fin".into(),
                name: "finance".into(),
                display_name: "Finance Portal".into(),
                ..Default::default()
            },
        ];
        assert_eq!(resolve_site_from_slice(&sites, "s-eng").unwrap().id, "s-eng");
        assert_eq!(resolve_site_from_slice(&sites, "eng").unwrap().id, "s-eng");
        assert_eq!(
            resolve_site_from_slice(&sites, "finance portal").unwrap().id,
            "s-fin"
        );
        assert!(resolve_site_from_slice(&sites, "missing").is_err());

        let drives = vec![
            DriveInfo {
                id: "d-me".into(),
                name: "OneDrive".into(),
                drive_type: "business".into(),
                ..Default::default()
            },
            DriveInfo {
                id: "d-docs".into(),
                name: "Shared Documents".into(),
                drive_type: "documentLibrary".into(),
                site_id: Some("s-eng".into()),
                site_name: Some("Engineering Hub".into()),
                ..Default::default()
            },
        ];
        assert_eq!(resolve_drive_from_slice(&drives, "onedrive").unwrap().id, "d-me");
        assert_eq!(resolve_drive_from_slice(&drives, "me").unwrap().id, "d-me");
        assert_eq!(resolve_drive_from_slice(&drives, "Shared").unwrap().id, "d-docs");

        let dup_drives = vec![
            DriveInfo {
                id: "d-eng-docs".into(),
                name: "Documents".into(),
                drive_type: "documentLibrary".into(),
                site_id: Some("s-eng".into()),
                site_name: Some("Engineering Hub".into()),
                ..Default::default()
            },
            DriveInfo {
                id: "d-fin-docs".into(),
                name: "Documents".into(),
                drive_type: "documentLibrary".into(),
                site_id: Some("s-fin".into()),
                site_name: Some("Finance Portal".into()),
                ..Default::default()
            },
            DriveInfo {
                id: "d-other".into(),
                name: "Archived Documents".into(),
                drive_type: "documentLibrary".into(),
                site_id: Some("s-other".into()),
                site_name: Some("Other".into()),
                ..Default::default()
            },
        ];
        let dup_err = resolve_drive_from_slice(&dup_drives, "Documents").unwrap_err();
        let amb = dup_err
            .downcast_ref::<AmbiguousMatch>()
            .expect("expected AmbiguousMatch for duplicate Documents");
        let cand_ids: Vec<&str> = amb.candidates.iter().map(|(_, id)| id.as_str()).collect();
        assert_eq!(cand_ids, vec!["d-eng-docs", "d-fin-docs"]);
        let dup_msg = format!("{dup_err:#}");
        assert!(dup_msg.contains("d-eng-docs") && dup_msg.contains("d-fin-docs"));
        assert!(!dup_msg.contains("d-other"));
        assert_eq!(
            resolve_drive_from_slice(&dup_drives, "d-fin-docs")
                .unwrap()
                .id,
            "d-fin-docs"
        );
    }

    #[tokio::test]
    async fn resolve_site_overlaps_followed_and_search_requests() {
        use axum::{routing::get, Json, Router};
        use serde_json::json;
        use std::sync::Arc;
        use tokio::{net::TcpListener, sync::Barrier};

        use crate::{
            auth::{AuthManager, StoredTokens},
            config::AppConfig,
        };

        let barrier = Arc::new(Barrier::new(2));
        let b1 = Arc::clone(&barrier);
        let b2 = Arc::clone(&barrier);

        let app = Router::new()
            .route(
                "/v1.0/me/followedSites",
                get(move || {
                    let b = Arc::clone(&b1);
                    async move {
                        b.wait().await;
                        Json(json!({ "value": [] }))
                    }
                }),
            )
            .route(
                "/v1.0/sites",
                get(move || {
                    let b = Arc::clone(&b2);
                    async move {
                        b.wait().await;
                        Json(json!({
                            "value": [{
                                "id": "site-search",
                                "name": "marketing",
                                "displayName": "Marketing Hub"
                            }]
                        }))
                    }
                }),
            );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1.0", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let cfg = AppConfig {
            graph_base_url: base,
            ..AppConfig::default()
        };
        let auth = AuthManager::from_tokens(
            cfg.clone(),
            StoredTokens {
                access_token: "test-token".into(),
                refresh_token: None,
                expires_at: chrono::Utc::now().timestamp() + 3600,
                client_id: "cid".into(),
                tenant_id: "tid".into(),
                user_id: Some("u1".into()),
                user_display_name: None,
                user_email: None,
                auth_mode: "test".into(),
            },
        );
        let graph = GraphClient::new(cfg, auth).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::open(&tmp.path().join("resolver-overlap.db")).unwrap();

        let resolved = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            resolve_site(&graph, &storage, "marketing", false),
        )
        .await
        .expect("followedSites and sites search must run concurrently")
        .unwrap();
        assert_eq!(resolved.id, "site-search");

        server.abort();
    }

    #[tokio::test]
    async fn resolve_target_drive_reuses_cached_site_drives_and_resolve_item_hints_root_path() {
        use axum::{routing::get, Json, Router};
        use serde_json::json;
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        use tokio::net::TcpListener;

        use crate::{
            auth::{AuthManager, StoredTokens},
            config::AppConfig,
        };

        let site_drives_hits = Arc::new(AtomicUsize::new(0));
        let item_by_id_hits = Arc::new(AtomicUsize::new(0));
        let item_by_path_hits = Arc::new(AtomicUsize::new(0));

        let sdh = Arc::clone(&site_drives_hits);
        let ibi = Arc::clone(&item_by_id_hits);
        let ibp = Arc::clone(&item_by_path_hits);

        let app = Router::new()
            .route(
                "/v1.0/sites/{site_id}/drives",
                get(move || {
                    let hits = Arc::clone(&sdh);
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        Json(json!({ "value": [] }))
                    }
                }),
            )
            .route(
                "/v1.0/drives/{drive_id}/items/{item_id}",
                get(move || {
                    let hits = Arc::clone(&ibi);
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        axum::http::StatusCode::NOT_FOUND
                    }
                }),
            )
            .route(
                "/v1.0/drives/{drive_id}/root:/report.md:",
                get(move || {
                    let hits = Arc::clone(&ibp);
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        Json(json!({
                            "id": "item-42",
                            "name": "report.md",
                            "size": 128,
                            "file": { "mimeType": "text/markdown" },
                            "parentReference": {
                                "driveId": "drv-eng",
                                "id": "root-id",
                                "path": "/drives/drv-eng/root:"
                            }
                        }))
                    }
                }),
            );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1.0", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let cfg = AppConfig {
            graph_base_url: base,
            ..AppConfig::default()
        };
        let auth = AuthManager::from_tokens(
            cfg.clone(),
            StoredTokens {
                access_token: "test-token".into(),
                refresh_token: None,
                expires_at: chrono::Utc::now().timestamp() + 3600,
                client_id: "cid".into(),
                tenant_id: "tid".into(),
                user_id: Some("u1".into()),
                user_display_name: None,
                user_email: None,
                auth_mode: "test".into(),
            },
        );
        let graph = GraphClient::new(cfg, auth).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::open(&tmp.path().join("resolver-cache.db")).unwrap();

        storage
            .upsert_sites(&[SharePointSite {
                id: "site-eng".into(),
                name: "engineering".into(),
                display_name: "Engineering".into(),
                ..Default::default()
            }])
            .unwrap();
        storage
            .upsert_drives(&[DriveInfo {
                id: "drv-eng".into(),
                name: "Documents".into(),
                drive_type: "documentLibrary".into(),
                site_id: Some("site-eng".into()),
                site_name: Some("Engineering".into()),
                ..Default::default()
            }])
            .unwrap();
        storage
            .upsert_drive_items(&[DriveItem {
                id: "item-42".into(),
                drive_id: "drv-eng".into(),
                parent_id: Some("root-id".into()),
                parent_path: String::new(),
                name: "report.md".into(),
                size: 128,
                ..Default::default()
            }])
            .unwrap();

        let drive = resolve_target_drive(
            &graph,
            &storage,
            None,
            Some("site-eng"),
            Persona::SharePoint,
            false,
        )
        .await
        .unwrap();
        assert_eq!(drive.id, "drv-eng");
        assert_eq!(
            site_drives_hits.load(Ordering::SeqCst),
            0,
            "--site drive resolution should reuse SQLite-cached site drives"
        );

        let item = resolve_item(&graph, &storage, "drv-eng", "report.md", false)
            .await
            .unwrap();
        assert_eq!(item.id, "item-42");
        assert_eq!(
            item_by_path_hits.load(Ordering::SeqCst),
            1,
            "bare cached root filename should verify live via /root:/report.md: first"
        );
        assert_eq!(
            item_by_id_hits.load(Ordering::SeqCst),
            0,
            "cached root filename hint should skip guaranteed 404 on /items/report.md"
        );

        server.abort();
    }
}
