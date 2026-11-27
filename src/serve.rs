//! Local HTTP JSON API (`sharepoint serve` / `bitefoundry-onedrive serve`, default port `8790`) + OpenAPI 3.1 powered by `axum`.

use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::Arc,
};

use anyhow::{bail, Context, Result};
use axum::{
    extract::{Path, Query, Request, State},
    http::{header, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD as B64_STANDARD, Engine as _};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::{
    auth::diagnose_access_token_scopes,
    cli,
    config::{self, AppConfig, Persona},
    graph::{
        drives::{
            explicit_if_match, is_precondition_failed_error, ConflictBehavior, UploadOptions,
        },
        media::decode_text_file,
        GraphClient,
    },
    resolver,
    storage::Storage,
};

#[derive(Debug, Clone)]
pub struct ServeOptions {
    pub bind: String,
    pub port: u16,
    /// Bearer key required on `/v1`. When absent on a loopback bind, `run` generates a
    /// random per-process key unless `allow_keyless` is set.
    pub api_key: Option<String>,
    /// Explicit opt-out of the generated key: serve `/v1` without authentication
    /// (loopback binds only).
    pub allow_keyless: bool,
    /// Allow `local_path` uploads and `dir` downloads on a keyless server.
    pub allow_local_paths: bool,
    /// Extra `Host` header values accepted besides the loopback names.
    pub allowed_hosts: Vec<String>,
    /// Extra browser origins allowed by CORS besides loopback origins.
    pub cors_origins: Vec<String>,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1".to_string(),
            port: config::DEFAULT_SERVE_PORT,
            api_key: None,
            allow_keyless: false,
            allow_local_paths: false,
            allowed_hosts: Vec::new(),
            cors_origins: Vec::new(),
        }
    }
}

/// Access policy of one router, decided once at startup.
#[derive(Debug, Clone, Default)]
pub struct RouterPolicy {
    /// Bearer key required on `/v1`; `None` (or blank) serves `/v1` without authentication.
    pub api_key: Option<String>,
    /// Whether requests may name server-side filesystem paths (`local_path` uploads,
    /// `dir` / `out_dir` downloads).
    pub allow_local_paths: bool,
    /// `Host` header values accepted besides `localhost`, `127.0.0.1` and `::1`.
    pub allowed_hosts: Vec<String>,
    /// Browser origins allowed by CORS besides loopback origins (exact match).
    pub cors_origins: Vec<String>,
}

impl RouterPolicy {
    /// Policy used by `build_router`: a configured key enables local paths; only the
    /// loopback host names and loopback browser origins are accepted.
    pub fn from_api_key(api_key: Option<String>) -> Self {
        let api_key = normalize_api_key(api_key);
        Self {
            allow_local_paths: api_key.is_some(),
            api_key,
            allowed_hosts: Vec::new(),
            cors_origins: Vec::new(),
        }
    }
}

pub struct AppState {
    pub config: AppConfig,
    pub graph: GraphClient,
    pub storage: Storage,
    pub api_key: Option<String>,
    pub allow_local_paths: bool,
    pub allowed_hosts: Vec<String>,
}

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        let status = if cli::is_forbidden_error(&err) {
            StatusCode::FORBIDDEN
        } else if is_precondition_failed_error(&err) {
            StatusCode::PRECONDITION_FAILED
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        Self::new(status, format!("{err:#}"))
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthResponse {
    pub ok: bool,
    pub service: String,
    pub user_id: Option<String>,
    pub version: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct SitesQuery {
    #[serde(default)]
    pub search: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub refresh: bool,
    #[serde(default)]
    pub offline: bool,
}

#[derive(Debug, Default, Deserialize)]
pub struct DrivesQuery {
    #[serde(default)]
    pub site: Option<String>,
    #[serde(default)]
    pub refresh: bool,
    #[serde(default)]
    pub offline: bool,
}

#[derive(Debug, Default, Deserialize)]
pub struct ListItemsQuery {
    #[serde(default)]
    pub drive: Option<String>,
    #[serde(default)]
    pub site: Option<String>,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub refresh: bool,
    #[serde(default)]
    pub offline: bool,
}

#[derive(Debug, Default, Deserialize)]
pub struct ItemTargetQuery {
    #[serde(default)]
    pub drive: Option<String>,
    #[serde(default)]
    pub site: Option<String>,
    #[serde(default)]
    pub offline: bool,
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
pub struct LimitOfflineQuery {
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offline: bool,
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub q: String,
    #[serde(default)]
    pub drive: Option<String>,
    #[serde(default)]
    pub site: Option<String>,
    #[serde(default)]
    pub remote: bool,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct CreateFolderBody {
    #[serde(default)]
    pub drive: Option<String>,
    #[serde(default)]
    pub site: Option<String>,
    #[serde(default)]
    pub parent_id: Option<String>,
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct UploadBody {
    #[serde(default)]
    pub drive: Option<String>,
    #[serde(default)]
    pub site: Option<String>,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub local_path: Option<String>,
    #[serde(default)]
    pub file_name: Option<String>,
    #[serde(default)]
    pub content_base64: Option<String>,
    #[serde(default)]
    pub content_text: Option<String>,
    #[serde(default)]
    pub item_id: Option<String>,
    #[serde(default)]
    pub if_match: Option<String>,
    #[serde(default, alias = "conflict")]
    pub conflict_behavior: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct PatchItemBody {
    #[serde(default)]
    pub drive: Option<String>,
    #[serde(default)]
    pub site: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub parent_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ShareItemBody {
    #[serde(default)]
    pub drive: Option<String>,
    #[serde(default)]
    pub site: Option<String>,
    #[serde(default = "default_share_type", rename = "type")]
    pub link_type: String,
    #[serde(default = "default_share_scope")]
    pub scope: String,
    #[serde(default)]
    pub expires: Option<String>,
}

fn default_share_type() -> String {
    "view".to_string()
}

fn default_share_scope() -> String {
    "organization".to_string()
}

#[derive(Debug, Default, Deserialize)]
pub struct DownloadBody {
    #[serde(default)]
    pub drive: Option<String>,
    #[serde(default)]
    pub site: Option<String>,
    #[serde(default)]
    pub item_id: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default, alias = "out_dir")]
    pub dir: Option<String>,
}

/// A blank or whitespace-only key counts as "no key" everywhere.
fn normalize_api_key(api_key: Option<String>) -> Option<String> {
    api_key
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
}

const LOOPBACK_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "::1"];

/// Normalise a host name for the allow-list: trimmed, lower-cased, without IPv6 brackets.
fn normalize_host_name(raw: &str) -> String {
    let raw = raw.trim();
    let raw = raw
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(raw);
    raw.to_ascii_lowercase()
}

/// Host part of a `Host` header (or URI authority): port and IPv6 brackets removed,
/// lower-cased. `None` when the value is empty or malformed.
pub(crate) fn host_without_port(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let host = if let Some(rest) = raw.strip_prefix('[') {
        let (inner, tail) = rest.split_once(']')?;
        if !(tail.is_empty()
            || tail
                .strip_prefix(':')
                .is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())))
        {
            return None;
        }
        inner
    } else {
        match raw.rsplit_once(':') {
            Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => {
                host
            }
            Some(_) => return None,
            None => raw,
        }
    };
    if host.is_empty() || host.contains(['/', '@', ' ', '[', ']']) {
        return None;
    }
    Some(host.to_ascii_lowercase())
}

/// Whether a `Host` header value names this server: a loopback name or an explicitly
/// allowed host. Blocks DNS-rebinding requests, which carry the attacker's host name.
pub(crate) fn is_allowed_host(raw: &str, allowed_hosts: &[String]) -> bool {
    let Some(host) = host_without_port(raw) else {
        return false;
    };
    LOOPBACK_HOSTS.contains(&host.as_str()) || allowed_hosts.contains(&host)
}

/// Exact loopback-origin check for CORS. Accepts only `http://localhost`,
/// `http://127.0.0.1` (any IPv4 loopback) and `http://[::1]`, with any port and nothing
/// else in the URL. Prefix look-alikes such as `http://localhost.evil.com` are rejected.
pub(crate) fn is_allowed_origin(origin: &str) -> bool {
    let Ok(url) = url::Url::parse(origin) else {
        return false;
    };
    if url.scheme() != "http" || !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    if !(url.path().is_empty() || url.path() == "/")
        || origin.trim_end().ends_with('/')
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return false;
    }
    match url.host() {
        Some(url::Host::Ipv4(addr)) => addr.is_loopback(),
        Some(url::Host::Ipv6(addr)) => addr.is_loopback(),
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

/// Canonical form of an explicitly allowed origin (`--cors-origin`): scheme, host and port
/// only. Rejects anything that is not a plain `http(s)://host[:port]` origin.
pub fn normalize_cors_origin(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    let url = url::Url::parse(trimmed)
        .with_context(|| format!("Invalid --cors-origin '{trimmed}': not a URL"))?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("Invalid --cors-origin '{trimmed}': scheme must be http or https");
    }
    if url.host().is_none() || !url.username().is_empty() || url.password().is_some() {
        bail!("Invalid --cors-origin '{trimmed}': expected scheme://host[:port]");
    }
    if !(url.path().is_empty() || url.path() == "/")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("Invalid --cors-origin '{trimmed}': an origin has no path, query or fragment");
    }
    Ok(url.origin().ascii_serialization())
}

/// Whether `origin` exactly equals one of the configured (already normalised) origins.
fn matches_extra_origin(origin: &str, cors_origins: &[String]) -> bool {
    if cors_origins.is_empty() {
        return false;
    }
    let Ok(url) = url::Url::parse(origin) else {
        return false;
    };
    if !url.username().is_empty()
        || url.password().is_some()
        || !(url.path().is_empty() || url.path() == "/")
        || origin.trim_end().ends_with('/')
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return false;
    }
    let canonical = url.origin().ascii_serialization();
    cors_origins.contains(&canonical)
}

/// Refuse to expose the API beyond loopback without a key.
pub(crate) fn validate_bind(addr: SocketAddr, has_key: bool) -> Result<()> {
    if !addr.ip().is_loopback() && !has_key {
        bail!(
            "Refusing to serve on {addr} without --api-key: the HTTP API would be open to the network. Pass --api-key <secret> or bind to 127.0.0.1."
        );
    }
    Ok(())
}

/// Random 32-byte Bearer key (hex) for a loopback server started without `--api-key`.
pub(crate) fn generate_api_key() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// `Host` values accepted for a server bound to `bind_ip`, besides the loopback names:
/// the literal bind address when it is a specific one, plus every `--allowed-host`.
pub(crate) fn allowed_hosts_for(bind_ip: IpAddr, extra: &[String]) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    if !bind_ip.is_unspecified() {
        hosts.push(bind_ip.to_string().to_ascii_lowercase());
    }
    for raw in extra {
        // Accept `name`, `name:port` and `[v6]:port`; only the host part is compared.
        let host = host_without_port(raw).unwrap_or_else(|| normalize_host_name(raw));
        if !host.is_empty() {
            hosts.push(host);
        }
    }
    hosts.retain(|h| !LOOPBACK_HOSTS.contains(&h.as_str()));
    hosts.sort();
    hosts.dedup();
    hosts
}

/// Router with the default policy for `api_key`: a configured key enables `local_path` /
/// `dir`; without one `/v1` is unauthenticated and those parameters are refused. Use
/// `build_router_with` to choose the policy explicitly.
pub fn build_router(
    config: AppConfig,
    graph: GraphClient,
    storage: Storage,
    api_key: Option<String>,
) -> Router {
    build_router_with(config, graph, storage, RouterPolicy::from_api_key(api_key))
}

pub fn build_router_with(
    config: AppConfig,
    graph: GraphClient,
    storage: Storage,
    policy: RouterPolicy,
) -> Router {
    let cors_origins: Vec<String> = policy
        .cors_origins
        .iter()
        .filter_map(|o| normalize_cors_origin(o).ok())
        .collect();
    let allowed_hosts: Vec<String> = policy
        .allowed_hosts
        .iter()
        .map(|h| host_without_port(h).unwrap_or_else(|| normalize_host_name(h)))
        .filter(|h| !h.is_empty())
        .collect();
    let state = Arc::new(AppState {
        config,
        graph,
        storage,
        api_key: normalize_api_key(policy.api_key),
        allow_local_paths: policy.allow_local_paths,
        allowed_hosts,
    });

    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(move |origin: &HeaderValue, _| {
            origin
                .to_str()
                .is_ok_and(|o| is_allowed_origin(o) || matches_extra_origin(o, &cors_origins))
        }))
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]);

    let v1 = Router::new()
        .route("/whoami", get(handle_whoami))
        .route("/sites", get(handle_sites))
        .route("/drives", get(handle_drives))
        .route("/items", get(handle_list_items))
        .route("/items/folder", post(handle_create_folder))
        .route("/items/upload", post(handle_upload))
        .route(
            "/items/{id}",
            get(handle_get_item)
                .patch(handle_patch_item)
                .delete(handle_delete_item),
        )
        .route("/items/{id}/children", get(handle_item_children))
        .route("/items/{id}/content", get(handle_item_content))
        .route("/items/{id}/versions", get(handle_item_versions))
        .route("/items/{id}/share", post(handle_share_item))
        .route("/items/{id}/download", post(handle_download_by_id))
        .route("/recent", get(handle_recent))
        .route("/shared", get(handle_shared))
        .route("/search", get(handle_search))
        .route("/download", post(handle_download))
        .layer(middleware::from_fn_with_state(state.clone(), auth_middleware));

    // Layer order (outermost first): Host guard -> CORS -> routes (`/v1` adds the key
    // check). The Host guard therefore covers `/health`, `/openapi.yaml` and preflights,
    // and CORS still answers preflights for allowed origins before the key check.
    Router::new()
        .route("/health", get(handle_health))
        .route("/openapi.yaml", get(handle_openapi))
        .nest("/v1", v1)
        .layer(cors)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            host_guard_middleware,
        ))
        .with_state(state)
}

/// What `run` will serve, derived from the command-line options before any socket opens.
#[derive(Debug, Clone)]
pub(crate) struct ServePlan {
    pub bind_addr: SocketAddr,
    pub policy: RouterPolicy,
    /// The key in `policy` was generated for this run (it must be shown to the operator).
    pub generated_key: bool,
}

pub(crate) fn plan_serve(opts: &ServeOptions) -> Result<ServePlan> {
    let bind_addr: SocketAddr = format!("{}:{}", opts.bind, opts.port)
        .parse()
        .with_context(|| format!("Invalid bind address '{}:{}'", opts.bind, opts.port))?;
    let mut api_key = normalize_api_key(opts.api_key.clone());
    // Checked against the key the operator passed: a generated key never unlocks a
    // non-loopback bind, and --allow-keyless cannot override the refusal.
    validate_bind(bind_addr, api_key.is_some())?;
    let cors_origins = opts
        .cors_origins
        .iter()
        .map(|o| normalize_cors_origin(o))
        .collect::<Result<Vec<_>>>()?;

    let mut generated_key = false;
    if api_key.is_none() && !opts.allow_keyless {
        api_key = Some(generate_api_key());
        generated_key = true;
    }
    Ok(ServePlan {
        bind_addr,
        policy: RouterPolicy {
            allow_local_paths: api_key.is_some() || opts.allow_local_paths,
            api_key,
            allowed_hosts: allowed_hosts_for(bind_addr.ip(), &opts.allowed_hosts),
            cors_origins,
        },
        generated_key,
    })
}

pub async fn run(
    config: AppConfig,
    graph: GraphClient,
    storage: Storage,
    opts: ServeOptions,
) -> Result<()> {
    let ServePlan {
        bind_addr,
        policy,
        generated_key,
    } = plan_serve(&opts)?;
    let api_key = policy.api_key.clone();
    let allow_local_paths = policy.allow_local_paths;
    let router = build_router_with(config, graph, storage, policy);
    let listener = TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("Failed to bind HTTP server on {bind_addr}"))?;
    // Everything below goes to stderr; stdout stays free for callers that capture it.
    eprintln!(
        "sharepoint HTTP API listening on http://{bind_addr} (GET /health, GET /openapi.yaml)"
    );
    let auth_line = match (api_key.as_deref(), generated_key) {
        (Some(_), true) => "Bearer key required on /v1 (generated for this run)",
        (Some(_), false) => "Bearer key required on /v1 (--api-key)",
        (None, _) => "DISABLED (--allow-keyless): any local process can call /v1",
    };
    eprintln!(
        "  auth: {auth_line}; local paths (local_path / dir): {}",
        if allow_local_paths { "enabled" } else { "disabled" }
    );
    if let (Some(key), true) = (api_key.as_deref(), generated_key) {
        eprintln!("  No --api-key given: generated a random key for this run (shown once).");
        eprintln!("  Authorization: Bearer {key}");
        eprintln!(
            "  Pass --api-key <secret> for a stable key, or --allow-keyless to serve without one."
        );
    }
    axum::serve(listener, router).await?;
    Ok(())
}

/// Rejects requests whose `Host` header does not name this server (DNS rebinding).
async fn host_guard_middleware(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        // HTTP/2 carries the authority in the request target instead of a Host header.
        .or_else(|| req.uri().authority().map(|a| a.as_str().to_string()));
    match host.as_deref() {
        Some(value) if is_allowed_host(value, &state.allowed_hosts) => next.run(req).await,
        other => ApiError::forbidden(format!(
            "Host header not allowed: {}. Use --allowed-host to permit it.",
            other.unwrap_or("(missing)")
        ))
        .into_response(),
    }
}

async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    if let Some(expected) = state.api_key.as_deref() {
        let provided = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::trim);
        if provided != Some(expected) {
            return ApiError::new(StatusCode::UNAUTHORIZED, "Invalid or missing Bearer API key")
                .into_response();
        }
    }
    next.run(req).await
}

async fn handle_health(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
    Json(HealthResponse {
        ok: true,
        service: "sharepoint".to_string(),
        user_id: state.graph.my_user_id().map(str::to_string),
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

async fn handle_openapi() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/yaml; charset=utf-8")],
        include_str!("../docs/openapi.yaml"),
    )
}

async fn handle_whoami(State(state): State<Arc<AppState>>) -> Json<Value> {
    let tokens = state.graph.auth().current_tokens().await;
    let source = cli::auth_source_label(tokens.as_ref(), &state.config);
    match tokens {
        Some(tok) => {
            let diag = diagnose_access_token_scopes(&tok.access_token);
            Json(json!({
                "authenticated": true,
                "user_id": tok.user_id,
                "display_name": tok.user_display_name,
                "email": tok.user_email,
                "auth_mode": tok.auth_mode,
                "auth_source": source,
                "scope_diagnostics": diag,
            }))
        }
        None => Json(json!({
            "authenticated": false,
            "auth_source": source,
        })),
    }
}

async fn handle_sites(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SitesQuery>,
) -> Result<Json<Value>, ApiError> {
    let sites = cli::fetch_sites(
        &state.graph,
        &state.storage,
        q.search.as_deref(),
        q.limit.unwrap_or(50),
        q.refresh,
        q.offline,
        false,
    )
    .await?;
    Ok(Json(json!({ "sites": sites })))
}

async fn handle_drives(
    State(state): State<Arc<AppState>>,
    Query(q): Query<DrivesQuery>,
) -> Result<Json<Value>, ApiError> {
    let drives = cli::fetch_drives(
        &state.graph,
        &state.storage,
        q.site.as_deref(),
        q.refresh,
        q.offline,
        false,
    )
    .await?;
    Ok(Json(json!({ "drives": drives })))
}

async fn handle_list_items(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ListItemsQuery>,
) -> Result<Json<Value>, ApiError> {
    let drive = resolver::resolve_target_drive(
        &state.graph,
        &state.storage,
        q.drive.as_deref(),
        q.site.as_deref(),
        Persona::detect(),
        q.offline,
    )
    .await?;
    let target = q.parent_id.as_deref().or(q.path.as_deref());
    let items = cli::fetch_folder_items(
        &state.graph,
        &state.storage,
        &drive.id,
        target,
        q.limit.unwrap_or(100),
        q.refresh,
        q.offline,
        false,
    )
    .await?;
    Ok(Json(json!({ "drive": drive, "items": items })))
}

async fn handle_get_item(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<ItemTargetQuery>,
) -> Result<Json<Value>, ApiError> {
    let drive = resolver::resolve_target_drive(
        &state.graph,
        &state.storage,
        q.drive.as_deref(),
        q.site.as_deref(),
        Persona::detect(),
        q.offline,
    )
    .await?;
    let item = resolver::resolve_item(&state.graph, &state.storage, &drive.id, &id, q.offline).await?;
    Ok(Json(json!({ "item": item })))
}

async fn handle_item_children(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<ListItemsQuery>,
) -> Result<Json<Value>, ApiError> {
    let drive = resolver::resolve_target_drive(
        &state.graph,
        &state.storage,
        q.drive.as_deref(),
        q.site.as_deref(),
        Persona::detect(),
        q.offline,
    )
    .await?;
    let items = cli::fetch_folder_items(
        &state.graph,
        &state.storage,
        &drive.id,
        Some(&id),
        q.limit.unwrap_or(100),
        q.refresh,
        q.offline,
        false,
    )
    .await?;
    Ok(Json(json!({ "drive_id": drive.id, "parent_id": id, "items": items })))
}

async fn handle_item_content(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<ItemTargetQuery>,
) -> Result<Json<Value>, ApiError> {
    let drive = resolver::resolve_target_drive(
        &state.graph,
        &state.storage,
        q.drive.as_deref(),
        q.site.as_deref(),
        Persona::detect(),
        false,
    )
    .await?;
    let item = resolver::resolve_item(&state.graph, &state.storage, &drive.id, &id, false).await?;
    let bytes = state
        .graph
        .download_item_bytes(
            item.effective_drive_id(),
            item.effective_item_id(),
            item.download_url.as_deref(),
            q.max_bytes.unwrap_or(cli::DEFAULT_TEXT_PREVIEW_MAX_BYTES),
        )
        .await?;
    let content = decode_text_file(&bytes)
        .map_err(|err| ApiError::new(StatusCode::UNSUPPORTED_MEDIA_TYPE, format!("{err:#}")))?;
    Ok(Json(json!({
        "ok": true,
        "id": item.id,
        "drive_id": item.drive_id,
        "name": item.name,
        "mime_type": item.mime_type,
        "e_tag": item.e_tag,
        "c_tag": item.c_tag,
        "size_bytes": bytes.len(),
        "content": content,
    })))
}

async fn handle_item_versions(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<ItemTargetQuery>,
) -> Result<Json<Value>, ApiError> {
    let drive = resolver::resolve_target_drive(
        &state.graph,
        &state.storage,
        q.drive.as_deref(),
        q.site.as_deref(),
        Persona::detect(),
        false,
    )
    .await?;
    let versions = state.graph.list_item_versions(&drive.id, &id).await?;
    Ok(Json(json!({ "drive_id": drive.id, "item_id": id, "versions": versions })))
}

async fn handle_create_folder(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateFolderBody>,
) -> Result<Json<Value>, ApiError> {
    let drive = resolver::resolve_target_drive(
        &state.graph,
        &state.storage,
        body.drive.as_deref(),
        body.site.as_deref(),
        Persona::detect(),
        false,
    )
    .await?;
    let created = state
        .graph
        .create_folder(&drive.id, body.parent_id.as_deref(), &body.name)
        .await?;
    let storage = state.storage.clone();
    let created_for_db = created.clone();
    let _ = tokio::task::spawn_blocking(move || {
        let _ = storage.upsert_drive_items(std::slice::from_ref(&created_for_db));
    })
    .await;
    Ok(Json(json!({ "item": created })))
}

const LOCAL_PATH_UPLOAD_DISABLED: &str = "local_path uploads are disabled on a server started without an API key. Send content_base64 or content_text, or restart with --api-key or --allow-local-paths.";
const DOWNLOAD_DIR_DISABLED: &str = "dir / out_dir is disabled on a server started without an API key. Omit it to use the configured download directory, or restart with --api-key or --allow-local-paths.";

async fn handle_upload(
    State(state): State<Arc<AppState>>,
    Json(body): Json<UploadBody>,
) -> Result<Json<Value>, ApiError> {
    // Checked before anything is resolved: a refused request makes no Graph call.
    if !state.allow_local_paths
        && body.local_path.as_deref().is_some_and(|p| !p.trim().is_empty())
    {
        return Err(ApiError::forbidden(LOCAL_PATH_UPLOAD_DISABLED));
    }
    let if_match = explicit_if_match(body.if_match.as_deref())
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let drive = resolver::resolve_target_drive(
        &state.graph,
        &state.storage,
        body.drive.as_deref(),
        body.site.as_deref(),
        Persona::detect(),
        false,
    )
    .await?;
    let explicit_conflict = match body.conflict_behavior.as_deref() {
        Some(raw) => Some(ConflictBehavior::parse_str(raw).map_err(|e| ApiError::bad_request(e.to_string()))?),
        None => None,
    };

    let uploaded = if let Some(lp) = body.local_path.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let path = PathBuf::from(lp);
        let storage_for_opts = state.storage.clone();
        let drive_id_for_opts = drive.id.clone();
        let parent_id_for_opts = body.parent_id.clone();
        let file_name_for_opts = body.file_name.clone();
        let path_for_opts = path.clone();
        let if_match_for_opts = if_match.clone();
        let mut opts = tokio::task::spawn_blocking(move || {
            cli::resolve_upload_options_for_local_file(
                &storage_for_opts,
                &drive_id_for_opts,
                parent_id_for_opts.as_deref(),
                None,
                file_name_for_opts.as_deref(),
                &path_for_opts,
                if_match_for_opts.as_deref(),
                explicit_conflict,
            )
        })
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        if let Some(id) = body.item_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            if if_match.is_none() && opts.target_item_id.as_deref() != Some(id) {
                opts.if_match = None;
                opts.expected_c_tag = None;
            }
            opts.target_item_id = Some(id.to_string());
        }
        let item = state
            .graph
            .upload_local_file_with_options(
                &drive.id,
                body.parent_id.as_deref(),
                body.file_name.as_deref(),
                &path,
                &opts,
            )
            .await?;
        let storage_for_base = state.storage.clone();
        let item_for_base = item.clone();
        let _ = tokio::task::spawn_blocking(move || {
            cli::refresh_download_baseline_after_upload(&storage_for_base, &path, &item_for_base, &opts);
        })
        .await;
        item
    } else if let Some(b64) = body.content_base64.as_deref() {
        let name = body
            .file_name
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ApiError::bad_request("file_name is required with content_base64"))?;
        let bytes = B64_STANDARD
            .decode(b64.trim())
            .map_err(|e| ApiError::bad_request(format!("Invalid base64: {e}")))?;
        let opts = UploadOptions {
            conflict_behavior: explicit_conflict,
            if_match,
            expected_c_tag: None,
            target_item_id: body.item_id.clone(),
        };
        state
            .graph
            .upload_bytes_with_options(&drive.id, body.parent_id.as_deref(), name, bytes, None, &opts)
            .await?
    } else if let Some(text) = body.content_text {
        let name = body
            .file_name
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ApiError::bad_request("file_name is required with content_text"))?;
        let opts = UploadOptions {
            conflict_behavior: explicit_conflict,
            if_match,
            expected_c_tag: None,
            target_item_id: body.item_id.clone(),
        };
        state
            .graph
            .upload_bytes_with_options(
                &drive.id,
                body.parent_id.as_deref(),
                name,
                text.into_bytes(),
                Some("text/plain; charset=utf-8"),
                &opts,
            )
            .await?
    } else {
        return Err(ApiError::bad_request(
            "Provide local_path, content_base64, or content_text",
        ));
    };

    let storage = state.storage.clone();
    let uploaded_for_db = uploaded.clone();
    let _ = tokio::task::spawn_blocking(move || {
        let _ = storage.upsert_drive_items(std::slice::from_ref(&uploaded_for_db));
    })
    .await;
    Ok(Json(json!({ "item": uploaded })))
}

async fn handle_patch_item(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<PatchItemBody>,
) -> Result<Json<Value>, ApiError> {
    let drive = resolver::resolve_target_drive(
        &state.graph,
        &state.storage,
        body.drive.as_deref(),
        body.site.as_deref(),
        Persona::detect(),
        false,
    )
    .await?;
    let updated = cli::rename_or_move_item_cached(
        &state.graph,
        &state.storage,
        &drive.id,
        &id,
        None,
        body.name.as_deref(),
        body.parent_id.as_deref(),
    )
    .await?;
    Ok(Json(json!({ "item": updated })))
}

async fn handle_delete_item(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<ItemTargetQuery>,
) -> Result<Json<Value>, ApiError> {
    let drive = resolver::resolve_target_drive(
        &state.graph,
        &state.storage,
        q.drive.as_deref(),
        q.site.as_deref(),
        Persona::detect(),
        false,
    )
    .await?;
    state.graph.delete_item(&drive.id, &id).await?;
    let storage = state.storage.clone();
    let did = drive.id.clone();
    let iid = id.clone();
    let _ = tokio::task::spawn_blocking(move || {
        let _ = storage.delete_drive_item(&did, &iid);
    })
    .await;
    Ok(Json(json!({ "ok": true, "deleted": true, "drive_id": drive.id, "item_id": id })))
}

async fn handle_share_item(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<ShareItemBody>,
) -> Result<Json<Value>, ApiError> {
    let drive = resolver::resolve_target_drive(
        &state.graph,
        &state.storage,
        body.drive.as_deref(),
        body.site.as_deref(),
        Persona::detect(),
        false,
    )
    .await?;
    let link = state
        .graph
        .create_share_link(
            &drive.id,
            &id,
            &body.link_type,
            &body.scope,
            body.expires.as_deref(),
        )
        .await?;
    Ok(Json(json!({ "link": link })))
}

async fn handle_recent(
    State(state): State<Arc<AppState>>,
    Query(q): Query<LimitOfflineQuery>,
) -> Result<Json<Value>, ApiError> {
    let limit = q.limit.unwrap_or(30);
    let items = if limit == 0 {
        Vec::new()
    } else if q.offline {
        let storage = state.storage.clone();
        tokio::task::spawn_blocking(move || storage.list_recent_items(limit))
            .await
            .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??
    } else {
        let live = state.graph.list_recent_items(limit).await?;
        let storage = state.storage.clone();
        let live_for_db = live.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let _ = storage.replace_recent_items(&live_for_db);
        })
        .await;
        live
    };
    Ok(Json(json!({ "items": items })))
}

async fn handle_shared(
    State(state): State<Arc<AppState>>,
    Query(q): Query<LimitOfflineQuery>,
) -> Result<Json<Value>, ApiError> {
    let limit = q.limit.unwrap_or(30);
    let items = if limit == 0 {
        Vec::new()
    } else if q.offline {
        let storage = state.storage.clone();
        tokio::task::spawn_blocking(move || storage.list_shared_items(limit))
            .await
            .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??
    } else {
        let live = state.graph.list_shared_with_me(limit).await?;
        let storage = state.storage.clone();
        let live_for_db = live.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let _ = storage.replace_shared_items(&live_for_db);
        })
        .await;
        live
    };
    Ok(Json(json!({ "items": items })))
}

async fn handle_search(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SearchQuery>,
) -> Result<Json<Value>, ApiError> {
    let drive_id = if q.drive.is_some() || q.site.is_some() {
        Some(
            resolver::resolve_target_drive(
                &state.graph,
                &state.storage,
                q.drive.as_deref(),
                q.site.as_deref(),
                Persona::detect(),
                !q.remote,
            )
            .await?
            .id,
        )
    } else {
        None
    };
    let items = cli::search_items_combined(
        &state.graph,
        &state.storage,
        &q.q,
        drive_id.as_deref(),
        q.remote,
        q.limit.unwrap_or(30),
    )
    .await?;
    Ok(Json(json!({ "query": q.q, "items": items })))
}

async fn handle_download(
    State(state): State<Arc<AppState>>,
    Json(body): Json<DownloadBody>,
) -> Result<Json<Value>, ApiError> {
    // Checked before anything is resolved or fetched: a refused request makes no Graph call.
    if !state.allow_local_paths && body.dir.as_deref().is_some_and(|d| !d.trim().is_empty()) {
        return Err(ApiError::forbidden(DOWNLOAD_DIR_DISABLED));
    }
    let target = body
        .item_id
        .as_deref()
        .or(body.path.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ApiError::bad_request("item_id or path is required"))?;
    let drive = resolver::resolve_target_drive(
        &state.graph,
        &state.storage,
        body.drive.as_deref(),
        body.site.as_deref(),
        Persona::detect(),
        false,
    )
    .await?;
    let item = resolver::resolve_item(&state.graph, &state.storage, &drive.id, target, false).await?;
    let dest_dir = cli::normalize_download_dir(&state.config, body.dir.as_deref());
    let downloaded = state
        .graph
        .download_item_to_file(
            item.effective_drive_id(),
            item.effective_item_id(),
            Some(&item.name),
            item.mime_type.as_deref(),
            item.download_url.as_deref(),
            &dest_dir,
        )
        .await?;
    let storage = state.storage.clone();
    let downloaded = tokio::task::spawn_blocking(move || {
        let mut dl = downloaded;
        cli::record_download_baseline(&storage, &item, &mut dl);
        dl
    })
    .await
    .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let value = serde_json::to_value(downloaded)
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(value))
}

async fn handle_download_by_id(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(mut body): Json<DownloadBody>,
) -> Result<Json<Value>, ApiError> {
    body.item_id = Some(id);
    handle_download(State(state), Json(body)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_exact_match_rejects_prefix_lookalikes() {
        for rejected in [
            "http://localhost.evil.com",
            "http://localhost.evil.com:8790",
            "http://127.0.0.1.evil.com",
            "http://localhost:1@evil.com",
            "http://localhostevil.com",
            "http://[::1].evil.com",
            "https://localhost",
            "null",
            "",
            "http://localhost/path",
            "http://localhost/",
            "http://localhost.",
            "http://user@localhost",
            "http://localhost?x=1",
            "http://localhost#frag",
            "file://localhost",
            "http://192.168.0.10:8790",
        ] {
            assert!(!is_allowed_origin(rejected), "must reject {rejected:?}");
        }
        for accepted in [
            "http://localhost",
            "http://localhost:5173",
            "http://LOCALHOST:3000",
            "http://127.0.0.1:8790",
            "http://127.0.0.1",
            "http://[::1]:8790",
        ] {
            assert!(is_allowed_origin(accepted), "must accept {accepted:?}");
        }
    }

    #[test]
    fn cors_origin_flag_is_exact_and_validated() {
        assert_eq!(
            normalize_cors_origin(" https://App.Example.com ").unwrap(),
            "https://app.example.com"
        );
        assert_eq!(
            normalize_cors_origin("https://app.example.com:8443/").unwrap(),
            "https://app.example.com:8443"
        );
        for bad in ["*", "app.example.com", "ftp://app.example.com", "https://app.example.com/x", "https://u:p@app.example.com"] {
            assert!(normalize_cors_origin(bad).is_err(), "must reject {bad:?}");
        }

        let allowed = vec![normalize_cors_origin("https://app.example.com").unwrap()];
        assert!(matches_extra_origin("https://app.example.com", &allowed));
        assert!(matches_extra_origin("https://APP.example.com", &allowed));
        assert!(!matches_extra_origin("https://app.example.com.evil.com", &allowed));
        assert!(!matches_extra_origin("http://app.example.com", &allowed));
        assert!(!matches_extra_origin("https://app.example.com:444", &allowed));
        assert!(!matches_extra_origin("https://app.example.com@evil.com", &allowed));
        assert!(!matches_extra_origin("https://app.example.com", &[]));
    }

    #[test]
    fn host_header_parsing_and_allow_list() {
        assert_eq!(host_without_port("127.0.0.1:8790").as_deref(), Some("127.0.0.1"));
        assert_eq!(host_without_port("LocalHost").as_deref(), Some("localhost"));
        assert_eq!(host_without_port("[::1]:8790").as_deref(), Some("::1"));
        assert_eq!(host_without_port("[::1]").as_deref(), Some("::1"));
        assert_eq!(host_without_port(""), None);
        assert_eq!(host_without_port("localhost:"), None);
        assert_eq!(host_without_port("localhost:80:90x"), None);
        assert_eq!(host_without_port("[::1]evil"), None);
        assert_eq!(host_without_port("user@localhost"), None);

        let none: Vec<String> = Vec::new();
        for ok in ["localhost", "localhost:8790", "127.0.0.1:8790", "[::1]:8790", "LOCALHOST:1"] {
            assert!(is_allowed_host(ok, &none), "must accept {ok:?}");
        }
        for bad in [
            "attacker.example",
            "attacker.example:8790",
            "localhost.attacker.example",
            "127.0.0.1.attacker.example:8790",
            "localhost.",
            "",
            "192.168.0.10:8790",
        ] {
            assert!(!is_allowed_host(bad, &none), "must reject {bad:?}");
        }
        let extra = allowed_hosts_for("127.0.0.1".parse().unwrap(), &["Files.Corp.Example:443".to_string()]);
        assert_eq!(extra, vec!["files.corp.example".to_string()]);
        assert!(is_allowed_host("files.corp.example:8790", &extra));
        assert!(!is_allowed_host("attacker.example", &extra));
    }

    #[test]
    fn allowed_hosts_follow_the_bind_address() {
        // A specific non-loopback bind address is reachable under its own literal.
        let lan = allowed_hosts_for("192.168.0.10".parse().unwrap(), &[]);
        assert_eq!(lan, vec!["192.168.0.10".to_string()]);
        assert!(is_allowed_host("192.168.0.10:8790", &lan));
        // An unspecified bind has no single literal: loopback names + --allowed-host only.
        let any = allowed_hosts_for("0.0.0.0".parse().unwrap(), &[]);
        assert!(any.is_empty());
        assert!(!is_allowed_host("192.168.0.10:8790", &any));
        assert!(is_allowed_host("127.0.0.1:8790", &any));
    }

    #[test]
    fn run_refuses_non_loopback_bind_without_api_key() {
        let lan: SocketAddr = "0.0.0.0:8790".parse().unwrap();
        let err = validate_bind(lan, false).unwrap_err().to_string();
        assert!(err.contains("--api-key"), "{err}");
        assert!(validate_bind(lan, true).is_ok());
        assert!(validate_bind("192.168.0.10:8790".parse().unwrap(), false).is_err());
        assert!(validate_bind("[::]:8790".parse().unwrap(), false).is_err());
        assert!(validate_bind("127.0.0.1:8790".parse().unwrap(), false).is_ok());
        assert!(validate_bind("[::1]:8790".parse().unwrap(), false).is_ok());

        // A blank or whitespace-only key is "no key" for the refusal and for the policy.
        for blank in ["", "   "] {
            let key = normalize_api_key(Some(blank.to_string()));
            assert!(key.is_none());
            assert!(validate_bind(lan, key.is_some()).is_err());
            let policy = RouterPolicy::from_api_key(Some(blank.to_string()));
            assert!(policy.api_key.is_none());
            assert!(!policy.allow_local_paths);
        }
        let keyed = RouterPolicy::from_api_key(Some(" secret ".to_string()));
        assert_eq!(keyed.api_key.as_deref(), Some("secret"));
        assert!(keyed.allow_local_paths);
    }

    #[test]
    fn serve_plan_generates_a_key_unless_keyless_is_explicit() {
        // Default: loopback, no key -> a generated key guards /v1 and unlocks local paths.
        let plan = plan_serve(&ServeOptions::default()).unwrap();
        assert!(plan.generated_key);
        assert_eq!(plan.policy.api_key.as_deref().map(str::len), Some(64));
        assert!(plan.policy.allow_local_paths);
        assert!(plan.bind_addr.ip().is_loopback());

        // An explicit key is used as given and never reported as generated.
        let keyed = plan_serve(&ServeOptions {
            api_key: Some("secret".to_string()),
            ..Default::default()
        })
        .unwrap();
        assert!(!keyed.generated_key);
        assert_eq!(keyed.policy.api_key.as_deref(), Some("secret"));
        assert!(keyed.policy.allow_local_paths);

        // A blank --api-key is no key: one is generated instead of serving open.
        let blank = plan_serve(&ServeOptions {
            api_key: Some("  ".to_string()),
            ..Default::default()
        })
        .unwrap();
        assert!(blank.generated_key);

        // --allow-keyless: no key, and local paths stay off without their own opt-in.
        let keyless = plan_serve(&ServeOptions {
            allow_keyless: true,
            ..Default::default()
        })
        .unwrap();
        assert!(!keyless.generated_key);
        assert!(keyless.policy.api_key.is_none());
        assert!(!keyless.policy.allow_local_paths);
        let keyless_paths = plan_serve(&ServeOptions {
            allow_keyless: true,
            allow_local_paths: true,
            ..Default::default()
        })
        .unwrap();
        assert!(keyless_paths.policy.api_key.is_none());
        assert!(keyless_paths.policy.allow_local_paths);

        // Non-loopback: refused without an explicit key, whatever else is passed.
        for allow_keyless in [false, true] {
            let err = plan_serve(&ServeOptions {
                bind: "0.0.0.0".to_string(),
                allow_keyless,
                allow_local_paths: true,
                ..Default::default()
            })
            .unwrap_err()
            .to_string();
            assert!(err.contains("--api-key"), "{err}");
        }
        let lan = plan_serve(&ServeOptions {
            bind: "192.168.0.10".to_string(),
            api_key: Some("secret".to_string()),
            allowed_hosts: vec!["files.corp.example".to_string()],
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            lan.policy.allowed_hosts,
            vec!["192.168.0.10".to_string(), "files.corp.example".to_string()]
        );

        // A malformed --cors-origin stops startup instead of being silently dropped.
        assert!(plan_serve(&ServeOptions {
            cors_origins: vec!["*".to_string()],
            ..Default::default()
        })
        .is_err());
    }

    #[test]
    fn generated_api_key_is_32_random_bytes_hex() {
        let a = generate_api_key();
        let b = generate_api_key();
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
