//! CLI argument definitions, subcommands, and hyphen-value normalization.

use std::{ffi::OsString, path::PathBuf};

use anyhow::Result;
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use serde_json::Value;

use crate::config::{self, Persona};

pub const DEFAULT_TEXT_PREVIEW_MAX_BYTES: usize = 256 * 1024;
pub(super) const DELETE_JSON_REQUIRES_YES: &str = "Deleting with --json requires passing --yes.";

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
    pub site_id: String,
    /// SharePoint list ID
    #[arg(long)]
    pub list_id: String,
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

pub(super) fn args_has_json_flag(args: &[OsString]) -> bool {
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

