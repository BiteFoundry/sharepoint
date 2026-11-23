//! Microsoft Graph v1.0 client for OneDrive & SharePoint (Sites, Drives, DriveItems, Uploads, Downloads, Search).

pub mod client;
pub mod drives;
pub mod lists;
pub mod media;
pub mod socketio;

use std::{
    sync::{Arc, OnceLock},
    time::Duration,
};

use anyhow::Result;

use crate::{auth::AuthManager, config::AppConfig};

/// Canonical prefix emitted by `GraphClient` HTTP errors; kept as a shared constant for safe parsing.
pub const GRAPH_API_ERROR_PREFIX: &str = "Graph API error (";

/// Extracts the 3-digit HTTP status code from a `GraphClient` API error if present.
pub(crate) fn graph_error_status(err: &anyhow::Error) -> Option<u16> {
    let msg = format!("{err:#}");
    let idx = msg.find(GRAPH_API_ERROR_PREFIX)?;
    let after_prefix = &msg[idx + GRAPH_API_ERROR_PREFIX.len()..];
    let close = after_prefix.find("): ")?;
    let detail = after_prefix[close + 3..].trim_start();
    let code_str: String = detail.chars().take_while(|c| c.is_ascii_digit()).collect();
    if code_str.len() == 3 {
        code_str.parse::<u16>().ok()
    } else {
        None
    }
}

pub(crate) fn is_graph_not_found_error(err: &anyhow::Error) -> bool {
    graph_error_status(err) == Some(404)
}

/// How long a download hop may take to answer with response headers.
pub(crate) const DOWNLOAD_HEADER_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a download body may stay silent between two reads. This is an inactivity
/// limit, not a cap on the whole transfer: a large file may take as long as it needs.
pub(crate) const DOWNLOAD_READ_TIMEOUT: Duration = Duration::from_secs(60);

const USER_AGENT: &str = concat!("sharepoint-cli/", env!("CARGO_PKG_VERSION"));

/// The two clients that carry file bodies: `http_download` (follows redirects, used for
/// pre-authenticated and redirect URLs) and `http_no_redirect` (the Graph `/content` hop).
/// Neither has a whole-request timeout, which would cut off any transfer longer than it.
fn build_download_clients(read_timeout: Duration) -> Result<(reqwest::Client, reqwest::Client)> {
    let http_download = crate::net::apply_tls_roots(reqwest::Client::builder())?
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(read_timeout)
        .tcp_nodelay(true)
        .build()?;
    let http_no_redirect = crate::net::apply_tls_roots(reqwest::Client::builder())?
        .user_agent(USER_AGENT)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(read_timeout)
        .tcp_nodelay(true)
        .build()?;
    Ok((http_download, http_no_redirect))
}

#[derive(Clone)]
pub struct GraphClient {
    auth: AuthManager,
    config: AppConfig,
    http: reqwest::Client,
    http_no_redirect: reqwest::Client,
    http_download: reqwest::Client,
    download_header_timeout: Duration,
    my_user_id: Option<String>,
    cached_my_user_id: Arc<OnceLock<String>>,
    resolve_user_mutex: Arc<tokio::sync::Mutex<()>>,
}

impl GraphClient {
    pub fn new(config: AppConfig, auth: AuthManager) -> Result<Self> {
        let http = crate::net::apply_tls_roots(reqwest::Client::builder())?
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .tcp_nodelay(true)
            .pool_max_idle_per_host(16)
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(30))
            .build()?;
        let (http_download, http_no_redirect) = build_download_clients(DOWNLOAD_READ_TIMEOUT)?;
        Ok(Self {
            auth,
            config,
            http,
            http_no_redirect,
            http_download,
            download_header_timeout: DOWNLOAD_HEADER_TIMEOUT,
            my_user_id: None,
            cached_my_user_id: Arc::new(OnceLock::new()),
            resolve_user_mutex: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    pub fn with_my_user_id(mut self, user_id: impl Into<String>) -> Self {
        let uid = user_id.into();
        let lock = OnceLock::new();
        let _ = lock.set(uid.clone());
        self.cached_my_user_id = Arc::new(lock);
        self.my_user_id = Some(uid);
        self
    }

    pub fn my_user_id(&self) -> Option<&str> {
        self.my_user_id
            .as_deref()
            .or_else(|| self.cached_my_user_id.get().map(String::as_str))
    }

    pub fn auth(&self) -> &AuthManager {
        &self.auth
    }

    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    pub(crate) fn http_no_redirect(&self) -> &reqwest::Client {
        &self.http_no_redirect
    }

    /// Client for file bodies fetched from pre-authenticated or redirect URLs.
    pub(crate) fn http_download(&self) -> &reqwest::Client {
        &self.http_download
    }

    pub(crate) fn download_header_timeout(&self) -> Duration {
        self.download_header_timeout
    }

    /// Overrides the download deadlines (`header`: time until response headers; `read`:
    /// longest silence in a body). For tests, which cannot wait for the 60 s defaults.
    #[doc(hidden)]
    pub fn with_download_timeouts(mut self, header: Duration, read: Duration) -> Result<Self> {
        let (http_download, http_no_redirect) = build_download_clients(read)?;
        self.http_download = http_download;
        self.http_no_redirect = http_no_redirect;
        self.download_header_timeout = header;
        Ok(self)
    }
}
