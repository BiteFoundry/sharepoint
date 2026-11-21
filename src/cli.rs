//! CLI parser, command dispatcher, and shared domain helpers for SharePoint and OneDrive.

use std::{
    ffi::OsString,
    io::{self, Write},
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use serde_json::{json, Value};

use crate::{
    auth::{
        diagnose_access_token_scopes, explain_graph_permission_error, read_external_token_file,
        AuthManager, LoginOptions, StoredTokens,
    },
    config::{self, AppConfig, Persona},
    graph::{
        drives::{ConflictBehavior, UploadOptions},
        media::decode_text_file,
        GraphClient,
    },
    img_preview,
    models::{
        format_human_size, DownloadBaseline, DownloadedFile, DriveInfo, DriveItem, SharePointSite,
        TreeNode,
    },
    resolver,
    serve::{self, ServeOptions},
    storage::Storage,
    tui, updater,
};

pub const DEFAULT_TEXT_PREVIEW_MAX_BYTES: usize = 256 * 1024;
const DELETE_JSON_REQUIRES_YES: &str = "Deleting with --json requires passing --yes.";

const ENV_AFTER_HELP: &str = "\
Dual-Persona Launchers:
  sharepoint                            Open SharePoint sites and document libraries
  bitefoundry-onedrive                   Open personal OneDrive

JSON Output (--json):
  login, logout, whoami, doctor, sites, drives, ls, tree, info, cat, download,
  upload, mkdir, mv, rm, search, share, recent, shared, update, lists items/columns/set-field,
  daemon start/status/stop/restart

Environment Variables:
  SHAREPOINT_PERSONA=<sharepoint|onedrive> Override the sharepoint command's default persona
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
    /// Read SharePoint List rows and columns, or update one field
    Lists {
        /// Print JSON output
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        action: ListAction,
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
        /// Return compact drive and item fields in JSON
        #[arg(long, requires = "json")]
        compact: bool,
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
        /// Fetch at most this many versions with --versions (default: all)
        #[arg(long, requires = "versions")]
        versions_limit: Option<usize>,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
        /// Return compact item fields, including its eTag, in JSON
        #[arg(long, requires = "json")]
        compact: bool,
    },
    /// Print UTF-8 or BOM-marked UTF-16 text up to a byte cap
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
    /// Upload a local file (session for files >= 4 MiB or conditional replacement)
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
        /// Optional exact target item ID to replace (works at any size, with or without --if-match)
        #[arg(long = "item-id", value_name = "ITEM_ID")]
        item_id: Option<String>,
        /// ETag precondition (`If-Match`) for replacement via an upload session at any size
        #[arg(long = "if-match", value_name = "ETAG")]
        if_match: Option<String>,
        /// Conflict behavior when target file already exists (`replace`, `fail`, `rename`)
        #[arg(long, value_parser = ["replace", "fail", "rename"])]
        conflict: Option<String>,
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

#[derive(Args, Debug)]
pub struct ListTarget {
    /// SharePoint site ID
    #[arg(long)]
    site_id: String,
    /// SharePoint list ID
    #[arg(long)]
    list_id: String,
}

#[derive(Subcommand, Debug)]
pub enum ListAction {
    /// List rows and their field values across all Graph pages
    Items {
        #[command(flatten)]
        target: ListTarget,
        /// Internal column name to return. Repeat to select multiple fields
        #[arg(long = "field")]
        fields: Vec<String>,
    },
    /// Discover column names, display names, and read-only flags
    Columns {
        #[command(flatten)]
        target: ListTarget,
    },
    /// Set one column by its internal name
    SetField {
        #[command(flatten)]
        target: ListTarget,
        /// Row ID
        #[arg(long)]
        item_id: String,
        /// Internal column name
        #[arg(long)]
        field: String,
        /// JSON value such as '"Ready"', false, or 0
        #[arg(long, value_name = "JSON", allow_hyphen_values = true, value_parser = parse_list_field_value)]
        value_json: Value,
        /// Apply only if this row ETag is current
        #[arg(long, value_name = "ETAG")]
        if_match: Option<String>,
    },
}

fn parse_list_field_value(input: &str) -> std::result::Result<Value, serde_json::Error> {
    serde_json::from_str(input)
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

fn is_hyphen_literal_value(s: &str) -> bool {
    let Some(rest) = s.strip_prefix('-') else {
        return false;
    };
    if rest.is_empty() {
        return true;
    }
    if s == "-h" {
        return false;
    }
    if s.contains('\n') || s.contains('\r') {
        return true;
    }
    if let Some(after_double) = rest.strip_prefix('-') {
        if after_double.is_empty() {
            return false;
        }
        return !after_double.as_bytes()[0].is_ascii_alphanumeric();
    }
    true
}

fn normalize_hyphen_option_arg(
    args: Vec<OsString>,
    target_flag: &str,
    other_value_flags: &[&str],
) -> Vec<OsString> {
    let mut out = Vec::with_capacity(args.len());
    out.push(args[0].clone());
    out.push(args[1].clone());

    let mut i = 2;
    while i < args.len() {
        let token = &args[i];
        let Some(text) = token.to_str() else {
            out.push(token.clone());
            i += 1;
            continue;
        };
        if text == "--" {
            out.extend_from_slice(&args[i..]);
            break;
        }
        if other_value_flags.contains(&text) {
            out.push(token.clone());
            if let Some(next) = args.get(i + 1) {
                out.push(next.clone());
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if text == target_flag {
            if let Some(next) = args.get(i + 1) {
                if next.to_str().is_some_and(is_hyphen_literal_value) {
                    let mut combined =
                        OsString::with_capacity(target_flag.len() + 1 + next.len());
                    combined.push(target_flag);
                    combined.push("=");
                    combined.push(next);
                    out.push(combined);
                    i += 2;
                    continue;
                }
            }
        }
        out.push(token.clone());
        i += 1;
    }
    out
}

fn normalize_single_hyphen_positional(
    args: Vec<OsString>,
    value_flags: &[&str],
) -> Vec<OsString> {
    let mut out = Vec::with_capacity(args.len() + 1);
    out.push(args[0].clone());
    out.push(args[1].clone());

    let mut deferred_positional: Option<OsString> = None;
    let mut seen_positional = false;
    let mut saw_double_dash = false;

    let mut i = 2;
    while i < args.len() {
        let token = &args[i];
        let Some(text) = token.to_str() else {
            seen_positional = true;
            out.push(token.clone());
            i += 1;
            continue;
        };
        if text == "--" {
            saw_double_dash = true;
            out.push(token.clone());
            if let Some(pos) = deferred_positional.take() {
                out.push(pos);
            }
            out.extend_from_slice(&args[i + 1..]);
            break;
        }
        if value_flags.contains(&text) {
            out.push(token.clone());
            if let Some(next) = args.get(i + 1) {
                out.push(next.clone());
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if text.starts_with('-') {
            if !seen_positional && is_hyphen_literal_value(text) {
                deferred_positional = Some(token.clone());
                seen_positional = true;
            } else {
                out.push(token.clone());
            }
            i += 1;
            continue;
        }
        seen_positional = true;
        out.push(token.clone());
        i += 1;
    }

    if let Some(pos) = deferred_positional {
        if !saw_double_dash {
            out.push(OsString::from("--"));
        }
        out.push(pos);
    }
    out
}

fn normalize_hyphen_text_args(args: Vec<OsString>) -> Vec<OsString> {
    if args.len() < 3 {
        return args;
    }
    let Some(subcmd) = args[1].to_str() else {
        return args;
    };
    match subcmd {
        "mv" => normalize_hyphen_option_arg(args, "--name", &["--drive", "--site", "--parent"]),
        "upload" | "up" => normalize_hyphen_option_arg(
            args,
            "--name",
            &["--drive", "--site", "--item-id", "--if-match", "--conflict"],
        ),
        "mkdir" => normalize_single_hyphen_positional(args, &["--drive", "--site", "--parent"]),
        "search" => {
            normalize_single_hyphen_positional(args, &["--drive", "--site", "--limit", "--top"])
        }
        _ => args,
    }
}

pub fn parse_cli_from<I, T>(args: I, bin_name: &'static str) -> std::result::Result<Cli, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let normalized = normalize_hyphen_text_args(args.into_iter().map(Into::into).collect());
    let mut matches = Cli::command()
        .name(bin_name)
        .bin_name(bin_name)
        .try_get_matches_from(normalized)?;
    Cli::from_arg_matches_mut(&mut matches)
}

pub async fn run_from_env() -> Result<()> {
    let persona = Persona::detect();
    let cli = parse_cli_from(std::env::args_os(), persona.bin_name())
        .unwrap_or_else(|err| err.exit());
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
        Some(Command::Lists { json, action }) => run_list_action(&config, action, json).await,
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
                let mut output = folder_listing_json(&target_drive, &items, compact);
                output["path"] = json!(path_or_id.as_deref().unwrap_or("/"));
                println!("{}", serde_json::to_string_pretty(&output)?);
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
            versions_limit,
            offline,
            json: as_json,
            compact,
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
                match graph
                    .list_item_versions_with_limit(
                        item.effective_drive_id(),
                        item.effective_item_id(),
                        versions_limit,
                    )
                    .await
                {
                    Ok(versions) => versions,
                    Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
                }
            } else {
                Vec::new()
            };
            if as_json {
                let mut output = file_info_json(&item, compact);
                output["versions"] = json!(vers);
                println!("{}", serde_json::to_string_pretty(&output)?);
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
                io::stdout().lock().write_all(text.as_bytes())?;
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
            item_id,
            if_match,
            conflict,
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
                resolver::resolve_item_for_write(&graph, &storage, &target_drive.id, &item_or_path)
                    .await?;
            let dest_parent_id = match parent.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                Some(p) if p == "/" || p.eq_ignore_ascii_case("root") => Some("root".to_string()),
                Some(p) => Some(
                    resolver::resolve_item_for_write(&graph, &storage, &target_drive.id, p)
                        .await?
                        .id,
                ),
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
                resolver::resolve_item_for_write(&graph, &storage, &target_drive.id, &item_or_path)
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

async fn run_list_action(config: &AppConfig, action: ListAction, as_json: bool) -> Result<()> {
    let (_, graph, _) = init_runtime(config, false).await?;
    let outcome: Result<()> = async {
        match action {
            ListAction::Items { target, fields } => {
                let items = graph
                    .list_items(&target.site_id, &target.list_id, &fields)
                    .await?;
                if as_json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({ "items": items }))?
                    );
                } else {
                    for item in items {
                        println!(
                            "{}  {}  {}",
                            item.id,
                            item.e_tag.as_deref().unwrap_or(""),
                            serde_json::to_string(&item.fields)?
                        );
                    }
                }
            }
            ListAction::Columns { target } => {
                let columns = graph.list_columns(&target.site_id, &target.list_id).await?;
                if as_json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({ "columns": columns }))?
                    );
                } else {
                    for column in columns {
                        let display = column.display_name.as_deref().unwrap_or(&column.name);
                        let access = if column.read_only == Some(true) {
                            "  (read-only)"
                        } else {
                            ""
                        };
                        println!("{}  {display}{access}", column.name);
                    }
                }
            }
            ListAction::SetField {
                target,
                item_id,
                field,
                value_json,
                if_match,
            } => {
                let update = graph
                    .update_list_item_field(
                        &target.site_id,
                        &target.list_id,
                        &item_id,
                        &field,
                        value_json,
                        if_match.as_deref(),
                    )
                    .await?;
                if as_json {
                    println!("{}", serde_json::to_string_pretty(&update)?);
                } else {
                    println!("Updated {} on row {}.", update.field, update.item_id);
                }
            }
        }
        Ok(())
    }
    .await;
    match outcome {
        Ok(()) => Ok(()),
        Err(err) => Err(wrap_graph_permission_error(&graph, err).await),
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
        println!("@bitefoundry/sharepoint v{} ({})", env!("CARGO_PKG_VERSION"), persona.display_name());
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
    let (authoritative_follow, mut combined) = if clean_search.is_none() {
        match graph.list_followed_sites().await {
            Ok(followed) => (true, followed),
            Err(_) => (false, Vec::new()),
        }
    } else {
        (false, Vec::new())
    };
    let discovered = graph.list_sites(search, limit).await?;
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

pub(crate) fn folder_listing_json(drive: &DriveInfo, items: &[DriveItem], compact: bool) -> Value {
    if compact {
        let items: Vec<_> = items.iter().map(compact_drive_item).collect();
        json!({ "drive": { "id": drive.id }, "items": items })
    } else {
        json!({ "drive": drive, "items": items })
    }
}

pub(crate) fn file_info_json(item: &DriveItem, compact: bool) -> Value {
    let item = if compact {
        let mut summary = compact_drive_item(item);
        summary["e_tag"] = json!(item.e_tag);
        summary
    } else {
        json!(item)
    };
    json!({ "item": item })
}

fn compact_drive_item(item: &DriveItem) -> Value {
    json!({
        "drive_id": item.drive_id,
        "id": item.id,
        "name": item.name,
        "is_folder": item.is_folder,
        "size": item.size,
        "last_modified_at": item.last_modified_at,
    })
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
    if limit == 0 {
        return Ok(Vec::new());
    }
    let target = path_or_id
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "/" && *s != "root");

    if !refresh && (offline || cached) {
        let local = match target {
            None => storage.list_folder_children(drive_id, None, Some(""), limit)?,
            Some(p) if p.starts_with('/') || p.contains('/') => {
                let norm = format!("/{}", p.trim_matches('/'));
                let by_path = storage.list_folder_children(drive_id, None, Some(&norm), limit)?;
                if !by_path.is_empty() {
                    by_path
                } else if let Some(folder) = storage.get_drive_item_by_path(drive_id, &norm)? {
                    storage.list_folder_children(drive_id, Some(&folder.id), None, limit)?
                } else {
                    Vec::new()
                }
            }
            Some(id) => {
                let by_id = storage.list_folder_children(drive_id, Some(id), None, limit)?;
                if !by_id.is_empty() {
                    by_id
                } else {
                    let norm = format!("/{id}");
                    let by_path =
                        storage.list_folder_children(drive_id, None, Some(&norm), limit)?;
                    if !by_path.is_empty() {
                        by_path
                    } else if let Some(folder) = storage.get_drive_item_by_path(drive_id, &norm)? {
                        storage.list_folder_children(drive_id, Some(&folder.id), None, limit)?
                    } else {
                        Vec::new()
                    }
                }
            }
        };
        if !local.is_empty() || offline {
            return Ok(local);
        }
    }

    let items = match target {
        None => {
            let page = graph
                .list_children_detailed(drive_id, None, Some(limit))
                .await?;
            if page.complete {
                let _ = storage.replace_folder_children(drive_id, None, &page.items);
            } else {
                let _ = storage.upsert_drive_items(&page.items);
            }
            page.items
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
            page.items
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
                    page.items
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
                    page.items
                }
                Err(err) => return Err(err),
            }
        }
    };
    Ok(items)
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

fn build_tree<'a>(
    graph: &'a GraphClient,
    storage: &'a Storage,
    drive_id: &'a str,
    parent_id: Option<&'a str>,
    depth: usize,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<TreeNode>>> + Send + 'a>> {
    Box::pin(async move {
        let page = graph
            .list_children_detailed(drive_id, parent_id, Some(200))
            .await?;
        if page.complete {
            let _ = storage.replace_folder_children(drive_id, parent_id, &page.items);
        } else {
            let _ = storage.upsert_drive_items(&page.items);
        }
        let items = page.items;
        let mut nodes = Vec::with_capacity(items.len());
        for item in items {
            let children = if item.is_folder && depth > 1 {
                build_tree(graph, storage, drive_id, Some(&item.id), depth - 1)
                    .await
                    .with_context(|| {
                        format!(
                            "Failed to list folder '{}' (id: {})",
                            item.display_path(),
                            item.id
                        )
                    })?
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
    fn compact_and_version_limit_require_their_output_options() {
        assert!(Cli::try_parse_from(["sharepoint", "ls", "--compact"]).is_err());
        assert!(Cli::try_parse_from(["sharepoint", "info", "file", "--versions-limit", "2"]).is_err());
        let parsed = Cli::try_parse_from([
            "onedrive", "info", "file", "--json", "--compact", "--versions", "--versions-limit", "2",
        ]).unwrap();
        assert!(matches!(parsed.command, Some(Command::Info {
            json: true, compact: true, versions: true, versions_limit: Some(2), ..
        })));
    }

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

    #[test]
    fn parses_hyphen_filenames_search_and_negative_json_for_both_launchers() {
        for bin in ["sharepoint", "bitefoundry-onedrive"] {
            for sample in ["-report.md", "- 목록 항목", "- first\n- second"] {
                let mv = parse_cli_from([bin, "mv", "item-1", "--name", sample, "--json"], bin)
                    .unwrap();
                assert!(matches!(
                    mv.command,
                    Some(Command::Mv {
                        ref item_or_path,
                        ref name,
                        json: true,
                        ..
                    }) if item_or_path == "item-1" && name.as_deref() == Some(sample)
                ));

                for upload_cmd in ["upload", "up"] {
                    let up = parse_cli_from(
                        [bin, upload_cmd, "test.txt", "--name", sample, "--json"],
                        bin,
                    )
                    .unwrap();
                    assert!(matches!(
                        up.command,
                        Some(Command::Upload {
                            ref local_path,
                            ref name,
                            json: true,
                            ..
                        }) if local_path == Path::new("test.txt") && name.as_deref() == Some(sample)
                    ));
                }

                let mk = parse_cli_from(
                    [bin, "mkdir", sample, "--parent", "/Archive", "--json"],
                    bin,
                )
                .unwrap();
                assert!(matches!(
                    mk.command,
                    Some(Command::Mkdir {
                        ref name,
                        ref parent,
                        json: true,
                        ..
                    }) if name == sample && parent.as_deref() == Some("/Archive")
                ));

                let sr = parse_cli_from(
                    [bin, "search", sample, "--remote", "--top", "15", "--json"],
                    bin,
                )
                .unwrap();
                assert!(matches!(
                    sr.command,
                    Some(Command::Search {
                        ref query,
                        remote: true,
                        limit: 15,
                        json: true,
                        ..
                    }) if query == sample
                ));
            }

            // Control case: lists set-field --value-json -4 keeps JSON number -4.
            let list_num = parse_cli_from(
                [
                    bin,
                    "lists",
                    "set-field",
                    "--site-id",
                    "site",
                    "--list-id",
                    "list",
                    "--item-id",
                    "item",
                    "--field",
                    "score",
                    "--value-json",
                    "-4",
                    "--json",
                ],
                bin,
            )
            .unwrap();
            assert!(matches!(
                list_num.command,
                Some(Command::Lists {
                    json: true,
                    action: ListAction::SetField { ref value_json, .. },
                }) if value_json == &json!(-4)
            ));
        }
    }

    #[test]
    fn preserves_equals_positional_separator_known_flags_missing_values_and_help() {
        use clap::error::ErrorKind;

        // Equals syntax accepts explicit flag-looking text.
        for literal in ["-report.md", "--json", "--dry-run", "--help", "-h"] {
            let eq = format!("--name={literal}");
            let mv = parse_cli_from(["sharepoint", "mv", "id", &eq], "sharepoint").unwrap();
            assert!(matches!(
                mv.command,
                Some(Command::Mv { ref name, json: false, .. }) if name.as_deref() == Some(literal)
            ));
            let up =
                parse_cli_from(["sharepoint", "upload", "test.txt", &eq], "sharepoint").unwrap();
            assert!(matches!(
                up.command,
                Some(Command::Upload { ref name, json: false, .. }) if name.as_deref() == Some(literal)
            ));
        }

        // Positional "--" accepts explicit flag-looking text.
        for literal in ["-report.md", "- 목록 항목", "--json", "--dry-run", "--help", "-h"] {
            let mk =
                parse_cli_from(["sharepoint", "mkdir", "--", literal], "sharepoint").unwrap();
            assert!(matches!(
                mk.command,
                Some(Command::Mkdir { ref name, json: false, .. }) if name == literal
            ));
            let sr =
                parse_cli_from(["sharepoint", "search", "--", literal], "sharepoint").unwrap();
            assert!(matches!(
                sr.command,
                Some(Command::Search { ref query, json: false, .. }) if query == literal
            ));
        }

        // Known flags and unknown long flags must NOT be swallowed as separate values.
        for bad_args in [
            vec!["sharepoint", "mv", "id", "--name"],
            vec!["sharepoint", "mv", "id", "--name", "--json"],
            vec!["sharepoint", "mv", "id", "--name", "--parent", "/Archive"],
            vec!["sharepoint", "mv", "id", "--name", "--dry-run"],
            vec!["sharepoint", "mv", "--parent", "--name", "-report.md", "id"],
            vec!["sharepoint", "mv", "id", "--parent", "-archive"],
            vec!["sharepoint", "upload", "test.txt", "--name"],
            vec!["sharepoint", "upload", "test.txt", "--name", "--json"],
            vec!["sharepoint", "upload", "test.txt", "--name", "--conflict", "replace"],
            vec!["sharepoint", "upload", "test.txt", "--name", "--dry-run"],
            vec!["sharepoint", "upload", "test.txt", "--conflict", "-bad"],
            vec!["sharepoint", "mkdir", "--json"],
            vec!["sharepoint", "mkdir", "--dry-run"],
            vec!["sharepoint", "mkdir", "-report.md", "--dry-run"],
            vec!["sharepoint", "mkdir", "--parent", "-bad", "GoodName"],
            vec!["sharepoint", "search", "--json"],
            vec!["sharepoint", "search", "--remote"],
            vec!["sharepoint", "search", "--dry-run"],
            vec!["sharepoint", "search", "--limit", "-1", "query"],
            vec![
                "sharepoint",
                "lists",
                "set-field",
                "--site-id",
                "s",
                "--list-id",
                "l",
                "--item-id",
                "i",
                "--field",
                "f",
                "--value-json",
                "--json",
            ],
        ] {
            let err = parse_cli_from(bad_args.clone(), "sharepoint")
                .expect_err(&format!("expected parse failure for {bad_args:?}"));
            assert_ne!(
                err.kind(),
                ErrorKind::DisplayHelp,
                "unexpected help exit for {bad_args:?}: {err}"
            );
        }

        // Missing value followed by -h / --help must never parse as a literal filename.
        for help_after_name in [
            vec!["sharepoint", "mv", "id", "--name", "--help"],
            vec!["sharepoint", "mv", "id", "--name", "-h"],
            vec!["sharepoint", "upload", "test.txt", "--name", "--help"],
            vec!["sharepoint", "upload", "test.txt", "--name", "-h"],
        ] {
            assert!(
                parse_cli_from(help_after_name.clone(), "sharepoint").is_err(),
                "must not swallow help flag in {help_after_name:?}"
            );
        }

        // Actual help invocations (standalone and trailing --help after hyphen values).
        for help_args in [
            vec!["sharepoint", "mv", "--help"],
            vec!["sharepoint", "mv", "-h"],
            vec!["sharepoint", "mv", "id", "--name", "-report.md", "--help"],
            vec!["sharepoint", "upload", "--help"],
            vec!["sharepoint", "upload", "-h"],
            vec!["sharepoint", "upload", "test.txt", "--name", "-report.md", "--help"],
            vec!["sharepoint", "mkdir", "--help"],
            vec!["sharepoint", "mkdir", "-h"],
            vec!["sharepoint", "mkdir", "-report.md", "--help"],
            vec!["sharepoint", "search", "--help"],
            vec!["sharepoint", "search", "-h"],
            vec!["sharepoint", "search", "- 목록 항목", "--help"],
            vec![
                "sharepoint",
                "lists",
                "set-field",
                "--site-id",
                "site",
                "--list-id",
                "list",
                "--item-id",
                "item",
                "--field",
                "score",
                "--value-json",
                "-4",
                "--help",
            ],
        ] {
            let err = parse_cli_from(help_args.clone(), "sharepoint")
                .expect_err(&format!("expected DisplayHelp for {help_args:?}"));
            assert_eq!(
                err.kind(),
                ErrorKind::DisplayHelp,
                "expected DisplayHelp for {help_args:?}, got: {err}"
            );
        }
    }
}
