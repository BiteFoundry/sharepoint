//! CLI parser, command dispatcher, and shared domain helpers for SharePoint and OneDrive.

mod args;
mod data;
mod files;
mod format;
mod lists;
mod sites;
mod system;

#[cfg(test)]
mod tests;

use std::ffi::OsString;

use anyhow::Result;

use crate::{
    config::{AppConfig, Persona},
    tui,
};

pub use args::{
    parse_cli_from, try_fast_help_or_version, Cli, Command, DaemonAction, ListAction, ListTarget,
    DEFAULT_TEXT_PREVIEW_MAX_BYTES,
};
pub use data::{
    discover_drives, fetch_drives, fetch_folder_items, fetch_folder_listing, fetch_sites,
    init_runtime, record_download_baseline, refresh_download_baseline_after_upload,
    rename_or_move_item_cached, resolve_upload_options_for_local_file, search_items_combined,
    DriveDiscovery,
};
pub use format::{
    auth_source_label, is_forbidden_error, normalize_download_dir, wrap_graph_permission_error,
    FolderListing,
};
pub(crate) use format::{file_info_fields_json, file_info_json, folder_listing_json, DetailHint};

#[cfg(test)]
use data::build_tree;
#[cfg(test)]
use format::tree_lines;

pub async fn run_from_env() -> Result<()> {
    let persona = Persona::detect();
    let raw_args: Vec<OsString> = std::env::args_os().collect();
    let cli = parse_cli_from(raw_args.iter().cloned(), persona.bin_name())
        .unwrap_or_else(|err| err.exit());
    let json_mode = args::args_has_json_flag(&raw_args);
    let res = run_cli(cli, persona).await;
    if let Err(ref err) = res {
        if json_mode {
            let payload = format::json_error_value(err);
            if let Ok(line) = serde_json::to_string(&payload) {
                println!("{line}");
            }
        }
    }
    res
}

pub async fn run_cli(cli: Cli, persona: Persona) -> Result<()> {
    let config = AppConfig::load();
    match cli.command {
        None => {
            let (auth, graph, storage) = init_runtime(&config, false).await?;
            tui::run_tui(config, auth, graph, storage, persona).await
        }
        Some(Command::Tui { persona: override_p }) => {
            let effective = override_p
                .as_deref()
                .and_then(Persona::parse_str)
                .unwrap_or(persona);
            let (auth, graph, storage) = init_runtime(&config, false).await?;
            tui::run_tui(config, auth, graph, storage, effective).await
        }
        Some(Command::Login {
            client_id,
            tenant_id,
            device_code,
            client_secret,
            access_token,
            token_file,
            port,
            json: as_json,
        }) => {
            system::cmd_login(
                &config,
                client_id,
                tenant_id,
                device_code,
                client_secret,
                access_token,
                token_file,
                port,
                as_json,
            )
            .await
        }
        Some(Command::Logout { json: as_json }) => system::cmd_logout(config, as_json).await,
        Some(Command::Whoami { json: as_json }) => {
            system::cmd_whoami(&config, persona, as_json).await
        }
        Some(Command::Doctor {
            online,
            json: as_json,
        }) => system::cmd_doctor(&config, persona, online, as_json).await,
        Some(Command::Sites {
            search,
            limit,
            refresh,
            offline,
            cached,
            json: as_json,
        }) => {
            sites::cmd_sites(&config, search, limit, refresh, offline, cached, as_json).await
        }
        Some(Command::Drives {
            site,
            refresh,
            offline,
            cached,
            json: as_json,
        }) => sites::cmd_drives(&config, site, refresh, offline, cached, as_json).await,
        Some(Command::Lists { json, action }) => {
            lists::run_list_action(&config, action, json).await
        }
        Some(Command::Ls {
            path_or_id,
            drive,
            site,
            limit,
            refresh,
            offline,
            cached,
            json: as_json,
            compact,
        }) => {
            files::cmd_ls(
                &config,
                persona,
                path_or_id,
                drive,
                site,
                limit,
                refresh,
                offline,
                cached,
                as_json,
                compact,
            )
            .await
        }
        Some(Command::Tree {
            path_or_id,
            drive,
            site,
            depth,
            json: as_json,
        }) => files::cmd_tree(&config, persona, path_or_id, drive, site, depth, as_json).await,
        Some(Command::Info {
            item_or_path,
            drive,
            site,
            versions,
            versions_limit,
            offline,
            json: as_json,
            compact: _,
            full,
        }) => {
            files::cmd_info(
                &config,
                persona,
                item_or_path,
                drive,
                site,
                versions,
                versions_limit,
                offline,
                as_json,
                full,
            )
            .await
        }
        Some(Command::Cat {
            item_or_path,
            drive,
            site,
            max_bytes,
            json: as_json,
        }) => files::cmd_cat(&config, persona, item_or_path, drive, site, max_bytes, as_json).await,
        Some(Command::Download {
            item_or_path,
            drive,
            site,
            dir,
            json: as_json,
        }) => files::cmd_download(&config, persona, item_or_path, drive, site, dir, as_json).await,
        Some(Command::Upload {
            local_path,
            dest_folder,
            drive,
            site,
            name,
            item_id,
            if_match,
            conflict,
            json: as_json,
        }) => {
            files::cmd_upload(
                &config,
                persona,
                local_path,
                dest_folder,
                drive,
                site,
                name,
                item_id,
                if_match,
                conflict,
                as_json,
            )
            .await
        }
        Some(Command::Mkdir {
            name,
            drive,
            site,
            parent,
            json: as_json,
        }) => files::cmd_mkdir(&config, persona, name, drive, site, parent, as_json).await,
        Some(Command::Mv {
            item_or_path,
            drive,
            site,
            name,
            parent,
            json: as_json,
        }) => files::cmd_mv(&config, persona, item_or_path, drive, site, name, parent, as_json).await,
        Some(Command::Rm {
            item_or_path,
            drive,
            site,
            yes,
            json: as_json,
        }) => files::cmd_rm(&config, persona, item_or_path, drive, site, yes, as_json).await,
        Some(Command::Search {
            query,
            drive,
            site,
            remote,
            limit,
            json: as_json,
        }) => files::cmd_search(&config, persona, query, drive, site, remote, limit, as_json).await,
        Some(Command::Share {
            item_or_path,
            drive,
            site,
            link_type,
            scope,
            expires,
            json: as_json,
        }) => {
            files::cmd_share(
                &config,
                persona,
                item_or_path,
                drive,
                site,
                link_type,
                scope,
                expires,
                as_json,
            )
            .await
        }
        Some(Command::Recent {
            limit,
            offline,
            cached,
            json: as_json,
        }) => sites::cmd_recent(&config, limit, offline, cached, as_json).await,
        Some(Command::Shared {
            limit,
            offline,
            cached,
            json: as_json,
        }) => sites::cmd_shared(&config, limit, offline, cached, as_json).await,
        Some(Command::Preview {
            item_or_path,
            drive,
            site,
            cols,
            rows,
            protocol,
        }) => {
            files::cmd_preview(&config, persona, item_or_path, drive, site, cols, rows, protocol)
                .await
        }
        Some(Command::PreviewFile {
            image_path,
            cols,
            rows,
            protocol,
        }) => files::cmd_preview_file(&config, image_path, cols, rows, protocol),
        Some(Command::Daemon { action }) => system::cmd_daemon(config, action).await,
        Some(Command::Serve {
            bind,
            port,
            api_key,
            allow_keyless,
            allow_local_paths,
            allowed_hosts,
            cors_origins,
        }) => {
            system::cmd_serve(
                config,
                bind,
                port,
                api_key,
                allow_keyless,
                allow_local_paths,
                allowed_hosts,
                cors_origins,
            )
            .await
        }
        Some(Command::Mcp) => system::cmd_mcp(config).await,
        Some(Command::Update {
            check,
            force,
            repo,
            json: as_json,
        }) => system::cmd_update(&config, check, force, repo, as_json).await,
    }
}
