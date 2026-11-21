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
        bail!(
            "Ambiguous SharePoint site '{q}' matches multiple sites:\n{}",
            format_site_candidates(&exact_name)
        );
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
        bail!(
            "Ambiguous SharePoint site prefix '{q}' matches multiple sites:\n{}",
            format_site_candidates(&prefix)
        );
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
        n if n > 1 => bail!(
            "Ambiguous SharePoint site '{q}' matches {n} sites:\n{}",
            format_site_candidates(&sub)
        ),
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

    // Fetch followed + discovered sites
    let mut combined = graph.list_followed_sites().await.unwrap_or_default();
    let discovered = graph.list_sites(Some("*"), 100).await.unwrap_or_default();
    for s in discovered {
        if !combined.iter().any(|existing| existing.id == s.id) {
            combined.push(s);
        }
    }
    if !combined.is_empty() {
        let _ = storage.upsert_sites(&combined);
        if let Ok(found) = resolve_site_from_slice(&combined, q) {
            return Ok(found);
        }
    }
    // Direct lookup fallback when user passes an exact site ID
    let direct = graph.get_site(q).await?;
    let _ = storage.upsert_sites(std::slice::from_ref(&direct));
    Ok(direct)
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
        n if n > 1 => bail!(
            "Ambiguous drive '{q}' matches {n} drives:\n{}",
            format_drive_candidates(&sub)
        ),
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
        let drives = if offline {
            storage.list_drives(Some(&site.id))?
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
        if let Ok(found) = resolve_drive_from_slice(&cached, d_str) {
            return Ok(found);
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
        bail!("Drive not found: {d_str}");
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
    }
}
