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

#[derive(Clone)]
pub struct GraphClient {
    auth: AuthManager,
    config: AppConfig,
    http: reqwest::Client,
    http_no_redirect: reqwest::Client,
    my_user_id: Option<String>,
    cached_my_user_id: Arc<OnceLock<String>>,
    resolve_user_mutex: Arc<tokio::sync::Mutex<()>>,
}

impl GraphClient {
    pub fn new(config: AppConfig, auth: AuthManager) -> Result<Self> {
        let ua = concat!("sharepoint-cli/", env!("CARGO_PKG_VERSION"));
        let http = reqwest::Client::builder()
            .user_agent(ua)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .tcp_nodelay(true)
            .pool_max_idle_per_host(16)
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(30))
            .build()?;
        let http_no_redirect = reqwest::Client::builder()
            .user_agent(ua)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(300))
            .tcp_nodelay(true)
            .build()?;
        Ok(Self {
            auth,
            config,
            http,
            http_no_redirect,
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
}
