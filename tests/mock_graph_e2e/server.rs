//! Shared mock Microsoft Graph server and harness helpers.

use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post, put},
    Json, Router,
};
use serde_json::{json, Value};
use tokio::{net::TcpListener, sync::Mutex};

use sharepoint_cli::{
    auth::{AuthManager, StoredTokens},
    config::AppConfig,
    graph::GraphClient,
    storage::Storage,
};

#[derive(Clone)]
pub(crate) struct MockGraphState {
    pub(crate) base_url: String,
    pub(crate) chunk_bytes: Arc<Mutex<Vec<u8>>>,
    pub(crate) chunk_count: Arc<AtomicUsize>,
    pub(crate) redirect_saw_auth: Arc<AtomicBool>,
    pub(crate) chunk_saw_auth: Arc<AtomicBool>,
    pub(crate) deleted_items: Arc<Mutex<Vec<String>>>,
    pub(crate) version_pages: Arc<AtomicUsize>,
}

pub(crate) async fn start_mock_graph() -> (String, MockGraphState, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}/v1.0");

    let state = MockGraphState {
        base_url: base_url.clone(),
        chunk_bytes: Arc::new(Mutex::new(Vec::new())),
        chunk_count: Arc::new(AtomicUsize::new(0)),
        redirect_saw_auth: Arc::new(AtomicBool::new(false)),
        chunk_saw_auth: Arc::new(AtomicBool::new(false)),
        deleted_items: Arc::new(Mutex::new(Vec::new())),
        version_pages: Arc::new(AtomicUsize::new(0)),
    };

    let app = Router::new()
        .route(
            "/v1.0/me",
            get(|| async {
                Json(json!({
                    "id": "user-1",
                    "displayName": "Jane Doe",
                    "mail": "jane@contoso.com"
                }))
            }),
        )
        .route(
            "/v1.0/me/drive",
            get(|| async {
                Json(json!({
                    "id": "drv-me",
                    "name": "OneDrive",
                    "driveType": "business",
                    "webUrl": "https://contoso-my.sharepoint.com/personal/jane",
                    "owner": { "user": { "displayName": "Jane Doe" } },
                    "quota": { "total": 10737418240u64, "used": 1048576u64, "remaining": 10736369664u64, "state": "normal" }
                }))
            }),
        )
        .route(
            "/v1.0/me/drive/recent",
            get(|| async {
                Json(json!({
                    "value": [{
                        "id": "item-recent-1",
                        "name": "recent-notes.md",
                        "size": 128,
                        "file": { "mimeType": "text/markdown" },
                        "parentReference": { "driveId": "drv-me", "id": "root", "path": "/drive/root:/Notes" },
                        "lastModifiedDateTime": "2026-11-19T10:00:00Z",
                        "lastModifiedBy": { "user": { "displayName": "Jane Doe" } },
                        "webUrl": "https://contoso-my.sharepoint.com/recent-notes.md"
                    }]
                }))
            }),
        )
        .route(
            "/v1.0/me/drive/sharedWithMe",
            get(|| async {
                Json(json!({
                    "value": [{
                        "id": "item-shared-1",
                        "name": "shared-budget.xlsx",
                        "size": 8192,
                        "file": { "mimeType": "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" },
                        "remoteItem": {
                            "id": "remote-item-99",
                            "parentReference": { "driveId": "drv-remote-fin" },
                            "shared": { "scope": "organization" }
                        },
                        "lastModifiedBy": { "user": { "displayName": "Bob CFO" } }
                    }]
                }))
            }),
        )
        .route(
            "/v1.0/me/followedSites",
            get(|| async {
                Json(json!({
                    "value": [{
                        "id": "site-eng",
                        "name": "engineering",
                        "displayName": "Engineering Hub",
                        "description": "Engineering documents",
                        "webUrl": "https://contoso.sharepoint.com/sites/engineering"
                    }]
                }))
            }),
        )
        .route(
            "/v1.0/sites",
            get(|| async {
                Json(json!({
                    "value": [
                        {
                            "id": "site-eng",
                            "name": "engineering",
                            "displayName": "Engineering Hub",
                            "webUrl": "https://contoso.sharepoint.com/sites/engineering"
                        },
                        {
                            "id": "site-mkt",
                            "name": "marketing",
                            "displayName": "Marketing Portal",
                            "webUrl": "https://contoso.sharepoint.com/sites/marketing"
                        }
                    ]
                }))
            }),
        )
        .route(
            "/v1.0/sites/{site_id}/drives",
            get(|Path(site_id): Path<String>| async move {
                Json(json!({
                    "value": [{
                        "id": format!("drv-{site_id}-docs"),
                        "name": "Documents",
                        "driveType": "documentLibrary",
                        "webUrl": format!("https://contoso.sharepoint.com/sites/{site_id}/Shared%20Documents")
                    }]
                }))
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/root/children",
            get(|State(st): State<MockGraphState>, Path(drive_id): Path<String>, Query(q): Query<std::collections::HashMap<String, String>>| async move {
                if q.get("page").map(String::as_str) == Some("2") {
                    Json(json!({
                        "value": [{
                            "id": "item-ko-1",
                            "name": "기획서.md",
                            "size": 42,
                            "file": { "mimeType": "text/markdown" },
                            "parentReference": { "driveId": drive_id, "id": "root", "path": "/drive/root:" },
                            "lastModifiedDateTime": "2026-11-19T09:00:00Z",
                            "lastModifiedBy": { "user": { "displayName": "김철수" } }
                        }]
                    }))
                } else {
                    Json(json!({
                        "value": [
                            {
                                "id": "folder-proj",
                                "name": "Projects",
                                "size": 0,
                                "folder": { "childCount": 2 },
                                "parentReference": { "driveId": drive_id, "id": "root", "path": "/drive/root:" },
                                "lastModifiedDateTime": "2026-11-19T12:00:00Z"
                            },
                            {
                                "id": "item-readme",
                                "name": "README.md",
                                "size": 25,
                                "file": { "mimeType": "text/markdown" },
                                "parentReference": { "driveId": drive_id, "id": "root", "path": "/drive/root:" },
                                "lastModifiedDateTime": "2026-11-19T11:00:00Z",
                                "lastModifiedBy": { "user": { "displayName": "Jane Doe" } },
                                "webUrl": "https://contoso.sharepoint.com/README.md"
                            }
                        ],
                        "@odata.nextLink": format!("{}/drives/{}/root/children?page=2", st.base_url, drive_id)
                    }))
                }
            })
            .post(|Path(drive_id): Path<String>, Json(body): Json<Value>| async move {
                let name = body.get("name").and_then(Value::as_str).unwrap_or("New Folder");
                (
                    StatusCode::CREATED,
                    Json(json!({
                        "id": "folder-created-1",
                        "name": name,
                        "size": 0,
                        "folder": { "childCount": 0 },
                        "parentReference": { "driveId": drive_id, "id": "root", "path": "/drive/root:" }
                    })),
                )
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/items/{item_id}",
            get(|Path((drive_id, item_id)): Path<(String, String)>| async move {
                Json(json!({
                    "id": item_id,
                    "name": if item_id == "item-img" { "chart.png" } else { "README.md" },
                    "size": 25,
                    "file": { "mimeType": if item_id == "item-img" { "image/png" } else { "text/markdown" } },
                    "parentReference": { "driveId": drive_id, "id": "root", "path": "/drive/root:" },
                    "eTag": "\"mock-etag\"",
                    "webUrl": format!("https://contoso.sharepoint.com/items/{item_id}")
                }))
            })
            .patch(|Path((drive_id, item_id)): Path<(String, String)>, Json(body): Json<Value>| async move {
                let name = body.get("name").and_then(Value::as_str).unwrap_or("renamed.md");
                Json(json!({
                    "id": item_id,
                    "name": name,
                    "size": 25,
                    "file": { "mimeType": "text/markdown" },
                    "parentReference": { "driveId": drive_id, "id": "folder-proj", "path": "/drive/root:/Projects" }
                }))
            })
            .delete(|State(st): State<MockGraphState>, Path((_drive_id, item_id)): Path<(String, String)>| async move {
                st.deleted_items.lock().await.push(item_id);
                StatusCode::NO_CONTENT
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/items/{item_id}/children",
            post(|Path((drive_id, item_id)): Path<(String, String)>, Json(body): Json<Value>| async move {
                if item_id.starts_with('/') || item_id.contains('/') {
                    return (
                        StatusCode::NOT_FOUND,
                        Json(json!({ "error": { "code": "itemNotFound", "message": "raw path not allowed as item_id" } })),
                    )
                        .into_response();
                }
                let name = body.get("name").and_then(Value::as_str).unwrap_or("New Folder");
                (
                    StatusCode::CREATED,
                    Json(json!({
                        "id": format!("folder-under-{item_id}"),
                        "name": name,
                        "size": 0,
                        "folder": { "childCount": 0 },
                        "parentReference": { "driveId": drive_id, "id": item_id, "path": format!("/drive/root:/{item_id}") }
                    })),
                )
                    .into_response()
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/items/{item_id}/versions",
            get(|State(st): State<MockGraphState>, Path((drive_id, item_id)): Path<(String, String)>, Query(query): Query<std::collections::HashMap<String, String>>| async move {
                st.version_pages.fetch_add(1, Ordering::SeqCst);
                if query.contains_key("page") {
                    return Json(json!({ "value": [{ "id": "1.0", "size": 12 }] }));
                }
                Json(json!({
                    "value": [{ "id": "2.0", "size": 25 }],
                    "@odata.nextLink": format!("{}/drives/{drive_id}/items/{item_id}/versions?page=2", st.base_url)
                }))
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/items/{item_id}/createLink",
            post(|Json(body): Json<Value>| async move {
                let link_type = body.get("type").and_then(Value::as_str).unwrap_or("view");
                let scope = body.get("scope").and_then(Value::as_str).unwrap_or("organization");
                Json(json!({
                    "id": "perm-1",
                    "link": {
                        "webUrl": "https://contoso.sharepoint.com/:t:/s/eng/share-xyz",
                        "type": link_type,
                        "scope": scope
                    }
                }))
            }),
        )
        // `/content` returns 302 redirect to `/blob/{item_id}` to test Authorization stripping!
        .route(
            "/v1.0/drives/{drive_id}/items/{item_id}/content",
            get(|State(st): State<MockGraphState>, Path((_drive_id, item_id)): Path<(String, String)>, headers: HeaderMap| async move {
                if headers.get(header::AUTHORIZATION).is_none() {
                    return (StatusCode::UNAUTHORIZED, "missing bearer on /content").into_response();
                }
                let loc = format!("{}/blob/{item_id}", st.base_url.trim_end_matches("/v1.0"));
                (StatusCode::FOUND, [(header::LOCATION, loc)]).into_response()
            }),
        )
        .route(
            "/blob/{item_id}",
            get(|State(st): State<MockGraphState>, Path(item_id): Path<String>, headers: HeaderMap| async move {
                if headers.get(header::AUTHORIZATION).is_some() {
                    st.redirect_saw_auth.store(true, Ordering::SeqCst);
                    return (StatusCode::BAD_REQUEST, "Authorization header MUST be stripped on redirect").into_response();
                }
                if item_id == "item-img" {
                    // Valid 1x1 PNG
                    let mut png = Vec::new();
                    let img = image::RgbaImage::from_pixel(1, 1, image::Rgba([0, 128, 255, 255]));
                    image::DynamicImage::ImageRgba8(img)
                        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
                        .unwrap();
                    ([(header::CONTENT_TYPE, "image/png")], png).into_response()
                } else {
                    (
                        [(header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
                        "# SharePoint CLI\nHello world!\n",
                    )
                        .into_response()
                }
            }),
        )
        // Simple upload (`PUT /drives/{drive_id}/root:/{name}:/content`)
        .route(
            "/v1.0/drives/{drive_id}/{*tail}",
            get(|Path((drive_id, tail)): Path<(String, String)>| async move {
                if tail.contains("search(q=") {
                    return Json(json!({
                        "value": [{
                            "id": "item-search-hit",
                            "name": "roadmap-2026.md",
                            "size": 512,
                            "file": { "mimeType": "text/markdown" },
                            "parentReference": { "driveId": drive_id, "id": "root", "path": "/drive/root:/Strategy" }
                        }]
                    }))
                    .into_response();
                }
                if tail.starts_with("root:/") && tail.ends_with(':') {
                    let clean = tail.trim_start_matches("root:/").trim_end_matches(':');
                    let name = clean.rsplit('/').next().unwrap_or(clean);
                    return Json(json!({
                        "id": "item-by-path",
                        "name": name,
                        "size": 25,
                        "file": { "mimeType": "text/markdown" },
                        "parentReference": { "driveId": drive_id, "id": "root", "path": "/drive/root:" }
                    }))
                    .into_response();
                }
                (StatusCode::NOT_FOUND, "not found").into_response()
            })
            .put(|Path((drive_id, tail)): Path<(String, String)>, body: Bytes| async move {
                let name = tail
                    .trim_start_matches("root:/")
                    .split(":/")
                    .next()
                    .unwrap_or("uploaded.bin")
                    .rsplit('/')
                    .next()
                    .unwrap_or("uploaded.bin");
                (
                    StatusCode::CREATED,
                    Json(json!({
                        "id": "item-uploaded-simple",
                        "name": name,
                        "size": body.len(),
                        "file": { "mimeType": "text/plain" },
                        "parentReference": { "driveId": drive_id, "id": "root", "path": "/drive/root:" }
                    })),
                )
            })
            .post(|State(st): State<MockGraphState>, Path((_drive_id, tail)): Path<(String, String)>| async move {
                if tail.ends_with("createUploadSession") {
                    let upload_url = format!("{}/upload-session/s1", st.base_url.trim_end_matches("/v1.0"));
                    return Json(json!({
                        "uploadUrl": upload_url,
                        "expirationDateTime": "2026-11-20T00:00:00Z"
                    }))
                    .into_response();
                }
                (StatusCode::NOT_FOUND, "not found").into_response()
            }),
        )
        .route(
            "/upload-session/s1",
            put(|State(st): State<MockGraphState>, headers: HeaderMap, body: Bytes| async move {
                if headers.get(header::AUTHORIZATION).is_some() {
                    st.chunk_saw_auth.store(true, Ordering::SeqCst);
                    return (StatusCode::BAD_REQUEST, "upload session must not receive Bearer auth").into_response();
                }
                let range = headers
                    .get("content-range")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                let mut acc = st.chunk_bytes.lock().await;
                acc.extend_from_slice(&body);
                st.chunk_count.fetch_add(1, Ordering::SeqCst);

                // Parse `bytes start-end/total`
                let total: usize = range
                    .split('/')
                    .nth(1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                if acc.len() >= total && total > 0 {
                    (
                        StatusCode::CREATED,
                        Json(json!({
                            "id": "item-uploaded-chunked",
                            "name": "large.bin",
                            "size": acc.len(),
                            "file": { "mimeType": "application/octet-stream" },
                            "parentReference": { "driveId": "drv-me", "id": "root", "path": "/drive/root:" }
                        })),
                    )
                        .into_response()
                } else {
                    (StatusCode::ACCEPTED, Json(json!({ "nextExpectedRanges": [] }))).into_response()
                }
            }),
        )
        .route(
            "/v1.0/search/query",
            post(|| async {
                Json(json!({
                    "value": [{
                        "hitsContainers": [{
                            "hits": [{
                                "resource": {
                                    "id": "item-cross-1",
                                    "name": "cross-site-spec.md",
                                    "size": 1024,
                                    "file": { "mimeType": "text/markdown" },
                                    "parentReference": { "driveId": "drv-site-eng-docs", "id": "root", "path": "/drive/root:" }
                                }
                            }]
                        }]
                    }]
                }))
            }),
        )
        .with_state(state.clone());

    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (base_url, state, handle)
}

pub(crate) fn onedrive_alias_exe(dir: &std::path::Path) -> std::path::PathBuf {
    let src = std::path::Path::new(env!("CARGO_BIN_EXE_sharepoint"));
    let dst = dir.join(format!(
        "bitefoundry-onedrive{}",
        std::env::consts::EXE_SUFFIX
    ));
    if std::fs::hard_link(src, &dst).is_err() {
        std::fs::copy(src, &dst).unwrap();
    }
    dst
}

pub(crate) fn make_graph_and_storage(base_url: &str) -> (AppConfig, GraphClient, Storage) {
    let cfg = AppConfig {
        graph_base_url: base_url.to_string(),
        ..Default::default()
    };
    let tokens = StoredTokens {
        access_token: "mock-access-token".to_string(),
        refresh_token: None,
        expires_at: chrono::Utc::now().timestamp() + 3600,
        client_id: cfg.client_id.clone(),
        tenant_id: cfg.tenant_id.clone(),
        user_id: Some("user-1".to_string()),
        user_display_name: Some("Jane Doe".to_string()),
        user_email: Some("jane@contoso.com".to_string()),
        auth_mode: "access_token".to_string(),
    };
    let auth = AuthManager::from_tokens(cfg.clone(), tokens);
    let graph = GraphClient::new(cfg.clone(), auth)
        .unwrap()
        .with_my_user_id("user-1");
    let storage = Storage::open_in_memory().unwrap();
    (cfg, graph, storage)
}

