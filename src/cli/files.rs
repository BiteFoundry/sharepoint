//! File and folder CLI command handlers (ls, tree, info, cat, download, upload, mkdir, mv, rm, search, share, preview).

use std::{
    io::{self, Write},
    path::PathBuf,
};

use anyhow::{bail, Context, Result};
use serde_json::json;

use crate::{
    config::{AppConfig, Persona},
    graph::{
        drives::{explicit_if_match, ConflictBehavior},
        media::decode_text_file,
    },
    models::format_human_size,
    resolver,
    safe_text::{sanitize_terminal_inline, sanitize_terminal_text},
};

use super::{
    args::DELETE_JSON_REQUIRES_YES,
    data::{
        build_tree, fetch_folder_listing, init_runtime, record_download_baseline,
        refresh_download_baseline_after_upload, rename_or_move_item_cached,
        resolve_upload_options_for_local_file, search_items_combined,
    },
    format::{
        file_info_json, folder_listing_json, print_tree_nodes, render_image_to_stdout,
        wrap_graph_permission_error, DetailHint,
    },
};

#[allow(clippy::too_many_arguments)]
pub(super) async fn cmd_ls(
    config: &AppConfig,
    persona: Persona,
    path_or_id: Option<String>,
    drive: Option<String>,
    site: Option<String>,
    limit: usize,
    refresh: bool,
    offline: bool,
    cached: bool,
    as_json: bool,
    compact: bool,
) -> Result<()> {
    let (_, graph, storage) = init_runtime(config, offline).await?;
    let target_drive = resolver::resolve_target_drive(
        &graph,
        &storage,
        drive.as_deref(),
        site.as_deref(),
        persona,
        offline,
    )
    .await?;
    let listing = fetch_folder_listing(
        &graph,
        &storage,
        &target_drive.id,
        path_or_id.as_deref(),
        limit,
        refresh,
        offline,
        cached,
    )
    .await?;
    if as_json {
        let mut output = folder_listing_json(&target_drive, &listing, compact);
        output["path"] = json!(path_or_id.as_deref().unwrap_or("/"));
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!(
            "{} ({})",
            sanitize_terminal_inline(&target_drive.display_label()),
            sanitize_terminal_inline(path_or_id.as_deref().unwrap_or("/"))
        );
        if listing.items.is_empty() {
            println!("  (empty)");
        } else {
            for item in &listing.items {
                let size = if item.is_folder {
                    format!(
                        "{} items",
                        item.child_count.map(|c| c.to_string()).unwrap_or_else(|| "-".into())
                    )
                } else {
                    format_human_size(item.size)
                };
                let modified = item
                    .last_modified_at
                    .as_deref()
                    .map(|s| s.get(..16).unwrap_or(s))
                    .unwrap_or("");
                println!(
                    "  [{:<4}] {:<38} {:>10}  {}  ({})",
                    item.type_badge(),
                    sanitize_terminal_inline(&item.name),
                    size,
                    sanitize_terminal_inline(modified),
                    sanitize_terminal_inline(&item.id)
                );
            }
        }
        if listing.has_more == Some(true) {
            eprintln!("More items exist; rerun with a larger --limit (now {limit}).");
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn cmd_tree(
    config: &AppConfig,
    persona: Persona,
    path_or_id: Option<String>,
    drive: Option<String>,
    site: Option<String>,
    depth: u8,
    as_json: bool,
) -> Result<()> {
    let (_, graph, storage) = init_runtime(config, false).await?;
    let target_drive = resolver::resolve_target_drive(
        &graph,
        &storage,
        drive.as_deref(),
        site.as_deref(),
        persona,
        false,
    )
    .await?;
    // A shortcut / shared folder (`remoteItem`) is listed in the drive it points into.
    let (tree_drive_id, parent_id) = match path_or_id.as_deref().map(str::trim).filter(|s| !s.is_empty() && *s != "/") {
        Some(p) => {
            let item = resolver::resolve_item(&graph, &storage, &target_drive.id, p, false).await?;
            (
                item.effective_drive_id().to_string(),
                Some(item.effective_item_id().to_string()),
            )
        }
        None => (target_drive.id.clone(), None),
    };
    let (nodes, root_cut) = build_tree(
        &graph,
        &storage,
        &tree_drive_id,
        parent_id.as_deref(),
        usize::from(depth),
    )
    .await?;
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "drive": target_drive,
                "tree": nodes,
                "has_more": root_cut,
            }))?
        );
    } else {
        println!("{}", sanitize_terminal_inline(&target_drive.display_label()));
        print_tree_nodes(&nodes, "");
        if root_cut {
            println!("… more items not shown (200 per folder)");
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn cmd_info(
    config: &AppConfig,
    persona: Persona,
    item_or_path: String,
    drive: Option<String>,
    site: Option<String>,
    versions: bool,
    versions_limit: Option<usize>,
    offline: bool,
    as_json: bool,
    full: bool,
) -> Result<()> {
    let (_, graph, storage) = init_runtime(config, offline).await?;
    let target_drive = resolver::resolve_target_drive(
        &graph,
        &storage,
        drive.as_deref(),
        site.as_deref(),
        persona,
        offline,
    )
    .await?;
    let item =
        resolver::resolve_item(&graph, &storage, &target_drive.id, &item_or_path, offline)
            .await?;
    let vers = if versions {
        match graph
            .list_item_versions_page(
                item.effective_drive_id(),
                item.effective_item_id(),
                versions_limit,
            )
            .await
        {
            Ok(page) => Some(page),
            Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
        }
    } else {
        None
    };
    if as_json {
        let mut output = file_info_json(&item, full, DetailHint::Cli, vers.is_some());
        if let Some(page) = &vers {
            output["versions"] = json!(page.versions);
            output["versions_has_more"] = json!(page.has_more);
        }
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("Name:      {}", sanitize_terminal_inline(&item.name));
        println!("ID:        {}", sanitize_terminal_inline(&item.id));
        println!("Drive:     {}", sanitize_terminal_inline(&item.drive_id));
        println!("Path:      {}", sanitize_terminal_inline(&item.display_path()));
        println!(
            "Type:      {}",
            if item.is_folder {
                std::borrow::Cow::Borrowed("Folder")
            } else {
                sanitize_terminal_inline(item.mime_type.as_deref().unwrap_or("File"))
            }
        );
        println!("Size:      {} ({} bytes)", format_human_size(item.size), item.size);
        if let Some(m) = &item.last_modified_at {
            println!("Modified:  {}", sanitize_terminal_inline(m));
        }
        if let Some(a) = &item.last_modified_by {
            println!("Author:    {}", sanitize_terminal_inline(a));
        }
        if let Some(s) = &item.shared_scope {
            println!("Sharing:   {}", sanitize_terminal_inline(s));
        }
        if let Some(u) = &item.web_url {
            println!("Web URL:   {}", sanitize_terminal_inline(u));
        }
        if let Some(page) = &vers {
            if !page.versions.is_empty() {
                println!("Versions ({}):", page.versions.len());
                for v in &page.versions {
                    println!(
                        "  v{}  {}  {}  {}",
                        sanitize_terminal_inline(&v.id),
                        format_human_size(v.size),
                        sanitize_terminal_inline(v.last_modified_at.as_deref().unwrap_or("")),
                        sanitize_terminal_inline(v.last_modified_by.as_deref().unwrap_or(""))
                    );
                }
            }
            if page.has_more == Some(true) {
                println!("  (more versions exist; raise --versions-limit)");
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn cmd_cat(
    config: &AppConfig,
    persona: Persona,
    item_or_path: String,
    drive: Option<String>,
    site: Option<String>,
    max_bytes: usize,
    as_json: bool,
) -> Result<()> {
    let (_, graph, storage) = init_runtime(config, false).await?;
    let target_drive = resolver::resolve_target_drive(
        &graph,
        &storage,
        drive.as_deref(),
        site.as_deref(),
        persona,
        false,
    )
    .await?;
    let item =
        resolver::resolve_item(&graph, &storage, &target_drive.id, &item_or_path, false)
            .await?;
    if item.is_folder {
        bail!("'{}' is a folder, not a file.", item.name);
    }
    let bytes = graph
        .download_item_bytes(
            item.effective_drive_id(),
            item.effective_item_id(),
            item.download_url.as_deref(),
            max_bytes,
        )
        .await?;
    let text = decode_text_file(&bytes)?;
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "id": item.id,
                "drive_id": item.drive_id,
                "name": item.name,
                "mime_type": item.mime_type,
                "e_tag": item.e_tag,
                "c_tag": item.c_tag,
                "size_bytes": bytes.len(),
                "content": text,
            }))?
        );
    } else {
        let safe_text = sanitize_terminal_text(&text);
        io::stdout().lock().write_all(safe_text.as_bytes())?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn cmd_download(
    config: &AppConfig,
    persona: Persona,
    item_or_path: String,
    drive: Option<String>,
    site: Option<String>,
    dir: Option<PathBuf>,
    as_json: bool,
) -> Result<()> {
    let (_, graph, storage) = init_runtime(config, false).await?;
    let target_drive = resolver::resolve_target_drive(
        &graph,
        &storage,
        drive.as_deref(),
        site.as_deref(),
        persona,
        false,
    )
    .await?;
    let item =
        resolver::resolve_item(&graph, &storage, &target_drive.id, &item_or_path, false)
            .await?;
    if item.is_folder {
        bail!("Cannot download folder '{}'; select a file.", item.name);
    }
    let dest_dir = dir.unwrap_or_else(|| config.resolved_download_dir());
    let mut downloaded = graph
        .download_item_to_file(
            item.effective_drive_id(),
            item.effective_item_id(),
            Some(&item.name),
            item.mime_type.as_deref(),
            item.download_url.as_deref(),
            &dest_dir,
        )
        .await?;
    record_download_baseline(&storage, &item, &mut downloaded);
    if as_json {
        println!("{}", serde_json::to_string_pretty(&downloaded)?);
    } else {
        println!(
            "Downloaded {} ({}) -> {}",
            sanitize_terminal_inline(&downloaded.file_name),
            format_human_size(downloaded.size_bytes),
            sanitize_terminal_inline(&downloaded.path.display().to_string())
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn cmd_upload(
    config: &AppConfig,
    persona: Persona,
    local_path: PathBuf,
    dest_folder: Option<String>,
    drive: Option<String>,
    site: Option<String>,
    name: Option<String>,
    item_id: Option<String>,
    if_match: Option<String>,
    conflict: Option<String>,
    as_json: bool,
) -> Result<()> {
    let if_match = explicit_if_match(if_match.as_deref())?;
    let (_, graph, storage) = init_runtime(config, false).await?;
    let target_drive = resolver::resolve_target_drive(
        &graph,
        &storage,
        drive.as_deref(),
        site.as_deref(),
        persona,
        false,
    )
    .await?;
    let parent_item = match dest_folder
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "/" && *s != "root")
    {
        Some(p) => Some(
            resolver::resolve_item_for_write(&graph, &storage, &target_drive.id, p).await?,
        ),
        None => None,
    };
    let parent_id = parent_item.as_ref().map(|p| p.id.clone());
    let parent_path = parent_item
        .as_ref()
        .map(|p| p.display_path())
        .unwrap_or_default();
    let explicit_conflict = match conflict.as_deref() {
        Some(raw) => Some(ConflictBehavior::parse_str(raw)?),
        None => None,
    };
    let mut upload_opts = resolve_upload_options_for_local_file(
        &storage,
        &target_drive.id,
        parent_id.as_deref(),
        Some(&parent_path),
        name.as_deref(),
        &local_path,
        if_match.as_deref(),
        explicit_conflict,
    );
    if let Some(explicit_id) = item_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if if_match
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_none()
            && upload_opts.target_item_id.as_deref() != Some(explicit_id)
        {
            upload_opts.if_match = None;
            upload_opts.expected_c_tag = None;
        }
        upload_opts.target_item_id = Some(explicit_id.to_string());
    }
    let uploaded = graph
        .upload_local_file_with_options(
            &target_drive.id,
            parent_id.as_deref(),
            name.as_deref(),
            &local_path,
            &upload_opts,
        )
        .await?;
    let _ = storage.upsert_drive_items(std::slice::from_ref(&uploaded));
    refresh_download_baseline_after_upload(&storage, &local_path, &uploaded, &upload_opts);
    if as_json {
        println!("{}", serde_json::to_string_pretty(&uploaded)?);
    } else {
        println!(
            "Uploaded {} ({}) -> {} (id: {})",
            sanitize_terminal_inline(&uploaded.name),
            format_human_size(uploaded.size),
            sanitize_terminal_inline(&uploaded.display_path()),
            sanitize_terminal_inline(&uploaded.id)
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn cmd_mkdir(
    config: &AppConfig,
    persona: Persona,
    name: String,
    drive: Option<String>,
    site: Option<String>,
    parent: Option<String>,
    as_json: bool,
) -> Result<()> {
    let (_, graph, storage) = init_runtime(config, false).await?;
    let target_drive = resolver::resolve_target_drive(
        &graph,
        &storage,
        drive.as_deref(),
        site.as_deref(),
        persona,
        false,
    )
    .await?;
    let parent_id = match parent
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "/" && *s != "root")
    {
        Some(p) => Some(
            resolver::resolve_item_for_write(&graph, &storage, &target_drive.id, p)
                .await?
                .id,
        ),
        None => None,
    };
    let folder = graph
        .create_folder(&target_drive.id, parent_id.as_deref(), &name)
        .await?;
    let _ = storage.upsert_drive_items(std::slice::from_ref(&folder));
    if as_json {
        println!("{}", serde_json::to_string_pretty(&folder)?);
    } else {
        println!(
            "Created folder '{}' (id: {})",
            sanitize_terminal_inline(&folder.display_path()),
            sanitize_terminal_inline(&folder.id)
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn cmd_mv(
    config: &AppConfig,
    persona: Persona,
    item_or_path: String,
    drive: Option<String>,
    site: Option<String>,
    name: Option<String>,
    parent: Option<String>,
    as_json: bool,
) -> Result<()> {
    if name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_none()
        && parent
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_none()
    {
        bail!("Must specify at least a new name or a destination parent folder ID.");
    }
    let (_, graph, storage) = init_runtime(config, false).await?;
    let target_drive = resolver::resolve_target_drive(
        &graph,
        &storage,
        drive.as_deref(),
        site.as_deref(),
        persona,
        false,
    )
    .await?;
    let item_fut =
        resolver::resolve_item_for_write(&graph, &storage, &target_drive.id, &item_or_path);
    let parent_fut = async {
        match parent.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(p) if p == "/" || p.eq_ignore_ascii_case("root") => {
                Ok(Some("root".to_string()))
            }
            Some(p) => resolver::resolve_item_for_write(&graph, &storage, &target_drive.id, p)
                .await
                .map(|dest| Some(dest.id)),
            None => Ok(None),
        }
    };
    let (item_res, dest_parent_res) = tokio::join!(item_fut, parent_fut);
    let item = item_res?;
    let dest_parent_id = dest_parent_res?;
    let updated = rename_or_move_item_cached(
        &graph,
        &storage,
        &target_drive.id,
        &item.id,
        Some(&item),
        name.as_deref(),
        dest_parent_id.as_deref(),
    )
    .await?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&updated)?);
    } else {
        println!(
            "Updated item -> {} (id: {})",
            sanitize_terminal_inline(&updated.display_path()),
            sanitize_terminal_inline(&updated.id)
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn cmd_rm(
    config: &AppConfig,
    persona: Persona,
    item_or_path: String,
    drive: Option<String>,
    site: Option<String>,
    yes: bool,
    as_json: bool,
) -> Result<()> {
    if as_json && !yes {
        bail!("{DELETE_JSON_REQUIRES_YES}");
    }
    let (_, graph, storage) = init_runtime(config, false).await?;
    let target_drive = resolver::resolve_target_drive(
        &graph,
        &storage,
        drive.as_deref(),
        site.as_deref(),
        persona,
        false,
    )
    .await?;
    let item =
        resolver::resolve_item_for_write(&graph, &storage, &target_drive.id, &item_or_path)
            .await?;
    if !yes {
        eprint!(
            "Delete '{}' ({})? [y/N]: ",
            sanitize_terminal_inline(&item.display_path()),
            sanitize_terminal_inline(&item.id)
        );
        let _ = io::stderr().flush();
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("Cancelled.");
            return Ok(());
        }
    }
    graph.delete_item(&target_drive.id, &item.id).await?;
    let _ = storage.delete_drive_item(&target_drive.id, &item.id);
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "deleted": true,
                "drive_id": target_drive.id,
                "item_id": item.id,
                "name": item.name,
            }))?
        );
    } else {
        println!(
            "Deleted '{}' ({}).",
            sanitize_terminal_inline(&item.display_path()),
            sanitize_terminal_inline(&item.id)
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn cmd_search(
    config: &AppConfig,
    persona: Persona,
    query: String,
    drive: Option<String>,
    site: Option<String>,
    remote: bool,
    limit: usize,
    as_json: bool,
) -> Result<()> {
    let (_, graph, storage) = init_runtime(config, !remote).await?;
    let drive_id = if drive.is_some() || site.is_some() {
        Some(
            resolver::resolve_target_drive(
                &graph,
                &storage,
                drive.as_deref(),
                site.as_deref(),
                persona,
                !remote,
            )
            .await?
            .id,
        )
    } else {
        None
    };
    let results =
        search_items_combined(&graph, &storage, &query, drive_id.as_deref(), remote, limit)
            .await?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&results)?);
    } else if results.is_empty() {
        println!(
            "No matching files found for '{}'.",
            sanitize_terminal_inline(&query)
        );
    } else {
        for item in &results {
            println!(
                "[{:<4}] {:<36} {:>10}  {}  (drive: {}, id: {})",
                item.type_badge(),
                sanitize_terminal_inline(&item.name),
                format_human_size(item.size),
                sanitize_terminal_inline(&item.display_path()),
                sanitize_terminal_inline(&item.drive_id),
                sanitize_terminal_inline(&item.id)
            );
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn cmd_share(
    config: &AppConfig,
    persona: Persona,
    item_or_path: String,
    drive: Option<String>,
    site: Option<String>,
    link_type: String,
    scope: String,
    expires: Option<String>,
    as_json: bool,
) -> Result<()> {
    let (_, graph, storage) = init_runtime(config, false).await?;
    let target_drive = resolver::resolve_target_drive(
        &graph,
        &storage,
        drive.as_deref(),
        site.as_deref(),
        persona,
        false,
    )
    .await?;
    let item =
        resolver::resolve_item(&graph, &storage, &target_drive.id, &item_or_path, false)
            .await?;
    let link = graph
        .create_share_link(
            item.effective_drive_id(),
            item.effective_item_id(),
            &link_type,
            &scope,
            expires.as_deref(),
        )
        .await?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&link)?);
    } else {
        println!(
            "Share link ({} / {}): {}",
            sanitize_terminal_inline(&link.link_type),
            sanitize_terminal_inline(&link.scope),
            sanitize_terminal_inline(&link.web_url)
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn cmd_preview(
    config: &AppConfig,
    persona: Persona,
    item_or_path: String,
    drive: Option<String>,
    site: Option<String>,
    cols: Option<u16>,
    rows: Option<u16>,
    protocol: Option<String>,
) -> Result<()> {
    let (_, graph, storage) = init_runtime(config, false).await?;
    let target_drive = resolver::resolve_target_drive(
        &graph,
        &storage,
        drive.as_deref(),
        site.as_deref(),
        persona,
        false,
    )
    .await?;
    let item =
        resolver::resolve_item(&graph, &storage, &target_drive.id, &item_or_path, false)
            .await?;
    let bytes = graph
        .download_item_bytes(
            item.effective_drive_id(),
            item.effective_item_id(),
            item.download_url.as_deref(),
            32 * 1024 * 1024,
        )
        .await?;
    render_image_to_stdout(config, &bytes, &item.name, cols, rows, protocol.as_deref())
}

pub(super) fn cmd_preview_file(
    config: &AppConfig,
    image_path: PathBuf,
    cols: Option<u16>,
    rows: Option<u16>,
    protocol: Option<String>,
) -> Result<()> {
    let bytes = std::fs::read(&image_path)
        .with_context(|| format!("Failed to read image: {}", image_path.display()))?;
    let label = image_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("image");
    render_image_to_stdout(config, &bytes, label, cols, rows, protocol.as_deref())
}
