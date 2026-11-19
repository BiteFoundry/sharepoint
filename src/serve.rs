//! Local HTTP JSON API (`sharepoint serve` / `onedrive serve`, default port `8790`) + OpenAPI 3.1 powered by `axum`.

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use axum::{
    extract::{Path, Query, Request, State},
    http::{header, Method, StatusCode},
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
    graph::GraphClient,
    resolver,
    storage::Storage,
};

#[derive(Debug, Clone)]
pub struct ServeOptions {
    pub bind: String,
    pub port: u16,
    pub api_key: Option<String>,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1".to_string(),
            port: config::DEFAULT_SERVE_PORT,
            api_key: None,
        }
    }
}

pub struct AppState {
    pub config: AppConfig,
    pub graph: GraphClient,
    pub storage: Storage,
    pub api_key: Option<String>,
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

pub fn build_router(
    config: AppConfig,
    graph: GraphClient,
    storage: Storage,
    api_key: Option<String>,
) -> Router {
    let has_key = api_key.as_deref().is_some_and(|t| !t.trim().is_empty());
    let state = Arc::new(AppState {
        config,
        graph,
        storage,
        api_key: if has_key { api_key } else { None },
    });

    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|origin, _| {
            origin.to_str().is_ok_and(|o| {
                o.starts_with("http://127.0.0.1")
                    || o.starts_with("http://localhost")
                    || o.starts_with("http://[::1]")
            })
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

    Router::new()
        .route("/health", get(handle_health))
        .route("/openapi.yaml", get(handle_openapi))
        .nest("/v1", v1)
        .layer(cors)
        .with_state(state)
}

pub async fn run(
    config: AppConfig,
    graph: GraphClient,
    storage: Storage,
    opts: ServeOptions,
) -> Result<()> {
    let bind_addr: SocketAddr = format!("{}:{}", opts.bind, opts.port)
        .parse()
        .with_context(|| format!("Invalid bind address '{}:{}'", opts.bind, opts.port))?;
    let router = build_router(config, graph, storage, opts.api_key);
    let listener = TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("Failed to bind HTTP server on {bind_addr}"))?;
    eprintln!(
        "sharepoint HTTP API listening on http://{bind_addr} (GET /health, GET /openapi.yaml)"
    );
    axum::serve(listener, router).await?;
    Ok(())
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
    Ok(Json(json!({
        "ok": true,
        "id": item.id,
        "drive_id": item.drive_id,
        "name": item.name,
        "mime_type": item.mime_type,
        "size_bytes": bytes.len(),
        "content": String::from_utf8_lossy(&bytes),
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
    let _ = state
        .storage
        .upsert_drive_items(std::slice::from_ref(&created));
    Ok(Json(json!({ "item": created })))
}

async fn handle_upload(
    State(state): State<Arc<AppState>>,
    Json(body): Json<UploadBody>,
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

    let uploaded = if let Some(lp) = body.local_path.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        state
            .graph
            .upload_local_file(
                &drive.id,
                body.parent_id.as_deref(),
                body.file_name.as_deref(),
                &PathBuf::from(lp),
            )
            .await?
    } else if let Some(b64) = body.content_base64.as_deref() {
        let name = body
            .file_name
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ApiError::bad_request("file_name is required with content_base64"))?;
        let bytes = B64_STANDARD
            .decode(b64.trim())
            .map_err(|e| ApiError::bad_request(format!("Invalid base64: {e}")))?;
        state
            .graph
            .upload_bytes(&drive.id, body.parent_id.as_deref(), name, bytes, None)
            .await?
    } else if let Some(text) = body.content_text {
        let name = body
            .file_name
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ApiError::bad_request("file_name is required with content_text"))?;
        state
            .graph
            .upload_bytes(
                &drive.id,
                body.parent_id.as_deref(),
                name,
                text.into_bytes(),
                Some("text/plain; charset=utf-8"),
            )
            .await?
    } else {
        return Err(ApiError::bad_request(
            "Provide local_path, content_base64, or content_text",
        ));
    };

    let _ = state
        .storage
        .upsert_drive_items(std::slice::from_ref(&uploaded));
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
    let updated = state
        .graph
        .rename_or_move_item(&drive.id, &id, body.name.as_deref(), body.parent_id.as_deref())
        .await?;
    let _ = state
        .storage
        .upsert_drive_items(std::slice::from_ref(&updated));
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
    let _ = state.storage.delete_drive_item(&drive.id, &id);
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
    let items = if q.offline {
        state.storage.list_recent_items(limit)?
    } else {
        let live = state.graph.list_recent_items(limit).await?;
        let _ = state.storage.replace_recent_items(&live);
        live
    };
    Ok(Json(json!({ "items": items })))
}

async fn handle_shared(
    State(state): State<Arc<AppState>>,
    Query(q): Query<LimitOfflineQuery>,
) -> Result<Json<Value>, ApiError> {
    let limit = q.limit.unwrap_or(30);
    let items = if q.offline {
        state.storage.list_shared_items(limit)?
    } else {
        let live = state.graph.list_shared_with_me(limit).await?;
        let _ = state.storage.replace_shared_items(&live);
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
    Ok(Json(serde_json::to_value(downloaded).unwrap()))
}

async fn handle_download_by_id(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(mut body): Json<DownloadBody>,
) -> Result<Json<Value>, ApiError> {
    body.item_id = Some(id);
    handle_download(State(state), Json(body)).await
}
