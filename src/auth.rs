//! OAuth 2.0 authentication (PKCE loopback, Device Code Flow, Client Secret / env token)
//! + OS keyring (`keyring`) with file fallback (`credentials.json`) + automatic refresh
//! + optional shared Microsoft 365 (`teams-cli`) session bootstrap.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::RwLock,
};
use url::Url;

use crate::config::{credential_fallback_path, legacy_teams_config_dir, write_atomic, AppConfig};

/// Default Microsoft Graph delegated OAuth scopes for `sharepoint-cli`.
pub const DEFAULT_SCOPES: &[&str] = &[
    "Files.Read",
    "Files.ReadWrite",
    "Files.Read.All",
    "Files.ReadWrite.All",
    "Sites.Read.All",
    "Sites.ReadWrite.All",
    "User.Read",
    "offline_access",
];

const KEYRING_SERVICE: &str = "sharepoint-cli";
const LEGACY_TEAMS_KEYRING_SERVICE: &str = "teams-cli";
const KEYRING_USER: &str = "default";
const LOGIN_CALLBACK_TIMEOUT_SECS: u64 = 300;
#[cfg(target_os = "linux")]
const LINUX_KEYRING_TIMEOUT: Duration = Duration::from_millis(800);

/// Set when D-Bus Secret Service / keyring fails or times out so subsequent
/// calls in the same process immediately fall back to `credentials.json`.
static KEYRING_RUNTIME_FAILED: AtomicBool = AtomicBool::new(false);
/// Emits the approaching-expiry warning for static external tokens at most once per process.
static EXPIRY_SOON_WARNED: AtomicBool = AtomicBool::new(false);
/// Emits the external refresh-command fallback warning at most once per process.
static REFRESH_CMD_WARNED: AtomicBool = AtomicBool::new(false);
const EXTERNAL_REFRESH_TIMEOUT: Duration = Duration::from_secs(60);
const REFRESH_CMD_FAILURE_COOLDOWN: Duration = Duration::from_secs(15);
#[cfg(target_os = "windows")]
const WINDOWS_CRED_MAX_BYTES: usize = 2500;

/// Canonical prefix emitted by `explain_graph_permission_error` for 403 Graph errors.
pub const INSUFFICIENT_PERMISSIONS_PREFIX: &str = "Insufficient Graph permissions (403):";

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Unix epoch seconds when `access_token` expires.
    pub expires_at: i64,
    pub client_id: String,
    pub tenant_id: String,
    pub user_id: Option<String>,
    pub user_display_name: Option<String>,
    pub user_email: Option<String>,
    pub auth_mode: String,
}

impl std::fmt::Debug for StoredTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredTokens")
            .field("access_token", &"[REDACTED]")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("expires_at", &self.expires_at)
            .field("client_id", &self.client_id)
            .field("tenant_id", &self.tenant_id)
            .field("user_id", &self.user_id)
            .field("user_display_name", &self.user_display_name)
            .field("user_email", &self.user_email)
            .field("auth_mode", &self.auth_mode)
            .finish()
    }
}

impl StoredTokens {
    pub fn is_expired_soon(&self, skew_secs: i64) -> bool {
        let now = chrono::Utc::now().timestamp();
        self.expires_at <= now + skew_secs
    }
}

#[derive(Debug, Clone, Default)]
pub struct LoginOptions {
    pub client_id: Option<String>,
    pub tenant_id: Option<String>,
    pub device_code: bool,
    pub client_secret: Option<String>,
    pub access_token: Option<String>,
    pub port: Option<u16>,
}

#[derive(Clone, Default)]
struct ExternalTokenCache {
    env_token: Option<(String, StoredTokens)>,
    file_token: Option<(
        PathBuf,
        Option<std::time::SystemTime>,
        u64,
        StoredTokens,
    )>,
    shared_file_token: Option<(
        PathBuf,
        Option<std::time::SystemTime>,
        u64,
        StoredTokens,
    )>,
}

#[derive(Clone)]
pub struct AuthManager {
    config: AppConfig,
    http: reqwest::Client,
    tokens: Arc<RwLock<Option<StoredTokens>>>,
    refresh_mutex: Arc<tokio::sync::Mutex<()>>,
    external_cache: Arc<std::sync::Mutex<Option<ExternalTokenCache>>>,
    refresh_cmd_cooldown_until: Arc<std::sync::Mutex<Option<std::time::Instant>>>,
    ephemeral: bool,
    #[cfg(test)]
    token_path_override: Option<PathBuf>,
    #[cfg(test)]
    legacy_teams_dir_override: Option<PathBuf>,
}

impl AuthManager {
    pub fn new(config: AppConfig) -> Result<Self> {
        let loaded = if should_skip_keyring_on_init(&config) {
            None
        } else {
            load_stored_tokens()?
        };
        let http = build_auth_http_client()?;
        Ok(Self {
            config,
            http,
            tokens: Arc::new(RwLock::new(loaded)),
            refresh_mutex: Arc::new(tokio::sync::Mutex::new(())),
            external_cache: Arc::new(std::sync::Mutex::new(None)),
            refresh_cmd_cooldown_until: Arc::new(std::sync::Mutex::new(None)),
            ephemeral: false,
            #[cfg(test)]
            token_path_override: None,
            #[cfg(test)]
            legacy_teams_dir_override: None,
        })
    }

    pub fn from_tokens(config: AppConfig, tokens: StoredTokens) -> Self {
        let http = build_auth_http_client().unwrap_or_else(|_| reqwest::Client::new());
        Self {
            config,
            http,
            tokens: Arc::new(RwLock::new(Some(tokens))),
            refresh_mutex: Arc::new(tokio::sync::Mutex::new(())),
            external_cache: Arc::new(std::sync::Mutex::new(None)),
            refresh_cmd_cooldown_until: Arc::new(std::sync::Mutex::new(None)),
            ephemeral: true,
            #[cfg(test)]
            token_path_override: None,
            #[cfg(test)]
            legacy_teams_dir_override: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn empty_for_test(config: AppConfig) -> Self {
        let http = build_auth_http_client().unwrap_or_else(|_| reqwest::Client::new());
        Self {
            config,
            http,
            tokens: Arc::new(RwLock::new(None)),
            refresh_mutex: Arc::new(tokio::sync::Mutex::new(())),
            external_cache: Arc::new(std::sync::Mutex::new(None)),
            refresh_cmd_cooldown_until: Arc::new(std::sync::Mutex::new(None)),
            ephemeral: true,
            token_path_override: None,
            legacy_teams_dir_override: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_token_path(mut self, path: PathBuf) -> Self {
        self.token_path_override = Some(path);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_legacy_teams_dir(mut self, dir: PathBuf) -> Self {
        self.legacy_teams_dir_override = Some(dir);
        self
    }

    fn persist_tokens(&self, tokens: &StoredTokens) -> Result<()> {
        #[cfg(test)]
        if let Some(ref path) = self.token_path_override {
            return save_tokens_to_path(tokens, path);
        }
        if self.ephemeral {
            return Ok(());
        }
        save_stored_tokens(tokens)
    }

    fn load_from_configured_source(&self) -> Result<Option<StoredTokens>> {
        #[cfg(test)]
        if let Some(ref path) = self.token_path_override {
            return load_tokens_from_path(path);
        }
        if self.ephemeral {
            return Ok(None);
        }
        load_stored_tokens()
    }

    fn effective_scopes_string(&self) -> String {
        if !self.config.scopes.is_empty() {
            self.config.scopes.join(" ")
        } else {
            DEFAULT_SCOPES.join(" ")
        }
    }

    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    pub fn http_client(&self) -> &reqwest::Client {
        &self.http
    }

    fn has_refresh_cmd(&self) -> bool {
        !self.config.token_refresh_cmd.trim().is_empty()
    }

    fn external_refresh_skew_secs(&self) -> i64 {
        (self.config.token_refresh_mins.min(1440) * 60) as i64
    }

    fn cached_env_access_token(&self) -> Option<StoredTokens> {
        let Ok(raw) = std::env::var("SHAREPOINT_ACCESS_TOKEN") else {
            if let Ok(mut guard) = self.external_cache.lock() {
                if let Some(ref mut cache) = *guard {
                    cache.env_token = None;
                }
            }
            return None;
        };
        if let Ok(guard) = self.external_cache.lock() {
            if let Some(ref cache) = *guard {
                if let Some((ref cached_raw, ref cached_tok)) = cache.env_token {
                    if *cached_raw == raw {
                        return Some(cached_tok.clone());
                    }
                }
            }
        }
        let parsed = env_access_token_from_str(&raw, &self.config);
        if let Ok(mut guard) = self.external_cache.lock() {
            let cache = guard.get_or_insert_with(ExternalTokenCache::default);
            cache.env_token = parsed.as_ref().map(|tok| (raw, tok.clone()));
        }
        parsed
    }

    fn cached_external_token_file(&self, path: &Path) -> Result<StoredTokens> {
        self.cached_token_file_with_mode(path, "token_file", false)
    }

    fn cached_shared_teams_token_file(&self, path: &Path) -> Result<StoredTokens> {
        self.cached_token_file_with_mode(path, "shared_teams_token_file", true)
    }

    fn cached_token_file_with_mode(
        &self,
        path: &Path,
        auth_mode: &str,
        shared_slot: bool,
    ) -> Result<StoredTokens> {
        let meta = std::fs::metadata(path)
            .with_context(|| format!("Failed to read external token file: {}", path.display()))?;
        let modified = meta.modified().ok();
        let len = meta.len();

        if modified.is_some() {
            if let Ok(guard) = self.external_cache.lock() {
                if let Some(ref cache) = *guard {
                    let slot = if shared_slot {
                        &cache.shared_file_token
                    } else {
                        &cache.file_token
                    };
                    if let Some((ref p, m, l, ref tok)) = *slot {
                        if p == path && m == modified && l == len {
                            return Ok(tok.clone());
                        }
                    }
                }
            }
        }

        match read_external_token_file_with_mode(path, auth_mode, &self.config) {
            Ok(tok) => {
                if let Ok(mut guard) = self.external_cache.lock() {
                    let cache = guard.get_or_insert_with(ExternalTokenCache::default);
                    let entry = Some((path.to_path_buf(), modified, len, tok.clone()));
                    if shared_slot {
                        cache.shared_file_token = entry;
                    } else {
                        cache.file_token = entry;
                    }
                }
                Ok(tok)
            }
            Err(err) => {
                if let Ok(mut guard) = self.external_cache.lock() {
                    if let Some(ref mut cache) = *guard {
                        if shared_slot {
                            cache.shared_file_token = None;
                        } else {
                            cache.file_token = None;
                        }
                    }
                }
                Err(err)
            }
        }
    }

    fn invalidate_file_token_cache(&self) {
        if let Ok(mut guard) = self.external_cache.lock() {
            if let Some(ref mut cache) = *guard {
                cache.file_token = None;
                cache.shared_file_token = None;
            }
        }
    }

    fn resolve_legacy_teams_config_dir(&self) -> Option<PathBuf> {
        #[cfg(test)]
        if let Some(ref dir) = self.legacy_teams_dir_override {
            return Some(dir.clone());
        }
        #[cfg(test)]
        if self.token_path_override.is_some() && std::env::var_os("TEAMS_CONFIG_DIR").is_none() {
            // Keep isolated token-path tests from accidentally reading the user's real teams-cli dir.
            return None;
        }
        if self.ephemeral {
            return None;
        }
        legacy_teams_config_dir()
    }

    fn resolve_shared_teams_token_file_path(&self) -> Option<PathBuf> {
        // With sharing switched off there is no shared file to find. Checked here, not at
        // the callers, so every path that asks for the file honours the setting.
        if !self.config.share_m365_auth {
            return None;
        }
        #[cfg(test)]
        if self.token_path_override.is_some()
            && self.legacy_teams_dir_override.is_none()
            && std::env::var_os("TEAMS_TOKEN_FILE").is_none()
            && std::env::var_os("TEAMS_CONFIG_DIR").is_none()
        {
            return None;
        }
        if self.ephemeral {
            #[cfg(test)]
            let allow_override = self.legacy_teams_dir_override.is_some();
            #[cfg(not(test))]
            let allow_override = false;
            if !allow_override {
                return None;
            }
        }
        if let Ok(val) = std::env::var("TEAMS_TOKEN_FILE") {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return Some(expand_tilde_path(trimmed));
            }
        }
        let teams_dir = self.resolve_legacy_teams_config_dir()?;
        let cfg_path = teams_dir.join("config.toml");
        let raw = std::fs::read_to_string(&cfg_path).ok()?;
        #[derive(Deserialize)]
        struct LegacyTeamsCfg {
            #[serde(default)]
            token_file: String,
        }
        let parsed: LegacyTeamsCfg = toml::from_str(&raw).ok()?;
        let trimmed = parsed.token_file.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(expand_tilde_path(trimmed))
        }
    }

    fn load_shared_teams_token_file_if_eligible(&self) -> Option<StoredTokens> {
        if !self.config.share_m365_auth {
            return None;
        }
        let path = self.resolve_shared_teams_token_file_path()?;
        if !path.exists() {
            return None;
        }
        let tok = self.cached_shared_teams_token_file(&path).ok()?;
        if token_is_sharepoint_eligible_or_opaque(&tok.access_token) {
            Some(tok)
        } else {
            None
        }
    }

    fn load_legacy_teams_stored_tokens(&self) -> Option<StoredTokens> {
        if !self.config.share_m365_auth {
            return None;
        }
        if self.ephemeral {
            #[cfg(test)]
            let allow_override = self.legacy_teams_dir_override.is_some();
            #[cfg(not(test))]
            let allow_override = false;
            if !allow_override {
                return None;
            }
        }
        #[cfg(test)]
        let skip_real_keyring = self.ephemeral
            || self.token_path_override.is_some()
            || self.legacy_teams_dir_override.is_some();
        #[cfg(not(test))]
        let skip_real_keyring = self.ephemeral;

        let keyring_tok = if !skip_real_keyring && should_use_keyring() {
            read_keyring_tokens(LEGACY_TEAMS_KEYRING_SERVICE)
        } else {
            None
        };

        let file_tok = self
            .resolve_legacy_teams_config_dir()
            .and_then(|dir| load_tokens_from_path(&dir.join("credentials.json")).ok().flatten());

        match (keyring_tok, file_tok) {
            (Some(kr), Some(fl)) => {
                if fl.expires_at > kr.expires_at {
                    Some(fl)
                } else {
                    Some(kr)
                }
            }
            (Some(kr), None) => Some(kr),
            (None, Some(fl)) => Some(fl),
            (None, None) => None,
        }
    }

    async fn resolve_current_tokens_raw(&self) -> Option<StoredTokens> {
        let mem_tok = self.tokens.read().await.clone();
        let env_tok = self.cached_env_access_token();
        let file_tok = self
            .config
            .resolved_token_file()
            .and_then(|path| self.cached_external_token_file(&path).ok());

        if let Some(ref mem) = mem_tok {
            if mem.auth_mode == "refresh_cmd" {
                let newer_than_env = match env_tok.as_ref() {
                    None => true,
                    Some(env) => {
                        mem.expires_at > env.expires_at
                            || (mem.expires_at == env.expires_at
                                && mem.access_token != env.access_token)
                    }
                };
                let newer_than_file = match file_tok.as_ref() {
                    None => true,
                    Some(file) => {
                        mem.expires_at > file.expires_at
                            || (mem.expires_at == file.expires_at
                                && mem.access_token != file.access_token)
                    }
                };
                if newer_than_env && newer_than_file {
                    return Some(mem.clone());
                }
            }
        }

        if let Some(env) = env_tok {
            return Some(env);
        }
        if let Some(file) = file_tok {
            return Some(file);
        }
        if mem_tok.is_some() {
            return mem_tok;
        }
        if let Ok(Some(disk)) = self.load_from_configured_source() {
            return Some(disk);
        }
        if let Some(shared_file) = self.load_shared_teams_token_file_if_eligible() {
            return Some(shared_file);
        }
        if let Some(mut teams_tok) = self.load_legacy_teams_stored_tokens() {
            if !teams_tok.is_expired_soon(0) && token_has_sharepoint_scopes(&teams_tok.access_token) {
                teams_tok.auth_mode = "shared_teams_session".to_string();
                return Some(teams_tok);
            }
        }
        None
    }

    pub async fn current_tokens(&self) -> Option<StoredTokens> {
        if let Some(tok) = self.resolve_current_tokens_raw().await {
            return Some(tok);
        }
        if self.has_refresh_cmd() {
            return self.run_external_refresh_cmd(None, true).await.ok();
        }
        None
    }

    /// Returns a valid Bearer access token, hot-reloading external `credentials.json` / `token_file` updates,
    /// running `SHAREPOINT_TOKEN_REFRESH_CMD`, refreshing via OAuth `refresh_token`, or bootstrapping from
    /// shared `teams-cli` M365 tokens when `config.share_m365_auth` is enabled.
    pub async fn valid_access_token(&self) -> Result<String> {
        let has_refresh_cmd = self.has_refresh_cmd();

        if let Some(tok) = self.resolve_current_tokens_raw().await {
            let is_external = matches!(
                tok.auth_mode.as_str(),
                "env_token" | "token_file" | "refresh_cmd" | "shared_teams_token_file"
            );
            if is_external {
                if has_refresh_cmd && tok.auth_mode != "shared_teams_token_file" {
                    let skew_secs = self.external_refresh_skew_secs();
                    if !tok.is_expired_soon(skew_secs) {
                        return Ok(tok.access_token);
                    }
                    let refreshed = self.run_external_refresh_cmd(Some(&tok), true).await?;
                    return Ok(refreshed.access_token);
                }

                if tok.is_expired_soon(0) {
                    if tok.auth_mode == "shared_teams_token_file" {
                        bail!(
                            "Shared Teams token expired (exp: {}, auth_mode={}) — refresh your Teams token or run 'sharepoint login'.",
                            format_local_exp(tok.expires_at),
                            tok.auth_mode
                        );
                    }
                    bail!(
                        "Token expired (exp: {}, auth_mode={}) — refresh your token and retry. (Auto-refresh supported via SHAREPOINT_TOKEN_FILE + SHAREPOINT_TOKEN_REFRESH_CMD)",
                        format_local_exp(tok.expires_at),
                        tok.auth_mode
                    );
                }
                if tok.is_expired_soon(300) && !EXPIRY_SOON_WARNED.swap(true, Ordering::Relaxed) {
                    eprintln!(
                        "Warning: token expiring soon (exp: {}) — refresh recommended.",
                        format_local_exp(tok.expires_at)
                    );
                }
                return Ok(tok.access_token);
            }

            if tok.auth_mode == "shared_teams_session" {
                if !tok.is_expired_soon(90) && token_has_sharepoint_scopes(&tok.access_token) {
                    return Ok(tok.access_token);
                }
                if self.can_refresh_token(&tok) {
                    let upgraded = self.upgrade_shared_teams_session(&tok).await?;
                    return Ok(upgraded.access_token);
                }
                if !tok.is_expired_soon(0) && token_has_sharepoint_scopes(&tok.access_token) {
                    return Ok(tok.access_token);
                }
                bail!("Shared Microsoft 365 session expired. Run 'sharepoint login' first.");
            }

            if !tok.is_expired_soon(90) {
                return Ok(tok.access_token);
            }
            if has_refresh_cmd {
                let refreshed = self.run_external_refresh_cmd(Some(&tok), true).await?;
                return Ok(refreshed.access_token);
            }
            if let Some(reloaded) = self
                .try_hot_reload_from_disk(&tok.access_token, tok.expires_at)
                .await
            {
                if !reloaded.is_expired_soon(30) {
                    return Ok(reloaded.access_token);
                }
            }
            if self.can_refresh_token(&tok) {
                // Ensure `self.tokens` holds `tok` before refreshing (e.g. if loaded from disk).
                {
                    let mut guard = self.tokens.write().await;
                    if guard.is_none() {
                        *guard = Some(tok.clone());
                    }
                }
                let refreshed = self.refresh_oauth_serialized(None, Some(90)).await?;
                return Ok(refreshed.access_token);
            }
            return Ok(tok.access_token);
        }

        if has_refresh_cmd {
            let refreshed = self.run_external_refresh_cmd(None, true).await?;
            return Ok(refreshed.access_token);
        }

        if let Some(token_path) = self.config.resolved_token_file() {
            let tok = self.cached_external_token_file(&token_path)?;
            if tok.is_expired_soon(0) {
                bail!(
                    "Token expired (exp: {}, auth_mode={}) — refresh your token and retry. (Auto-refresh supported via SHAREPOINT_TOKEN_FILE + SHAREPOINT_TOKEN_REFRESH_CMD)",
                    format_local_exp(tok.expires_at),
                    tok.auth_mode
                );
            }
            return Ok(tok.access_token);
        }

        if let Ok(Some(reloaded)) = self.load_from_configured_source() {
            let mut guard = self.tokens.write().await;
            *guard = Some(reloaded.clone());
            return Ok(reloaded.access_token);
        }

        // Shared M365 fallback: check teams-cli StoredTokens even if its access token is expired
        // or lacks SharePoint/OneDrive scopes, as long as it has a refresh_token we can silently exchange.
        if self.config.share_m365_auth {
            if let Some(teams_tok) = self.load_legacy_teams_stored_tokens() {
                if !teams_tok.is_expired_soon(90) && token_has_sharepoint_scopes(&teams_tok.access_token)
                {
                    return Ok(teams_tok.access_token);
                }
                if teams_tok
                    .refresh_token
                    .as_deref()
                    .map(|s| !s.trim().is_empty())
                    .unwrap_or(false)
                {
                    let upgraded = self.upgrade_shared_teams_session(&teams_tok).await?;
                    return Ok(upgraded.access_token);
                }
            }
        }

        bail!("Not logged in. Run 'sharepoint login' first.")
    }

    async fn upgrade_shared_teams_session(&self, teams_tok: &StoredTokens) -> Result<StoredTokens> {
        let _refresh_guard = self.refresh_mutex.lock().await;

        // Re-check in case another concurrent task already cached an Outlook token.
        if let Some(ref mem) = *self.tokens.read().await {
            if !mem.is_expired_soon(30) && token_has_sharepoint_scopes(&mem.access_token) {
                return Ok(mem.clone());
            }
        }
        if let Ok(Some(disk)) = self.load_from_configured_source() {
            if !disk.is_expired_soon(10) {
                let mut guard = self.tokens.write().await;
                *guard = Some(disk.clone());
                return Ok(disk);
            }
        }

        let rt = teams_tok
            .refresh_token
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("Shared Microsoft 365 session has no refresh token. Run 'sharepoint login'."))?;

        let token_url = self.token_endpoint(&teams_tok.tenant_id);
        let scope = self.effective_scopes_string();
        let params = [
            ("client_id", teams_tok.client_id.as_str()),
            ("grant_type", "refresh_token"),
            ("refresh_token", rt),
            ("scope", scope.as_str()),
        ];

        let token_resp = self
            .post_token_form(&token_url, &params)
            .await
            .map_err(|err| {
                anyhow!(
                    "Shared Microsoft 365 session could not acquire SharePoint/OneDrive scopes ({err}). Run 'sharepoint login' to consent SharePoint/OneDrive permissions."
                )
            })?;

        let expires_in = token_resp.expires_in.unwrap_or(3600).max(60);
        let claims = parse_jwt_claims(&token_resp.access_token).unwrap_or_default();
        let updated = StoredTokens {
            access_token: token_resp.access_token,
            refresh_token: token_resp
                .refresh_token
                .or_else(|| teams_tok.refresh_token.clone()),
            expires_at: chrono::Utc::now().timestamp() + expires_in,
            client_id: teams_tok.client_id.clone(),
            tenant_id: teams_tok.tenant_id.clone(),
            user_id: teams_tok.user_id.clone().or(claims.user_id),
            user_display_name: teams_tok
                .user_display_name
                .clone()
                .or(claims.user_display_name),
            user_email: teams_tok.user_email.clone().or(claims.user_email),
            auth_mode: "shared_teams_session".to_string(),
        };

        let mut guard = self.tokens.write().await;
        self.persist_tokens(&updated)?;
        *guard = Some(updated.clone());
        Ok(updated)
    }

    async fn try_hot_reload_from_disk(
        &self,
        current_access_token: &str,
        current_expires_at: i64,
    ) -> Option<StoredTokens> {
        let disk_tok = self.load_from_configured_source().ok().flatten()?;
        if disk_tok.access_token != current_access_token
            && disk_tok.expires_at >= current_expires_at
        {
            let mut guard = self.tokens.write().await;
            *guard = Some(disk_tok.clone());
            return Some(disk_tok);
        }
        None
    }

    /// Returns whether stored tokens can currently be refreshed (`token_refresh_cmd`, refresh token, or client secret).
    pub async fn is_refreshable(&self) -> bool {
        if self.has_refresh_cmd() {
            return true;
        }
        if self.cached_env_access_token().is_some() || self.config.resolved_token_file().is_some() {
            return false;
        }
        let guard = self.tokens.read().await;
        if let Some(tok) = guard.as_ref() {
            return self.can_refresh_token(tok);
        }
        drop(guard);
        if self.load_shared_teams_token_file_if_eligible().is_some() {
            return false;
        }
        if let Some(teams_tok) = self.load_legacy_teams_stored_tokens() {
            return self.can_refresh_token(&teams_tok);
        }
        false
    }

    fn can_refresh_token(&self, tok: &StoredTokens) -> bool {
        if tok
            .refresh_token
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
        {
            return true;
        }
        tok.auth_mode == "client_credentials" && env_client_secret().is_some()
    }

    pub async fn login(&self, opts: &LoginOptions) -> Result<StoredTokens> {
        let client_id = opts
            .client_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.config.client_id)
            .to_string();
        let tenant_id = opts
            .tenant_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.config.tenant_id)
            .to_string();

        // 1. Direct Access Token
        if let Some(raw_token) = opts
            .access_token
            .as_deref()
            .map(strip_bearer_prefix)
            .filter(|s| !s.is_empty())
        {
            let mut stored = stored_tokens_from_jwt(
                raw_token,
                None,
                None,
                &client_id,
                &tenant_id,
                "access_token",
            );
            if opts
                .client_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .is_some()
            {
                stored.client_id = client_id;
            }
            if opts
                .tenant_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .is_some()
            {
                stored.tenant_id = tenant_id;
            }
            return self.store_tokens(stored).await;
        }

        // 2. Client Credentials (explicit option takes precedence over device_code; env var applies if not device_code)
        if let Some(secret) = opts
            .client_secret
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let stored = self
                .login_client_credentials(&client_id, &tenant_id, secret)
                .await?;
            return self.store_tokens(stored).await;
        }

        if !opts.device_code {
            if let Some(secret) = env_client_secret() {
                let stored = self
                    .login_client_credentials(&client_id, &tenant_id, &secret)
                    .await?;
                return self.store_tokens(stored).await;
            }
        }

        // 3. Device Code Flow
        if opts.device_code {
            let stored = self.login_device_code(&client_id, &tenant_id).await?;
            return self.store_tokens(stored).await;
        }

        // 4. Interactive Browser Flow (Authorization Code + PKCE)
        let stored = self
            .login_pkce_loopback(&client_id, &tenant_id, opts.port)
            .await?;
        self.store_tokens(stored).await
    }

    pub async fn refresh_now(&self) -> Result<StoredTokens> {
        if self.has_refresh_cmd() {
            let current = self.resolve_current_tokens_raw().await;
            return self.run_external_refresh_cmd(current.as_ref(), true).await;
        }
        if self.cached_env_access_token().is_some() {
            bail!("SHAREPOINT_ACCESS_TOKEN environment variable token cannot be auto-refreshed (SHAREPOINT_TOKEN_REFRESH_CMD is not set).");
        }
        if let Some(path) = self.config.resolved_token_file() {
            return self.cached_external_token_file(&path);
        }
        if let Some(shared_path) = self.resolve_shared_teams_token_file_path() {
            if shared_path.exists() && self.tokens.read().await.is_none() {
                return self.cached_shared_teams_token_file(&shared_path);
            }
        }
        if self.tokens.read().await.is_none() && self.config.share_m365_auth {
            if let Some(teams_tok) = self.load_legacy_teams_stored_tokens() {
                if self.can_refresh_token(&teams_tok) {
                    return self.upgrade_shared_teams_session(&teams_tok).await;
                }
            }
        }
        self.refresh_oauth_serialized(None, None).await
    }

    /// Refreshes or hot-reloads the token only if it still matches `failed_access_token` (prevents thundering herd on 401).
    pub(crate) async fn refresh_for_failed_token(
        &self,
        failed_access_token: &str,
    ) -> Result<StoredTokens> {
        if self.has_refresh_cmd() {
            let current = self.resolve_current_tokens_raw().await;
            if let Some(ref cur) = current {
                if cur.access_token != failed_access_token && !cur.is_expired_soon(30) {
                    return Ok(cur.clone());
                }
            }
            let refreshed = self.run_external_refresh_cmd(current.as_ref(), false).await?;
            if refreshed.access_token == failed_access_token {
                bail!("External token refresh returned the same rejected token.");
            }
            return Ok(refreshed);
        }
        if self.cached_env_access_token().is_some() {
            bail!("SHAREPOINT_ACCESS_TOKEN environment variable token cannot be auto-refreshed.");
        }
        if let Some(path) = self.config.resolved_token_file() {
            if let Ok(disk_tok) = self.cached_external_token_file(&path) {
                if disk_tok.access_token != failed_access_token && !disk_tok.is_expired_soon(0) {
                    return Ok(disk_tok);
                }
            }
            bail!("External token file is expired or was rejected. Refresh your token or set SHAREPOINT_TOKEN_REFRESH_CMD.");
        }
        if self.tokens.read().await.is_none() {
            if let Some(shared_path) = self.resolve_shared_teams_token_file_path() {
                if shared_path.exists() {
                    if let Ok(disk_tok) = self.cached_shared_teams_token_file(&shared_path) {
                        if disk_tok.access_token != failed_access_token
                            && !disk_tok.is_expired_soon(0)
                            && token_is_sharepoint_eligible_or_opaque(&disk_tok.access_token)
                        {
                            return Ok(disk_tok);
                        }
                    }
                    bail!("Shared Teams token file was rejected or expired. Refresh your Teams token or run 'sharepoint login'.");
                }
            }
            if self.config.share_m365_auth {
                if let Some(teams_tok) = self.load_legacy_teams_stored_tokens() {
                    if self.can_refresh_token(&teams_tok) {
                        return self.upgrade_shared_teams_session(&teams_tok).await;
                    }
                }
            }
        }
        self.refresh_oauth_serialized(Some(failed_access_token), None).await
    }

    async fn run_external_refresh_cmd(
        &self,
        current: Option<&StoredTokens>,
        allow_cooldown: bool,
    ) -> Result<StoredTokens> {
        let _lock = self.refresh_mutex.lock().await;
        let skew_secs = self.external_refresh_skew_secs();
        let old_token = current.map(|c| c.access_token.as_str());
        let token_file = self.config.resolved_token_file();

        // Double-check if another concurrent task/process already refreshed `token_file`.
        if let Some(ref path) = token_file {
            if let Ok(reloaded) = self.cached_external_token_file(path) {
                if !reloaded.is_expired_soon(skew_secs.min(60))
                    && Some(reloaded.access_token.as_str()) != old_token
                {
                    return Ok(reloaded);
                }
            }
        } else if let Some(mem) = self.tokens.read().await.clone() {
            if mem.auth_mode == "refresh_cmd"
                && !mem.is_expired_soon(skew_secs.min(60))
                && Some(mem.access_token.as_str()) != old_token
            {
                return Ok(mem);
            }
        }

        if allow_cooldown {
            if let Some(cur) = current {
                if !cur.is_expired_soon(0) {
                    if let Ok(guard) = self.refresh_cmd_cooldown_until.lock() {
                        if let Some(until) = *guard {
                            if std::time::Instant::now() < until {
                                return Ok(cur.clone());
                            }
                        }
                    }
                }
            }
        }

        let cmd_str = self.config.token_refresh_cmd.trim();
        let exec_res = execute_external_refresh_shell(cmd_str, token_file.as_deref()).await;

        match exec_res {
            Ok(stdout) => {
                self.invalidate_file_token_cache();
                let parsed = if let Some(ref path) = token_file {
                    if path.exists() {
                        match self.cached_external_token_file(path) {
                            Ok(file_tok)
                                if Some(file_tok.access_token.as_str()) != old_token
                                    || stdout.trim().is_empty() =>
                            {
                                Ok(file_tok)
                            }
                            Ok(file_tok) => match parse_external_token_str(
                                &stdout,
                                "refresh_cmd",
                                &self.config,
                                "token_refresh_cmd stdout",
                            ) {
                                Ok(stdout_tok) => {
                                    *self.tokens.write().await = Some(stdout_tok.clone());
                                    Ok(stdout_tok)
                                }
                                Err(_) => Ok(file_tok),
                            },
                            Err(file_err) => parse_external_token_str(
                                &stdout,
                                "refresh_cmd",
                                &self.config,
                                "token_refresh_cmd stdout",
                            )
                            .map_err(|_| file_err),
                        }
                    } else {
                        let tok = parse_external_token_str(
                            &stdout,
                            "refresh_cmd",
                            &self.config,
                            "token_refresh_cmd stdout",
                        )?;
                        *self.tokens.write().await = Some(tok.clone());
                        Ok(tok)
                    }
                } else {
                    let tok = parse_external_token_str(
                        &stdout,
                        "refresh_cmd",
                        &self.config,
                        "token_refresh_cmd stdout",
                    )?;
                    *self.tokens.write().await = Some(tok.clone());
                    Ok(tok)
                };

                let new_tok = match parsed {
                    Ok(t) => t,
                    Err(err) => {
                        let res = fallback_or_bail_refresh_error(current, &err.to_string());
                        if res.is_ok() {
                            self.record_refresh_cmd_cooldown();
                        }
                        return res;
                    }
                };

                if new_tok.is_expired_soon(0) {
                    bail!(
                        "External token refresh failed (exp: {}): token is still expired after refresh.",
                        format_local_exp(new_tok.expires_at)
                    );
                }
                self.clear_refresh_cmd_cooldown();
                Ok(new_tok)
            }
            Err(err) => {
                let res = fallback_or_bail_refresh_error(current, &err.to_string());
                if res.is_ok() {
                    self.record_refresh_cmd_cooldown();
                }
                res
            }
        }
    }

    fn record_refresh_cmd_cooldown(&self) {
        if let Ok(mut guard) = self.refresh_cmd_cooldown_until.lock() {
            *guard = Some(std::time::Instant::now() + REFRESH_CMD_FAILURE_COOLDOWN);
        }
    }

    fn clear_refresh_cmd_cooldown(&self) {
        if let Ok(mut guard) = self.refresh_cmd_cooldown_until.lock() {
            *guard = None;
        }
    }

    /// Serializes OAuth refreshes via `refresh_mutex` and releases `self.tokens` across the network call.
    async fn refresh_oauth_serialized(
        &self,
        expected_failed_token: Option<&str>,
        min_valid_skew_secs: Option<i64>,
    ) -> Result<StoredTokens> {
        let _refresh_guard = self.refresh_mutex.lock().await;

        let current_opt = { self.tokens.read().await.clone() };
        if let Some(failed_tok) = expected_failed_token {
            if let Some(ref cur) = current_opt {
                if cur.access_token != failed_tok && !cur.is_expired_soon(30) {
                    return Ok(cur.clone());
                }
            }
            if let Ok(Some(disk_tok)) = self.load_from_configured_source() {
                if disk_tok.access_token != failed_tok && !disk_tok.is_expired_soon(10) {
                    let mut guard = self.tokens.write().await;
                    *guard = Some(disk_tok.clone());
                    return Ok(disk_tok);
                }
            }
        } else if let Some(skew_secs) = min_valid_skew_secs {
            if let Some(ref cur) = current_opt {
                if !cur.is_expired_soon(skew_secs) {
                    return Ok(cur.clone());
                }
            }
        }

        let current = current_opt
            .ok_or_else(|| anyhow!("No stored token found. Run 'sharepoint login' first."))?;

        let token_url = self.token_endpoint(&current.tenant_id);
        let scope = self.effective_scopes_string();

        let token_resp = if let Some(rt) = current
            .refresh_token
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let params = [
                ("client_id", current.client_id.as_str()),
                ("grant_type", "refresh_token"),
                ("refresh_token", rt),
                ("scope", scope.as_str()),
            ];
            self.post_token_form(&token_url, &params).await?
        } else if current.auth_mode == "client_credentials" {
            let secret = env_client_secret().ok_or_else(|| {
                anyhow!("SHAREPOINT_CLIENT_SECRET environment variable is required to refresh client_credentials token.")
            })?;
            let params = [
                ("client_id", current.client_id.as_str()),
                ("client_secret", secret.as_str()),
                ("grant_type", "client_credentials"),
                ("scope", "https://graph.microsoft.com/.default"),
            ];
            self.post_token_form(&token_url, &params).await?
        } else {
            bail!("Cannot refresh token (no refresh_token). Run 'sharepoint login' again.");
        };

        let expires_in = token_resp.expires_in.unwrap_or(3600).max(60);
        let claims = parse_jwt_claims(&token_resp.access_token).unwrap_or_default();
        let updated = StoredTokens {
            access_token: token_resp.access_token,
            refresh_token: token_resp.refresh_token.or(current.refresh_token),
            expires_at: chrono::Utc::now().timestamp() + expires_in,
            client_id: current.client_id,
            tenant_id: current.tenant_id,
            user_id: current.user_id.or(claims.user_id),
            user_display_name: current.user_display_name.or(claims.user_display_name),
            user_email: current.user_email.or(claims.user_email),
            auth_mode: current.auth_mode,
        };

        let mut guard = self.tokens.write().await;
        self.persist_tokens(&updated)?;
        *guard = Some(updated.clone());
        Ok(updated)
    }

    async fn store_tokens(&self, tokens: StoredTokens) -> Result<StoredTokens> {
        let mut guard = self.tokens.write().await;
        self.persist_tokens(&tokens)?;
        *guard = Some(tokens.clone());
        Ok(tokens)
    }

    async fn login_client_credentials(
        &self,
        client_id: &str,
        tenant_id: &str,
        client_secret: &str,
    ) -> Result<StoredTokens> {
        let token_url = self.token_endpoint(tenant_id);
        let params = [
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("grant_type", "client_credentials"),
            ("scope", "https://graph.microsoft.com/.default"),
        ];
        let resp = self.post_token_form(&token_url, &params).await?;
        let expires_in = resp.expires_in.unwrap_or(3600).max(60);
        Ok(StoredTokens {
            access_token: resp.access_token,
            refresh_token: resp.refresh_token,
            expires_at: chrono::Utc::now().timestamp() + expires_in,
            client_id: client_id.to_string(),
            tenant_id: tenant_id.to_string(),
            user_id: None,
            user_display_name: None,
            user_email: None,
            auth_mode: "client_credentials".to_string(),
        })
    }

    async fn login_device_code(&self, client_id: &str, tenant_id: &str) -> Result<StoredTokens> {
        let device_url = self.devicecode_endpoint(tenant_id);
        let token_url = self.token_endpoint(tenant_id);
        let scope = self.effective_scopes_string();

        let res = self
            .http
            .post(&device_url)
            .form(&[("client_id", client_id), ("scope", scope.as_str())])
            .send()
            .await
            .context("Failed to send Device Code request")?;

        let status = res.status();
        let body_text = res.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("Failed to start Device Code flow ({}): {}", status, body_text);
        }
        let dc: DeviceCodeResponse =
            serde_json::from_str(&body_text).context("Failed to parse Device Code response")?;

        eprintln!("\n[Microsoft SharePoint / OneDrive Login - Device Code]");
        eprintln!("Open the following URL in your browser and enter the code:");
        eprintln!("  URL : {}", dc.verification_uri);
        eprintln!("  Code: {}", dc.user_code);
        if let Some(msg) = dc.message.as_deref().filter(|s| !s.trim().is_empty()) {
            eprintln!("  {}", msg.trim());
        }
        eprintln!();

        if should_open_browser() {
            let _ = open::that(&dc.verification_uri);
        }

        let mut interval_secs = dc.interval.unwrap_or(5).max(1);
        let deadline =
            tokio::time::Instant::now() + Duration::from_secs(dc.expires_in.max(30) as u64);

        loop {
            if tokio::time::Instant::now() >= deadline {
                bail!("Device Code authentication timed out. Please try again.");
            }
            tokio::time::sleep(Duration::from_secs(interval_secs)).await;

            let poll_res = self
                .http
                .post(&token_url)
                .form(&[
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ("client_id", client_id),
                    ("device_code", dc.device_code.as_str()),
                ])
                .send()
                .await
                .context("Failed to poll Device Code token")?;

            let poll_status = poll_res.status();
            let poll_text = poll_res.text().await.unwrap_or_default();
            if poll_status.is_success() {
                let tok: TokenResponse =
                    serde_json::from_str(&poll_text).context("Failed to parse token response")?;
                let expires_in = tok.expires_in.unwrap_or(3600).max(60);
                let claims = parse_jwt_claims(&tok.access_token).unwrap_or_default();
                return Ok(StoredTokens {
                    access_token: tok.access_token,
                    refresh_token: tok.refresh_token,
                    expires_at: chrono::Utc::now().timestamp() + expires_in,
                    client_id: client_id.to_string(),
                    tenant_id: tenant_id.to_string(),
                    user_id: claims.user_id,
                    user_display_name: claims.user_display_name,
                    user_email: claims.user_email,
                    auth_mode: "device_code".to_string(),
                });
            }

            let err_body: OAuthErrorResponse =
                serde_json::from_str(&poll_text).unwrap_or(OAuthErrorResponse {
                    error: format!("http_{}", poll_status.as_u16()),
                    error_description: Some(poll_text.clone()),
                });

            match err_body.error.as_str() {
                "authorization_pending" => continue,
                "slow_down" => {
                    interval_secs += 5;
                    continue;
                }
                "expired_token" => {
                    bail!("Device Code has expired. Please try again.");
                }
                "authorization_declined" | "access_denied" => {
                    bail!("Authentication was declined by the user.");
                }
                other => {
                    let desc = err_body.error_description.unwrap_or_default();
                    bail!("Device Code authentication error ({other}): {desc}");
                }
            }
        }
    }

    async fn login_pkce_loopback(
        &self,
        client_id: &str,
        tenant_id: &str,
        preferred_port: Option<u16>,
    ) -> Result<StoredTokens> {
        let code_verifier = generate_random_b64(32);
        let code_challenge = compute_pkce_s256(&code_verifier);
        let expected_state = generate_random_b64(16);

        let bind_port = preferred_port.unwrap_or(0);
        let listener = TcpListener::bind(("127.0.0.1", bind_port))
            .await
            .with_context(|| format!("Failed to bind local callback port (127.0.0.1:{bind_port})"))?;
        let actual_port = listener.local_addr()?.port();
        let redirect_uri = format!("http://localhost:{actual_port}");
        let scope = self.effective_scopes_string();

        let mut auth_url = Url::parse(&self.authorize_endpoint(tenant_id))
            .context("Failed to build authorize URL")?;
        auth_url
            .query_pairs_mut()
            .append_pair("client_id", client_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("response_mode", "query")
            .append_pair("scope", &scope)
            .append_pair("state", &expected_state)
            .append_pair("code_challenge", &code_challenge)
            .append_pair("code_challenge_method", "S256");

        eprintln!("\n[Microsoft SharePoint / OneDrive Login - Browser Auth (PKCE)]");
        eprintln!("If the browser does not open automatically, copy and open the URL below:");
        eprintln!("{}\n", auth_url.as_str());

        if should_open_browser() {
            let _ = open::that(auth_url.as_str());
        }

        let code = tokio::time::timeout(
            Duration::from_secs(LOGIN_CALLBACK_TIMEOUT_SECS),
            accept_pkce_callback(&listener, &expected_state),
        )
        .await
        .map_err(|_| anyhow!("Timed out waiting for browser login callback (5 minutes)."))??;

        let token_url = self.token_endpoint(tenant_id);
        let params = [
            ("client_id", client_id),
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("code_verifier", code_verifier.as_str()),
            ("scope", scope.as_str()),
        ];
        let tok = self.post_token_form(&token_url, &params).await?;
        let expires_in = tok.expires_in.unwrap_or(3600).max(60);
        let claims = parse_jwt_claims(&tok.access_token).unwrap_or_default();

        Ok(StoredTokens {
            access_token: tok.access_token,
            refresh_token: tok.refresh_token,
            expires_at: chrono::Utc::now().timestamp() + expires_in,
            client_id: client_id.to_string(),
            tenant_id: tenant_id.to_string(),
            user_id: claims.user_id,
            user_display_name: claims.user_display_name,
            user_email: claims.user_email,
            auth_mode: "pkce".to_string(),
        })
    }

    fn authorize_endpoint(&self, tenant_id: &str) -> String {
        format!(
            "{}/{}/oauth2/v2.0/authorize",
            self.config.auth_base_url.trim_end_matches('/'),
            tenant_id
        )
    }

    fn token_endpoint(&self, tenant_id: &str) -> String {
        format!(
            "{}/{}/oauth2/v2.0/token",
            self.config.auth_base_url.trim_end_matches('/'),
            tenant_id
        )
    }

    fn devicecode_endpoint(&self, tenant_id: &str) -> String {
        format!(
            "{}/{}/oauth2/v2.0/devicecode",
            self.config.auth_base_url.trim_end_matches('/'),
            tenant_id
        )
    }

    async fn post_token_form(&self, url: &str, params: &[(&str, &str)]) -> Result<TokenResponse> {
        let mut req = self.http.post(url).form(params);
        if let Ok(origin) = std::env::var("SHAREPOINT_TOKEN_ORIGIN") {
            let trimmed = origin.trim();
            if !trimmed.is_empty() {
                req = req.header("Origin", trimmed);
            }
        }
        let res = req
            .send()
            .await
            .context("Failed to send OAuth token request")?;
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        if !status.is_success() {
            if let Ok(err) = serde_json::from_str::<OAuthErrorResponse>(&text) {
                let desc = err.error_description.unwrap_or_default();
                bail!("OAuth token error ({} - {}): {}", status, err.error, desc);
            }
            bail!("OAuth token error ({}): {}", status, text);
        }
        serde_json::from_str::<TokenResponse>(&text).context("Failed to parse OAuth token JSON")
    }

    pub async fn update_identity(
        &self,
        user_id: String,
        display_name: String,
        email: Option<String>,
    ) -> Result<()> {
        let mut guard = self.tokens.write().await;
        if let Some(ref mut tok) = *guard {
            tok.user_id = Some(user_id);
            tok.user_display_name = Some(display_name);
            tok.user_email = email;
            if !matches!(
                tok.auth_mode.as_str(),
                "env_token" | "token_file" | "refresh_cmd" | "shared_teams_token_file"
            ) {
                self.persist_tokens(tok)?;
            }
        }
        Ok(())
    }

    pub async fn logout(&self) -> Result<()> {
        let mut guard = self.tokens.write().await;
        *guard = None;
        #[cfg(test)]
        if let Some(ref path) = self.token_path_override {
            return delete_tokens_at_path(path);
        }
        if self.ephemeral {
            return Ok(());
        }
        delete_stored_tokens()
    }
}

fn read_keyring_tokens(service: &str) -> Option<StoredTokens> {
    #[cfg(target_os = "linux")]
    {
        let service_owned = service.to_string();
        if let Some(Ok(secret_json)) = run_linux_keyring_op(move || {
            let entry = keyring::Entry::new(&service_owned, KEYRING_USER)?;
            entry.get_password()
        }) {
            if !secret_json.trim().is_empty() {
                if let Ok(parsed) = serde_json::from_str::<StoredTokens>(&secret_json) {
                    return Some(parsed);
                }
            }
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        if let Ok(entry) = keyring::Entry::new(service, KEYRING_USER) {
            if let Ok(secret_json) = entry.get_password() {
                if !secret_json.trim().is_empty() {
                    if let Ok(parsed) = serde_json::from_str::<StoredTokens>(&secret_json) {
                        return Some(parsed);
                    }
                }
            }
        }
        None
    }
}

pub fn load_stored_tokens() -> Result<Option<StoredTokens>> {
    let keyring_tok = if should_use_keyring() {
        read_keyring_tokens(KEYRING_SERVICE)
    } else {
        None
    };
    let file_tok = load_tokens_from_path(&credential_fallback_path())?;
    match (keyring_tok, file_tok) {
        (Some(kr), Some(fl)) => {
            if fl.expires_at > kr.expires_at {
                Ok(Some(fl))
            } else {
                Ok(Some(kr))
            }
        }
        (Some(kr), None) => Ok(Some(kr)),
        (None, Some(fl)) => Ok(Some(fl)),
        (None, None) => Ok(None),
    }
}

pub fn save_stored_tokens(tokens: &StoredTokens) -> Result<()> {
    let fallback = credential_fallback_path();
    if !should_use_keyring() {
        return save_tokens_to_path(tokens, &fallback);
    }

    let json = serde_json::to_string_pretty(tokens)?;
    #[cfg(target_os = "linux")]
    {
        let json_for_keyring = json;
        if let Some(Ok(())) = run_linux_keyring_op(move || {
            let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)?;
            entry.set_password(&json_for_keyring)
        }) {
            let _ = save_tokens_to_path(tokens, &fallback);
            return Ok(());
        }
        return save_tokens_to_path(tokens, &fallback);
    }
    #[cfg(not(target_os = "linux"))]
    {
        #[cfg(target_os = "windows")]
        if json.len() > WINDOWS_CRED_MAX_BYTES {
            return save_tokens_to_path(tokens, &fallback);
        }
        match keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER) {
            Ok(entry) => {
                if entry.set_password(&json).is_ok() {
                    let _ = save_tokens_to_path(tokens, &fallback);
                    return Ok(());
                }
                let _ = entry.delete_credential();
                save_tokens_to_path(tokens, &fallback)
            }
            Err(_) => save_tokens_to_path(tokens, &fallback),
        }
    }
}

pub fn delete_stored_tokens() -> Result<()> {
    if should_use_keyring() {
        #[cfg(target_os = "linux")]
        {
            let _ = run_linux_keyring_op(|| {
                let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)?;
                match entry.delete_credential() {
                    Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                    Err(e) => Err(e),
                }
            });
        }
        #[cfg(not(target_os = "linux"))]
        {
            if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER) {
                let _ = entry.delete_credential();
            }
        }
    }
    delete_tokens_at_path(&credential_fallback_path())
}

pub(crate) fn load_tokens_from_path(path: &Path) -> Result<Option<StoredTokens>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read credential file: {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(None);
    }
    let parsed: StoredTokens = serde_json::from_str(&raw)
        .with_context(|| format!("Failed to parse credential file: {}", path.display()))?;
    Ok(Some(parsed))
}

pub(crate) fn save_tokens_to_path(tokens: &StoredTokens, path: &Path) -> Result<()> {
    let json = serde_json::to_vec_pretty(tokens)?;
    write_atomic(path, &json)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

pub(crate) fn delete_tokens_at_path(path: &Path) -> Result<()> {
    if path.exists() {
        std::fs::remove_file(path)
            .with_context(|| format!("Failed to delete credential file: {}", path.display()))?;
    }
    Ok(())
}

/// Computes RFC 7636 PKCE `S256` challenge: `BASE64URL-ENCODE(SHA256(ASCII(code_verifier)))`.
pub fn compute_pkce_s256(code_verifier: &str) -> String {
    let digest = Sha256::digest(code_verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

fn generate_random_b64(num_bytes: usize) -> String {
    let mut buf = vec![0u8; num_bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    URL_SAFE_NO_PAD.encode(buf)
}

fn build_auth_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("sharepoint-cli/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()
        .context("Failed to initialize HTTP client")
}

fn is_truthy_env(val: Option<&str>) -> bool {
    val.map(|v| {
        let t = v.trim().to_ascii_lowercase();
        matches!(t.as_str(), "1" | "true" | "yes")
    })
    .unwrap_or(false)
}

fn keyring_disabled() -> bool {
    is_truthy_env(std::env::var("SHAREPOINT_DISABLE_KEYRING").ok().as_deref())
}

#[cfg(any(target_os = "linux", test))]
fn linux_secret_service_likely_available_with<F>(
    dbus_addr: Option<&str>,
    runtime_failed: bool,
    path_exists: F,
) -> bool
where
    F: Fn(&Path) -> bool,
{
    if runtime_failed {
        return false;
    }
    let Some(raw) = dbus_addr.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };

    for segment in raw.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(params) = segment.strip_prefix("unix:") {
            let mut socket_path: Option<&str> = None;
            for kv in params.split(',') {
                if let Some(p) = kv.trim().strip_prefix("path=") {
                    socket_path = Some(p.trim());
                    break;
                }
            }
            match socket_path {
                Some(p) if !p.is_empty() && path_exists(Path::new(p)) => return true,
                Some(_) => continue,
                None => return true,
            }
        } else {
            return true;
        }
    }

    false
}

#[cfg(target_os = "linux")]
fn linux_secret_service_likely_available() -> bool {
    let addr = std::env::var("DBUS_SESSION_BUS_ADDRESS").ok();
    linux_secret_service_likely_available_with(
        addr.as_deref(),
        KEYRING_RUNTIME_FAILED.load(Ordering::Relaxed),
        |p| p.exists(),
    )
}

fn should_use_keyring() -> bool {
    if keyring_disabled() || KEYRING_RUNTIME_FAILED.load(Ordering::Relaxed) {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        if !linux_secret_service_likely_available() {
            return false;
        }
    }
    true
}

#[cfg(target_os = "linux")]
fn run_linux_keyring_op<T, F>(op: F) -> Option<std::result::Result<T, keyring::Error>>
where
    T: Send + 'static,
    F: FnOnce() -> std::result::Result<T, keyring::Error> + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    if std::thread::Builder::new()
        .name("sharepoint-keyring".to_string())
        .spawn(move || {
            let _ = tx.send(op());
        })
        .is_err()
    {
        KEYRING_RUNTIME_FAILED.store(true, Ordering::Relaxed);
        return None;
    }

    match rx.recv_timeout(LINUX_KEYRING_TIMEOUT) {
        Ok(Ok(val)) => Some(Ok(val)),
        Ok(Err(err)) => {
            if !matches!(err, keyring::Error::NoEntry) {
                KEYRING_RUNTIME_FAILED.store(true, Ordering::Relaxed);
            }
            Some(Err(err))
        }
        Err(_) => {
            KEYRING_RUNTIME_FAILED.store(true, Ordering::Relaxed);
            None
        }
    }
}

fn should_open_browser_with_env(
    is_unix_non_macos: bool,
    no_browser: Option<&str>,
    display: Option<&str>,
    wayland_display: Option<&str>,
    wsl_distro_name: Option<&str>,
) -> bool {
    if is_truthy_env(no_browser) {
        return false;
    }
    if !is_unix_non_macos {
        return true;
    }
    let has_non_empty = |v: Option<&str>| v.map(|s| !s.trim().is_empty()).unwrap_or(false);
    has_non_empty(display)
        || has_non_empty(wayland_display)
        || has_non_empty(wsl_distro_name)
}

fn should_open_browser() -> bool {
    let no_browser = std::env::var("SHAREPOINT_NO_BROWSER")
        .ok()
        .or_else(|| std::env::var("TEAMS_NO_BROWSER").ok());
    should_open_browser_with_env(
        cfg!(all(unix, not(target_os = "macos"))),
        no_browser.as_deref(),
        std::env::var("DISPLAY").ok().as_deref(),
        std::env::var("WAYLAND_DISPLAY").ok().as_deref(),
        std::env::var("WSL_DISTRO_NAME").ok().as_deref(),
    )
}

fn env_client_secret() -> Option<String> {
    for key in ["SHAREPOINT_CLIENT_SECRET", "AZURE_CLIENT_SECRET"] {
        if let Ok(val) = std::env::var(key) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn should_skip_keyring_on_init(config: &AppConfig) -> bool {
    env_access_token(config).is_some()
        || config.resolved_token_file().is_some()
        || !config.token_refresh_cmd.trim().is_empty()
}

fn env_access_token(config: &AppConfig) -> Option<StoredTokens> {
    let raw = std::env::var("SHAREPOINT_ACCESS_TOKEN").ok()?;
    env_access_token_from_str(&raw, config)
}

fn env_access_token_from_str(raw: &str, config: &AppConfig) -> Option<StoredTokens> {
    let trimmed = strip_bearer_prefix(raw);
    if trimmed.is_empty() {
        return None;
    }
    Some(stored_tokens_from_jwt(
        trimmed,
        None,
        None,
        &config.client_id,
        &config.tenant_id,
        "env_token",
    ))
}

fn expand_tilde_path(input: &str) -> PathBuf {
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenScopeDiagnostics {
    pub scopes: Vec<String>,
    pub roles: Vec<String>,
    pub has_scope_claims: bool,
    pub can_read_files: bool,
    pub can_write_files: bool,
    pub can_read_all_files: bool,
    pub can_read_sites: bool,
    pub can_write_sites: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct JwtClaims {
    exp: Option<i64>,
    user_id: Option<String>,
    user_display_name: Option<String>,
    user_email: Option<String>,
    tenant_id: Option<String>,
    client_id: Option<String>,
    scopes: Vec<String>,
    roles: Vec<String>,
}

fn token_has_sharepoint_scopes(token: &str) -> bool {
    let Some(claims) = parse_jwt_claims(token) else {
        return false;
    };
    claims
        .scopes
        .iter()
        .chain(claims.roles.iter())
        .any(|s| is_sharepoint_scope_name(s))
}

fn token_is_sharepoint_eligible_or_opaque(token: &str) -> bool {
    let Some(claims) = parse_jwt_claims(token) else {
        return true;
    };
    if claims.scopes.is_empty() && claims.roles.is_empty() {
        return true;
    }
    claims
        .scopes
        .iter()
        .chain(claims.roles.iter())
        .any(|s| is_sharepoint_scope_name(s))
}

fn is_sharepoint_scope_name(scope: &str) -> bool {
    let lower = scope.trim().to_ascii_lowercase();
    lower.starts_with("files.") || lower.starts_with("sites.")
}

/// Inspects JWT `scp` / `roles` claims on an access token to detect missing OneDrive / SharePoint Graph permissions.
pub fn diagnose_access_token_scopes(access_token: &str) -> TokenScopeDiagnostics {
    let Some(claims) = parse_jwt_claims(access_token) else {
        return TokenScopeDiagnostics::default();
    };
    let has_scope_claims = !claims.scopes.is_empty() || !claims.roles.is_empty();
    let has_any = |candidates: &[&str]| -> bool {
        candidates.iter().any(|target| {
            claims
                .scopes
                .iter()
                .chain(claims.roles.iter())
                .any(|s| s.eq_ignore_ascii_case(target))
        })
    };

    let can_read_files = has_any(&[
        "Files.Read",
        "Files.ReadWrite",
        "Files.Read.All",
        "Files.ReadWrite.All",
        "Sites.Read.All",
        "Sites.ReadWrite.All",
    ]);
    let can_write_files = has_any(&[
        "Files.ReadWrite",
        "Files.ReadWrite.All",
        "Sites.ReadWrite.All",
    ]);
    let can_read_all_files = has_any(&[
        "Files.Read.All",
        "Files.ReadWrite.All",
        "Sites.Read.All",
        "Sites.ReadWrite.All",
    ]);
    let can_read_sites = has_any(&[
        "Sites.Read.All",
        "Sites.ReadWrite.All",
        "Sites.Manage.All",
        "Sites.FullControl.All",
    ]);
    let can_write_sites = has_any(&[
        "Sites.ReadWrite.All",
        "Sites.Manage.All",
        "Sites.FullControl.All",
    ]);

    let mut warnings = Vec::new();
    if has_scope_claims {
        if !can_read_files {
            warnings.push(
                "Current token lacks `Files.Read` or `Files.ReadWrite`; browsing OneDrive files may be denied.".to_string(),
            );
        } else if !can_write_files {
            warnings.push(
                "Current token lacks `Files.ReadWrite` or `Files.ReadWrite.All`; uploading, renaming, or deleting files may be denied.".to_string(),
            );
        }
        if !can_read_sites {
            warnings.push(
                "Current token lacks `Sites.Read.All` or `Sites.ReadWrite.All`; discovering SharePoint sites and document libraries may be denied.".to_string(),
            );
        } else if !can_write_sites {
            warnings.push(
                "Current token lacks `Sites.ReadWrite.All`; writing to SharePoint document libraries may be denied.".to_string(),
            );
        }
    }

    TokenScopeDiagnostics {
        scopes: claims.scopes,
        roles: claims.roles,
        has_scope_claims,
        can_read_files,
        can_write_files,
        can_read_all_files,
        can_read_sites,
        can_write_sites,
        warnings,
    }
}

fn contains_http_403_token(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 3 <= bytes.len() {
        if &bytes[i..i + 3] == b"403" {
            let prev_ok = if i == 0 {
                true
            } else {
                let p = bytes[i - 1];
                !p.is_ascii_alphanumeric() && p != b':' && p != b'/'
            };
            let next_ok = if i + 3 == bytes.len() {
                true
            } else {
                !bytes[i + 3].is_ascii_alphanumeric()
            };
            if prev_ok && next_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Converts a raw Graph 403 / insufficient-privileges error into an actionable English explanation with SharePoint/OneDrive scope context.
pub fn explain_graph_permission_error(err_msg: &str, access_token: Option<&str>) -> Option<String> {
    let detail = err_msg
        .split_once("): ")
        .map(|(_, d)| d)
        .unwrap_or(err_msg);
    let lower = detail.to_ascii_lowercase();
    let looks_forbidden = contains_http_403_token(&lower)
        || lower.contains("forbidden")
        || lower.contains("authorization_requestdenied")
        || lower.contains("erroraccessdenied")
        || lower.contains("accessdenied")
        || lower.contains("insufficient privileges")
        || lower.contains("missing scope")
        || lower.contains("aclcheckfailed");
    if !looks_forbidden {
        return None;
    }
    if let Some(tok) = access_token {
        let diag = diagnose_access_token_scopes(tok);
        let full_lower = err_msg.to_ascii_lowercase();
        let is_write = full_lower.contains("(put ")
            || full_lower.contains("(post ")
            || full_lower.contains("(patch ")
            || full_lower.contains("(delete ");
        let targeted = if full_lower.contains("/sites") || full_lower.contains("/followedsites") {
            if !diag.can_read_sites {
                Some(
                    "Current token lacks `Sites.Read.All` or `Sites.ReadWrite.All`; accessing SharePoint sites may be denied."
                        .to_string(),
                )
            } else if is_write && !diag.can_write_sites {
                Some(
                    "Current token lacks `Sites.ReadWrite.All`; modifying SharePoint site content may be denied."
                        .to_string(),
                )
            } else {
                None
            }
        } else if is_write && !diag.can_write_files {
            Some(
                "Current token lacks `Files.ReadWrite` or `Files.ReadWrite.All`; uploading, creating folders, moving, or deleting items may be denied."
                    .to_string(),
            )
        } else if !diag.can_read_files {
            Some(
                "Current token lacks `Files.Read` or `Files.Read.All`; reading OneDrive/SharePoint files may be denied."
                    .to_string(),
            )
        } else {
            None
        };
        if let Some(msg) = targeted.or_else(|| diag.warnings.into_iter().next()) {
            return Some(format!("{INSUFFICIENT_PERMISSIONS_PREFIX} {msg}"));
        }
    }
    Some(format!(
        "{INSUFFICIENT_PERMISSIONS_PREFIX} current token lacks required SharePoint/OneDrive scopes (`Files.ReadWrite.All`, `Sites.ReadWrite.All`) or access to this resource is restricted."
    ))
}

fn strip_bearer_prefix(raw: &str) -> &str {
    let trimmed = raw.trim();
    if trimmed.len() >= 7 && trimmed[..7].eq_ignore_ascii_case("bearer ") {
        trimmed[7..].trim()
    } else {
        trimmed
    }
}

fn parse_jwt_claims(token: &str) -> Option<JwtClaims> {
    let token = strip_bearer_prefix(token);
    let payload_b64 = token.split('.').nth(1)?.trim();
    if payload_b64.is_empty() {
        return None;
    }
    let normalized: String = payload_b64
        .trim_end_matches('=')
        .chars()
        .map(|c| match c {
            '+' => '-',
            '/' => '_',
            _ => c,
        })
        .collect();
    let decoded = URL_SAFE_NO_PAD.decode(&normalized).ok()?;
    let val: serde_json::Value = serde_json::from_slice(&decoded).ok()?;

    let claim_str = |key: &str| -> Option<String> {
        val.get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let exp = val
        .get("exp")
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)));

    let scopes = match val.get("scp") {
        Some(serde_json::Value::String(s)) => s
            .split_whitespace()
            .filter(|w| !w.is_empty())
            .map(str::to_string)
            .collect(),
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    };
    let roles = match val.get("roles") {
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        Some(serde_json::Value::String(s)) => s
            .split_whitespace()
            .filter(|w| !w.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    };

    Some(JwtClaims {
        exp,
        user_id: claim_str("oid").or_else(|| claim_str("sub")),
        user_display_name: claim_str("name"),
        user_email: claim_str("upn")
            .or_else(|| claim_str("preferred_username"))
            .or_else(|| claim_str("email")),
        tenant_id: claim_str("tid"),
        client_id: claim_str("appid").or_else(|| claim_str("azp")),
        scopes,
        roles,
    })
}

fn stored_tokens_from_jwt_with_claims(
    token: String,
    claims: JwtClaims,
    refresh_token: Option<String>,
    expires_at_override: Option<i64>,
    fallback_client_id: &str,
    fallback_tenant_id: &str,
    auth_mode: &str,
) -> StoredTokens {
    let expires_at = expires_at_override
        .or(claims.exp)
        .unwrap_or_else(|| chrono::Utc::now().timestamp() + 3600);
    StoredTokens {
        access_token: token,
        refresh_token,
        expires_at,
        client_id: claims
            .client_id
            .unwrap_or_else(|| fallback_client_id.to_string()),
        tenant_id: claims
            .tenant_id
            .unwrap_or_else(|| fallback_tenant_id.to_string()),
        user_id: claims.user_id,
        user_display_name: claims.user_display_name,
        user_email: claims.user_email,
        auth_mode: auth_mode.to_string(),
    }
}

fn stored_tokens_from_jwt(
    raw_token: &str,
    refresh_token: Option<String>,
    expires_at_override: Option<i64>,
    fallback_client_id: &str,
    fallback_tenant_id: &str,
    auth_mode: &str,
) -> StoredTokens {
    let token = strip_bearer_prefix(raw_token).to_string();
    let claims = parse_jwt_claims(&token).unwrap_or_default();
    stored_tokens_from_jwt_with_claims(
        token,
        claims,
        refresh_token,
        expires_at_override,
        fallback_client_id,
        fallback_tenant_id,
        auth_mode,
    )
}

fn json_non_empty_str(val: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| {
        val.get(*k)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    })
}

fn parse_expiry_value(val: &serde_json::Value) -> Option<i64> {
    use chrono::TimeZone;
    if let Some(ts) = val.as_i64() {
        return Some(ts);
    }
    if let Some(f) = val.as_f64() {
        return Some(f as i64);
    }
    let s = val.as_str()?.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(ts) = s.parse::<i64>() {
        return Some(ts);
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp());
    }
    for fmt in [
        "%Y-%m-%d %H:%M:%S%.f%:z",
        "%Y-%m-%d %H:%M:%S%:z",
        "%Y-%m-%d %H:%M:%S%.f %z",
        "%Y-%m-%d %H:%M:%S %z",
    ] {
        if let Ok(dt) = chrono::DateTime::parse_from_str(s, fmt) {
            return Some(dt.timestamp());
        }
    }
    for fmt in [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
    ] {
        if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            if let Some(dt) = chrono::Local.from_local_datetime(&ndt).earliest() {
                return Some(dt.timestamp());
            }
        }
    }
    None
}

fn parse_external_token_str(
    raw: &str,
    auth_mode: &str,
    config: &AppConfig,
    source_label: &str,
) -> Result<StoredTokens> {
    let cleaned = raw.trim_start_matches('\u{feff}').trim();
    if cleaned.is_empty() {
        bail!("Token file is empty: {source_label}");
    }

    if cleaned.starts_with('{') {
        let val: serde_json::Value = serde_json::from_str(cleaned)
            .with_context(|| format!("Failed to parse external token JSON: {source_label}"))?;
        let raw_token = json_non_empty_str(&val, &["access_token", "accessToken", "token"])
            .ok_or_else(|| {
                anyhow!("External token JSON is missing access_token/accessToken/token field: {source_label}")
            })?;
        let refresh_token = json_non_empty_str(&val, &["refresh_token", "refreshToken"]);
        let token = strip_bearer_prefix(&raw_token).to_string();
        let claims = parse_jwt_claims(&token).unwrap_or_default();
        let exp_override = ["expires_at", "expires_on", "expiresOn"]
            .iter()
            .find_map(|k| val.get(*k).and_then(parse_expiry_value))
            .or(claims.exp)
            .or_else(|| {
                val.get("expires_in")
                    .and_then(|v| v.as_i64())
                    .map(|secs| chrono::Utc::now().timestamp() + secs)
            });

        let mut tok = stored_tokens_from_jwt_with_claims(
            token,
            claims,
            refresh_token,
            exp_override,
            &config.client_id,
            &config.tenant_id,
            auth_mode,
        );
        if let Some(cid) = json_non_empty_str(&val, &["client_id", "clientId"]) {
            tok.client_id = cid;
        }
        if let Some(tid) = json_non_empty_str(&val, &["tenant_id", "tenantId", "tenant"]) {
            tok.tenant_id = tid;
        }
        if tok.user_id.is_none() {
            tok.user_id = json_non_empty_str(&val, &["user_id", "userId"]);
        }
        if tok.user_display_name.is_none() {
            tok.user_display_name =
                json_non_empty_str(&val, &["user_display_name", "userDisplayName"]);
        }
        if tok.user_email.is_none() {
            tok.user_email = json_non_empty_str(&val, &["user_email", "userEmail"]);
        }
        return Ok(tok);
    }

    let first_line = cleaned
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .ok_or_else(|| anyhow!("Token file is empty: {source_label}"))?;
    let token = strip_bearer_prefix(first_line);
    if token.is_empty() {
        bail!("Token file is empty: {source_label}");
    }
    Ok(stored_tokens_from_jwt(
        token,
        None,
        None,
        &config.client_id,
        &config.tenant_id,
        auth_mode,
    ))
}

/// Reads an external access token (raw JWT or JSON from Azure CLI / `StoredTokens`)
/// from `path`, normalizing BOM, optional `Bearer ` prefix, and JWT claims.
pub fn read_external_token_file(path: &Path, config: &AppConfig) -> Result<StoredTokens> {
    read_external_token_file_with_mode(path, "token_file", config)
}

fn read_external_token_file_with_mode(
    path: &Path,
    auth_mode: &str,
    config: &AppConfig,
) -> Result<StoredTokens> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read external token file: {}", path.display()))?;
    parse_external_token_str(&raw, auth_mode, config, &path.display().to_string())
}

async fn execute_external_refresh_shell(cmd_str: &str, token_file: Option<&Path>) -> Result<String> {
    #[cfg(windows)]
    let mut cmd = {
        use std::os::windows::process::CommandExt;
        let mut std_cmd = std::process::Command::new("cmd");
        std_cmd.raw_arg(format!("/S /C \"{cmd_str}\""));
        tokio::process::Command::from(std_cmd)
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(cmd_str);
        c
    };
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);
    if let Some(path) = token_file {
        cmd.env("SHAREPOINT_TOKEN_FILE", path);
    }

    let child = cmd
        .spawn()
        .with_context(|| format!("Failed to execute external token refresh command: {cmd_str}"))?;
    let output = tokio::time::timeout(EXTERNAL_REFRESH_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| anyhow!("External token refresh command timed out (60s)"))??;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let detail = if stderr.is_empty() {
            stdout.trim().to_string()
        } else {
            stderr
        };
        let code_str = output
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".to_string());
        if detail.is_empty() {
            bail!("exit code {code_str}");
        }
        bail!("exit code {code_str}: {detail}");
    }
    Ok(stdout)
}

fn fallback_or_bail_refresh_error(
    current: Option<&StoredTokens>,
    err: &str,
) -> Result<StoredTokens> {
    if let Some(cur) = current {
        if !cur.is_expired_soon(0) {
            if !REFRESH_CMD_WARNED.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "Warning: external token refresh command failed ({err}); continuing with existing token until expiration ({}).",
                    format_local_exp(cur.expires_at)
                );
            }
            return Ok(cur.clone());
        }
        bail!(
            "External token refresh failed (exp: {}): {err}",
            format_local_exp(cur.expires_at)
        );
    }
    bail!("External token refresh failed (exp: none): {err}");
}

fn format_local_exp(ts: i64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_opt(ts, 0).single() {
        Some(dt) => dt.format("%Y-%m-%d %H:%M").to_string(),
        None => ts.to_string(),
    }
}

async fn accept_pkce_callback(listener: &TcpListener, expected_state: &str) -> Result<String> {
    loop {
        let (mut socket, _) = listener
            .accept()
            .await
            .context("Failed to accept callback connection")?;

        let mut buf = Vec::with_capacity(4096);
        let read_res = tokio::time::timeout(Duration::from_secs(5), async {
            let mut chunk = [0u8; 2048];
            loop {
                let n = socket.read(&mut chunk).await?;
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() >= 16384 {
                    break;
                }
            }
            Ok::<(), std::io::Error>(())
        })
        .await;
        if !matches!(read_res, Ok(Ok(()))) {
            let _ = write_http_response(
                &mut socket,
                "408 Request Timeout",
                "<h1>Request Timeout</h1>",
            )
            .await;
            continue;
        }

        let req_text = String::from_utf8_lossy(&buf);
        let first_line = req_text.lines().next().unwrap_or("");
        let target = first_line.split_whitespace().nth(1).unwrap_or("/");

        let parsed = match Url::parse(&format!("http://localhost{target}")) {
            Ok(u) => u,
            Err(_) => {
                let _ = write_http_response(
                    &mut socket,
                    "400 Bad Request",
                    "<h1>Invalid request</h1>",
                )
                .await;
                continue;
            }
        };

        if !matches!(parsed.path(), "/" | "/callback") {
            let _ = write_http_response(&mut socket, "404 Not Found", "<h1>Not Found</h1>").await;
            continue;
        }

        let params: HashMap<String, String> = parsed.query_pairs().into_owned().collect();

        if let Some(err) = params.get("error") {
            let desc = params
                .get("error_description")
                .cloned()
                .unwrap_or_default();
            let html = format!(
                "<!doctype html><html><body style=\"font-family:sans-serif;padding:2rem\"><h2>Login Failed</h2><p>{}: {}</p></body></html>",
                html_escape(err),
                html_escape(&desc)
            );
            let _ = write_http_response(&mut socket, "400 Bad Request", &html).await;
            bail!("Browser OAuth authentication failed ({err}): {desc}");
        }

        let state = params.get("state").map(String::as_str).unwrap_or("");
        if state != expected_state {
            let _ = write_http_response(
                &mut socket,
                "400 Bad Request",
                "<!doctype html><html><body style=\"font-family:sans-serif;padding:2rem\"><h2>State mismatch</h2><p>Security verification (state) did not match.</p></body></html>",
            )
            .await;
            continue;
        }

        let code = match params.get("code").filter(|s| !s.is_empty()) {
            Some(c) => c.clone(),
            None => {
                let _ = write_http_response(
                    &mut socket,
                    "400 Bad Request",
                    "<!doctype html><html><body style=\"font-family:sans-serif;padding:2rem\"><h2>Missing code</h2><p>Authorization code is missing.</p></body></html>",
                )
                .await;
                continue;
            }
        };

        let success_html = r#"<!doctype html>
<html lang="en">
<head><meta charset="utf-8"><title>SharePoint / OneDrive CLI Login</title></head>
<body style="font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif;display:flex;justify-content:center;align-items:center;height:100vh;margin:0;background:#f5f5f7;color:#1d1d1f">
  <div style="background:#fff;padding:2.5rem 3rem;border-radius:12px;box-shadow:0 4px 20px rgba(0,0,0,.08);text-align:center">
    <h1 style="margin:0 0 .5rem;color:#0078d4">Microsoft SharePoint / OneDrive CLI Login Complete</h1>
    <p style="margin:0;color:#555">Authentication succeeded. You can close this window and return to the terminal.</p>
  </div>
</body>
</html>"#;
        let _ = write_http_response(&mut socket, "200 OK", success_html).await;
        return Ok(code);
    }
}

async fn write_http_response(
    socket: &mut tokio::net::TcpStream,
    status: &str,
    body_html: &str,
) -> Result<()> {
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body_html}",
        body_html.len()
    );
    socket.write_all(resp.as_bytes()).await?;
    socket.flush().await?;
    let _ = socket.shutdown().await;
    Ok(())
}

fn html_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    #[serde(alias = "verification_url")]
    verification_uri: String,
    expires_in: i64,
    #[serde(default)]
    interval: Option<u64>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OAuthErrorResponse {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EnvVarGuard, TEST_ENV_LOCK as ENV_LOCK};
    use tempfile::tempdir;

    #[test]
    fn test_default_scopes_contract() {
        assert_eq!(
            DEFAULT_SCOPES,
            &[
                "Files.Read",
                "Files.ReadWrite",
                "Files.Read.All",
                "Files.ReadWrite.All",
                "Sites.Read.All",
                "Sites.ReadWrite.All",
                "User.Read",
                "offline_access",
            ]
        );
    }

    #[test]
    fn test_pkce_s256_rfc7636_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let expected = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert_eq!(compute_pkce_s256(verifier), expected);
    }

    #[test]
    fn test_stored_tokens_expiry_skew() {
        let now = chrono::Utc::now().timestamp();
        let fresh = StoredTokens {
            access_token: "tok".into(),
            refresh_token: Some("rt".into()),
            expires_at: now + 300,
            client_id: "cid".into(),
            tenant_id: "common".into(),
            user_id: None,
            user_display_name: None,
            user_email: None,
            auth_mode: "pkce".into(),
        };
        assert!(!fresh.is_expired_soon(90));
        assert!(fresh.is_expired_soon(300));

        let expiring = StoredTokens {
            expires_at: now + 30,
            ..fresh
        };
        assert!(expiring.is_expired_soon(90));
        assert!(!expiring.is_expired_soon(10));
    }

    #[test]
    fn test_file_fallback_roundtrip_and_delete() {
        let dir = tempdir().unwrap();
        let cred_path = dir.path().join("nested").join("credentials.json");

        assert!(load_tokens_from_path(&cred_path).unwrap().is_none());

        let tokens = StoredTokens {
            access_token: "eyJ-access".into(),
            refresh_token: Some("0.AR-refresh".into()),
            expires_at: 1_900_000_000,
            client_id: "client-123".into(),
            tenant_id: "tenant-456".into(),
            user_id: Some("user-1".into()),
            user_display_name: Some("Alice".into()),
            user_email: Some("alice@example.com".into()),
            auth_mode: "pkce".into(),
        };

        save_tokens_to_path(&tokens, &cred_path).unwrap();
        assert!(cred_path.exists());

        let loaded = load_tokens_from_path(&cred_path).unwrap().unwrap();
        assert_eq!(loaded, tokens);

        delete_tokens_at_path(&cred_path).unwrap();
        assert!(!cred_path.exists());
        assert!(load_tokens_from_path(&cred_path).unwrap().is_none());
    }

    #[tokio::test]
    async fn test_direct_access_token_login_and_valid_token() {
        let _env_lock = ENV_LOCK.lock().await;
        let _env_guard = EnvVarGuard::clear_auth_env();
        let mut cfg = AppConfig::default();
        cfg.client_id = "cid".into();
        cfg.tenant_id = "tid".into();

        let tok = StoredTokens {
            access_token: "direct-tok".into(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 1800,
            client_id: cfg.client_id.clone(),
            tenant_id: cfg.tenant_id.clone(),
            user_id: None,
            user_display_name: None,
            user_email: None,
            auth_mode: "access_token".into(),
        };
        let mgr = AuthManager::from_tokens(cfg, tok);
        assert_eq!(mgr.valid_access_token().await.unwrap(), "direct-tok");
        assert!(!mgr.is_refreshable().await);
    }

    #[tokio::test]
    async fn test_client_credentials_login_and_concurrent_refresh() {
        use axum::{routing::post, Json, Router};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let _env_lock = ENV_LOCK.lock().await;
        let _env_guard = EnvVarGuard::clear_auth_env();

        let hits = Arc::new(AtomicUsize::new(0));
        let hits_clone = hits.clone();

        let app = Router::new().route(
            "/tenant-1/oauth2/v2.0/token",
            post(move |body: String| {
                let hits = hits_clone.clone();
                async move {
                    let count = hits.fetch_add(1, Ordering::SeqCst) + 1;
                    if body.contains("grant_type=client_credentials") {
                        Json(serde_json::json!({
                            "access_token": format!("cc-token-{count}"),
                            "expires_in": 3600
                        }))
                    } else {
                        Json(serde_json::json!({
                            "access_token": format!("refreshed-token-{count}"),
                            "refresh_token": "new-rt",
                            "expires_in": 3600
                        }))
                    }
                }
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let dir = tempdir().unwrap();
        let cred_path = dir.path().join("creds.json");

        let mut cfg = AppConfig::default();
        cfg.auth_base_url = format!("http://{addr}");
        cfg.client_id = "cid".into();
        cfg.tenant_id = "tenant-1".into();

        let dummy = StoredTokens {
            access_token: "old".into(),
            refresh_token: Some("rt-0".into()),
            expires_at: 0,
            client_id: "cid".into(),
            tenant_id: "tenant-1".into(),
            user_id: None,
            user_display_name: None,
            user_email: None,
            auth_mode: "pkce".into(),
        };
        let mgr = AuthManager::from_tokens(cfg.clone(), dummy).with_token_path(cred_path.clone());
        let logged_in = mgr
            .login(&LoginOptions {
                client_secret: Some("secret-xyz".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(logged_in.auth_mode, "client_credentials");
        assert_eq!(logged_in.access_token, "cc-token-1");

        let expired_tok = StoredTokens {
            access_token: "stale-tok".into(),
            refresh_token: Some("rt-1".into()),
            expires_at: chrono::Utc::now().timestamp() + 300,
            client_id: "cid".into(),
            tenant_id: "tenant-1".into(),
            user_id: None,
            user_display_name: None,
            user_email: None,
            auth_mode: "pkce".into(),
        };
        let mgr2 = Arc::new(
            AuthManager::from_tokens(cfg, expired_tok.clone()).with_token_path(cred_path.clone()),
        );
        save_tokens_to_path(&expired_tok, &cred_path).unwrap();
        let hits_before = hits.load(Ordering::SeqCst);
        let (r1, r2, r3) = tokio::join!(
            mgr2.refresh_for_failed_token("stale-tok"),
            mgr2.refresh_for_failed_token("stale-tok"),
            mgr2.refresh_for_failed_token("stale-tok"),
        );
        assert_eq!(r1.unwrap().access_token, r2.unwrap().access_token);
        assert_eq!(r3.unwrap().refresh_token.as_deref(), Some("new-rt"));
        assert_eq!(hits.load(Ordering::SeqCst) - hits_before, 1);
    }

    #[tokio::test]
    async fn test_hot_reload_external_f12_token_from_disk() {
        let _env_lock = ENV_LOCK.lock().await;
        let _env_guard = EnvVarGuard::clear_auth_env();
        let dir = tempdir().unwrap();
        let cred_path = dir.path().join("credentials.json");
        let now = chrono::Utc::now().timestamp();

        let initial = StoredTokens {
            access_token: "f12-old-token".into(),
            refresh_token: None,
            expires_at: now + 20,
            client_id: "cid".into(),
            tenant_id: "tid".into(),
            user_id: Some("u1".into()),
            user_display_name: Some("Tester".into()),
            user_email: None,
            auth_mode: "access_token".into(),
        };
        save_tokens_to_path(&initial, &cred_path).unwrap();

        let mgr = AuthManager::from_tokens(AppConfig::default(), initial)
            .with_token_path(cred_path.clone());

        let externally_refreshed = StoredTokens {
            access_token: "f12-fresh-token".into(),
            refresh_token: None,
            expires_at: now + 3600,
            client_id: "cid".into(),
            tenant_id: "tid".into(),
            user_id: Some("u1".into()),
            user_display_name: Some("Tester".into()),
            user_email: None,
            auth_mode: "access_token".into(),
        };
        save_tokens_to_path(&externally_refreshed, &cred_path).unwrap();

        assert_eq!(mgr.valid_access_token().await.unwrap(), "f12-fresh-token");
    }

    #[test]
    fn test_linux_secret_service_availability_detection() {
        let dir = tempdir().unwrap();
        let existing_socket = dir.path().join("bus");
        std::fs::write(&existing_socket, b"").unwrap();
        let existing_str = existing_socket.to_string_lossy().to_string();
        let missing_str = dir.path().join("missing-bus").to_string_lossy().to_string();

        assert!(!linux_secret_service_likely_available_with(
            None,
            false,
            |p| p.exists()
        ));
        assert!(!linux_secret_service_likely_available_with(
            Some("   "),
            false,
            |p| p.exists()
        ));

        let missing_addr = format!("unix:path={missing_str}");
        assert!(!linux_secret_service_likely_available_with(
            Some(&missing_addr),
            false,
            |p| p.exists()
        ));

        let existing_addr = format!("unix:path={existing_str},guid=123456");
        assert!(linux_secret_service_likely_available_with(
            Some(&existing_addr),
            false,
            |p| p.exists()
        ));

        assert!(!linux_secret_service_likely_available_with(
            Some(&existing_addr),
            true,
            |p| p.exists()
        ));

        let multi_addr = format!("unix:path={missing_str};unix:path={existing_str}");
        assert!(linux_secret_service_likely_available_with(
            Some(&multi_addr),
            false,
            |p| p.exists()
        ));

        assert!(linux_secret_service_likely_available_with(
            Some("unix:abstract=/tmp/dbus-test"),
            false,
            |p| p.exists()
        ));
    }

    #[test]
    fn test_should_open_browser_headless_linux_and_overrides() {
        assert!(!should_open_browser_with_env(
            false,
            Some("1"),
            Some(":0"),
            None,
            None
        ));
        assert!(!should_open_browser_with_env(
            true,
            Some("true"),
            Some(":0"),
            Some("wayland-0"),
            Some("Ubuntu")
        ));
        assert!(should_open_browser_with_env(false, None, None, None, None));
        assert!(!should_open_browser_with_env(true, None, None, None, None));
        assert!(!should_open_browser_with_env(
            true,
            Some("0"),
            Some("  "),
            Some(""),
            None
        ));
        assert!(should_open_browser_with_env(
            true,
            None,
            Some(":0"),
            None,
            None
        ));
        assert!(should_open_browser_with_env(
            true,
            None,
            None,
            Some("wayland-0"),
            None
        ));
        assert!(should_open_browser_with_env(
            true,
            None,
            None,
            None,
            Some("AlmaLinux-8")
        ));
    }

    fn make_test_jwt_with_encoder(payload: &serde_json::Value, standard_padded: bool) -> String {
        use base64::engine::general_purpose::STANDARD;
        let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256","typ":"JWT"}"#);
        let json_bytes = serde_json::to_vec(payload).unwrap();
        let body = if standard_padded {
            STANDARD.encode(json_bytes)
        } else {
            URL_SAFE_NO_PAD.encode(json_bytes)
        };
        format!("{header}.{body}.sig")
    }

    #[test]
    fn test_parse_jwt_claims_padded_and_standard_base64() {
        let exp = 1_893_456_000_i64;
        let payload = serde_json::json!({
            "exp": exp,
            "oid": "oid-user-123?>",
            "name": "Bob",
            "upn": "bob@contoso.com",
            "tid": "tenant-abc",
            "appid": "app-xyz"
        });
        let jwt_std = make_test_jwt_with_encoder(&payload, true);
        let jwt_url = make_test_jwt_with_encoder(&payload, false);

        for tok in [&jwt_std, &jwt_url, &format!("Bearer {jwt_std}")] {
            let claims = parse_jwt_claims(tok).expect("claims must parse");
            assert_eq!(claims.exp, Some(exp));
            assert_eq!(claims.user_id.as_deref(), Some("oid-user-123?>"));
            assert_eq!(claims.user_display_name.as_deref(), Some("Bob"));
            assert_eq!(claims.user_email.as_deref(), Some("bob@contoso.com"));
            assert_eq!(claims.tenant_id.as_deref(), Some("tenant-abc"));
            assert_eq!(claims.client_id.as_deref(), Some("app-xyz"));
        }
    }

    #[test]
    fn test_read_external_token_file_raw_and_azure_cli_json() {
        let _env_lock = ENV_LOCK.blocking_lock();
        let _env_guard = EnvVarGuard::clear_auth_env();
        let dir = tempdir().unwrap();
        let cfg = AppConfig::default();
        let future_exp = chrono::Utc::now().timestamp() + 3600;
        let jwt = make_test_jwt_with_encoder(
            &serde_json::json!({
                "exp": future_exp,
                "oid": "oid-42",
                "name": "Alice",
                "upn": "alice@example.com",
                "tid": "tid-42",
                "appid": "cid-42"
            }),
            true,
        );

        let raw_path = dir.path().join("access_token");
        std::fs::write(&raw_path, format!("\u{feff}\n  Bearer {jwt}  \n\n")).unwrap();
        let raw_tok = read_external_token_file(&raw_path, &cfg).unwrap();
        assert_eq!(raw_tok.access_token, jwt);
        assert_eq!(raw_tok.expires_at, future_exp);
        assert_eq!(raw_tok.user_id.as_deref(), Some("oid-42"));
        assert_eq!(raw_tok.auth_mode, "token_file");

        let az_path = dir.path().join("az.json");
        let az_json = serde_json::json!({
            "accessToken": jwt,
            "expiresOn": "2030-01-15T12:00:00Z",
            "tenant": "az-tenant",
            "tokenType": "Bearer"
        });
        std::fs::write(&az_path, az_json.to_string()).unwrap();
        let az_tok = read_external_token_file(&az_path, &cfg).unwrap();
        assert_eq!(az_tok.access_token, jwt);
        assert_eq!(az_tok.expires_at, 1_894_708_800);
        assert_eq!(az_tok.tenant_id, "az-tenant");
        assert_eq!(az_tok.auth_mode, "token_file");

        let empty_path = dir.path().join("empty");
        std::fs::write(&empty_path, "\u{feff}   \n").unwrap();
        let err = read_external_token_file(&empty_path, &cfg).unwrap_err();
        assert!(err.to_string().contains("Token file is empty"));
    }

    #[tokio::test]
    async fn test_token_refresh_cmd_auto_refreshes_expiring_token_file() {
        let _env_lock = ENV_LOCK.lock().await;
        let _env_guard = EnvVarGuard::clear_auth_env();
        let dir = tempdir().unwrap();
        let token_path = dir.path().join("access_token");
        let now = chrono::Utc::now().timestamp();

        let soon_jwt =
            make_test_jwt_with_encoder(&serde_json::json!({ "exp": now + 60, "oid": "u-1" }), false);
        let fresh_jwt = make_test_jwt_with_encoder(
            &serde_json::json!({ "exp": now + 3600, "oid": "u-1" }),
            false,
        );
        std::fs::write(&token_path, &soon_jwt).unwrap();

        let mut cfg = AppConfig::default();
        cfg.token_file = token_path.to_string_lossy().to_string();
        cfg.token_refresh_mins = 5;
        #[cfg(windows)]
        {
            cfg.token_refresh_cmd = format!("echo {fresh_jwt}> \"{}\"", token_path.display());
        }
        #[cfg(not(windows))]
        {
            cfg.token_refresh_cmd =
                format!("printf '%s\\n' '{fresh_jwt}' > '{}'", token_path.display());
        }

        let mgr = AuthManager::new(cfg).unwrap();
        assert!(mgr.is_refreshable().await);
        assert_eq!(mgr.valid_access_token().await.unwrap(), fresh_jwt);
        assert_eq!(mgr.current_tokens().await.unwrap().access_token, fresh_jwt);
    }

    #[tokio::test]
    async fn test_token_refresh_cmd_failure_fallback_and_bail() {
        let _env_lock = ENV_LOCK.lock().await;
        let _env_guard = EnvVarGuard::clear_auth_env();
        let dir = tempdir().unwrap();
        let token_path = dir.path().join("access_token");
        let now = chrono::Utc::now().timestamp();

        let soon_jwt = make_test_jwt_with_encoder(&serde_json::json!({ "exp": now + 120 }), false);
        std::fs::write(&token_path, &soon_jwt).unwrap();
        let mut cfg = AppConfig::default();
        cfg.token_file = token_path.to_string_lossy().to_string();
        cfg.token_refresh_mins = 5;
        cfg.token_refresh_cmd = "exit 1".to_string();

        let mgr = AuthManager::new(cfg.clone()).unwrap();
        assert_eq!(mgr.valid_access_token().await.unwrap(), soon_jwt);

        let expired_jwt =
            make_test_jwt_with_encoder(&serde_json::json!({ "exp": now - 60 }), false);
        std::fs::write(&token_path, &expired_jwt).unwrap();
        let err = mgr.valid_access_token().await.unwrap_err();
        assert!(err.to_string().contains("External token refresh failed"));
    }

    #[tokio::test]
    async fn test_expired_external_tokens_return_clear_expiry_error() {
        let _env_lock = ENV_LOCK.lock().await;
        let _env_guard = EnvVarGuard::clear_auth_env();
        let dir = tempdir().unwrap();
        let token_path = dir.path().join("expired_token");
        let now = chrono::Utc::now().timestamp();
        let expired_jwt =
            make_test_jwt_with_encoder(&serde_json::json!({ "exp": now - 120 }), false);
        std::fs::write(&token_path, &expired_jwt).unwrap();

        let mut cfg = AppConfig::default();
        cfg.token_file = token_path.to_string_lossy().to_string();
        let mgr_file = AuthManager::new(cfg.clone()).unwrap();
        assert!(!mgr_file.is_refreshable().await);
        let msg_file = mgr_file.valid_access_token().await.unwrap_err().to_string();
        assert!(msg_file.contains("Token expired (exp:") && msg_file.contains("auth_mode=token_file"));

        let env_tok = env_access_token_from_str(&expired_jwt, &AppConfig::default()).unwrap();
        assert_eq!(env_tok.auth_mode, "env_token");
        let mgr_env = AuthManager::from_tokens(AppConfig::default(), env_tok);
        let msg_env = mgr_env.valid_access_token().await.unwrap_err().to_string();
        assert!(msg_env.contains("Token expired (exp:") && msg_env.contains("auth_mode=env_token"));
    }

    #[tokio::test]
    async fn test_external_token_skips_keyring_and_persistence() {
        let _env_lock = ENV_LOCK.lock().await;
        let _env_guard = EnvVarGuard::clear_auth_env();
        let dir = tempdir().unwrap();
        let cred_path = dir.path().join("should_not_be_created_creds.json");
        let token_path = dir.path().join("ext_token");
        let now = chrono::Utc::now().timestamp();
        let fresh_jwt =
            make_test_jwt_with_encoder(&serde_json::json!({ "exp": now + 3600 }), false);
        std::fs::write(&token_path, &fresh_jwt).unwrap();

        let mut cfg = AppConfig::default();
        cfg.token_file = token_path.to_string_lossy().to_string();
        assert!(should_skip_keyring_on_init(&cfg));

        let ext_tok = read_external_token_file(&token_path, &cfg).unwrap();
        let mgr = AuthManager::from_tokens(cfg, ext_tok).with_token_path(cred_path.clone());
        mgr.update_identity(
            "u-ext".into(),
            "External User".into(),
            Some("ext@example.com".into()),
        )
        .await
        .unwrap();
        assert!(!cred_path.exists());
    }

    #[test]
    fn test_diagnose_access_token_scopes_sharepoint_permissions() {
        let read_only_jwt = make_test_jwt_with_encoder(
            &serde_json::json!({
                "scp": "User.Read Files.Read",
                "oid": "u-1"
            }),
            false,
        );
        let diag_ro = diagnose_access_token_scopes(&read_only_jwt);
        assert!(diag_ro.has_scope_claims);
        assert!(diag_ro.can_read_files);
        assert!(!diag_ro.can_write_files);
        assert!(!diag_ro.can_read_sites);
        assert!(diag_ro.warnings.iter().any(|w| w.contains("Files.ReadWrite")));
        assert!(diag_ro.warnings.iter().any(|w| w.contains("Sites.Read.All")));

        let full_jwt = make_test_jwt_with_encoder(
            &serde_json::json!({
                "scp": "User.Read Files.ReadWrite Files.ReadWrite.All Sites.ReadWrite.All",
                "oid": "u-2"
            }),
            false,
        );
        let diag_full = diagnose_access_token_scopes(&full_jwt);
        assert!(diag_full.has_scope_claims);
        assert!(diag_full.can_read_files);
        assert!(diag_full.can_write_files);
        assert!(diag_full.can_read_all_files);
        assert!(diag_full.can_read_sites);
        assert!(diag_full.can_write_sites);
        assert!(diag_full.warnings.is_empty());

        let app_jwt = make_test_jwt_with_encoder(
            &serde_json::json!({
                "roles": ["Files.Read.All", "Sites.Read.All"],
                "appid": "app-1"
            }),
            false,
        );
        let diag_app = diagnose_access_token_scopes(&app_jwt);
        assert!(diag_app.can_read_files);
        assert!(!diag_app.can_write_files);
        assert!(diag_app.can_read_sites);
        assert!(!diag_app.can_write_sites);

        let opaque = diagnose_access_token_scopes("opaque-non-jwt-token");
        assert!(!opaque.has_scope_claims);
        assert!(opaque.warnings.is_empty());
    }

    #[test]
    fn test_explain_graph_permission_error_sharepoint_403_vs_network() {
        let read_only_jwt = make_test_jwt_with_encoder(
            &serde_json::json!({
                "scp": "User.Read Files.Read"
            }),
            false,
        );
        let explained_upload = explain_graph_permission_error(
            "Graph API error (PUT https://graph.microsoft.com/v1.0/drives/d1/root:/a.txt:/content): 403 Forbidden [accessDenied]",
            Some(&read_only_jwt),
        )
        .expect("403 on upload must be explained");
        assert!(explained_upload.contains("403"));
        assert!(explained_upload.contains("Files.ReadWrite"));

        let explained_sites = explain_graph_permission_error(
            "Graph API error (GET https://graph.microsoft.com/v1.0/sites?search=*): 403 Forbidden",
            Some(&read_only_jwt),
        )
        .expect("403 on sites must be explained");
        assert!(explained_sites.contains("Sites.Read.All"));

        assert_eq!(
            explain_graph_permission_error(
                "Graph HTTP request failed (GET http://127.0.0.1:40312/v1.0/me/drive): connection refused",
                Some(&read_only_jwt),
            ),
            None
        );
    }

    #[tokio::test]
    async fn test_shared_teams_token_file_fallback_with_sharepoint_and_teams_only_scopes() {
        let _env_lock = ENV_LOCK.lock().await;
        let _env_guard = EnvVarGuard::clear_auth_env();
        let dir = tempdir().unwrap();
        let teams_dir = dir.path().join("teams-cli");
        std::fs::create_dir_all(&teams_dir).unwrap();
        let teams_tok_path = dir.path().join("shared_teams_token.txt");
        std::fs::write(
            teams_dir.join("config.toml"),
            format!(
                "token_file = \"{}\"\n",
                teams_tok_path.to_string_lossy().replace('\\', "\\\\")
            ),
        )
        .unwrap();

        let now = chrono::Utc::now().timestamp();
        let teams_only_jwt = make_test_jwt_with_encoder(
            &serde_json::json!({
                "exp": now + 3600,
                "scp": "User.Read Chat.ReadWrite"
            }),
            false,
        );
        std::fs::write(&teams_tok_path, &teams_only_jwt).unwrap();

        let sp_cred_path = dir.path().join("sharepoint_creds.json");
        let mut cfg = AppConfig::default();
        cfg.share_m365_auth = true;

        // 1. Teams-only token in teams-cli token_file must NOT be reused
        let mgr_skip = AuthManager::empty_for_test(cfg.clone())
            .with_token_path(sp_cred_path.clone())
            .with_legacy_teams_dir(teams_dir.clone());
        assert!(mgr_skip.current_tokens().await.is_none());
        assert!(mgr_skip.valid_access_token().await.is_err());

        // 2. Token with SharePoint/OneDrive scopes in teams-cli token_file IS reused with (mtime, len) caching
        let sp_jwt = make_test_jwt_with_encoder(
            &serde_json::json!({
                "exp": now + 3600,
                "scp": "User.Read Files.ReadWrite Sites.Read.All"
            }),
            false,
        );
        std::fs::write(&teams_tok_path, &sp_jwt).unwrap();
        let tok1 = mgr_skip.current_tokens().await.unwrap();
        assert_eq!(tok1.auth_mode, "shared_teams_token_file");
        assert_eq!(tok1.access_token, sp_jwt);
        assert_eq!(mgr_skip.valid_access_token().await.unwrap(), sp_jwt);

        // 3. Updating token file content (even opaque) invalidates (mtime, len) cache
        std::fs::write(&teams_tok_path, "opaque-shared-m365-token-v2").unwrap();
        let tok2 = mgr_skip.current_tokens().await.unwrap();
        assert_eq!(tok2.auth_mode, "shared_teams_token_file");
        assert_eq!(tok2.access_token, "opaque-shared-m365-token-v2");

        // 4. share_m365_auth = false disables fallback completely
        let mut disabled_cfg = cfg;
        disabled_cfg.share_m365_auth = false;
        let mgr_disabled = AuthManager::empty_for_test(disabled_cfg)
            .with_token_path(sp_cred_path)
            .with_legacy_teams_dir(teams_dir);
        assert!(mgr_disabled.current_tokens().await.is_none());
    }

    /// A teams-cli config dir whose `token_file` holds a valid, SharePoint-eligible token,
    /// and no SharePoint session of our own. Returns `(tempdir, teams_dir, sp_cred_path, token)`.
    fn shared_teams_token_file_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, String) {
        let dir = tempdir().unwrap();
        let teams_dir = dir.path().join("teams-cli");
        std::fs::create_dir_all(&teams_dir).unwrap();
        let teams_tok_path = dir.path().join("shared_teams_token.txt");
        std::fs::write(
            teams_dir.join("config.toml"),
            format!(
                "token_file = \"{}\"\n",
                teams_tok_path.to_string_lossy().replace('\\', "\\\\")
            ),
        )
        .unwrap();
        let sp_jwt = make_test_jwt_with_encoder(
            &serde_json::json!({
                "exp": chrono::Utc::now().timestamp() + 3600,
                "scp": "User.Read Files.ReadWrite Sites.Read.All"
            }),
            false,
        );
        std::fs::write(&teams_tok_path, &sp_jwt).unwrap();
        let sp_cred_path = dir.path().join("sharepoint_creds.json");
        (dir, teams_dir, sp_cred_path, sp_jwt)
    }

    fn auth_manager_for_sharing(share: bool, sp_cred_path: PathBuf, teams_dir: PathBuf) -> AuthManager {
        let cfg = AppConfig {
            share_m365_auth: share,
            // Nothing listens here: a refresh that reaches OAuth fails fast instead of
            // leaving the machine.
            auth_base_url: "http://127.0.0.1:1".into(),
            ..AppConfig::default()
        };
        AuthManager::empty_for_test(cfg)
            .with_token_path(sp_cred_path)
            .with_legacy_teams_dir(teams_dir)
    }

    #[tokio::test]
    async fn refresh_now_ignores_shared_teams_token_file_when_sharing_disabled() {
        let _env_lock = ENV_LOCK.lock().await;
        let _env_guard = EnvVarGuard::clear_auth_env();
        let (_dir, teams_dir, sp_cred_path, sp_jwt) = shared_teams_token_file_fixture();

        let disabled = auth_manager_for_sharing(false, sp_cred_path.clone(), teams_dir.clone());
        match disabled.refresh_now().await {
            Ok(tok) => panic!(
                "sharing is off, yet refresh_now returned a token (auth_mode {}, shared: {})",
                tok.auth_mode,
                tok.access_token == sp_jwt
            ),
            Err(err) => assert!(
                !format!("{err:#}").contains("Shared Teams token file"),
                "the shared file must not be consulted at all: {err:#}"
            ),
        }
        assert!(disabled.current_tokens().await.is_none());

        // Control: with the default (sharing on) the same setup hands out the Teams token.
        let enabled = auth_manager_for_sharing(true, sp_cred_path, teams_dir);
        let tok = enabled.refresh_now().await.expect("shared token when sharing is on");
        assert_eq!(tok.access_token, sp_jwt);
        assert_eq!(tok.auth_mode, "shared_teams_token_file");
    }

    #[tokio::test]
    async fn refresh_for_failed_token_ignores_shared_teams_token_file_when_sharing_disabled() {
        let _env_lock = ENV_LOCK.lock().await;
        let _env_guard = EnvVarGuard::clear_auth_env();
        let (_dir, teams_dir, sp_cred_path, sp_jwt) = shared_teams_token_file_fixture();

        let disabled = auth_manager_for_sharing(false, sp_cred_path.clone(), teams_dir.clone());
        match disabled.refresh_for_failed_token("rejected-token").await {
            Ok(tok) => panic!(
                "sharing is off, yet the 401 path returned a token (auth_mode {}, shared: {})",
                tok.auth_mode,
                tok.access_token == sp_jwt
            ),
            Err(err) => assert!(
                !format!("{err:#}").contains("Shared Teams token file"),
                "the shared file must not be consulted at all: {err:#}"
            ),
        }

        // Control: with sharing on, the 401 path picks up the Teams token.
        let enabled = auth_manager_for_sharing(true, sp_cred_path, teams_dir);
        let tok = enabled
            .refresh_for_failed_token("rejected-token")
            .await
            .expect("shared token when sharing is on");
        assert_eq!(tok.access_token, sp_jwt);
    }

    #[tokio::test]
    async fn test_shared_teams_session_reuse_and_silent_refresh_upgrade() {
        use axum::{http::StatusCode, routing::post, Json, Router};
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let _env_lock = ENV_LOCK.lock().await;
        let _env_guard = EnvVarGuard::clear_auth_env();

        let reject_consent = Arc::new(AtomicBool::new(false));
        let reject_clone = reject_consent.clone();
        let refresh_hits = Arc::new(AtomicUsize::new(0));
        let hits_clone = refresh_hits.clone();
        let now = chrono::Utc::now().timestamp();
        let upgraded_jwt = make_test_jwt_with_encoder(
            &serde_json::json!({
                "exp": now + 3600,
                "scp": "User.Read Files.ReadWrite Files.ReadWrite.All Sites.ReadWrite.All",
                "oid": "u-shared"
            }),
            false,
        );
        let upgraded_clone = upgraded_jwt.clone();

        let app = Router::new().route(
            "/tid-shared/oauth2/v2.0/token",
            post(move |body: String| {
                let reject = reject_clone.clone();
                let hits = hits_clone.clone();
                let jwt = upgraded_clone.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    assert!(body.contains("grant_type=refresh_token"));
                    assert!(body.contains("Files.ReadWrite"));
                    if reject.load(Ordering::SeqCst) {
                        (
                            StatusCode::BAD_REQUEST,
                            Json(serde_json::json!({
                                "error": "invalid_grant",
                                "error_description": "AADSTS65001: The user or administrator has not consented to use the application."
                            })),
                        )
                    } else {
                        (
                            StatusCode::OK,
                            Json(serde_json::json!({
                                "access_token": jwt,
                                "refresh_token": "rt-upgraded",
                                "expires_in": 3600
                            })),
                        )
                    }
                }
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let dir = tempdir().unwrap();
        let teams_dir = dir.path().join("teams-cli");
        std::fs::create_dir_all(&teams_dir).unwrap();
        let sp_cred_path = dir.path().join("sharepoint-cli").join("credentials.json");

        let mut cfg = AppConfig::default();
        cfg.auth_base_url = format!("http://{addr}");
        cfg.share_m365_auth = true;

        // 1. Teams session already has Files.ReadWrite -> reused directly without hitting token endpoint
        let teams_with_files = StoredTokens {
            access_token: make_test_jwt_with_encoder(
                &serde_json::json!({
                    "exp": now + 3600,
                    "scp": "User.Read Chat.Read Files.ReadWrite"
                }),
                false,
            ),
            refresh_token: Some("rt-teams".into()),
            expires_at: now + 3600,
            client_id: "cid".into(),
            tenant_id: "tid-shared".into(),
            user_id: Some("u-shared".into()),
            user_display_name: Some("Shared User".into()),
            user_email: Some("shared@example.com".into()),
            auth_mode: "pkce".into(),
        };
        save_tokens_to_path(&teams_with_files, &teams_dir.join("credentials.json")).unwrap();

        let mgr_reuse = AuthManager::empty_for_test(cfg.clone())
            .with_token_path(sp_cred_path.clone())
            .with_legacy_teams_dir(teams_dir.clone());
        let reused_tok = mgr_reuse.valid_access_token().await.unwrap();
        assert_eq!(reused_tok, teams_with_files.access_token);
        assert_eq!(refresh_hits.load(Ordering::SeqCst), 0);
        assert_eq!(
            mgr_reuse.current_tokens().await.unwrap().auth_mode,
            "shared_teams_session"
        );

        // 2. Teams session has only Chat.Read + refresh_token -> silent refresh upgrades & persists to sharepoint-cli
        let teams_chat_only = StoredTokens {
            access_token: make_test_jwt_with_encoder(
                &serde_json::json!({
                    "exp": now + 3600,
                    "scp": "User.Read Chat.Read"
                }),
                false,
            ),
            refresh_token: Some("rt-teams".into()),
            expires_at: now + 3600,
            client_id: "cid".into(),
            tenant_id: "tid-shared".into(),
            user_id: Some("u-shared".into()),
            user_display_name: Some("Shared User".into()),
            user_email: Some("shared@example.com".into()),
            auth_mode: "pkce".into(),
        };
        save_tokens_to_path(&teams_chat_only, &teams_dir.join("credentials.json")).unwrap();

        let mgr_upgrade = AuthManager::empty_for_test(cfg.clone())
            .with_token_path(sp_cred_path.clone())
            .with_legacy_teams_dir(teams_dir.clone());
        let upgraded = mgr_upgrade.valid_access_token().await.unwrap();
        assert_eq!(upgraded, upgraded_jwt);
        assert_eq!(refresh_hits.load(Ordering::SeqCst), 1);
        let persisted = load_tokens_from_path(&sp_cred_path).unwrap().unwrap();
        assert_eq!(persisted.access_token, upgraded_jwt);
        assert_eq!(persisted.refresh_token.as_deref(), Some("rt-upgraded"));
        assert_eq!(persisted.auth_mode, "shared_teams_session");

        // 3. Entra consent error -> clean English error instructing user to run `sharepoint login`
        reject_consent.store(true, Ordering::SeqCst);
        let sp_cred_path_2 = dir.path().join("sharepoint-cli-2").join("credentials.json");
        let mgr_fail = AuthManager::empty_for_test(cfg)
            .with_token_path(sp_cred_path_2)
            .with_legacy_teams_dir(teams_dir);
        let err = mgr_fail.valid_access_token().await.unwrap_err().to_string();
        assert!(err.contains("sharepoint login"), "unexpected error: {err}");
    }

    #[test]
    fn test_stored_tokens_debug_redacts_secrets() {
        let tok = StoredTokens {
            access_token: "super-secret-access-token".into(),
            refresh_token: Some("super-secret-refresh-token".into()),
            expires_at: 1_900_000_000,
            client_id: "cid".into(),
            tenant_id: "tid".into(),
            user_id: Some("u1".into()),
            user_display_name: Some("Alice".into()),
            user_email: Some("alice@example.com".into()),
            auth_mode: "pkce".into(),
        };
        let dbg = format!("{tok:?}");
        assert!(!dbg.contains("super-secret-access-token"));
        assert!(!dbg.contains("super-secret-refresh-token"));
        assert!(dbg.contains("[REDACTED]"));
        assert!(dbg.contains("Alice"));
    }

    #[tokio::test]
    async fn test_accept_pkce_callback_ignores_stray_requests() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_task = tokio::spawn(async move {
            accept_pkce_callback(&listener, "expected-state-123").await
        });

        let client = reqwest::Client::new();
        let stray1 = client
            .get(format!("http://{addr}/?foo=bar"))
            .send()
            .await
            .unwrap();
        assert_eq!(stray1.status(), reqwest::StatusCode::BAD_REQUEST);

        let stray2 = client
            .get(format!("http://{addr}/callback?code=wrong&state=bad-state"))
            .send()
            .await
            .unwrap();
        assert_eq!(stray2.status(), reqwest::StatusCode::BAD_REQUEST);

        let ok_resp = client
            .get(format!(
                "http://{addr}/?code=valid-auth-code&state=expected-state-123"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(ok_resp.status(), reqwest::StatusCode::OK);

        let code = server_task.await.unwrap().unwrap();
        assert_eq!(code, "valid-auth-code");
    }

    #[tokio::test]
    async fn test_from_tokens_is_ephemeral_and_never_persists_to_disk() {
        use axum::{routing::post, Json, Router};

        let _env_lock = ENV_LOCK.lock().await;
        let _env_guard = EnvVarGuard::clear_auth_env();

        let dir = tempdir().unwrap();
        let config_dir_str = dir.path().to_string_lossy().to_string();
        let _override_guard = EnvVarGuard::set(&[
            ("OUTLOOK_CONFIG_DIR", Some(&config_dir_str)),
            ("SHAREPOINT_DISABLE_KEYRING", Some("1")),
        ]);
        let cred_path = dir.path().join("credentials.json");

        let app = Router::new().route(
            "/tenant-ephemeral/oauth2/v2.0/token",
            post(|| async {
                Json(serde_json::json!({
                    "access_token": "ephemeral-refreshed-tok",
                    "refresh_token": "ephemeral-new-rt",
                    "expires_in": 3600
                }))
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let mut cfg = AppConfig::default();
        cfg.auth_base_url = format!("http://{addr}");
        cfg.client_id = "cid-eph".into();
        cfg.tenant_id = "tenant-ephemeral".into();

        let expired = StoredTokens {
            access_token: "ephemeral-old-tok".into(),
            refresh_token: Some("ephemeral-rt-0".into()),
            expires_at: chrono::Utc::now().timestamp() - 300,
            client_id: "cid-eph".into(),
            tenant_id: "tenant-ephemeral".into(),
            user_id: None,
            user_display_name: None,
            user_email: None,
            auth_mode: "pkce".into(),
        };

        let mgr = AuthManager::from_tokens(cfg, expired);
        assert!(mgr.ephemeral);
        assert!(mgr.token_path_override.is_none());

        // Refresh updates in-memory tokens without creating credentials.json on disk.
        let tok = mgr.valid_access_token().await.unwrap();
        assert_eq!(tok, "ephemeral-refreshed-tok");
        assert!(!cred_path.exists());

        let mem = mgr.current_tokens().await.unwrap();
        assert_eq!(mem.access_token, "ephemeral-refreshed-tok");
        assert_eq!(mem.refresh_token.as_deref(), Some("ephemeral-new-rt"));

        // Profile/identity cache update stays in memory and does not persist to disk.
        mgr.update_identity(
            "eph-u1".into(),
            "Ephemeral User".into(),
            Some("eph@example.com".into()),
        )
        .await
        .unwrap();
        assert!(!cred_path.exists());
        let after_identity = mgr.current_tokens().await.unwrap();
        assert_eq!(after_identity.user_id.as_deref(), Some("eph-u1"));
        assert_eq!(
            after_identity.user_display_name.as_deref(),
            Some("Ephemeral User")
        );
        assert_eq!(
            after_identity.user_email.as_deref(),
            Some("eph@example.com")
        );

        // Ephemeral AuthManager also never reads or deletes pre-existing disk credentials.
        save_tokens_to_path(&after_identity, &cred_path).unwrap();
        assert!(mgr.load_from_configured_source().unwrap().is_none());
        mgr.logout().await.unwrap();
        assert!(mgr.current_tokens().await.is_none());
        assert!(cred_path.exists());
    }
}
