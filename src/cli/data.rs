//! Shared runtime initialization, cached data fetchers, upload baselines, and tree builder.

use std::path::Path;

use anyhow::{Context, Result};
use futures_util::StreamExt;

use crate::{
    auth::AuthManager,
    config::{self, AppConfig},
    graph::{
        drives::{ConflictBehavior, UploadOptions},
        GraphClient,
    },
    models::{DownloadBaseline, DownloadedFile, DriveInfo, DriveItem, SharePointSite, TreeNode},
    resolver,
    storage::Storage,
};

use super::format::FolderListing;

pub async fn init_runtime(
    config: &AppConfig,
    allow_unauthenticated: bool,
) -> Result<(AuthManager, GraphClient, Storage)> {
    let auth = AuthManager::new(config.clone())?;
    let tokens = auth.current_tokens().await;
    if tokens.is_none() && !allow_unauthenticated {
        let _ = auth.valid_access_token().await?;
    }
    let user_id = auth
        .current_tokens()
        .await
        .and_then(|t| t.user_id)
        .filter(|s| !s.trim().is_empty());
    let mut graph = GraphClient::new(config.clone(), auth.clone())?;
    if let Some(ref uid) = user_id {
        graph = graph.with_my_user_id(uid.clone());
    }
    let db = config::db_path(user_id.as_deref());
    let storage = Storage::open(&db)?;
    Ok((auth, graph, storage))
}

pub async fn fetch_sites(
    graph: &GraphClient,
    storage: &Storage,
    search: Option<&str>,
    limit: usize,
    refresh: bool,
    offline: bool,
    cached: bool,
) -> Result<Vec<SharePointSite>> {
    let clean_search = search
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "*");
    if !refresh && (offline || cached) {
        let local = storage.list_sites()?;
        let filtered: Vec<SharePointSite> = if let Some(q) = clean_search {
            let ql = q.to_lowercase();
            local
                .into_iter()
                .filter(|s| {
                    s.name.to_lowercase().contains(&ql)
                        || s.display_name.to_lowercase().contains(&ql)
                        || s.web_url
                            .as_deref()
                            .is_some_and(|u| u.to_lowercase().contains(&ql))
                        || s.id.to_lowercase().contains(&ql)
                })
                .collect()
        } else {
            local
        };
        if !filtered.is_empty() || offline {
            return Ok(filtered.into_iter().take(limit).collect());
        }
    }
    let (authoritative_follow, mut combined, discovered) = if clean_search.is_none() {
        let (followed_res, discovered_res) =
            tokio::join!(graph.list_followed_sites(), graph.list_sites(search, limit));
        let (auth_follow, followed) = match followed_res {
            Ok(followed) => (true, followed),
            Err(_) => (false, Vec::new()),
        };
        (auth_follow, followed, discovered_res?)
    } else {
        (false, Vec::new(), graph.list_sites(search, limit).await?)
    };
    for s in discovered {
        if !combined.iter().any(|existing| existing.id == s.id) {
            combined.push(s);
        }
    }
    if authoritative_follow {
        let _ = storage.upsert_sites_with_follow_state(&combined);
    } else {
        let _ = storage.upsert_sites(&combined);
    }
    Ok(combined.into_iter().take(limit).collect())
}

pub(super) const DRIVE_DISCOVERY_SITE_CAP: usize = 200;

pub struct DriveDiscovery {
    pub drives: Vec<DriveInfo>,
    pub sites_scanned: Option<usize>,
    pub sites_failed: usize,
    pub has_more: Option<bool>,
}

pub async fn fetch_drives(
    graph: &GraphClient,
    storage: &Storage,
    site_query: Option<&str>,
    refresh: bool,
    offline: bool,
    cached: bool,
) -> Result<Vec<DriveInfo>> {
    Ok(discover_drives(graph, storage, site_query, refresh, offline, cached)
        .await?
        .drives)
}

pub async fn discover_drives(
    graph: &GraphClient,
    storage: &Storage,
    site_query: Option<&str>,
    refresh: bool,
    offline: bool,
    cached: bool,
) -> Result<DriveDiscovery> {
    if let Some(sq) = site_query.map(str::trim).filter(|s| !s.is_empty()) {
        let site = if offline {
            let sites = storage.list_sites()?;
            resolver::resolve_site_from_slice(&sites, sq)?
        } else {
            resolver::resolve_site(graph, storage, sq, cached && !refresh).await?
        };
        if !refresh && (offline || cached) {
            let local = storage.list_drives(Some(&site.id))?;
            if !local.is_empty() || offline {
                return Ok(DriveDiscovery {
                    drives: local,
                    sites_scanned: None,
                    sites_failed: 0,
                    has_more: None,
                });
            }
        }
        let drives = graph
            .list_site_drives(&site.id, Some(site.effective_name()))
            .await?;
        let _ = storage.upsert_drives(&drives);
        return Ok(DriveDiscovery {
            drives,
            sites_scanned: Some(1),
            sites_failed: 0,
            has_more: Some(false),
        });
    }

    if !refresh && (offline || cached) {
        let local = storage.list_drives(None)?;
        if !local.is_empty() || offline {
            return Ok(DriveDiscovery {
                drives: local,
                sites_scanned: None,
                sites_failed: 0,
                has_more: None,
            });
        }
    }

    let mut all = Vec::new();
    let (me_res, listed) = tokio::join!(
        graph.get_my_drive(),
        fetch_sites(
            graph,
            storage,
            None,
            DRIVE_DISCOVERY_SITE_CAP + 1,
            false,
            false,
            false,
        ),
    );
    if let Ok(me) = me_res {
        all.push(me);
    }
    let (sites, has_more) = match listed {
        Ok(mut sites) => {
            let more = sites.len() > DRIVE_DISCOVERY_SITE_CAP;
            sites.truncate(DRIVE_DISCOVERY_SITE_CAP);
            (sites, Some(more))
        }
        Err(_) => (Vec::new(), None),
    };
    let site_targets: Vec<(String, String)> = sites
        .iter()
        .map(|s| (s.id.clone(), s.effective_name().to_string()))
        .collect();
    let results: Vec<_> = futures_util::stream::iter(site_targets)
        .map(|(id, name)| async move { graph.list_site_drives(&id, Some(&name)).await })
        .buffered(8)
        .collect()
        .await;
    let mut sites_failed = 0;
    for result in results {
        match result {
            Ok(site_drives) => {
                for d in site_drives {
                    if !all.iter().any(|existing| existing.id == d.id) {
                        all.push(d);
                    }
                }
            }
            Err(_) => sites_failed += 1,
        }
    }
    let _ = storage.upsert_drives(&all);
    Ok(DriveDiscovery {
        drives: all,
        sites_scanned: Some(sites.len()),
        sites_failed,
        has_more,
    })
}

#[allow(clippy::too_many_arguments)]
pub async fn fetch_folder_items(
    graph: &GraphClient,
    storage: &Storage,
    drive_id: &str,
    path_or_id: Option<&str>,
    limit: usize,
    refresh: bool,
    offline: bool,
    cached: bool,
) -> Result<Vec<DriveItem>> {
    Ok(
        fetch_folder_listing(graph, storage, drive_id, path_or_id, limit, refresh, offline, cached)
            .await?
            .items,
    )
}

#[allow(clippy::too_many_arguments)]
pub async fn fetch_folder_listing(
    graph: &GraphClient,
    storage: &Storage,
    drive_id: &str,
    path_or_id: Option<&str>,
    limit: usize,
    refresh: bool,
    offline: bool,
    cached: bool,
) -> Result<FolderListing> {
    if limit == 0 {
        return Ok(FolderListing {
            items: Vec::new(),
            has_more: None,
            source: if !refresh && (offline || cached) {
                "cache"
            } else {
                "remote"
            },
        });
    }
    let target = path_or_id
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "/" && *s != "root");

    if !refresh && (offline || cached) {
        let probe_limit = limit.saturating_add(1);
        let mut local = match target {
            None => storage.list_folder_children(drive_id, None, Some(""), probe_limit)?,
            Some(p) if p.starts_with('/') || p.contains('/') => {
                let norm = format!("/{}", p.trim_matches('/'));
                let by_path =
                    storage.list_folder_children(drive_id, None, Some(&norm), probe_limit)?;
                if !by_path.is_empty() {
                    by_path
                } else if let Some(folder) = storage.get_drive_item_by_path(drive_id, &norm)? {
                    storage.list_folder_children(drive_id, Some(&folder.id), None, probe_limit)?
                } else {
                    Vec::new()
                }
            }
            Some(id) => {
                let by_id = storage.list_folder_children(drive_id, Some(id), None, probe_limit)?;
                if !by_id.is_empty() {
                    by_id
                } else {
                    let norm = format!("/{id}");
                    let by_path =
                        storage.list_folder_children(drive_id, None, Some(&norm), probe_limit)?;
                    if !by_path.is_empty() {
                        by_path
                    } else if let Some(folder) = storage.get_drive_item_by_path(drive_id, &norm)? {
                        storage.list_folder_children(
                            drive_id,
                            Some(&folder.id),
                            None,
                            probe_limit,
                        )?
                    } else {
                        Vec::new()
                    }
                }
            }
        };
        if !local.is_empty() || offline {
            let has_more = local.len() > limit;
            local.truncate(limit);
            return Ok(FolderListing {
                items: local,
                has_more: Some(has_more),
                source: "cache",
            });
        }
    }

    let (items, complete) = match target {
        None => {
            let page = graph
                .list_children_detailed(drive_id, None, Some(limit))
                .await?;
            if page.complete {
                let _ = storage.replace_folder_children(drive_id, None, &page.items);
            } else {
                let _ = storage.upsert_drive_items(&page.items);
            }
            (page.items, page.complete)
        }
        Some(p) if p.starts_with('/') || p.contains('/') => {
            let page = graph
                .list_children_by_path_detailed(drive_id, p, Some(limit))
                .await?;
            if page.complete {
                let _ = storage.replace_folder_children_by_path(drive_id, p, &page.items);
            } else {
                let _ = storage.upsert_drive_items(&page.items);
            }
            (page.items, page.complete)
        }
        Some(id_or_name) => {
            // Try direct item ID children first, fall back to path-based children
            match graph
                .list_children_detailed(drive_id, Some(id_or_name), Some(limit))
                .await
            {
                Ok(page) => {
                    if page.complete {
                        let _ = storage.replace_folder_children(
                            drive_id,
                            Some(id_or_name),
                            &page.items,
                        );
                    } else {
                        let _ = storage.upsert_drive_items(&page.items);
                    }
                    (page.items, page.complete)
                }
                Err(err) if crate::graph::is_graph_not_found_error(&err) => {
                    let page = graph
                        .list_children_by_path_detailed(drive_id, id_or_name, Some(limit))
                        .await?;
                    if page.complete {
                        let _ = storage.replace_folder_children_by_path(
                            drive_id,
                            id_or_name,
                            &page.items,
                        );
                    } else {
                        let _ = storage.upsert_drive_items(&page.items);
                    }
                    (page.items, page.complete)
                }
                Err(err) => return Err(err),
            }
        }
    };
    Ok(FolderListing {
        items,
        has_more: Some(!complete),
        source: "remote",
    })
}

pub fn record_download_baseline(
    storage: &Storage,
    item: &DriveItem,
    downloaded: &mut DownloadedFile,
) {
    downloaded.drive_id = Some(item.effective_drive_id().to_string());
    downloaded.item_id = Some(item.effective_item_id().to_string());
    downloaded.e_tag = item.e_tag.clone();
    downloaded.c_tag = item.c_tag.clone();
    let baseline = DownloadBaseline {
        local_path: crate::storage::drives::normalize_local_baseline_path(&downloaded.path),
        drive_id: item.effective_drive_id().to_string(),
        item_id: item.effective_item_id().to_string(),
        parent_id: item.parent_id.clone(),
        parent_path: item.parent_path.clone(),
        remote_name: item.name.clone(),
        e_tag: item.e_tag.clone(),
        c_tag: item.c_tag.clone(),
        last_modified_at: item.last_modified_at.clone(),
        downloaded_at: chrono::Utc::now().to_rfc3339(),
    };
    let _ = storage.upsert_download_baseline(&baseline);
}

#[allow(clippy::too_many_arguments)]
pub fn resolve_upload_options_for_local_file(
    storage: &Storage,
    drive_id: &str,
    parent_id: Option<&str>,
    parent_path: Option<&str>,
    remote_name_override: Option<&str>,
    local_path: &Path,
    explicit_if_match: Option<&str>,
    explicit_conflict: Option<ConflictBehavior>,
) -> UploadOptions {
    let target_name = remote_name_override
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            local_path
                .file_name()
                .and_then(|s| s.to_str())
                .map(str::to_string)
        });
    let matched_baseline = target_name.as_deref().and_then(|name| {
        storage
            .get_download_baseline(local_path)
            .ok()
            .flatten()
            .filter(|b| {
                b.drive_id == drive_id
                    && b.remote_name.eq_ignore_ascii_case(name)
                    && upload_parent_matches_baseline(b, parent_id, parent_path)
            })
    });

    let clean_if_match = explicit_if_match
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if clean_if_match.is_some() {
        return UploadOptions {
            conflict_behavior: explicit_conflict,
            if_match: clean_if_match,
            expected_c_tag: None,
            target_item_id: matched_baseline.map(|b| b.item_id),
        };
    }
    if let Some(conflict) = explicit_conflict {
        return UploadOptions {
            conflict_behavior: Some(conflict),
            if_match: None,
            expected_c_tag: None,
            target_item_id: None,
        };
    }

    if let Some(baseline) = matched_baseline {
        let e_tag = baseline
            .e_tag
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let c_tag = baseline
            .c_tag
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if e_tag.is_some() || c_tag.is_some() {
            return UploadOptions {
                conflict_behavior: None,
                if_match: e_tag,
                expected_c_tag: c_tag,
                target_item_id: Some(baseline.item_id),
            };
        }
    }

    UploadOptions::default()
}

pub fn refresh_download_baseline_after_upload(
    storage: &Storage,
    local_path: &Path,
    uploaded: &DriveItem,
    _used_options: &UploadOptions,
) {
    let baseline = DownloadBaseline {
        local_path: crate::storage::drives::normalize_local_baseline_path(local_path),
        drive_id: uploaded.effective_drive_id().to_string(),
        item_id: uploaded.effective_item_id().to_string(),
        parent_id: uploaded.parent_id.clone(),
        parent_path: uploaded.parent_path.clone(),
        remote_name: uploaded.name.clone(),
        e_tag: uploaded.e_tag.clone(),
        c_tag: uploaded.c_tag.clone(),
        last_modified_at: uploaded.last_modified_at.clone(),
        downloaded_at: chrono::Utc::now().to_rfc3339(),
    };
    let _ = storage.update_download_baseline_if_same_item(&baseline);
}

fn upload_parent_matches_baseline(
    baseline: &DownloadBaseline,
    target_parent_id: Option<&str>,
    target_parent_path: Option<&str>,
) -> bool {
    fn norm_pid(p: Option<&str>) -> Option<&str> {
        p.map(str::trim).filter(|s| !s.is_empty() && *s != "root")
    }
    fn norm_pp(p: &str) -> String {
        let t = p.trim().trim_end_matches('/');
        if t.is_empty() || t == "/" {
            String::new()
        } else if t.starts_with('/') {
            t.to_string()
        } else {
            format!("/{t}")
        }
    }

    let t_pid = norm_pid(target_parent_id);
    let b_pid = norm_pid(baseline.parent_id.as_deref());
    let b_pp = norm_pp(&baseline.parent_path);
    let t_pp = target_parent_path.map(norm_pp);

    match (t_pid, b_pid) {
        (None, None) => b_pp.is_empty(),
        (None, Some(_)) => b_pp.is_empty() && t_pp.as_deref().unwrap_or("").is_empty(),
        (Some(tp), Some(bp)) if tp == bp => true,
        _ => t_pp
            .as_deref()
            .is_some_and(|tp| tp.eq_ignore_ascii_case(&b_pp)),
    }
}

pub async fn search_items_combined(
    graph: &GraphClient,
    storage: &Storage,
    query: &str,
    drive_id: Option<&str>,
    remote: bool,
    limit: usize,
) -> Result<Vec<DriveItem>> {
    if query.trim().is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    if !remote {
        return Ok(storage
            .search_items(query, drive_id, limit)?
            .into_iter()
            .map(|h| h.item)
            .collect());
    }

    let remote_items = if let Some(did) = drive_id {
        graph.search_drive(did, query, limit).await?
    } else {
        match graph.search_across_sites(query, limit).await {
            Ok(items) => items,
            Err(cross_err) => {
                // Fallback to personal drive search when `/search/query` is unavailable
                let me = graph.get_my_drive().await.with_context(|| {
                    format!(
                        "Cross-site search failed ({cross_err:#}); personal drive fallback failed"
                    )
                })?;
                graph
                    .search_drive(&me.id, query, limit)
                    .await
                    .with_context(|| {
                        format!(
                            "Cross-site search failed ({cross_err:#}); personal drive search fallback failed"
                        )
                    })?
            }
        }
    };

    let _ = storage.upsert_drive_items(&remote_items);

    let mut results = Vec::with_capacity(limit.min(remote_items.len()));
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    for item in remote_items {
        if results.len() >= limit {
            break;
        }
        if seen.insert((item.drive_id.clone(), item.id.clone())) {
            results.push(item);
        }
    }

    if results.len() < limit {
        let local_items = storage.search_items(query, drive_id, limit.saturating_add(seen.len()))?;
        for hit in local_items {
            if results.len() >= limit {
                break;
            }
            if seen.insert((hit.item.drive_id.clone(), hit.item.id.clone())) {
                results.push(hit.item);
            }
        }
    }

    Ok(results)
}

const TREE_FETCH_CONCURRENCY: usize = 4;

pub(super) async fn build_tree(
    graph: &GraphClient,
    storage: &Storage,
    drive_id: &str,
    parent_id: Option<&str>,
    depth: usize,
) -> Result<(Vec<TreeNode>, bool)> {
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(TREE_FETCH_CONCURRENCY));
    build_tree_inner(graph, storage, drive_id, parent_id, depth, sem).await
}

/// Resolves pre-PATCH folder path when needed (so `upsert_drive_item_after_move` can rewrite
/// cached descendant paths even when the target folder row was not yet cached), executes
/// `rename_or_move_item`, and updates the SQLite cache atomically.
pub async fn rename_or_move_item_cached(
    graph: &GraphClient,
    storage: &Storage,
    drive_id: &str,
    item_id: &str,
    known_pre_patch_item: Option<&DriveItem>,
    new_name: Option<&str>,
    new_parent_id: Option<&str>,
) -> Result<DriveItem> {
    let pre_patch_folder_path = match known_pre_patch_item {
        Some(item) => item.is_folder.then(|| item.display_path()),
        None => {
            let folder_cached = storage
                .get_drive_item(drive_id, item_id)
                .ok()
                .flatten()
                .is_some();
            if !folder_cached
                && !storage
                    .has_cached_child_for_parent(drive_id, item_id)
                    .unwrap_or(false)
                && storage.has_nested_drive_items(drive_id).unwrap_or(false)
            {
                graph
                    .get_item(drive_id, item_id)
                    .await
                    .ok()
                    .filter(|existing| existing.is_folder)
                    .map(|existing| existing.display_path())
            } else {
                None
            }
        }
    };
    let updated = graph
        .rename_or_move_item(drive_id, item_id, new_name, new_parent_id)
        .await?;
    let _ = storage.upsert_drive_item_after_move(&updated, pre_patch_folder_path.as_deref());
    Ok(updated)
}

type BuildTreeFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<(Vec<TreeNode>, bool)>> + Send + 'a>>;

fn build_tree_inner<'a>(
    graph: &'a GraphClient,
    storage: &'a Storage,
    drive_id: &'a str,
    parent_id: Option<&'a str>,
    depth: usize,
    sem: std::sync::Arc<tokio::sync::Semaphore>,
) -> BuildTreeFuture<'a> {
    Box::pin(async move {
        let page = {
            let _permit = sem
                .acquire()
                .await
                .map_err(|_| anyhow::anyhow!("tree concurrency semaphore closed"))?;
            graph
                .list_children_detailed(drive_id, parent_id, Some(200))
                .await?
        };
        if page.complete {
            let _ = storage.replace_folder_children(drive_id, parent_id, &page.items);
        } else {
            let _ = storage.upsert_drive_items(&page.items);
        }
        let cut = !page.complete;
        let items = page.items;
        if depth <= 1 {
            let nodes = items
                .into_iter()
                .map(|item| {
                    let has_more = if item.is_folder {
                        item.child_count.map(|count| count > 0)
                    } else {
                        Some(false)
                    };
                    TreeNode {
                        item,
                        children: Vec::new(),
                        has_more,
                    }
                })
                .collect();
            return Ok((nodes, cut));
        }

        let results: Vec<Result<TreeNode>> = futures_util::stream::iter(items)
            .map(|item| {
                let sem = std::sync::Arc::clone(&sem);
                async move {
                    if item.is_folder {
                        // A shortcut / shared folder (`remoteItem`) lists its children in the
                        // drive it points into, under its remote id.
                        let (children, child_cut) = build_tree_inner(
                            graph,
                            storage,
                            item.effective_drive_id(),
                            Some(item.effective_item_id()),
                            depth - 1,
                            sem,
                        )
                        .await
                        .with_context(|| {
                            format!(
                                "Failed to list folder '{}' (id: {})",
                                item.display_path(),
                                item.id
                            )
                        })?;
                        Ok(TreeNode {
                            item,
                            children,
                            has_more: Some(child_cut),
                        })
                    } else {
                        Ok(TreeNode {
                            item,
                            children: Vec::new(),
                            has_more: Some(false),
                        })
                    }
                }
            })
            .buffered(TREE_FETCH_CONCURRENCY)
            .collect()
            .await;

        let mut nodes = Vec::with_capacity(results.len());
        for res in results {
            nodes.push(res?);
        }
        Ok((nodes, cut))
    })
}
