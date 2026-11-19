//! Application configuration, dual-persona detection (`sharepoint`/`sp` vs `onedrive`/`od`),
//! local paths, and settings (`config.toml`).
//!
//! Shared foundation module: treat public signatures here as stable across all modules.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const APP_DIR_NAME: &str = "sharepoint-cli";
pub const LEGACY_TEAMS_APP_DIR_NAME: &str = "teams-cli";
pub const DEFAULT_GRAPH_BASE_URL: &str = "https://graph.microsoft.com/v1.0";
pub const DEFAULT_AUTH_BASE_URL: &str = "https://login.microsoftonline.com";
/// Default Microsoft multi-tenant public client ID fallback when user does not supply a custom Azure App Client ID.
/// Users can always override via `--client-id`, `SHAREPOINT_CLIENT_ID`, `ONEDRIVE_CLIENT_ID`, `TEAMS_CLIENT_ID`, `AZURE_CLIENT_ID`, or `config.toml`.
pub const DEFAULT_PUBLIC_CLIENT_ID: &str = "14d82eec-204b-4c2f-b7e8-296a70dab67e";
pub const DEFAULT_TENANT_ID: &str = "common";
pub const DEFAULT_GITHUB_REPO: &str = "BiteFoundry/sharepoint";
pub const DEFAULT_SERVE_PORT: u16 = 8790;

/// List density mode in TUI (`Cozy` default vs `Compact`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DensityMode {
    #[default]
    Cozy,
    Compact,
}

impl DensityMode {
    pub fn toggle(self) -> Self {
        match self {
            Self::Cozy => Self::Compact,
            Self::Compact => Self::Cozy,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Cozy => "Cozy",
            Self::Compact => "Compact",
        }
    }
}

/// Terminal color theme preset (`Auto` detects `NO_COLOR`, `COLORTERM`, and `TERM`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePreset {
    #[default]
    Auto,
    Dark,
    Light,
    Ansi256,
    Ansi16,
    NoColor,
}

impl ThemePreset {
    pub fn next(self) -> Self {
        match self {
            Self::Auto => Self::Dark,
            Self::Dark => Self::Light,
            Self::Light => Self::Ansi256,
            Self::Ansi256 => Self::Ansi16,
            Self::Ansi16 => Self::NoColor,
            Self::NoColor => Self::Auto,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Dark => "Midnight Dark",
            Self::Light => "Daylight Light",
            Self::Ansi256 => "ANSI 256",
            Self::Ansi16 => "ANSI 16",
            Self::NoColor => "No Color",
        }
    }
}

/// Terminal image rendering protocol preference (`auto` detects Kitty / iTerm2 / Sixel / Halfblock).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageProtocolPreference {
    #[default]
    Auto,
    Kitty,
    #[serde(alias = "iterm")]
    Iterm2,
    Sixel,
    #[serde(alias = "halfblocks", alias = "ansi")]
    Halfblock,
}

impl ImageProtocolPreference {
    pub fn parse_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Some(Self::Auto),
            "kitty" => Some(Self::Kitty),
            "iterm2" | "iterm" => Some(Self::Iterm2),
            "sixel" => Some(Self::Sixel),
            "halfblock" | "halfblocks" | "ansi" => Some(Self::Halfblock),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Kitty => "kitty",
            Self::Iterm2 => "iterm2",
            Self::Sixel => "sixel",
            Self::Halfblock => "halfblock",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Kitty => "Kitty",
            Self::Iterm2 => "iTerm2",
            Self::Sixel => "Sixel",
            Self::Halfblock => "Halfblock",
        }
    }
}

/// Dual-persona entry identity: `SharePoint` (`sharepoint`/`sp`) vs `OneDrive` (`onedrive`/`od`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Persona {
    #[default]
    SharePoint,
    OneDrive,
}

impl Persona {
    /// Detects entry persona from `SHAREPOINT_PERSONA` env var first, falling back to `argv[0]`.
    pub fn detect() -> Self {
        let env_val = std::env::var("SHAREPOINT_PERSONA").ok();
        let argv0 = std::env::args_os()
            .next()
            .map(|s| s.to_string_lossy().into_owned());
        Self::detect_from(env_val.as_deref(), argv0.as_deref())
    }

    pub fn detect_from(env_persona: Option<&str>, argv0: Option<&str>) -> Self {
        if let Some(val) = env_persona {
            if let Some(p) = Self::parse_str(val) {
                return p;
            }
        }
        if let Some(raw_bin) = argv0 {
            let file_name = Path::new(raw_bin)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(raw_bin)
                .to_ascii_lowercase();
            let stem = file_name
                .strip_suffix(".exe")
                .or_else(|| file_name.strip_suffix(".js"))
                .or_else(|| file_name.strip_suffix(".cmd"))
                .or_else(|| file_name.strip_suffix(".ps1"))
                .unwrap_or(&file_name);
            if stem == "od" || stem == "onedrive" || stem.starts_with("onedrive-") {
                return Self::OneDrive;
            }
        }
        Self::SharePoint
    }

    pub fn parse_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "onedrive" | "od" | "personal" => Some(Self::OneDrive),
            "sharepoint" | "sp" | "sites" => Some(Self::SharePoint),
            _ => None,
        }
    }

    pub fn bin_name(self) -> &'static str {
        match self {
            Self::SharePoint => "sharepoint",
            Self::OneDrive => "onedrive",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::SharePoint => "SharePoint",
            Self::OneDrive => "OneDrive",
        }
    }

    pub fn as_str(self) -> &'static str {
        self.bin_name()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// Entra ID Application (client) ID.
    pub client_id: String,
    /// Entra ID Tenant ID (`common`, `organizations`, domain, or tenant GUID).
    pub tenant_id: String,
    /// Optional OAuth scopes override. Empty uses the standard OneDrive + SharePoint delegated scope set.
    pub scopes: Vec<String>,
    /// Base URL for Microsoft Graph v1.0 (overridable for sovereign clouds or mock testing).
    pub graph_base_url: String,
    /// Base URL for Entra OAuth2 endpoints (overridable for sovereign clouds or mock testing).
    pub auth_base_url: String,
    /// GitHub `owner/repo` used by `sharepoint update`.
    pub github_repo: String,
    /// Enable desktop toast/bell notifications in TUI.
    pub notify: bool,
    /// Show left navigation sidebar in wide/medium layout.
    pub show_nav_rail: bool,
    /// Render inline image previews directly inside the right inspector pane.
    pub inline_images: bool,
    /// Preferred terminal image rendering protocol (`auto`, `kitty`, `iterm2`, `sixel`, `halfblock`).
    pub image_protocol: ImageProtocolPreference,
    /// Custom download directory. Empty defaults to `Downloads/sharepoint`.
    pub download_dir: String,
    /// Active folder refresh poll interval (ms) in TUI.
    pub active_poll_ms: u64,
    /// Background sites/drives poll interval (ms).
    pub background_poll_ms: u64,
    /// Optional external token file path (`SHAREPOINT_TOKEN_FILE` or `config.toml`), read live on demand.
    pub token_file: String,
    /// Refresh command string (`SHAREPOINT_TOKEN_REFRESH_CMD` or `config.toml`), executed when token is missing or within `token_refresh_mins` of expiry.
    pub token_refresh_cmd: String,
    /// Refresh threshold in minutes before token expiry (`SHAREPOINT_TOKEN_REFRESH_MINS`, default `5`).
    pub token_refresh_mins: u64,
    /// Allow falling back to `TEAMS_TOKEN_FILE` or `teams-cli` stored M365 tokens when no `sharepoint-cli` token is configured (`SHAREPOINT_SHARE_M365_AUTH`, default `true`).
    pub share_m365_auth: bool,
    /// CLI local cache TTL in seconds (`SHAREPOINT_CLI_CACHE_SECS`, default `0`).
    pub cli_cache_ttl_secs: u64,
    /// Automatically start and attach to the lightweight background sync daemon (`SHAREPOINT_AUTO_DAEMON=1|true`, default `false`).
    pub auto_daemon: bool,
    /// TUI list density (`cozy` or `compact`).
    pub density: DensityMode,
    /// TUI color theme preset (`auto`, `dark`, `light`, `ansi256`, `ansi16`, `nocolor`).
    pub theme: ThemePreset,
    /// User-pinned site IDs shown in the sidebar.
    pub pinned_sites: Vec<String>,
    /// User-pinned folder keys (`drive_id:item_id`) shown in the sidebar.
    pub pinned_folders: Vec<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            client_id: first_non_empty_env(&[
                "SHAREPOINT_CLIENT_ID",
                "ONEDRIVE_CLIENT_ID",
                "TEAMS_CLIENT_ID",
                "AZURE_CLIENT_ID",
            ])
            .unwrap_or_else(|| DEFAULT_PUBLIC_CLIENT_ID.to_string()),
            tenant_id: first_non_empty_env(&[
                "SHAREPOINT_TENANT_ID",
                "ONEDRIVE_TENANT_ID",
                "TEAMS_TENANT_ID",
                "AZURE_TENANT_ID",
            ])
            .unwrap_or_else(|| DEFAULT_TENANT_ID.to_string()),
            scopes: Vec::new(),
            graph_base_url: first_non_empty_env(&[
                "SHAREPOINT_GRAPH_BASE_URL",
                "ONEDRIVE_GRAPH_BASE_URL",
            ])
            .unwrap_or_else(|| DEFAULT_GRAPH_BASE_URL.to_string()),
            auth_base_url: first_non_empty_env(&[
                "SHAREPOINT_AUTH_BASE_URL",
                "ONEDRIVE_AUTH_BASE_URL",
            ])
            .unwrap_or_else(|| DEFAULT_AUTH_BASE_URL.to_string()),
            github_repo: first_non_empty_env(&["SHAREPOINT_GITHUB_REPO"])
                .unwrap_or_else(|| DEFAULT_GITHUB_REPO.to_string()),
            notify: true,
            show_nav_rail: true,
            inline_images: true,
            image_protocol: first_non_empty_env(&[
                "SHAREPOINT_IMAGE_PROTOCOL",
                "ONEDRIVE_IMAGE_PROTOCOL",
                "TEAMS_IMAGE_PROTOCOL",
            ])
            .and_then(|s| ImageProtocolPreference::parse_str(&s))
            .unwrap_or_default(),
            download_dir: String::new(),
            active_poll_ms: 15_000,
            background_poll_ms: 60_000,
            token_file: first_non_empty_env(&["SHAREPOINT_TOKEN_FILE", "ONEDRIVE_TOKEN_FILE"])
                .unwrap_or_default(),
            token_refresh_cmd: first_non_empty_env(&[
                "SHAREPOINT_TOKEN_REFRESH_CMD",
                "ONEDRIVE_TOKEN_REFRESH_CMD",
            ])
            .unwrap_or_default(),
            token_refresh_mins: first_non_empty_env(&[
                "SHAREPOINT_TOKEN_REFRESH_MINS",
                "ONEDRIVE_TOKEN_REFRESH_MINS",
            ])
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(5),
            share_m365_auth: first_non_empty_env(&[
                "SHAREPOINT_SHARE_M365_AUTH",
                "ONEDRIVE_SHARE_M365_AUTH",
            ])
            .and_then(|s| parse_env_bool(&s))
            .unwrap_or(true),
            cli_cache_ttl_secs: first_non_empty_env(&[
                "SHAREPOINT_CLI_CACHE_SECS",
                "ONEDRIVE_CLI_CACHE_SECS",
            ])
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0),
            auto_daemon: first_non_empty_env(&["SHAREPOINT_AUTO_DAEMON", "ONEDRIVE_AUTO_DAEMON"])
                .and_then(|s| parse_env_bool(&s))
                .unwrap_or(false),
            density: DensityMode::default(),
            theme: ThemePreset::default(),
            pinned_sites: Vec::new(),
            pinned_folders: Vec::new(),
        }
    }
}

impl AppConfig {
    pub fn load() -> Self {
        let Ok(mut cfg) = Self::load_from(&config_file_path()) else {
            return Self::default();
        };
        if let Some(cid) = first_non_empty_env(&[
            "SHAREPOINT_CLIENT_ID",
            "ONEDRIVE_CLIENT_ID",
            "TEAMS_CLIENT_ID",
            "AZURE_CLIENT_ID",
        ]) {
            cfg.client_id = cid;
        }
        if let Some(tid) = first_non_empty_env(&[
            "SHAREPOINT_TENANT_ID",
            "ONEDRIVE_TENANT_ID",
            "TEAMS_TENANT_ID",
            "AZURE_TENANT_ID",
        ]) {
            cfg.tenant_id = tid;
        }
        if let Some(gurl) =
            first_non_empty_env(&["SHAREPOINT_GRAPH_BASE_URL", "ONEDRIVE_GRAPH_BASE_URL"])
        {
            cfg.graph_base_url = gurl;
        }
        if let Some(aurl) =
            first_non_empty_env(&["SHAREPOINT_AUTH_BASE_URL", "ONEDRIVE_AUTH_BASE_URL"])
        {
            cfg.auth_base_url = aurl;
        }
        if let Some(repo) = first_non_empty_env(&["SHAREPOINT_GITHUB_REPO"]) {
            cfg.github_repo = repo;
        }
        if let Some(tf) = first_non_empty_env(&["SHAREPOINT_TOKEN_FILE", "ONEDRIVE_TOKEN_FILE"]) {
            cfg.token_file = tf;
        }
        if let Some(rcmd) = first_non_empty_env(&[
            "SHAREPOINT_TOKEN_REFRESH_CMD",
            "ONEDRIVE_TOKEN_REFRESH_CMD",
        ]) {
            cfg.token_refresh_cmd = rcmd;
        }
        if let Some(rmins) = first_non_empty_env(&[
            "SHAREPOINT_TOKEN_REFRESH_MINS",
            "ONEDRIVE_TOKEN_REFRESH_MINS",
        ]) {
            if let Ok(parsed) = rmins.parse::<u64>() {
                cfg.token_refresh_mins = parsed;
            }
        }
        if let Some(share) = first_non_empty_env(&[
            "SHAREPOINT_SHARE_M365_AUTH",
            "ONEDRIVE_SHARE_M365_AUTH",
        ]) {
            if let Some(parsed) = parse_env_bool(&share) {
                cfg.share_m365_auth = parsed;
            }
        }
        if let Some(ttl) =
            first_non_empty_env(&["SHAREPOINT_CLI_CACHE_SECS", "ONEDRIVE_CLI_CACHE_SECS"])
        {
            if let Ok(parsed) = ttl.parse::<u64>() {
                cfg.cli_cache_ttl_secs = parsed;
            }
        }
        if let Some(ad) =
            first_non_empty_env(&["SHAREPOINT_AUTO_DAEMON", "ONEDRIVE_AUTO_DAEMON"])
        {
            if let Some(parsed) = parse_env_bool(&ad) {
                cfg.auto_daemon = parsed;
            }
        }
        if let Some(proto) = first_non_empty_env(&[
            "SHAREPOINT_IMAGE_PROTOCOL",
            "ONEDRIVE_IMAGE_PROTOCOL",
            "TEAMS_IMAGE_PROTOCOL",
        ]) {
            if let Some(parsed) = ImageProtocolPreference::parse_str(&proto) {
                cfg.image_protocol = parsed;
            }
        }
        cfg
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;
        toml::from_str(&raw)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&config_file_path())
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        ensure_parent(path)?;
        let raw = toml::to_string_pretty(self).context("Failed to serialize config")?;
        write_atomic(path, raw.as_bytes())
    }

    pub fn effective_scopes(&self) -> Vec<String> {
        if !self.scopes.is_empty() {
            return self.scopes.clone();
        }
        vec![
            "Files.Read".to_string(),
            "Files.ReadWrite".to_string(),
            "Files.Read.All".to_string(),
            "Files.ReadWrite.All".to_string(),
            "Sites.Read.All".to_string(),
            "Sites.ReadWrite.All".to_string(),
            "User.Read".to_string(),
            "offline_access".to_string(),
        ]
    }

    pub fn resolved_download_dir(&self) -> PathBuf {
        if self.download_dir.trim().is_empty() {
            default_download_dir()
        } else {
            expand_tilde(self.download_dir.trim())
        }
    }

    pub fn resolved_token_file(&self) -> Option<PathBuf> {
        let trimmed = self.token_file.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(expand_tilde(trimmed))
        }
    }
}

pub fn config_dir() -> PathBuf {
    if let Some(override_dir) =
        first_non_empty_env(&["SHAREPOINT_CONFIG_DIR", "ONEDRIVE_CONFIG_DIR"])
    {
        return PathBuf::from(override_dir);
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_DIR_NAME)
}

pub fn data_dir() -> PathBuf {
    if let Some(override_dir) = first_non_empty_env(&["SHAREPOINT_DATA_DIR", "ONEDRIVE_DATA_DIR"]) {
        return PathBuf::from(override_dir);
    }
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_DIR_NAME)
}

/// Returns the legacy `teams-cli` config directory so `auth.rs` can inspect
/// `teams-cli/config.toml` and `teams-cli/credentials.json` read-only when `share_m365_auth` is enabled.
pub fn legacy_teams_config_dir() -> Option<PathBuf> {
    if let Ok(override_dir) = std::env::var("TEAMS_CONFIG_DIR") {
        if !override_dir.trim().is_empty() {
            return Some(PathBuf::from(override_dir.trim()));
        }
    }
    dirs::config_dir().map(|dir| dir.join(LEGACY_TEAMS_APP_DIR_NAME))
}

pub fn config_file_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn credential_fallback_path() -> PathBuf {
    config_dir().join("credentials.json")
}

pub fn db_path(user_id: Option<&str>) -> PathBuf {
    let bucket = user_id
        .filter(|id| !id.trim().is_empty())
        .unwrap_or("default");
    let sanitized: String = bucket
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    data_dir().join(sanitized).join("cache.db")
}

pub fn default_download_dir() -> PathBuf {
    dirs::download_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("sharepoint")
}

pub fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
    }
    Ok(())
}

static ATOMIC_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Atomically writes `data` to `path` using a unique temp file (`create_new(true)`),
/// flushing before rename. Files whose name contains `"credentials"` default to `0o600` on Unix.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let is_secret = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.to_ascii_lowercase().contains("credentials"))
        .unwrap_or(false);
    write_atomic_with_mode(path, data, is_secret)
}

/// Atomically writes `data` to `path` with `0o600` permissions at creation time on Unix.
pub fn write_atomic_0600(path: &Path, data: &[u8]) -> Result<()> {
    write_atomic_with_mode(path, data, true)
}

fn write_atomic_with_mode(path: &Path, data: &[u8], mode_0600: bool) -> Result<()> {
    ensure_parent(path)?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let pid = std::process::id();
    let seq = ATOMIC_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".{name}.{pid}.{seq}.tmp"));

    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    if mode_0600 {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = mode_0600;

    let write_res = (|| -> Result<()> {
        let mut file = opts
            .open(&tmp)
            .with_context(|| format!("Failed to create temp file: {}", tmp.display()))?;
        file.write_all(data)
            .with_context(|| format!("Failed to write temp file: {}", tmp.display()))?;
        file.flush()
            .with_context(|| format!("Failed to flush temp file: {}", tmp.display()))?;
        Ok(())
    })();

    if let Err(err) = write_res {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }

    if let Err(rename_err) = fs::rename(&tmp, path) {
        if let Err(replace_err) =
            replace_existing_with_backup(&tmp, path, parent, name, pid, seq, rename_err)
        {
            let _ = fs::remove_file(&tmp);
            return Err(replace_err);
        }
    }
    Ok(())
}

fn replace_existing_with_backup(
    tmp: &Path,
    path: &Path,
    parent: &Path,
    name: &str,
    pid: u32,
    seq: u64,
    original_err: std::io::Error,
) -> Result<()> {
    if path.symlink_metadata().is_err() {
        return Err(original_err)
            .with_context(|| format!("Failed to save file: {}", path.display()));
    }
    let backup = parent.join(format!(".{name}.{pid}.{seq}.bak"));
    fs::rename(path, &backup)
        .with_context(|| format!("Failed to back up existing file: {}", path.display()))?;
    match fs::rename(tmp, path) {
        Ok(()) => {
            let _ = fs::remove_file(&backup);
            Ok(())
        }
        Err(err) => {
            let _ = fs::rename(&backup, path);
            Err(err).with_context(|| format!("Atomic file save failed: {}", path.display()))
        }
    }
}

fn first_non_empty_env(keys: &[&str]) -> Option<String> {
    for &key in keys {
        if let Ok(val) = std::env::var(key) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn parse_env_bool(input: &str) -> Option<bool> {
    match input.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn expand_tilde(input: &str) -> PathBuf {
    if input == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from("~"));
    }
    if let Some(rest) = input
        .strip_prefix("~/")
        .or_else(|| input.strip_prefix("~\\"))
    {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(input)
}

#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
pub(crate) struct EnvVarGuard {
    saved: Vec<(&'static str, Option<String>)>,
}

#[cfg(test)]
impl EnvVarGuard {
    pub(crate) fn set(vars: &[(&'static str, Option<&str>)]) -> Self {
        let mut saved = Vec::with_capacity(vars.len());
        for &(key, val) in vars {
            saved.push((key, std::env::var(key).ok()));
            match val {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
        Self { saved }
    }

    pub(crate) fn clear_auth_env() -> Self {
        Self::set(&[
            ("SHAREPOINT_PERSONA", None),
            ("SHAREPOINT_CONFIG_DIR", None),
            ("SHAREPOINT_DATA_DIR", None),
            ("SHAREPOINT_CLIENT_ID", None),
            ("SHAREPOINT_TENANT_ID", None),
            ("SHAREPOINT_CLIENT_SECRET", None),
            ("SHAREPOINT_GRAPH_BASE_URL", None),
            ("SHAREPOINT_AUTH_BASE_URL", None),
            ("SHAREPOINT_GITHUB_REPO", None),
            ("SHAREPOINT_TOKEN_FILE", None),
            ("SHAREPOINT_TOKEN_REFRESH_CMD", None),
            ("SHAREPOINT_TOKEN_REFRESH_MINS", None),
            ("SHAREPOINT_SHARE_M365_AUTH", None),
            ("SHAREPOINT_CLI_CACHE_SECS", None),
            ("SHAREPOINT_AUTO_DAEMON", None),
            ("SHAREPOINT_IMAGE_PROTOCOL", None),
            ("SHAREPOINT_ACCESS_TOKEN", None),
            ("SHAREPOINT_DISABLE_KEYRING", None),
            ("SHAREPOINT_NO_BROWSER", None),
            ("SHAREPOINT_TOKEN_ORIGIN", None),
            ("ONEDRIVE_CONFIG_DIR", None),
            ("ONEDRIVE_DATA_DIR", None),
            ("ONEDRIVE_CLIENT_ID", None),
            ("ONEDRIVE_TENANT_ID", None),
            ("ONEDRIVE_GRAPH_BASE_URL", None),
            ("ONEDRIVE_AUTH_BASE_URL", None),
            ("ONEDRIVE_TOKEN_FILE", None),
            ("ONEDRIVE_TOKEN_REFRESH_CMD", None),
            ("ONEDRIVE_TOKEN_REFRESH_MINS", None),
            ("ONEDRIVE_SHARE_M365_AUTH", None),
            ("ONEDRIVE_CLI_CACHE_SECS", None),
            ("ONEDRIVE_AUTO_DAEMON", None),
            ("ONEDRIVE_IMAGE_PROTOCOL", None),
            ("ONEDRIVE_ACCESS_TOKEN", None),
            ("TEAMS_CONFIG_DIR", None),
            ("TEAMS_DATA_DIR", None),
            ("TEAMS_CLIENT_ID", None),
            ("TEAMS_TENANT_ID", None),
            ("TEAMS_IMAGE_PROTOCOL", None),
            ("TEAMS_TOKEN_FILE", None),
            ("TEAMS_NO_BROWSER", None),
            ("TEAMS_ACCESS_TOKEN", None),
            ("AZURE_CLIENT_ID", None),
            ("AZURE_TENANT_ID", None),
            ("AZURE_CLIENT_SECRET", None),
        ])
    }
}

#[cfg(test)]
impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        for (key, orig) in self.saved.drain(..) {
            match orig {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_persona_detection_env_and_argv0() {
        assert_eq!(Persona::detect_from(None, Some("sharepoint")), Persona::SharePoint);
        assert_eq!(Persona::detect_from(None, Some("/usr/local/bin/sp")), Persona::SharePoint);
        assert_eq!(Persona::detect_from(None, Some("onedrive.exe")), Persona::OneDrive);
        assert_eq!(Persona::detect_from(None, Some("C:\\bin\\od.exe")), Persona::OneDrive);
        assert_eq!(Persona::detect_from(None, Some("bin/od.js")), Persona::OneDrive);
        assert_eq!(
            Persona::detect_from(Some("onedrive"), Some("sharepoint.exe")),
            Persona::OneDrive
        );
        assert_eq!(
            Persona::detect_from(Some("sharepoint"), Some("onedrive.exe")),
            Persona::SharePoint
        );
    }

    #[test]
    fn test_config_roundtrip_and_defaults() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let mut cfg = AppConfig::default();
        assert_eq!(DEFAULT_SERVE_PORT, 8790);
        assert_eq!(APP_DIR_NAME, "sharepoint-cli");
        assert_eq!(DEFAULT_GITHUB_REPO, "BiteFoundry/sharepoint");
        assert!(cfg.effective_scopes().iter().any(|s| s == "Files.ReadWrite.All"));
        assert!(cfg.effective_scopes().iter().any(|s| s == "Sites.ReadWrite.All"));

        cfg.theme = ThemePreset::Light;
        cfg.density = DensityMode::Compact;
        cfg.pinned_sites = vec!["site-1".into()];
        cfg.save_to(&path).unwrap();

        let loaded = AppConfig::load_from(&path).unwrap();
        assert_eq!(loaded.theme, ThemePreset::Light);
        assert_eq!(loaded.density, DensityMode::Compact);
        assert_eq!(loaded.pinned_sites, vec!["site-1".to_string()]);
    }
}
