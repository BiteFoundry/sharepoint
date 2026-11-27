//! Sites, drives, recent, and shared-with-me CLI handlers.

use anyhow::Result;

use crate::{
    config::AppConfig,
    models::format_human_size,
    safe_text::sanitize_terminal_inline,
};

use super::data::{discover_drives, fetch_sites, init_runtime, DRIVE_DISCOVERY_SITE_CAP};

#[allow(clippy::too_many_arguments)]
pub(super) async fn cmd_sites(
    config: &AppConfig,
    search: Option<String>,
    limit: usize,
    refresh: bool,
    offline: bool,
    cached: bool,
    as_json: bool,
) -> Result<()> {
    let (_, graph, storage) = init_runtime(config, offline).await?;
    let sites = fetch_sites(&graph, &storage, search.as_deref(), limit, refresh, offline, cached).await?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&sites)?);
    } else if sites.is_empty() {
        println!("No SharePoint sites found.");
    } else {
        for s in &sites {
            let star = if s.is_followed { "*" } else { " " };
            println!(
                "{star} {:<32}  {}  ({})",
                sanitize_terminal_inline(s.effective_name()),
                sanitize_terminal_inline(s.web_url.as_deref().unwrap_or("")),
                sanitize_terminal_inline(&s.id)
            );
        }
    }
    Ok(())
}

pub(super) async fn cmd_drives(
    config: &AppConfig,
    site: Option<String>,
    refresh: bool,
    offline: bool,
    cached: bool,
    as_json: bool,
) -> Result<()> {
    let (_, graph, storage) = init_runtime(config, offline).await?;
    let found =
        discover_drives(&graph, &storage, site.as_deref(), refresh, offline, cached)
            .await?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&found.drives)?);
    } else if found.drives.is_empty() {
        println!("No drives found.");
    } else {
        for d in &found.drives {
            let quota = match (d.quota_used, d.quota_total) {
                (Some(u), Some(t)) => {
                    format!(" [{}/{}]", format_human_size(u), format_human_size(t))
                }
                _ => String::new(),
            };
            println!(
                "{:<34}  {:<16}{}  ({})",
                sanitize_terminal_inline(&d.display_label()),
                sanitize_terminal_inline(&d.drive_type),
                quota,
                sanitize_terminal_inline(&d.id)
            );
        }
    }
    if found.has_more == Some(true) {
        eprintln!(
            "warning: scanned the first {DRIVE_DISCOVERY_SITE_CAP} SharePoint sites and more exist; pass --site <NAME_OR_ID> for a specific site."
        );
    }
    if found.sites_failed > 0 {
        eprintln!(
            "warning: could not list libraries for {} site(s).",
            found.sites_failed
        );
    }
    if found.sites_scanned.is_some() && found.has_more.is_none() {
        eprintln!(
            "warning: SharePoint site discovery failed; only drives that could be listed are shown."
        );
    }
    Ok(())
}

pub(super) async fn cmd_recent(
    config: &AppConfig,
    limit: usize,
    offline: bool,
    cached: bool,
    as_json: bool,
) -> Result<()> {
    let (_, graph, storage) = init_runtime(config, offline).await?;
    let items = if limit == 0 {
        Vec::new()
    } else if offline || cached {
        let c = storage.list_recent_items(limit)?;
        if !c.is_empty() || offline {
            c
        } else {
            let live = graph.list_recent_items(limit).await?;
            let _ = storage.replace_recent_items(&live);
            live
        }
    } else {
        let live = graph.list_recent_items(limit).await?;
        let _ = storage.replace_recent_items(&live);
        live
    };
    if as_json {
        println!("{}", serde_json::to_string_pretty(&items)?);
    } else if items.is_empty() {
        println!("No recent files found.");
    } else {
        for item in &items {
            println!(
                "[{:<4}] {:<36} {:>10}  {}  ({})",
                item.type_badge(),
                sanitize_terminal_inline(&item.name),
                format_human_size(item.size),
                sanitize_terminal_inline(
                    item.last_modified_at
                        .as_deref()
                        .map(|s| s.get(..16).unwrap_or(s))
                        .unwrap_or("")
                ),
                sanitize_terminal_inline(&item.id)
            );
        }
    }
    Ok(())
}

pub(super) async fn cmd_shared(
    config: &AppConfig,
    limit: usize,
    offline: bool,
    cached: bool,
    as_json: bool,
) -> Result<()> {
    let (_, graph, storage) = init_runtime(config, offline).await?;
    let items = if limit == 0 {
        Vec::new()
    } else if offline || cached {
        let c = storage.list_shared_items(limit)?;
        if !c.is_empty() || offline {
            c
        } else {
            let live = graph.list_shared_with_me(limit).await?;
            let _ = storage.replace_shared_items(&live);
            live
        }
    } else {
        let live = graph.list_shared_with_me(limit).await?;
        let _ = storage.replace_shared_items(&live);
        live
    };
    if as_json {
        println!("{}", serde_json::to_string_pretty(&items)?);
    } else if items.is_empty() {
        println!("No shared files found.");
    } else {
        for item in &items {
            println!(
                "[{:<4}] {:<36} {:>10}  {}  ({})",
                item.type_badge(),
                sanitize_terminal_inline(&item.name),
                format_human_size(item.size),
                sanitize_terminal_inline(item.last_modified_by.as_deref().unwrap_or("")),
                sanitize_terminal_inline(&item.id)
            );
        }
    }
    Ok(())
}
