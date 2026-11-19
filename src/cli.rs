//! Unified `sharepoint` / `sp` / `onedrive` / `od` CLI parser, command dispatcher, and shared domain helpers.

use std::{
    io::{self, Write},
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serde_json::json;

use crate::{
    auth::{
        diagnose_access_token_scopes, explain_graph_permission_error, read_external_token_file,
        AuthManager, LoginOptions, StoredTokens,
    },
    config::{self, AppConfig, Persona},
    graph::GraphClient,
    img_preview,
    models::{format_human_size, DriveInfo, DriveItem, SharePointSite, TreeNode},
    resolver,
    serve::{self, ServeOptions},
    storage::Storage,
    tui, updater,
};

pub const DEFAULT_TEXT_PREVIEW_MAX_BYTES: usize = 256 * 1024;
const DELETE_JSON_REQUIRES_YES: &str = "Deleting with --json requires passing --yes.";

const ENV_AFTER_HELP: &str = "\
Dual-Persona Launchers:
  sharepoint / sp                       Boot into SharePoint Sites & Document Libraries
  onedrive / od                         Boot directly into Personal OneDrive (SHAREPOINT_PERSONA=onedrive)

Environment Variables:
  SHAREPOINT_PERSONA=<sharepoint|onedrive> Override default persona
  SHAREPOINT_TOKEN_FILE=<PATH>          Load an external token file (JWT or JSON) directly
  SHAREPOINT_TOKEN_REFRESH_CMD=<CMD>    Command hook run when token nears expiry
  SHAREPOINT_ACCESS_TOKEN=<JWT>         Inject a Bearer access token directly
  SHAREPOINT_SHARE_M365_AUTH=<1|0>      Reuse stored teams-cli / TEAMS_TOKEN_FILE session [default: 1]
  SHAREPOINT_DISABLE_KEYRING=1          Disable OS keyring (credentials.json only)
  SHAREPOINT_CLIENT_ID / TENANT_ID      Override Entra app Client ID / Tenant ID
  SHAREPOINT_CONFIG_DIR / DATA_DIR      Override config and SQLite cache directories
  SHAREPOINT_AUTO_DAEMON=1              Auto-start/attach to background sync daemon
  SHAREPOINT_IMAGE_PROTOCOL=<PROTO>     Terminal image protocol (auto, kitty, iterm2, sixel, halfblock)

Background Sync Daemon:
  sharepoint daemon start | status | stop | restart";

#[derive(Parser, Debug)]
#[command(
    name = "sharepoint",
    version,
    about = "Unified Microsoft SharePoint & OneDrive terminal client (CLI · TUI · HTTP API · MCP)",
    long_about = "Terminal client for Microsoft SharePoint Sites, Document Libraries & Personal OneDrive powered by Microsoft Graph v1.0.\n\
                  Running without a subcommand checks authentication and launches the interactive 3-pane TUI.",
    after_help = ENV_AFTER_HELP
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Sign in with Microsoft Entra ID (OAuth 2.0) and store tokens
    #[command(visible_alias = "in")]
    Login {
        #[arg(long)]
        client_id: Option<String>,
        #[arg(long)]
        tenant_id: Option<String>,
        #[arg(long)]
        device_code: bool,
        #[arg(long)]
        client_secret: Option<String>,
        #[arg(long, visible_alias = "token")]
        access_token: Option<String>,
        #[arg(long, value_name = "PATH")]
        token_file: Option<PathBuf>,
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        json: bool,
    },
    /// Remove stored authentication tokens
    #[command(visible_alias = "out")]
    Logout {
        #[arg(long)]
        json: bool,
    },
    /// Display the currently signed-in user profile, persona, token expiry, scopes, and auth source
    Whoami {
        #[arg(long)]
        json: bool,
    },
    /// Show config/DB/keyring paths, image protocol, and optional live Graph diagnostics
    Doctor {
        /// Probe live Graph connectivity across OneDrive and SharePoint Sites
        #[arg(long, visible_alias = "probe")]
        online: bool,
        #[arg(long)]
        json: bool,
    },
    /// List followed and discovered SharePoint sites
    Sites {
        #[arg(long)]
        search: Option<String>,
        #[arg(long, visible_alias = "top", default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long, visible_alias = "fast")]
        cached: bool,
        #[arg(long)]
        json: bool,
    },
    /// List Personal OneDrive and SharePoint document libraries (drives)
    Drives {
        /// Filter document libraries by SharePoint site name or ID
        #[arg(long)]
        site: Option<String>,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long, visible_alias = "fast")]
        cached: bool,
        #[arg(long)]
        json: bool,
    },
    /// List files and folders inside a OneDrive or SharePoint drive
    Ls {
        /// Optional folder path (e.g. /Projects/Q4) or folder item ID
        path_or_id: Option<String>,
        /// Target drive name, ID, or alias (me/onedrive)
        #[arg(long)]
        drive: Option<String>,
        /// Target SharePoint site name or ID
        #[arg(long)]
        site: Option<String>,
        #[arg(long, visible_alias = "top", default_value_t = 100)]
        limit: usize,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long, visible_alias = "fast")]
        cached: bool,
        #[arg(long)]
        json: bool,
    },
    /// Print a recursive directory tree for a OneDrive or SharePoint folder
    Tree {
        /// Optional starting folder path or item ID (defaults to drive root)
        path_or_id: Option<String>,
        #[arg(long)]
        drive: Option<String>,
        #[arg(long)]
        site: Option<String>,
        /// Maximum recursion depth
        #[arg(long, default_value_t = 2)]
        depth: usize,
        #[arg(long)]
        json: bool,
    },
    /// Show detailed metadata (and optional version history) for a file or folder
    Info {
        /// Item ID or path (e.g. /Docs/spec.md)
        item_or_path: String,
        #[arg(long)]
        drive: Option<String>,
        #[arg(long)]
        site: Option<String>,
        /// Also fetch and display version history
        #[arg(long)]
        versions: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// Print text/Markdown/JSON/YAML/CSV/code file contents up to a safe byte cap
    Cat {
        /// Item ID or path
        item_or_path: String,
        #[arg(long)]
        drive: Option<String>,
        #[arg(long)]
        site: Option<String>,
        /// Maximum bytes to fetch for text preview
        #[arg(long = "max-bytes", default_value_t = DEFAULT_TEXT_PREVIEW_MAX_BYTES)]
        max_bytes: usize,
        #[arg(long)]
        json: bool,
    },
    /// Download a file to local disk (returns structured metadata in --json mode)
    #[command(visible_alias = "dl")]
    Download {
        /// Item ID or path
        item_or_path: String,
        #[arg(long)]
        drive: Option<String>,
        #[arg(long)]
        site: Option<String>,
        /// Output directory (defaults to configured download_dir)
        #[arg(long, visible_alias = "out", value_name = "DIR")]
        dir: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Upload a local file (auto-selects simple PUT < 4 MiB or resumable chunked session >= 4 MiB)
    #[command(visible_alias = "up")]
    Upload {
        /// Local file path to upload
        local_path: PathBuf,
        /// Optional destination folder path or item ID (defaults to drive root)
        dest_folder: Option<String>,
        #[arg(long)]
        drive: Option<String>,
        #[arg(long)]
        site: Option<String>,
        /// Optional remote filename override
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Create a new folder in OneDrive or a SharePoint document library
    Mkdir {
        /// New folder name
        name: String,
        #[arg(long)]
        drive: Option<String>,
        #[arg(long)]
        site: Option<String>,
        /// Parent folder path or item ID (defaults to drive root)
        #[arg(long)]
        parent: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Move and/or rename a file or folder
    Mv {
        /// Item ID or path to modify
        item_or_path: String,
        #[arg(long)]
        drive: Option<String>,
        #[arg(long)]
        site: Option<String>,
        /// New filename
        #[arg(long)]
        name: Option<String>,
        /// Destination parent folder ID or path
        #[arg(long)]
        parent: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Delete a file or folder (--yes required with --json)
    Rm {
        /// Item ID or path to delete
        item_or_path: String,
        #[arg(long)]
        drive: Option<String>,
        #[arg(long)]
        site: Option<String>,
        /// Confirm deletion non-interactively
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        json: bool,
    },
    /// Search cached files via SQLite FTS5 or live across OneDrive & SharePoint via Graph
    Search {
        query: String,
        #[arg(long)]
        drive: Option<String>,
        #[arg(long)]
        site: Option<String>,
        /// Query Microsoft Graph remotely in addition to local SQLite FTS5
        #[arg(long)]
        remote: bool,
        #[arg(long, visible_alias = "top", default_value_t = 30)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Create a sharing link for a file or folder
    Share {
        /// Item ID or path
        item_or_path: String,
        #[arg(long)]
        drive: Option<String>,
        #[arg(long)]
        site: Option<String>,
        /// Link permission type (view, edit, embed)
        #[arg(long = "type", default_value = "view", value_parser = ["view", "edit", "embed"])]
        link_type: String,
        /// Link scope (organization, anonymous)
        #[arg(long, default_value = "organization", value_parser = ["organization", "anonymous"])]
        scope: String,
        /// Optional ISO-8601 expiration timestamp
        #[arg(long)]
        expires: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// List recently accessed files from Personal OneDrive
    Recent {
        #[arg(long, visible_alias = "top", default_value_t = 30)]
        limit: usize,
        #[arg(long)]
        offline: bool,
        #[arg(long, visible_alias = "fast")]
        cached: bool,
        #[arg(long)]
        json: bool,
    },
    /// List files shared with you
    Shared {
        #[arg(long, visible_alias = "top", default_value_t = 30)]
        limit: usize,
        #[arg(long)]
        offline: bool,
        #[arg(long, visible_alias = "fast")]
        cached: bool,
        #[arg(long)]
        json: bool,
    },
    /// Render a remote OneDrive/SharePoint image file directly in the terminal
    Preview {
        item_or_path: String,
        #[arg(long)]
        drive: Option<String>,
        #[arg(long)]
        site: Option<String>,
        #[arg(long)]
        cols: Option<u16>,
        #[arg(long)]
        rows: Option<u16>,
        #[arg(long, value_name = "PROTO")]
        protocol: Option<String>,
    },
    /// Render a local image file in the terminal
    #[command(name = "preview-file")]
    PreviewFile {
        image_path: PathBuf,
        #[arg(long)]
        cols: Option<u16>,
        #[arg(long)]
        rows: Option<u16>,
        #[arg(long, value_name = "PROTO")]
        protocol: Option<String>,
    },
    /// Launch the interactive 3-pane Minimal-Chrome TUI
    Tui {
        /// Override startup persona (sharepoint or onedrive)
        #[arg(long)]
        persona: Option<String>,
    },
    /// Manage the shared background sync IPC daemon
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Start the local HTTP JSON API server (default port 8790)
    #[command(visible_alias = "api")]
    Serve {
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        #[arg(long, default_value_t = config::DEFAULT_SERVE_PORT)]
        port: u16,
        #[arg(long)]
        api_key: Option<String>,
    },
    /// Start the Model Context Protocol (MCP) server over stdio
    #[command(visible_alias = "mcp-server")]
    Mcp,
    /// Check GitHub Releases and self-update the binary with SHA-256 verification
    Update {
        #[arg(long)]
        check: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum DaemonAction {
    Start {
        #[arg(long)]
        json: bool,
    },
    Status {
        #[arg(long)]
        json: bool,
    },
    Stop {
        #[arg(long)]
        json: bool,
    },
    Restart {
        #[arg(long)]
        json: bool,
    },
    #[command(hide = true)]
    Run,
}

pub fn try_fast_help_or_version() -> Option<Result<()>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 2 && matches!(args[1].as_str(), "-V" | "--version") {
        let persona = Persona::detect();
        println!("{} {}", persona.bin_name(), env!("CARGO_PKG_VERSION"));
        return Some(Ok(()));
    }
    None
}

pub async fn run_from_env() -> Result<()> {
    let cli = Cli::parse();
    let persona = Persona::detect();
    run_cli(cli, persona).await
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
            let mut cfg = config.clone();
            if let Some(tf) = token_file {
                cfg.token_file = tf.to_string_lossy().into_owned();
                let tok = read_external_token_file(&tf, &cfg)?;
                cfg.save()?;
                if as_json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({
                            "ok": true,
                            "auth_mode": tok.auth_mode,
                            "token_file": cfg.token_file,
                            "user_id": tok.user_id,
                            "user_email": tok.user_email,
                        }))?
                    );
                } else {
                    println!("Configured external token file: {}", tf.display());
                }
                return Ok(());
            }
            let auth = AuthManager::new(cfg.clone())?;
            let tokens = auth
                .login(&LoginOptions {
                    client_id,
                    tenant_id,
                    device_code,
                    client_secret,
                    access_token,
                    port,
                })
                .await?;
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "ok": true,
                        "auth_mode": tokens.auth_mode,
                        "user_id": tokens.user_id,
                        "display_name": tokens.user_display_name,
                        "email": tokens.user_email,
                        "expires_at": tokens.expires_at,
                    }))?
                );
            } else {
                println!(
                    "Signed in as {} ({})",
                    tokens
                        .user_display_name
                        .as_deref()
                        .or(tokens.user_email.as_deref())
                        .unwrap_or("Microsoft 365 User"),
                    tokens.auth_mode
                );
            }
            Ok(())
        }
        Some(Command::Logout { json: as_json }) => {
            let auth = AuthManager::new(config)?;
            auth.logout().await?;
            if as_json {
                println!("{}", serde_json::to_string_pretty(&json!({ "ok": true }))?);
            } else {
                println!("Signed out and removed stored credentials.");
            }
            Ok(())
        }
        Some(Command::Whoami { json: as_json }) => cmd_whoami(&config, persona, as_json).await,
        Some(Command::Doctor { online, json: as_json }) => {
            cmd_doctor(&config, persona, online, as_json).await
        }
        Some(Command::Sites {
            search,
            limit,
            refresh,
            offline,
            cached,
            json: as_json,
        }) => {
            let (_, graph, storage) = init_runtime(&config, offline).await?;
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
                        s.effective_name(),
                        s.web_url.as_deref().unwrap_or(""),
                        s.id
                    );
                }
            }
            Ok(())
        }
        Some(Command::Drives {
            site,
            refresh,
            offline,
            cached,
            json: as_json,
        }) => {
            let (_, graph, storage) = init_runtime(&config, offline).await?;
            let drives = fetch_drives(&graph, &storage, site.as_deref(), refresh, offline, cached).await?;
            if as_json {
                println!("{}", serde_json::to_string_pretty(&drives)?);
            } else if drives.is_empty() {
                println!("No drives found.");
            } else {
                for d in &drives {
                    let quota = match (d.quota_used, d.quota_total) {
                        (Some(u), Some(t)) => {
                            format!(" [{}/{}]", format_human_size(u), format_human_size(t))
                        }
                        _ => String::new(),
                    };
                    println!(
                        "{:<34}  {:<16}{}  ({})",
                        d.display_label(),
                        d.drive_type,
                        quota,
                        d.id
                    );
                }
            }
            Ok(())
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
        }) => {
            let (_, graph, storage) = init_runtime(&config, offline).await?;
            let target_drive = resolver::resolve_target_drive(
                &graph,
                &storage,
                drive.as_deref(),
                site.as_deref(),
                persona,
                offline,
            )
            .await?;
            let items = fetch_folder_items(
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
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "drive": target_drive,
                        "path": path_or_id.unwrap_or_else(|| "/".to_string()),
                        "items": items,
                    }))?
                );
            } else {
                println!(
                    "{} ({})",
                    target_drive.display_label(),
                    path_or_id.as_deref().unwrap_or("/")
                );
                if items.is_empty() {
                    println!("  (empty)");
                } else {
                    for item in &items {
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
                            item.name,
                            size,
                            modified,
                            item.id
                        );
                    }
                }
            }
            Ok(())
        }
        Some(Command::Tree {
            path_or_id,
            drive,
            site,
            depth,
            json: as_json,
        }) => {
            let (_, graph, storage) = init_runtime(&config, false).await?;
            let target_drive = resolver::resolve_target_drive(
                &graph,
                &storage,
                drive.as_deref(),
                site.as_deref(),
                persona,
                false,
            )
            .await?;
            let parent_id = match path_or_id.as_deref().map(str::trim).filter(|s| !s.is_empty() && *s != "/") {
                Some(p) => Some(resolver::resolve_item(&graph, &storage, &target_drive.id, p, false).await?.id),
                None => None,
            };
            let nodes = build_tree(&graph, &storage, &target_drive.id, parent_id.as_deref(), depth.clamp(1, 5)).await?;
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "drive": target_drive,
                        "tree": nodes,
                    }))?
                );
            } else {
                println!("{}", target_drive.display_label());
                print_tree_nodes(&nodes, "");
            }
            Ok(())
        }
        Some(Command::Info {
            item_or_path,
            drive,
            site,
            versions,
            offline,
            json: as_json,
        }) => {
            let (_, graph, storage) = init_runtime(&config, offline).await?;
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
            let vers = if versions && !offline {
                graph
                    .list_item_versions(item.effective_drive_id(), item.effective_item_id())
                    .await
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "item": item,
                        "versions": vers,
                    }))?
                );
            } else {
                println!("Name:      {}", item.name);
                println!("ID:        {}", item.id);
                println!("Drive:     {}", item.drive_id);
                println!("Path:      {}", item.display_path());
                println!("Type:      {}", if item.is_folder { "Folder" } else { item.mime_type.as_deref().unwrap_or("File") });
                println!("Size:      {} ({} bytes)", format_human_size(item.size), item.size);
                if let Some(m) = &item.last_modified_at {
                    println!("Modified:  {m}");
                }
                if let Some(a) = &item.last_modified_by {
                    println!("Author:    {a}");
                }
                if let Some(s) = &item.shared_scope {
                    println!("Sharing:   {s}");
                }
                if let Some(u) = &item.web_url {
                    println!("Web URL:   {u}");
                }
                if !vers.is_empty() {
                    println!("Versions ({}):", vers.len());
                    for v in &vers {
                        println!(
                            "  v{}  {}  {}  {}",
                            v.id,
                            format_human_size(v.size),
                            v.last_modified_at.as_deref().unwrap_or(""),
                            v.last_modified_by.as_deref().unwrap_or("")
                        );
                    }
                }
            }
            Ok(())
        }
        Some(Command::Cat {
            item_or_path,
            drive,
            site,
            max_bytes,
            json: as_json,
        }) => {
            let (_, graph, storage) = init_runtime(&config, false).await?;
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
            let text = String::from_utf8_lossy(&bytes).into_owned();
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "ok": true,
                        "id": item.id,
                        "drive_id": item.drive_id,
                        "name": item.name,
                        "mime_type": item.mime_type,
                        "size_bytes": bytes.len(),
                        "content": text,
                    }))?
                );
            } else {
                print!("{text}");
                if !text.ends_with('\n') {
                    println!();
                }
            }
            Ok(())
        }
        Some(Command::Download {
            item_or_path,
            drive,
            site,
            dir,
            json: as_json,
        }) => {
            let (_, graph, storage) = init_runtime(&config, false).await?;
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
            let downloaded = graph
                .download_item_to_file(
                    item.effective_drive_id(),
                    item.effective_item_id(),
                    Some(&item.name),
                    item.mime_type.as_deref(),
                    item.download_url.as_deref(),
                    &dest_dir,
                )
                .await?;
            if as_json {
                println!("{}", serde_json::to_string_pretty(&downloaded)?);
            } else {
                println!(
                    "Downloaded {} ({}) -> {}",
                    downloaded.file_name,
                    format_human_size(downloaded.size_bytes),
                    downloaded.path.display()
                );
            }
            Ok(())
        }
        Some(Command::Upload {
            local_path,
            dest_folder,
            drive,
            site,
            name,
            json: as_json,
        }) => {
            let (_, graph, storage) = init_runtime(&config, false).await?;
            let target_drive = resolver::resolve_target_drive(
                &graph,
                &storage,
                drive.as_deref(),
                site.as_deref(),
                persona,
                false,
            )
            .await?;
            let parent_id = match dest_folder.as_deref().map(str::trim).filter(|s| !s.is_empty() && *s != "/") {
                Some(p) => Some(resolver::resolve_item(&graph, &storage, &target_drive.id, p, false).await?.id),
                None => None,
            };
            let uploaded = graph
                .upload_local_file(
                    &target_drive.id,
                    parent_id.as_deref(),
                    name.as_deref(),
                    &local_path,
                )
                .await?;
            let _ = storage.upsert_drive_items(std::slice::from_ref(&uploaded));
            if as_json {
                println!("{}", serde_json::to_string_pretty(&uploaded)?);
            } else {
                println!(
                    "Uploaded {} ({}) -> {} (id: {})",
                    uploaded.name,
                    format_human_size(uploaded.size),
                    uploaded.display_path(),
                    uploaded.id
                );
            }
            Ok(())
        }
        Some(Command::Mkdir {
            name,
            drive,
            site,
            parent,
            json: as_json,
        }) => {
            let (_, graph, storage) = init_runtime(&config, false).await?;
            let target_drive = resolver::resolve_target_drive(
                &graph,
                &storage,
                drive.as_deref(),
                site.as_deref(),
                persona,
                false,
            )
            .await?;
            let parent_id = match parent.as_deref().map(str::trim).filter(|s| !s.is_empty() && *s != "/") {
                Some(p) => Some(resolver::resolve_item(&graph, &storage, &target_drive.id, p, false).await?.id),
                None => None,
            };
            let folder = graph
                .create_folder(&target_drive.id, parent_id.as_deref(), &name)
                .await?;
            let _ = storage.upsert_drive_items(std::slice::from_ref(&folder));
            if as_json {
                println!("{}", serde_json::to_string_pretty(&folder)?);
            } else {
                println!("Created folder '{}' (id: {})", folder.display_path(), folder.id);
            }
            Ok(())
        }
        Some(Command::Mv {
            item_or_path,
            drive,
            site,
            name,
            parent,
            json: as_json,
        }) => {
            let (_, graph, storage) = init_runtime(&config, false).await?;
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
            let dest_parent_id = match parent.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                Some("/") | Some("root") => Some("root".to_string()),
                Some(p) => Some(resolver::resolve_item(&graph, &storage, &target_drive.id, p, false).await?.id),
                None => None,
            };
            let updated = graph
                .rename_or_move_item(
                    &target_drive.id,
                    &item.id,
                    name.as_deref(),
                    dest_parent_id.as_deref(),
                )
                .await?;
            let _ = storage.upsert_drive_items(std::slice::from_ref(&updated));
            if as_json {
                println!("{}", serde_json::to_string_pretty(&updated)?);
            } else {
                println!("Updated item -> {} (id: {})", updated.display_path(), updated.id);
            }
            Ok(())
        }
        Some(Command::Rm {
            item_or_path,
            drive,
            site,
            yes,
            json: as_json,
        }) => {
            if as_json && !yes {
                bail!("{DELETE_JSON_REQUIRES_YES}");
            }
            let (_, graph, storage) = init_runtime(&config, false).await?;
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
            if !yes {
                eprint!("Delete '{}' ({})? [y/N]: ", item.display_path(), item.id);
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
                println!("Deleted '{}' ({}).", item.display_path(), item.id);
            }
            Ok(())
        }
        Some(Command::Search {
            query,
            drive,
            site,
            remote,
            limit,
            json: as_json,
        }) => {
            let (_, graph, storage) = init_runtime(&config, !remote).await?;
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
                println!("No matching files found for '{query}'.");
            } else {
                for item in &results {
                    println!(
                        "[{:<4}] {:<36} {:>10}  {}  (drive: {}, id: {})",
                        item.type_badge(),
                        item.name,
                        format_human_size(item.size),
                        item.display_path(),
                        item.drive_id,
                        item.id
                    );
                }
            }
            Ok(())
        }
        Some(Command::Share {
            item_or_path,
            drive,
            site,
            link_type,
            scope,
            expires,
            json: as_json,
        }) => {
            let (_, graph, storage) = init_runtime(&config, false).await?;
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
                    link.link_type, link.scope, link.web_url
                );
            }
            Ok(())
        }
        Some(Command::Recent {
            limit,
            offline,
            cached,
            json: as_json,
        }) => {
            let (_, graph, storage) = init_runtime(&config, offline).await?;
            let items = if offline || cached {
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
                        item.name,
                        format_human_size(item.size),
                        item.last_modified_at
                            .as_deref()
                            .map(|s| s.get(..16).unwrap_or(s))
                            .unwrap_or(""),
                        item.id
                    );
                }
            }
            Ok(())
        }
        Some(Command::Shared {
            limit,
            offline,
            cached,
            json: as_json,
        }) => {
            let (_, graph, storage) = init_runtime(&config, offline).await?;
            let items = if offline || cached {
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
                        item.name,
                        format_human_size(item.size),
                        item.last_modified_by.as_deref().unwrap_or(""),
                        item.id
                    );
                }
            }
            Ok(())
        }
        Some(Command::Preview {
            item_or_path,
            drive,
            site,
            cols,
            rows,
            protocol,
        }) => {
            let (_, graph, storage) = init_runtime(&config, false).await?;
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
            render_image_to_stdout(&config, &bytes, &item.name, cols, rows, protocol.as_deref())
        }
        Some(Command::PreviewFile {
            image_path,
            cols,
            rows,
            protocol,
        }) => {
            let bytes = std::fs::read(&image_path)
                .with_context(|| format!("Failed to read image: {}", image_path.display()))?;
            let label = image_path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("image");
            render_image_to_stdout(&config, &bytes, label, cols, rows, protocol.as_deref())
        }
        Some(Command::Daemon { action }) => match action {
            DaemonAction::Start { json: as_json } => {
                let status = crate::daemon::ensure_daemon_started(&config).await?;
                print_daemon_status(&status, as_json)
            }
            DaemonAction::Status { json: as_json } => {
                let status = crate::daemon::query_daemon_status(&config).await?;
                print_daemon_status(&status, as_json)
            }
            DaemonAction::Stop { json: as_json } => {
                let stopped = crate::daemon::stop_daemon(&config).await?;
                if as_json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({ "ok": true, "stopped": stopped }))?
                    );
                } else if stopped {
                    println!("SharePoint background sync daemon stopped.");
                } else {
                    println!("Daemon was not running.");
                }
                Ok(())
            }
            DaemonAction::Restart { json: as_json } => {
                let _ = crate::daemon::stop_daemon(&config).await;
                let status = crate::daemon::ensure_daemon_started(&config).await?;
                print_daemon_status(&status, as_json)
            }
            DaemonAction::Run => crate::daemon::run_daemon_foreground(config).await,
        },
        Some(Command::Serve {
            bind,
            port,
            api_key,
        }) => {
            let (_, graph, storage) = init_runtime(&config, false).await?;
            serve::run(
                config,
                graph,
                storage,
                ServeOptions {
                    bind,
                    port,
                    api_key,
                },
            )
            .await
        }
        Some(Command::Mcp) => {
            let (_, graph, storage) = init_runtime(&config, false).await?;
            crate::mcp::run(config, graph, storage).await
        }
        Some(Command::Update {
            check,
            force,
            repo,
            json: as_json,
        }) => {
            let target_repo = repo.as_deref().unwrap_or(&config.github_repo);
            let res = updater::check_or_apply_update(target_repo, check, force).await?;
            if as_json {
                println!("{}", serde_json::to_string_pretty(&res)?);
            } else if res.updated {
                println!(
                    "Updated sharepoint {} -> {} ({})",
                    res.current_version,
                    res.latest_version,
                    res.binary_path.display()
                );
            } else if res.update_available {
                println!(
                    "Update available: {} -> {} ({})",
                    res.current_version, res.latest_version, res.release_url
                );
            } else {
                println!("Up to date (v{}).", res.current_version);
            }
            Ok(())
        }
    }
}

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

async fn cmd_whoami(config: &AppConfig, persona: Persona, as_json: bool) -> Result<()> {
    let auth = AuthManager::new(config.clone())?;
    let tokens = auth.current_tokens().await;
    let source = auth_source_label(tokens.as_ref(), config);
    let Some(tok) = tokens else {
        if as_json {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "authenticated": false,
                    "persona": persona.as_str(),
                    "auth_source": source,
                }))?
            );
        } else {
            println!("Not signed in (persona: {}). Run 'sharepoint login'.", persona.display_name());
        }
        return Ok(());
    };
    let diag = diagnose_access_token_scopes(&tok.access_token);
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "authenticated": true,
                "persona": persona.as_str(),
                "user_id": tok.user_id,
                "display_name": tok.user_display_name,
                "email": tok.user_email,
                "tenant_id": tok.tenant_id,
                "client_id": tok.client_id,
                "auth_mode": tok.auth_mode,
                "auth_source": source,
                "expires_at": tok.expires_at,
                "scope_diagnostics": diag,
            }))?
        );
    } else {
        println!("Persona:  {}", persona.display_name());
        println!(
            "User:     {} ({})",
            tok.user_display_name.as_deref().unwrap_or("Unknown"),
            tok.user_email.as_deref().unwrap_or("-")
        );
        println!("Auth:     {} ({})", tok.auth_mode, source);
        if !diag.warnings.is_empty() {
            for w in &diag.warnings {
                println!("Warning:  {w}");
            }
        }
    }
    Ok(())
}

async fn cmd_doctor(config: &AppConfig, persona: Persona, online: bool, as_json: bool) -> Result<()> {
    let auth = AuthManager::new(config.clone())?;
    let tokens = auth.current_tokens().await;
    let user_id = tokens.as_ref().and_then(|t| t.user_id.clone());
    let caps = img_preview::detect_graphics_caps(config.image_protocol);
    let mut online_probe = json!(null);
    if online {
        if let Ok(graph) = GraphClient::new(config.clone(), auth.clone()) {
            let me_ok = graph.get_me().await.is_ok();
            let drive_ok = graph.get_my_drive().await.is_ok();
            let sites_ok = graph.list_sites(Some("*"), 5).await.is_ok();
            online_probe = json!({
                "me": me_ok,
                "onedrive": drive_ok,
                "sharepoint_sites": sites_ok,
            });
        }
    }
    let info = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "persona": persona.as_str(),
        "config_file": config::config_file_path(),
        "db_path": config::db_path(user_id.as_deref()),
        "download_dir": config.resolved_download_dir(),
        "graph_base_url": config.graph_base_url,
        "serve_port_default": config::DEFAULT_SERVE_PORT,
        "authenticated": tokens.is_some(),
        "auth_source": auth_source_label(tokens.as_ref(), config),
        "image_protocol": caps.protocol.as_str(),
        "online_probe": online_probe,
    });
    if as_json {
        println!("{}", serde_json::to_string_pretty(&info)?);
    } else {
        println!("@shinick/sharepoint v{} ({})", env!("CARGO_PKG_VERSION"), persona.display_name());
        println!("Config:         {}", config::config_file_path().display());
        println!("SQLite DB:      {}", config::db_path(user_id.as_deref()).display());
        println!("Download dir:   {}", config.resolved_download_dir().display());
        println!("Graph URL:      {}", config.graph_base_url);
        println!("Authenticated:  {}", tokens.is_some());
        println!("Image protocol: {}", caps.protocol.label());
        if online {
            println!("Online probe:   {online_probe}");
        }
    }
    Ok(())
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
    if !refresh && (offline || cached) && search.is_none() {
        let local = storage.list_sites()?;
        if !local.is_empty() || offline {
            return Ok(local.into_iter().take(limit).collect());
        }
    }
    let mut combined = if search.is_none() {
        graph.list_followed_sites().await.unwrap_or_default()
    } else {
        Vec::new()
    };
    let discovered = graph.list_sites(search, limit).await?;
    for s in discovered {
        if !combined.iter().any(|existing| existing.id == s.id) {
            combined.push(s);
        }
    }
    let _ = storage.upsert_sites(&combined);
    Ok(combined.into_iter().take(limit).collect())
}

pub async fn fetch_drives(
    graph: &GraphClient,
    storage: &Storage,
    site_query: Option<&str>,
    refresh: bool,
    offline: bool,
    cached: bool,
) -> Result<Vec<DriveInfo>> {
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
                return Ok(local);
            }
        }
        let drives = graph
            .list_site_drives(&site.id, Some(site.effective_name()))
            .await?;
        let _ = storage.upsert_drives(&drives);
        return Ok(drives);
    }

    if !refresh && (offline || cached) {
        let local = storage.list_drives(None)?;
        if !local.is_empty() || offline {
            return Ok(local);
        }
    }

    let mut all = Vec::new();
    if let Ok(me) = graph.get_my_drive().await {
        all.push(me);
    }
    let sites = fetch_sites(graph, storage, None, 20, false, false, false)
        .await
        .unwrap_or_default();
    for s in sites.iter().take(10) {
        if let Ok(sd) = graph.list_site_drives(&s.id, Some(s.effective_name())).await {
            for d in sd {
                if !all.iter().any(|existing| existing.id == d.id) {
                    all.push(d);
                }
            }
        }
    }
    let _ = storage.upsert_drives(&all);
    Ok(all)
}

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
    let target = path_or_id
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "/" && *s != "root");

    if !refresh && (offline || cached) {
        let local = match target {
            None => storage.list_folder_children(drive_id, None, Some(""), limit)?,
            Some(p) if p.starts_with('/') || p.contains('/') => {
                let norm = format!("/{}", p.trim_matches('/'));
                storage.list_folder_children(drive_id, None, Some(&norm), limit)?
            }
            Some(id) => storage.list_folder_children(drive_id, Some(id), None, limit)?,
        };
        if !local.is_empty() || offline {
            return Ok(local);
        }
    }

    let items = match target {
        None => {
            let live = graph.list_children(drive_id, None, Some(limit)).await?;
            let _ = storage.replace_folder_children(drive_id, None, &live);
            live
        }
        Some(p) if p.starts_with('/') || p.contains('/') => {
            let live = graph.list_children_by_path(drive_id, p, Some(limit)).await?;
            let _ = storage.upsert_drive_items(&live);
            live
        }
        Some(id_or_name) => {
            // Try direct item ID children first, fall back to path-based children
            match graph.list_children(drive_id, Some(id_or_name), Some(limit)).await {
                Ok(live) => {
                    let _ = storage.replace_folder_children(drive_id, Some(id_or_name), &live);
                    live
                }
                Err(_) => {
                    let live = graph
                        .list_children_by_path(drive_id, id_or_name, Some(limit))
                        .await?;
                    let _ = storage.upsert_drive_items(&live);
                    live
                }
            }
        }
    };
    Ok(items)
}

pub async fn search_items_combined(
    graph: &GraphClient,
    storage: &Storage,
    query: &str,
    drive_id: Option<&str>,
    remote: bool,
    limit: usize,
) -> Result<Vec<DriveItem>> {
    let mut results: Vec<DriveItem> = storage
        .search_items(query, drive_id, limit)?
        .into_iter()
        .map(|h| h.item)
        .collect();

    if remote {
        let remote_items = if let Some(did) = drive_id {
            graph.search_drive(did, query, limit).await?
        } else {
            match graph.search_across_sites(query, limit).await {
                Ok(items) => items,
                Err(_) => {
                    // Fallback to personal drive search when `/search/query` is unavailable
                    if let Ok(me) = graph.get_my_drive().await {
                        graph.search_drive(&me.id, query, limit).await.unwrap_or_default()
                    } else {
                        Vec::new()
                    }
                }
            }
        };
        let _ = storage.upsert_drive_items(&remote_items);
        for item in remote_items {
            if !results
                .iter()
                .any(|existing| existing.drive_id == item.drive_id && existing.id == item.id)
            {
                results.push(item);
            }
        }
    }
    results.truncate(limit);
    Ok(results)
}

fn build_tree<'a>(
    graph: &'a GraphClient,
    storage: &'a Storage,
    drive_id: &'a str,
    parent_id: Option<&'a str>,
    depth: usize,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<TreeNode>>> + Send + 'a>> {
    Box::pin(async move {
        let items = graph.list_children(drive_id, parent_id, Some(200)).await?;
        let _ = storage.upsert_drive_items(&items);
        let mut nodes = Vec::with_capacity(items.len());
        for item in items {
            let children = if item.is_folder && depth > 1 {
                build_tree(graph, storage, drive_id, Some(&item.id), depth - 1)
                    .await
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            nodes.push(TreeNode { item, children });
        }
        Ok(nodes)
    })
}

fn print_tree_nodes(nodes: &[TreeNode], prefix: &str) {
    for (idx, node) in nodes.iter().enumerate() {
        let is_last = idx + 1 == nodes.len();
        let branch = if is_last { "└── " } else { "├── " };
        let suffix = if node.item.is_folder { "/" } else { "" };
        println!("{prefix}{branch}{}{suffix}", node.item.name);
        if !node.children.is_empty() {
            let next_prefix = format!("{prefix}{}", if is_last { "    " } else { "│   " });
            print_tree_nodes(&node.children, &next_prefix);
        }
    }
}

fn render_image_to_stdout(
    config: &AppConfig,
    bytes: &[u8],
    label: &str,
    cols: Option<u16>,
    rows: Option<u16>,
    protocol: Option<&str>,
) -> Result<()> {
    let pref = protocol
        .and_then(config::ImageProtocolPreference::parse_str)
        .unwrap_or(config.image_protocol);
    let out = img_preview::render_ansi_stdout_with_pref(
        bytes,
        cols.unwrap_or(80),
        rows.unwrap_or(28),
        pref,
    )?;
    print!("{out}");
    let _ = io::stdout().flush();
    eprintln!("{label}");
    Ok(())
}

fn print_daemon_status(status: &crate::daemon::DaemonStatus, as_json: bool) -> Result<()> {
    if as_json {
        println!("{}", serde_json::to_string_pretty(status)?);
    } else if status.running {
        println!(
            "Daemon running (pid={:?}, transport={:?}, clients={}, summary={})",
            status.pid, status.transport, status.attached_clients, status.sync_summary
        );
    } else {
        println!("Daemon is not running.");
    }
    Ok(())
}

pub fn auth_source_label(tokens: Option<&StoredTokens>, config: &AppConfig) -> String {
    if std::env::var("SHAREPOINT_ACCESS_TOKEN")
        .ok()
        .is_some_and(|v| !v.trim().is_empty())
    {
        return "env:SHAREPOINT_ACCESS_TOKEN".to_string();
    }
    if let Some(tf) = config.resolved_token_file() {
        return format!("token_file:{}", tf.display());
    }
    match tokens {
        Some(t) => format!("stored:{}", t.auth_mode),
        None => "none".to_string(),
    }
}

pub fn is_forbidden_error(err: &anyhow::Error) -> bool {
    let lower = format!("{err:#}").to_ascii_lowercase();
    lower.contains("403")
        || lower.contains("forbidden")
        || lower.contains("accessdenied")
        || lower.contains("insufficient")
}

pub async fn wrap_graph_permission_error(
    graph: &GraphClient,
    err: anyhow::Error,
) -> anyhow::Error {
    let raw = format!("{err:#}");
    let tok = graph
        .auth()
        .current_tokens()
        .await
        .map(|t| t.access_token);
    if let Some(explained) = explain_graph_permission_error(&raw, tok.as_deref()) {
        return anyhow::anyhow!("{explained}\nCause: {raw}");
    }
    err
}

pub fn normalize_download_dir(config: &AppConfig, override_dir: Option<&str>) -> PathBuf {
    match override_dir.map(str::trim).filter(|s| !s.is_empty()) {
        Some(d) => Path::new(d).to_path_buf(),
        None => config.resolved_download_dir(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_all_sharepoint_onedrive_cli_subcommands() {
        let c1 = Cli::try_parse_from(["sharepoint", "sites", "--json"]).unwrap();
        assert!(matches!(c1.command, Some(Command::Sites { json: true, .. })));

        let c2 = Cli::try_parse_from(["onedrive", "ls", "/Documents", "--json"]).unwrap();
        assert!(matches!(c2.command, Some(Command::Ls { json: true, .. })));

        let c3 = Cli::try_parse_from(["sp", "download", "item-1", "--dir", "/tmp", "--json"]).unwrap();
        assert!(matches!(c3.command, Some(Command::Download { json: true, .. })));

        let c4 = Cli::try_parse_from(["od", "rm", "item-1", "--yes", "--json"]).unwrap();
        assert!(matches!(c4.command, Some(Command::Rm { yes: true, json: true, .. })));

        let c5 = Cli::try_parse_from(["sharepoint", "serve"]).unwrap();
        assert!(matches!(
            c5.command,
            Some(Command::Serve {
                port: config::DEFAULT_SERVE_PORT,
                ..
            })
        ));
    }
}
