//! CLI parser, command dispatcher, and shared domain helpers for SharePoint and OneDrive.

use std::{
    ffi::OsString,
    io::{self, Write},
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use futures_util::StreamExt;
use serde_json::{json, Value};

use crate::{
    auth::{
        diagnose_access_token_scopes, explain_graph_permission_error, read_external_token_file,
        AuthManager, LoginOptions, StoredTokens,
    },
    config::{self, AppConfig, Persona},
    graph::{
        drives::{explicit_if_match, ConflictBehavior, UploadOptions},
        media::decode_text_file,
        GraphClient,
    },
    img_preview,
    models::{
        format_human_size, DownloadBaseline, DownloadedFile, DriveInfo, DriveItem, SharePointSite,
        TreeNode,
    },
    resolver,
    safe_text::{sanitize_terminal_inline, sanitize_terminal_text},
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
  upload, mkdir, mv, rm, search, share, recent, shared, update, lists ls/items/columns/set-field,
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
        /// Maximum recursion depth (1-5)
        #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u8).range(1..=5))]
        depth: u8,
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
        /// Also fetch and display version history (online only)
        #[arg(long, conflicts_with = "offline")]
        versions: bool,
        /// Fetch at most this many versions with --versions (default: all)
        #[arg(long, requires = "versions")]
        versions_limit: Option<usize>,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
        /// Compatibility alias: compact JSON is already the default
        #[arg(long, requires = "json")]
        compact: bool,
        /// Return every item field in JSON
        #[arg(long, requires = "json", conflicts_with = "compact")]
        full: bool,
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
        /// Address to listen on. A non-loopback address requires --api-key
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        #[arg(long, default_value_t = config::DEFAULT_SERVE_PORT)]
        port: u16,
        /// Bearer key clients must send. Without it a random key is generated at startup
        /// and printed once to stderr
        #[arg(long)]
        api_key: Option<String>,
        /// Serve without any key instead of generating one (loopback binds only)
        #[arg(long, conflicts_with = "api_key")]
        allow_keyless: bool,
        /// Allow local_path uploads and dir downloads on a server running without a key
        #[arg(long)]
        allow_local_paths: bool,
        /// Extra Host header value to accept besides localhost / 127.0.0.1 / ::1
        /// (repeatable), e.g. a reverse-proxy hostname
        #[arg(long = "allowed-host", value_name = "NAME")]
        allowed_hosts: Vec<String>,
        /// Extra browser origin allowed by CORS besides loopback origins (repeatable),
        /// e.g. https://app.example.com
        #[arg(long = "cors-origin", value_name = "ORIGIN")]
        cors_origins: Vec<String>,
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
    /// List SharePoint Lists in a site
    Ls {
        /// SharePoint site ID
        #[arg(long)]
        site_id: String,
    },
    /// List one page of rows and their field values
    Items {
        #[command(flatten)]
        target: ListTarget,
        /// Internal column name to return. Repeat to select multiple fields
        #[arg(long = "field")]
        fields: Vec<String>,
        /// Rows in the first page (1-200)
        #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u16).range(1..=200), conflicts_with = "cursor")]
        limit: u16,
        /// next_cursor from the previous page; pass the same site, list and --field values
        #[arg(long)]
        cursor: Option<String>,
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
        "info" => {
            normalize_single_hyphen_positional(args, &["--drive", "--site", "--versions-limit"])
        }
        "cat" => normalize_single_hyphen_positional(args, &["--drive", "--site", "--max-bytes"]),
        "download" | "dl" => {
            normalize_single_hyphen_positional(args, &["--drive", "--site", "--dir", "--out"])
        }
        "rm" => normalize_single_hyphen_positional(args, &["--drive", "--site"]),
        _ => args,
    }
}

fn args_has_json_flag(args: &[OsString]) -> bool {
    for arg in args.iter().skip(1) {
        if arg == "--" {
            break;
        }
        if arg == "--json" {
            return true;
        }
    }
    false
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
    let raw_args: Vec<OsString> = std::env::args_os().collect();
    let cli = parse_cli_from(raw_args.iter().cloned(), persona.bin_name())
        .unwrap_or_else(|err| err.exit());
    let json_mode = args_has_json_flag(&raw_args);
    let res = run_cli(cli, persona).await;
    if let Err(ref err) = res {
        if json_mode {
            let payload = json_error_value(err);
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
            let mut cfg = config.clone();
            if let Some(tf) = token_file {
                cfg.token_file = tf.to_string_lossy().into_owned();
                let tok = read_external_token_file(&tf, &cfg)?;
                // Only this key: `cfg` also holds whatever the environment set for this run.
                config::persist_config_keys(&[(
                    "token_file",
                    toml::Value::String(cfg.token_file.clone()),
                )])?;
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
                    sanitize_terminal_inline(
                        tokens
                            .user_display_name
                            .as_deref()
                            .or(tokens.user_email.as_deref())
                            .unwrap_or("Microsoft 365 User")
                    ),
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
                        sanitize_terminal_inline(s.effective_name()),
                        sanitize_terminal_inline(s.web_url.as_deref().unwrap_or("")),
                        sanitize_terminal_inline(&s.id)
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
                let safe_text = sanitize_terminal_text(&text);
                io::stdout().lock().write_all(safe_text.as_bytes())?;
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
                    sanitize_terminal_inline(&downloaded.file_name),
                    format_human_size(downloaded.size_bytes),
                    sanitize_terminal_inline(&downloaded.path.display().to_string())
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
            let if_match = explicit_if_match(if_match.as_deref())?;
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
                    sanitize_terminal_inline(&uploaded.name),
                    format_human_size(uploaded.size),
                    sanitize_terminal_inline(&uploaded.display_path()),
                    sanitize_terminal_inline(&uploaded.id)
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
                println!(
                    "Created folder '{}' (id: {})",
                    sanitize_terminal_inline(&folder.display_path()),
                    sanitize_terminal_inline(&folder.id)
                );
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
                    sanitize_terminal_inline(&link.link_type),
                    sanitize_terminal_inline(&link.scope),
                    sanitize_terminal_inline(&link.web_url)
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
                        sanitize_terminal_inline(&item.name),
                        format_human_size(item.size),
                        sanitize_terminal_inline(item.last_modified_by.as_deref().unwrap_or("")),
                        sanitize_terminal_inline(&item.id)
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
            allow_keyless,
            allow_local_paths,
            allowed_hosts,
            cors_origins,
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
                    allow_keyless,
                    allow_local_paths,
                    allowed_hosts,
                    cors_origins,
                },
            )
            .await
        }
        Some(Command::Mcp) => {
            let (_, graph, storage) = init_runtime(&config, true).await?;
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
            ListAction::Ls { site_id } => {
                let lists = graph.list_lists(&site_id).await?;
                if as_json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({
                            "lists": lists,
                            "returned": lists.len(),
                        }))?
                    );
                } else {
                    for list in lists {
                        let display = list.display_name.as_deref().unwrap_or(&list.name);
                        println!(
                            "{}  {}",
                            sanitize_terminal_inline(&list.id),
                            sanitize_terminal_inline(display)
                        );
                    }
                }
            }
            ListAction::Items {
                target,
                fields,
                limit,
                cursor,
            } => {
                let page = graph
                    .list_items_page(
                        &target.site_id,
                        &target.list_id,
                        &fields,
                        usize::from(limit),
                        cursor.as_deref(),
                    )
                    .await?;
                if as_json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({
                            "items": page.items,
                            "returned": page.items.len(),
                            "has_more": page.next_cursor.is_some(),
                            "next_cursor": page.next_cursor,
                        }))?
                    );
                } else {
                    for item in &page.items {
                        let fields_json = serde_json::to_string(&item.fields)?;
                        println!(
                            "{}  {}  {}",
                            sanitize_terminal_inline(&item.id),
                            sanitize_terminal_inline(item.e_tag.as_deref().unwrap_or("")),
                            sanitize_terminal_inline(&fields_json)
                        );
                    }
                    if let Some(next) = &page.next_cursor {
                        eprintln!("More rows: rerun with --cursor {next}");
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
                        println!(
                            "{}  {}{access}",
                            sanitize_terminal_inline(&column.name),
                            sanitize_terminal_inline(display)
                        );
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
                    println!(
                        "Updated {} on row {}.",
                        sanitize_terminal_inline(&update.field),
                        sanitize_terminal_inline(&update.item_id)
                    );
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
            sanitize_terminal_inline(tok.user_display_name.as_deref().unwrap_or("Unknown")),
            sanitize_terminal_inline(tok.user_email.as_deref().unwrap_or("-"))
        );
        println!("Auth:     {} ({})", tok.auth_mode, sanitize_terminal_inline(&source));
        if !diag.warnings.is_empty() {
            for w in &diag.warnings {
                println!("Warning:  {}", sanitize_terminal_inline(w));
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
            let (me_res, drive_res, sites_res) = tokio::join!(
                graph.get_me(),
                graph.get_my_drive(),
                graph.list_sites(Some("*"), 5),
            );
            online_probe = json!({
                "me": me_res.is_ok(),
                "onedrive": drive_res.is_ok(),
                "sharepoint_sites": sites_res.is_ok(),
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

const DRIVE_DISCOVERY_SITE_CAP: usize = 200;

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

pub struct FolderListing {
    pub items: Vec<DriveItem>,
    pub has_more: Option<bool>,
    pub source: &'static str,
}

pub(crate) fn folder_listing_json(
    drive: &DriveInfo,
    listing: &FolderListing,
    compact: bool,
) -> Value {
    if compact {
        let items: Vec<_> = listing.items.iter().map(compact_drive_item).collect();
        json!({
            "drive": { "id": drive.id },
            "items": items,
            "returned": listing.items.len(),
            "has_more": listing.has_more,
            "source": listing.source,
        })
    } else {
        json!({
            "drive": drive,
            "items": listing.items,
            "returned": listing.items.len(),
            "has_more": listing.has_more,
            "source": listing.source,
        })
    }
}

pub(crate) enum DetailHint {
    Cli,
    Mcp,
}

pub(crate) fn file_info_json(
    item: &DriveItem,
    full: bool,
    hint: DetailHint,
    versions_included: bool,
) -> Value {
    let mut all = json!(item);
    all["download_url"] = json!(item.download_url);
    if full {
        return json!({ "item": all });
    }
    let mut summary = compact_drive_item(item);
    summary["e_tag"] = json!(item.e_tag);
    summary["parent_path"] = json!(item.parent_path);
    let omitted_fields: Vec<&str> = all
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(key, value)| {
            key.as_str() != "list_item_fields"
                && summary.get(key.as_str()).is_none()
                && !value.is_null()
        })
        .map(|(key, _)| key.as_str())
        .collect();
    let mut omitted = json!({ "fields": omitted_fields });
    if !item.list_item_fields.is_empty() {
        let custom_cols: Vec<&str> = item.list_item_fields.keys().map(String::as_str).collect();
        omitted["list_item_fields"] = json!(custom_cols);
    }
    if !versions_included {
        omitted["versions"] = Value::Null;
    }
    summary["omitted"] = omitted;
    summary["hint"] = json!(match hint {
        DetailHint::Cli => format!(
            "sharepoint info {} --drive {} --json --full",
            shell_quote(&item.id),
            shell_quote(&item.drive_id)
        ),
        DetailHint::Mcp => format!(
            "Call get_file_info with item_id {:?}, drive {:?} and fields: [...] for specific fields or compact:false for every field.",
            item.id, item.drive_id
        ),
    });
    json!({ "item": summary })
}

fn canonical_drive_item_field(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "id" => Some("id"),
        "drive_id" | "driveid" => Some("drive_id"),
        "parent_id" | "parentid" => Some("parent_id"),
        "parent_path" | "parentpath" => Some("parent_path"),
        "name" => Some("name"),
        "size" => Some("size"),
        "is_folder" | "isfolder" => Some("is_folder"),
        "child_count" | "childcount" => Some("child_count"),
        "mime_type" | "mimetype" => Some("mime_type"),
        "web_url" | "weburl" => Some("web_url"),
        "download_url" | "downloadurl" => Some("download_url"),
        "e_tag" | "etag" => Some("e_tag"),
        "c_tag" | "ctag" => Some("c_tag"),
        "created_at" | "createdat" | "createddatetime" => Some("created_at"),
        "last_modified_at" | "lastmodifiedat" | "lastmodifieddatetime" => Some("last_modified_at"),
        "created_by" | "createdby" => Some("created_by"),
        "last_modified_by" | "lastmodifiedby" => Some("last_modified_by"),
        "shared_scope" | "sharedscope" => Some("shared_scope"),
        "remote_drive_id" | "remotedriveid" => Some("remote_drive_id"),
        "remote_item_id" | "remoteitemid" => Some("remote_item_id"),
        "description" => Some("description"),
        "sha256_hash" | "sha256hash" | "sha256" => Some("sha256_hash"),
        "quick_xor_hash" | "quickxorhash" => Some("quick_xor_hash"),
        _ => None,
    }
}

pub(crate) fn file_info_fields_json(item: &DriveItem, requested_fields: &[String]) -> Value {
    let mut all = json!(item);
    all["download_url"] = json!(item.download_url);

    let mut summary = compact_drive_item(item);
    summary["e_tag"] = json!(item.e_tag);
    summary["parent_path"] = json!(item.parent_path);

    let mut include_all_list_fields = false;
    let mut selected_list_fields: std::collections::BTreeMap<String, Value> =
        std::collections::BTreeMap::new();
    let mut missing_fields: Vec<String> = Vec::new();
    let mut push_missing = |raw: &str| {
        if !missing_fields.iter().any(|m| m.eq_ignore_ascii_case(raw)) {
            missing_fields.push(raw.to_string());
        }
    };

    for raw in requested_fields {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let lower = trimmed.to_ascii_lowercase();
        if lower == "hashes" {
            let mut hashes = serde_json::Map::new();
            if let Some(ref h) = item.sha256_hash {
                hashes.insert("sha256_hash".to_string(), json!(h));
            }
            if let Some(ref h) = item.quick_xor_hash {
                hashes.insert("quick_xor_hash".to_string(), json!(h));
            }
            if hashes.is_empty() {
                push_missing(trimmed);
            } else {
                summary["hashes"] = Value::Object(hashes);
            }
            continue;
        }
        if lower == "list_item_fields" || lower == "listitemfields" {
            if item.list_item_fields.is_empty() {
                push_missing(trimmed);
            } else {
                include_all_list_fields = true;
            }
            continue;
        }
        if let Some(canon) = canonical_drive_item_field(trimmed) {
            if let Some(val) = all.get(canon).filter(|v| !v.is_null()) {
                summary[canon] = val.clone();
                continue;
            }
            if let Some((k, v)) = item
                .list_item_fields
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(trimmed))
            {
                selected_list_fields.insert(k.clone(), v.clone());
                continue;
            }
            push_missing(trimmed);
            continue;
        }
        if let Some((k, v)) = item
            .list_item_fields
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(trimmed))
        {
            selected_list_fields.insert(k.clone(), v.clone());
        } else {
            push_missing(trimmed);
        }
    }

    if include_all_list_fields {
        summary["list_item_fields"] = json!(item.list_item_fields);
    } else if !selected_list_fields.is_empty() {
        summary["list_item_fields"] = json!(selected_list_fields);
    }

    if !missing_fields.is_empty() {
        summary["missing_fields"] = json!(missing_fields);
    }

    json!({ "item": summary })
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
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

async fn build_tree(
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

fn print_tree_nodes(nodes: &[TreeNode], prefix: &str) {
    let mut lines = Vec::new();
    tree_lines(nodes, prefix, &mut lines);
    for line in lines {
        println!("{line}");
    }
}

/// Renders `nodes` as box-drawing tree lines. A folder past `--depth` that still holds items
/// ends in `…`; an expanded folder cut at the per-folder limit gets a note line.
fn tree_lines(nodes: &[TreeNode], prefix: &str, out: &mut Vec<String>) {
    for (idx, node) in nodes.iter().enumerate() {
        let is_last = idx + 1 == nodes.len();
        let branch = if is_last { "└── " } else { "├── " };
        let suffix = if node.item.is_folder { "/" } else { "" };
        let depth_capped =
            node.item.is_folder && node.children.is_empty() && node.has_more == Some(true);
        let cue = if depth_capped { " …" } else { "" };
        out.push(format!(
            "{prefix}{branch}{}{suffix}{cue}",
            sanitize_terminal_inline(&node.item.name)
        ));
        if !node.children.is_empty() {
            let next_prefix = format!("{prefix}{}", if is_last { "    " } else { "│   " });
            tree_lines(&node.children, &next_prefix, out);
            if node.has_more == Some(true) {
                out.push(format!("{next_prefix}… more items not shown (200 per folder)"));
            }
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
    eprintln!("{}", sanitize_terminal_inline(label));
    Ok(())
}

fn print_daemon_status(status: &crate::daemon::DaemonStatus, as_json: bool) -> Result<()> {
    if as_json {
        println!("{}", serde_json::to_string_pretty(status)?);
    } else if status.running {
        println!(
            "Daemon running (pid={:?}, transport={:?}, clients={}, summary={})",
            status.pid,
            status.transport,
            status.attached_clients,
            sanitize_terminal_inline(&status.sync_summary)
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

fn json_error_value(err: &anyhow::Error) -> Value {
    let message = format!("{err:#}");
    let http_status = crate::graph::graph_error_status(err);
    let ambiguous = err.downcast_ref::<resolver::AmbiguousMatch>();

    let code = if message.starts_with("Not logged in") || http_status == Some(401) {
        "not_authenticated"
    } else if ambiguous.is_some() {
        "ambiguous"
    } else if message.contains("Upload conflict (412") || http_status == Some(412) {
        "precondition_failed"
    } else if http_status == Some(403) {
        "forbidden"
    } else if http_status == Some(404) || message.to_ascii_lowercase().contains("not found") {
        "not_found"
    } else {
        "error"
    };

    let mut error = serde_json::Map::new();
    error.insert("code".to_string(), json!(code));
    if let Some(status) = http_status {
        error.insert("http_status".to_string(), json!(status));
    }
    error.insert("message".to_string(), json!(message));
    if let Some(amb) = ambiguous {
        let candidates: Vec<Value> = amb
            .candidates
            .iter()
            .map(|(name, id)| json!({ "name": name, "id": id }))
            .collect();
        error.insert("candidates".to_string(), Value::Array(candidates));
    }

    json!({
        "ok": false,
        "error": Value::Object(error),
    })
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

                let inf = parse_cli_from([bin, "info", sample, "--drive", "d1", "--json"], bin)
                    .unwrap();
                assert!(matches!(
                    inf.command,
                    Some(Command::Info {
                        ref item_or_path,
                        json: true,
                        ..
                    }) if item_or_path == sample
                ));

                let ct = parse_cli_from([bin, "cat", sample, "--max-bytes", "64", "--json"], bin)
                    .unwrap();
                assert!(matches!(
                    ct.command,
                    Some(Command::Cat {
                        ref item_or_path,
                        max_bytes: 64,
                        json: true,
                        ..
                    }) if item_or_path == sample
                ));

                let dl = parse_cli_from([bin, "dl", sample, "--dir", "out", "--json"], bin)
                    .unwrap();
                assert!(matches!(
                    dl.command,
                    Some(Command::Download {
                        ref item_or_path,
                        json: true,
                        ..
                    }) if item_or_path == sample
                ));

                let rm = parse_cli_from([bin, "rm", sample, "--yes", "--json"], bin).unwrap();
                assert!(matches!(
                    rm.command,
                    Some(Command::Rm {
                        ref item_or_path,
                        yes: true,
                        json: true,
                        ..
                    }) if item_or_path == sample
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

    #[tokio::test]
    async fn fetch_sites_discover_drives_and_build_tree_run_concurrently() {
        use axum::{extract::Path as AxumPath, routing::get, Json, Router};
        use std::sync::Arc;
        use tokio::{net::TcpListener, sync::Barrier};

        // 1. fetch_sites overlaps /me/followedSites and /sites?search=*
        let sites_barrier = Arc::new(Barrier::new(2));
        let sb1 = Arc::clone(&sites_barrier);
        let sb2 = Arc::clone(&sites_barrier);
        let sites_router = Router::new()
            .route(
                "/v1.0/me/followedSites",
                get(move || {
                    let b = Arc::clone(&sb1);
                    async move {
                        b.wait().await;
                        Json(json!({
                            "value": [{ "id": "site-a", "name": "alpha", "displayName": "Alpha" }]
                        }))
                    }
                }),
            )
            .route(
                "/v1.0/sites",
                get(move || {
                    let b = Arc::clone(&sb2);
                    async move {
                        b.wait().await;
                        Json(json!({
                            "value": [{ "id": "site-b", "name": "beta", "displayName": "Beta" }]
                        }))
                    }
                }),
            );
        let listener1 = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base1 = format!("http://{}/v1.0", listener1.local_addr().unwrap());
        let srv1 = tokio::spawn(async move { axum::serve(listener1, sites_router).await.unwrap() });

        let make_graph = |base_url: String| {
            let cfg = AppConfig {
                graph_base_url: base_url,
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
            GraphClient::new(cfg, auth).unwrap()
        };

        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::open(&tmp.path().join("cli-conc.db")).unwrap();
        let g1 = make_graph(base1);

        let sites = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            fetch_sites(&g1, &storage, None, 20, true, false, false),
        )
        .await
        .expect("fetch_sites must overlap followedSites and sites discovery")
        .unwrap();
        assert_eq!(sites.len(), 2);
        srv1.abort();

        // 2. discover_drives overlaps /me/drive with fetch_sites, and build_tree overlaps sibling folders
        let disc_barrier = Arc::new(Barrier::new(3));
        let db1 = Arc::clone(&disc_barrier);
        let db2 = Arc::clone(&disc_barrier);
        let db3 = Arc::clone(&disc_barrier);
        let tree_barrier = Arc::new(Barrier::new(2));
        let tb = Arc::clone(&tree_barrier);

        let router2 = Router::new()
            .route(
                "/v1.0/me/drive",
                get(move || {
                    let b = Arc::clone(&db1);
                    async move {
                        b.wait().await;
                        Json(json!({ "id": "drv-me", "name": "OneDrive", "driveType": "business" }))
                    }
                }),
            )
            .route(
                "/v1.0/me/followedSites",
                get(move || {
                    let b = Arc::clone(&db2);
                    async move {
                        b.wait().await;
                        Json(json!({
                            "value": [{ "id": "site-1", "name": "eng", "displayName": "Engineering" }]
                        }))
                    }
                }),
            )
            .route(
                "/v1.0/sites",
                get(move || {
                    let b = Arc::clone(&db3);
                    async move {
                        b.wait().await;
                        Json(json!({ "value": [] }))
                    }
                }),
            )
            .route(
                "/v1.0/sites/site-1/drives",
                get(|| async {
                    Json(json!({
                        "value": [{ "id": "drv-eng", "name": "Documents", "driveType": "documentLibrary" }]
                    }))
                }),
            )
            .route(
                "/v1.0/drives/drv-eng/root/children",
                get(|| async {
                    Json(json!({
                        "value": [
                            { "id": "fld-a", "name": "Alpha", "folder": { "childCount": 1 } },
                            { "id": "fld-b", "name": "Beta", "folder": { "childCount": 1 } }
                        ]
                    }))
                }),
            )
            .route(
                "/v1.0/drives/drv-eng/items/{item_id}/children",
                get(move |AxumPath(item_id): AxumPath<String>| {
                    let b = Arc::clone(&tb);
                    async move {
                        b.wait().await;
                        Json(json!({
                            "value": [{
                                "id": format!("file-in-{item_id}"),
                                "name": format!("{item_id}.txt"),
                                "size": 10,
                                "file": { "mimeType": "text/plain" }
                            }]
                        }))
                    }
                }),
            );
        let listener2 = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base2 = format!("http://{}/v1.0", listener2.local_addr().unwrap());
        let srv2 = tokio::spawn(async move { axum::serve(listener2, router2).await.unwrap() });
        let g2 = make_graph(base2);

        let discovered = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            discover_drives(&g2, &storage, None, true, false, false),
        )
        .await
        .expect("discover_drives must overlap get_my_drive with fetch_sites")
        .unwrap();
        assert_eq!(discovered.drives.len(), 2);

        let (nodes, truncated) = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            build_tree(&g2, &storage, "drv-eng", None, 2),
        )
        .await
        .expect("build_tree must list sibling subfolders concurrently")
        .unwrap();
        assert!(!truncated);
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].item.id, "fld-a");
        assert_eq!(nodes[0].children[0].item.id, "file-in-fld-a");
        assert_eq!(nodes[1].item.id, "fld-b");
        assert_eq!(nodes[1].children[0].item.id, "file-in-fld-b");

        srv2.abort();
    }

    #[tokio::test]
    async fn build_tree_descends_shortcuts_with_effective_ids_and_marks_depth_cut_folders() {
        use axum::{http::StatusCode as AxumStatus, routing::get, Json, Router};
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        use tokio::net::TcpListener;

        // My drive holds a shortcut to a folder shared out of another drive (`remoteItem`).
        // Its children live at /drives/drv-remote/items/remote-fld; the shortcut's own id in
        // drv-me lists nothing the user can read.
        let wrong_calls = Arc::new(AtomicUsize::new(0));
        let wc = Arc::clone(&wrong_calls);
        let router = Router::new()
            .route(
                "/v1.0/drives/drv-me/root/children",
                get(|| async {
                    Json(json!({
                        "value": [{
                            "id": "shortcut-1",
                            "name": "Team Share",
                            "parentReference": { "driveId": "drv-me", "id": "root-me" },
                            "remoteItem": {
                                "id": "remote-fld",
                                "folder": { "childCount": 2 },
                                "parentReference": { "driveId": "drv-remote" }
                            }
                        }]
                    }))
                }),
            )
            .route(
                "/v1.0/drives/drv-me/items/shortcut-1/children",
                get(move || {
                    let wc = Arc::clone(&wc);
                    async move {
                        wc.fetch_add(1, Ordering::SeqCst);
                        (
                            AxumStatus::NOT_FOUND,
                            Json(json!({ "error": { "code": "itemNotFound", "message": "not here" } })),
                        )
                    }
                }),
            )
            .route(
                "/v1.0/drives/drv-remote/items/remote-fld/children",
                get(|| async {
                    Json(json!({
                        "value": [
                            {
                                "id": "deep-fld",
                                "name": "Deep",
                                "folder": { "childCount": 5 },
                                "parentReference": { "driveId": "drv-remote", "id": "remote-fld" }
                            },
                            {
                                "id": "plan-1",
                                "name": "plan.md",
                                "size": 3,
                                "file": { "mimeType": "text/markdown" },
                                "parentReference": { "driveId": "drv-remote", "id": "remote-fld" }
                            }
                        ]
                    }))
                }),
            );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1.0", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

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
        let storage = Storage::open(&tmp.path().join("cli-tree-remote.db")).unwrap();

        let (nodes, truncated) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            build_tree(&graph, &storage, "drv-me", None, 2),
        )
        .await
        .expect("build_tree must not hang")
        .expect("a shortcut folder must be listed through its remote drive and item ids");
        assert!(!truncated);
        assert_eq!(wrong_calls.load(Ordering::SeqCst), 0, "the shortcut's local id was listed");

        assert_eq!(nodes.len(), 1);
        let share = &nodes[0];
        assert_eq!(share.item.effective_drive_id(), "drv-remote");
        assert_eq!(share.item.effective_item_id(), "remote-fld");
        assert_eq!(share.has_more, Some(false));
        let child_ids: Vec<&str> = share.children.iter().map(|n| n.item.id.as_str()).collect();
        assert_eq!(child_ids, ["deep-fld", "plan-1"]);
        let deep = &share.children[0];
        assert!(deep.children.is_empty(), "Deep sits past --depth 2");
        assert_eq!(deep.has_more, Some(true));

        let mut lines = Vec::new();
        tree_lines(&nodes, "", &mut lines);
        assert_eq!(
            lines,
            [
                "└── Team Share/",
                "    ├── Deep/ …",
                "    └── plan.md",
            ],
            "a folder cut by --depth that still holds items ends in …"
        );

        server.abort();
    }
}
