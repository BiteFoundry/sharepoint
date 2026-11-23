//! End-to-end tests against a local mock Microsoft Graph server (`axum`) covering:
//! - Personal OneDrive (`/me/drive`, `/recent`, `/sharedWithMe`)
//! - SharePoint Sites (`/me/followedSites`, `/sites?search=*`, `/sites/{id}/drives`)
//! - Folder children listing with `@odata.nextLink` pagination + path-based addressing
//! - Simple upload (`< 4 MiB`) and resumable chunked upload session (`createUploadSession` + multi-chunk `Content-Range`)
//! - Download (`/content` 302 redirect with explicit Authorization header stripping!)
//! - mkdir, rename/move, delete, versions, share links
//! - SQLite WAL + FTS5 search including Korean `<3` char `LIKE` fallback
//! - HTTP API (`serve::build_router`, port 8790 contract)
//! - MCP Server (`mcp::handle_rpc_line`, all 16 tools + `delete_item` `confirm: true` guard)

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
use tempfile::tempdir;
use tokio::{net::TcpListener, sync::Mutex};

use sharepoint_cli::{
    auth::{AuthManager, StoredTokens},
    config::AppConfig,
    graph::GraphClient,
    mcp,
    serve,
    storage::Storage,
};

#[derive(Clone)]
struct MockGraphState {
    base_url: String,
    chunk_bytes: Arc<Mutex<Vec<u8>>>,
    chunk_count: Arc<AtomicUsize>,
    redirect_saw_auth: Arc<AtomicBool>,
    chunk_saw_auth: Arc<AtomicBool>,
    deleted_items: Arc<Mutex<Vec<String>>>,
    version_pages: Arc<AtomicUsize>,
}

async fn start_mock_graph() -> (String, MockGraphState, tokio::task::JoinHandle<()>) {
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

fn make_graph_and_storage(base_url: &str) -> (AppConfig, GraphClient, Storage) {
    let mut cfg = AppConfig::default();
    cfg.graph_base_url = base_url.to_string();
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

#[tokio::test]
async fn e2e_onedrive_sharepoint_pagination_uploads_downloads_and_fts5() {
    let (base_url, mock_state, server) = start_mock_graph().await;
    let (_cfg, graph, storage) = make_graph_and_storage(&base_url);

    // 1. OneDrive + Recent + Shared
    let my_drive = graph.get_my_drive().await.unwrap();
    assert_eq!(my_drive.id, "drv-me");
    assert!(my_drive.is_personal_onedrive());

    let recent = graph.list_recent_items(10).await.unwrap();
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].name, "recent-notes.md");

    let shared = graph.list_shared_with_me(10).await.unwrap();
    assert_eq!(shared.len(), 1);
    assert_eq!(shared[0].effective_drive_id(), "drv-remote-fin");
    assert_eq!(shared[0].effective_item_id(), "remote-item-99");

    // 2. SharePoint Sites + Drives
    let followed = graph.list_followed_sites().await.unwrap();
    assert_eq!(followed.len(), 1);
    assert!(followed[0].is_followed);

    let all_sites = graph.list_sites(Some("*"), 50).await.unwrap();
    assert_eq!(all_sites.len(), 2);

    let eng_drives = graph
        .list_site_drives("site-eng", Some("Engineering Hub"))
        .await
        .unwrap();
    assert_eq!(eng_drives.len(), 1);
    assert_eq!(eng_drives[0].id, "drv-site-eng-docs");

    // 3. Multi-page @odata.nextLink pagination + SQLite FTS5 + Korean <3 char LIKE fallback
    let children = graph.list_children("drv-me", None, None).await.unwrap();
    assert_eq!(children.len(), 3, "must follow @odata.nextLink across 2 pages");
    storage.upsert_drive_items(&children).unwrap();
    assert_eq!(storage.search_items("README", Some("drv-me"), 10).unwrap().len(), 1);
    assert_eq!(storage.search_items("기획", Some("drv-me"), 10).unwrap().len(), 1);

    // 4. Path-based lookup
    let by_path = graph.get_item_by_path("drv-me", "/Docs/spec.md").await.unwrap();
    assert_eq!(by_path.name, "spec.md");

    // 5. Download with 302 redirect Authorization stripping!
    let tmp = tempdir().unwrap();
    let dl = graph
        .download_item_to_file("drv-me", "item-img", Some("chart.png"), Some("image/png"), None, tmp.path())
        .await
        .unwrap();
    assert!(dl.ok);
    assert!(dl.is_image);
    assert_eq!(dl.mime_type, "image/png");
    assert!(dl.size_bytes > 20);
    assert!(dl.path.exists());
    assert!(
        !mock_state.redirect_saw_auth.load(Ordering::SeqCst),
        "Authorization header must be stripped on 302 redirect"
    );

    // 6. Simple upload (< 4 MiB)
    let simple = graph
        .upload_file_simple("drv-me", None, "hello.txt", b"hello sharepoint".to_vec(), None)
        .await
        .unwrap();
    assert_eq!(simple.id, "item-uploaded-simple");
    assert_eq!(simple.size, 16);

    // 7. Chunked upload session (2 chunks, verifying no Bearer header sent to uploadUrl)
    let payload = vec![b'A'; 700_000];
    let chunked = graph
        .upload_bytes_chunked("drv-me", None, "large.bin", &payload, 320 * 1024)
        .await
        .unwrap();
    assert_eq!(chunked.id, "item-uploaded-chunked");
    assert_eq!(chunked.size as usize, payload.len());
    assert!(mock_state.chunk_count.load(Ordering::SeqCst) >= 2);
    assert!(
        !mock_state.chunk_saw_auth.load(Ordering::SeqCst),
        "Pre-authenticated upload session URL must not receive Bearer auth"
    );

    // 8. mkdir, rename, versions, share link, delete, cross-site search
    let folder = graph.create_folder("drv-me", None, "Reports").await.unwrap();
    assert_eq!(folder.name, "Reports");
    assert!(folder.is_folder);

    let renamed = graph
        .rename_or_move_item("drv-me", "item-readme", Some("GUIDE.md"), Some("folder-proj"))
        .await
        .unwrap();
    assert_eq!(renamed.name, "GUIDE.md");

    let versions = graph
        .list_item_versions_with_limit("drv-me", "item-readme", Some(1))
        .await
        .unwrap();
    assert_eq!(versions.len(), 1);
    assert_eq!(versions[0].id, "2.0");
    assert_eq!(mock_state.version_pages.load(Ordering::SeqCst), 1);

    let versions = graph.list_item_versions("drv-me", "item-readme").await.unwrap();
    let ids: Vec<_> = versions.iter().map(|v| v.id.as_str()).collect();
    assert_eq!(ids, ["2.0", "1.0"]);
    assert_eq!(mock_state.version_pages.load(Ordering::SeqCst), 3);

    let share = graph
        .create_share_link("drv-me", "item-readme", "view", "organization", None)
        .await
        .unwrap();
    assert!(share.web_url.contains("share-xyz"));

    let cross = graph.search_across_sites("spec", 10).await.unwrap();
    assert_eq!(cross.len(), 1);
    assert_eq!(cross[0].id, "item-cross-1");

    graph.delete_item("drv-me", "item-readme").await.unwrap();
    assert_eq!(mock_state.deleted_items.lock().await.as_slice(), &["item-readme"]);

    server.abort();
}

#[tokio::test]
async fn e2e_http_api_and_mcp_tools_parity() {
    let (base_url, _mock_state, graph_server) = start_mock_graph().await;
    let (cfg, graph, storage) = make_graph_and_storage(&base_url);

    // --- HTTP API (`serve::build_router`) ---
    let router = serve::build_router(
        cfg.clone(),
        graph.clone(),
        storage.clone(),
        Some("secret-api-key".to_string()),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_addr = listener.local_addr().unwrap();
    let api_server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let http = reqwest::Client::new();
    let health: Value = http
        .get(format!("http://{api_addr}/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["ok"], true);
    assert_eq!(health["service"], "sharepoint");

    // Unauthorized without API key
    let unauth = http
        .get(format!("http://{api_addr}/v1/sites"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

    // Authorized requests
    let sites_resp: Value = http
        .get(format!("http://{api_addr}/v1/sites"))
        .bearer_auth("secret-api-key")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(sites_resp["sites"].as_array().unwrap().len(), 2);

    let tmp_dl = tempdir().unwrap();
    let dl_resp: Value = http
        .post(format!("http://{api_addr}/v1/download"))
        .bearer_auth("secret-api-key")
        .json(&json!({
            "drive": "me",
            "item_id": "item-img",
            "dir": tmp_dl.path().to_string_lossy()
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(dl_resp["ok"], true);
    assert_eq!(dl_resp["file_name"], "chart.png");
    assert_eq!(dl_resp["mime_type"], "image/png");
    assert_eq!(dl_resp["is_image"], true);

    // HTTP /v1/items/upload with empty if_match returns 400 Bad Request (WI-1)
    let bad_if_match = http
        .post(format!("http://{api_addr}/v1/items/upload"))
        .bearer_auth("secret-api-key")
        .json(&json!({
            "drive": "me",
            "file_name": "note.md",
            "content_text": "hello",
            "if_match": ""
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_if_match.status(), StatusCode::BAD_REQUEST);
    let bad_if_match_body: Value = bad_if_match.json().await.unwrap();
    assert!(bad_if_match_body["error"]
        .as_str()
        .unwrap_or("")
        .contains("If-Match requires a non-empty ETag"));

    // --- MCP Server (`mcp::handle_rpc_line`) ---
    let init = mcp::handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
    )
    .await
    .unwrap();
    assert_eq!(init["result"]["serverInfo"]["name"], "sharepoint");

    let list_tools = mcp::handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
    )
    .await
    .unwrap();
    assert!(list_tools["result"]["tools"].as_array().unwrap().len() >= 15);

    let compact_info = mcp::handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"get_file_info","arguments":{"drive":"me","item_id":"item-readme","compact":true}}}"#,
    )
    .await
    .unwrap();
    let compact = &compact_info["result"]["structuredContent"];
    assert_eq!(compact, &json!({ "item": {
        "drive_id": "drv-me",
        "id": "item-readme",
        "name": "README.md",
        "parent_path": "",
        "is_folder": false,
        "size": 25,
        "last_modified_at": null,
        "e_tag": "\"mock-etag\"",
        "omitted": {
            "fields": ["mime_type", "parent_id", "web_url"],
            "versions": null
        },
        "hint": "Call get_file_info with item_id \"item-readme\", drive \"drv-me\" and compact:false for every field."
    } }));
    let text = compact_info["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(&serde_json::from_str::<Value>(text).unwrap(), compact);

    // read_text_file
    let read_res = mcp::handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"read_text_file","arguments":{"drive":"me","item_id":"item-readme"}}}"#,
    )
    .await
    .unwrap();
    assert_eq!(read_res["result"]["isError"], false);
    assert!(read_res["result"]["structuredContent"]["content"]
        .as_str()
        .unwrap()
        .contains("SharePoint CLI"));

    // download_file returns { ok, path, file_name, mime_type, size_bytes, is_image }
    let mcp_dl_req = json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "download_file",
            "arguments": {
                "drive": "me",
                "item_id": "item-img",
                "dir": tmp_dl.path().to_string_lossy()
            }
        }
    });
    let mcp_dl = mcp::handle_rpc_line(&cfg, &graph, &storage, &mcp_dl_req.to_string())
        .await
        .unwrap();
    let sc = &mcp_dl["result"]["structuredContent"];
    assert_eq!(sc["ok"], true);
    assert_eq!(sc["mime_type"], "image/png");
    assert_eq!(sc["is_image"], true);
    assert!(sc["size_bytes"].as_u64().unwrap() > 0);

    // delete_item without confirm: true MUST be rejected
    let del_rejected = mcp::handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"delete_item","arguments":{"drive":"me","item_id":"item-readme"}}}"#,
    )
    .await
    .unwrap();
    assert_eq!(del_rejected["result"]["isError"], true);

    // delete_item with confirm: true succeeds
    let del_ok = mcp::handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"delete_item","arguments":{"drive":"me","item_id":"item-readme","confirm":true}}}"#,
    )
    .await
    .unwrap();
    assert_eq!(del_ok["result"]["isError"], false);
    assert_eq!(del_ok["result"]["structuredContent"]["deleted"], true);

    // WI-3 (F-06): create_folder accepts both a folder path and a folder ID
    let cf_by_path = mcp::handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"create_folder","arguments":{"drive":"me","parent_id":"/Team Docs","name":"Path Child"}}}"#,
    )
    .await
    .unwrap();
    assert_eq!(cf_by_path["result"]["isError"], false, "{cf_by_path}");
    assert_eq!(
        cf_by_path["result"]["structuredContent"]["item"]["name"],
        "Path Child"
    );
    assert_eq!(
        cf_by_path["result"]["structuredContent"]["item"]["parent_id"],
        "item-by-path"
    );

    let cf_by_id = mcp::handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"create_folder","arguments":{"drive":"me","parent_id":"folder-proj","name":"Id Child"}}}"#,
    )
    .await
    .unwrap();
    assert_eq!(cf_by_id["result"]["isError"], false, "{cf_by_id}");
    assert_eq!(
        cf_by_id["result"]["structuredContent"]["item"]["name"],
        "Id Child"
    );
    assert_eq!(
        cf_by_id["result"]["structuredContent"]["item"]["parent_id"],
        "folder-proj"
    );

    api_server.abort();
    graph_server.abort();
}

#[tokio::test]
async fn e2e_sync_fixes_write_targeting_conditional_upload_partial_limit_and_offline_isolation() {
    use sharepoint_cli::{
        cli,
        graph::drives::{ConflictBehavior, UploadOptions},
        resolver,
    };

    #[derive(Clone)]
    struct SyncMock {
        base_url: String,
        items: Arc<Mutex<std::collections::HashMap<(String, String), Value>>>,
        contents: Arc<Mutex<std::collections::HashMap<(String, String), Vec<u8>>>>,
        sessions: Arc<Mutex<std::collections::HashMap<String, (String, Option<String>, String)>>>,
        followed_sites: Arc<Mutex<Vec<Value>>>,
        requests: Arc<Mutex<Vec<(String, String, Option<String>)>>>,
        /// When set, bumps `item-doc` eTag to this value right after the next GET /items/item-doc
        /// so we can test the documented `POST .../items/{id}/createUploadSession` If-Match 412 guard.
        mutate_doc_etag_after_next_get: Arc<Mutex<Option<String>>>,
    }

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}/v1.0");

    let mut init_items = std::collections::HashMap::new();
    init_items.insert(
        ("drv-me".to_string(), "fld-fin".to_string()),
        json!({
            "id": "fld-fin",
            "name": "Finance",
            "size": 0,
            "folder": { "childCount": 1 },
            "parentReference": { "driveId": "drv-me", "id": "root", "path": "/drive/root:" }
        }),
    );
    init_items.insert(
        ("drv-me".to_string(), "item-secret".to_string()),
        json!({
            "id": "item-secret",
            "name": "secret.md",
            "size": 12,
            "file": { "mimeType": "text/markdown" },
            "parentReference": { "driveId": "drv-me", "id": "fld-fin", "path": "/drive/root:/Finance" }
        }),
    );
    init_items.insert(
        ("drv-me".to_string(), "item-doc".to_string()),
        json!({
            "id": "item-doc",
            "name": "doc.md",
            "size": 9,
            "eTag": "\"etag-v1\"",
            "cTag": "\"ctag-v1\"",
            "file": { "mimeType": "text/markdown" },
            "parentReference": { "driveId": "drv-me", "id": "root", "path": "/drive/root:" }
        }),
    );
    init_items.insert(
        ("drv-me".to_string(), "item-root-2".to_string()),
        json!({
            "id": "item-root-2",
            "name": "zeta.txt",
            "size": 5,
            "file": { "mimeType": "text/plain" },
            "parentReference": { "driveId": "drv-me", "id": "root", "path": "/drive/root:" }
        }),
    );

    let mut init_contents = std::collections::HashMap::new();
    init_contents.insert(
        ("drv-me".to_string(), "item-doc".to_string()),
        b"VERSION_1".to_vec(),
    );

    let st = SyncMock {
        base_url: base_url.clone(),
        items: Arc::new(Mutex::new(init_items)),
        contents: Arc::new(Mutex::new(init_contents)),
        sessions: Arc::new(Mutex::new(std::collections::HashMap::new())),
        followed_sites: Arc::new(Mutex::new(vec![json!({
            "id": "site-eng",
            "name": "engineering",
            "displayName": "Engineering Hub",
            "webUrl": "https://synthetic.sharepoint.test/sites/engineering"
        })])),
        requests: Arc::new(Mutex::new(Vec::new())),
        mutate_doc_etag_after_next_get: Arc::new(Mutex::new(None)),
    };

    let router = Router::new()
        .route(
            "/v1.0/me/followedSites",
            get(|State(s): State<SyncMock>| async move {
                s.requests
                    .lock()
                    .await
                    .push(("GET".into(), "/v1.0/me/followedSites".into(), None));
                let vals = s.followed_sites.lock().await.clone();
                Json(json!({ "value": vals }))
            }),
        )
        .route(
            "/v1.0/sites",
            get(|State(s): State<SyncMock>, Query(q): Query<std::collections::HashMap<String, String>>| async move {
                let search = q.get("search").cloned().unwrap_or_else(|| "*".into());
                s.requests
                    .lock()
                    .await
                    .push(("GET".into(), format!("/v1.0/sites?search={search}"), None));
                let all = vec![
                    json!({
                        "id": "site-eng",
                        "name": "engineering",
                        "displayName": "Engineering Hub",
                        "webUrl": "https://synthetic.sharepoint.test/sites/engineering"
                    }),
                    json!({
                        "id": "site-mkt",
                        "name": "marketing",
                        "displayName": "Marketing Portal",
                        "webUrl": "https://synthetic.sharepoint.test/sites/marketing"
                    }),
                ];
                let filtered: Vec<Value> = if search == "*" {
                    all
                } else {
                    let ql = search.to_lowercase();
                    all.into_iter()
                        .filter(|v| v["displayName"].as_str().unwrap_or("").to_lowercase().contains(&ql))
                        .collect()
                };
                Json(json!({ "value": filtered }))
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/root/children",
            get(|State(s): State<SyncMock>, Path(drive_id): Path<String>, Query(q): Query<std::collections::HashMap<String, String>>| async move {
                let top: usize = q.get("$top").and_then(|v| v.parse().ok()).unwrap_or(200);
                s.requests.lock().await.push(("GET".into(), format!("/v1.0/drives/{drive_id}/root/children?$top={top}"), None));
                let items = s.items.lock().await;
                let mut out: Vec<Value> = items
                    .iter()
                    .filter(|((d, _), v)| {
                        d == &drive_id
                            && v["parentReference"]["path"]
                                .as_str()
                                .unwrap_or("")
                                .ends_with("root:")
                    })
                    .map(|(_, v)| v.clone())
                    .collect();
                out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
                out.truncate(top);
                Json(json!({ "value": out }))
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/items/{item_id}",
            get(|State(s): State<SyncMock>, Path((drive_id, item_id)): Path<(String, String)>| async move {
                s.requests.lock().await.push(("GET".into(), format!("/v1.0/drives/{drive_id}/items/{item_id}"), None));
                let found = s.items.lock().await.get(&(drive_id.clone(), item_id.clone())).cloned();
                if item_id == "item-doc" {
                    if let Some(next_etag) = s.mutate_doc_etag_after_next_get.lock().await.take() {
                        if let Some(entry) = s.items.lock().await.get_mut(&(drive_id, item_id.clone())) {
                            entry["eTag"] = json!(next_etag);
                        }
                        s.contents
                            .lock()
                            .await
                            .insert(("drv-me".into(), item_id), b"VERSION_RACED_BEFORE_SESSION".to_vec());
                    }
                }
                match found {
                    Some(v) => (StatusCode::OK, Json(v)).into_response(),
                    None => (StatusCode::NOT_FOUND, Json(json!({ "error": { "code": "itemNotFound" } }))).into_response(),
                }
            })
            .delete(|State(s): State<SyncMock>, Path((drive_id, item_id)): Path<(String, String)>| async move {
                s.requests.lock().await.push(("DELETE".into(), format!("/v1.0/drives/{drive_id}/items/{item_id}"), None));
                s.items.lock().await.remove(&(drive_id, item_id));
                StatusCode::NO_CONTENT
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/items/{item_id}/content",
            get(|State(s): State<SyncMock>, Path((drive_id, item_id)): Path<(String, String)>| async move {
                match s.contents.lock().await.get(&(drive_id, item_id)).cloned() {
                    Some(data) => ([(header::CONTENT_TYPE, "text/markdown")], data).into_response(),
                    None => StatusCode::NOT_FOUND.into_response(),
                }
            })
            // Intentionally IGNORES If-Match/If-None-Match on PUT /content to prove client does not rely on undocumented PUT headers!
            .put(|State(s): State<SyncMock>, Path((drive_id, item_id)): Path<(String, String)>, headers: HeaderMap, body: Bytes| async move {
                let if_match = headers.get("if-match").and_then(|v| v.to_str().ok()).map(str::to_string);
                let req_num = {
                    let mut reqs = s.requests.lock().await;
                    reqs.push(("PUT".into(), format!("/v1.0/drives/{drive_id}/items/{item_id}/content"), if_match));
                    reqs.len()
                };
                let mut items = s.items.lock().await;
                let Some(existing) = items.get(&(drive_id.clone(), item_id.clone())).cloned() else {
                    return StatusCode::NOT_FOUND.into_response();
                };
                let mut updated = existing;
                updated["size"] = json!(body.len());
                updated["eTag"] = json!(format!("\"etag-put-{req_num}\""));
                updated["cTag"] = json!(format!("\"ctag-put-{req_num}\""));
                items.insert((drive_id.clone(), item_id.clone()), updated.clone());
                s.contents.lock().await.insert((drive_id, item_id), body.to_vec());
                (StatusCode::OK, Json(updated)).into_response()
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/items/{item_id}/createUploadSession",
            post(|State(s): State<SyncMock>, Path((drive_id, item_id)): Path<(String, String)>, headers: HeaderMap| async move {
                let if_match = headers.get("if-match").and_then(|v| v.to_str().ok()).map(str::to_string);
                let req_num = {
                    let mut reqs = s.requests.lock().await;
                    reqs.push(("POST".into(), format!("/v1.0/drives/{drive_id}/items/{item_id}/createUploadSession"), if_match.clone()));
                    reqs.len()
                };
                let items = s.items.lock().await;
                let Some(existing) = items.get(&(drive_id.clone(), item_id.clone())) else {
                    return (StatusCode::NOT_FOUND, Json(json!({ "error": { "code": "itemNotFound" } }))).into_response();
                };
                if let Some(ref expected) = if_match {
                    let current = existing["eTag"].as_str().unwrap_or("");
                    if expected != current {
                        return (
                            StatusCode::PRECONDITION_FAILED,
                            Json(json!({ "error": { "code": "resourceModified", "message": "createUploadSession If-Match mismatch" } })),
                        )
                            .into_response();
                    }
                }
                let name = existing["name"].as_str().unwrap_or("file.bin").to_string();
                let sid = format!("sess-{req_num}");
                s.sessions.lock().await.insert(sid.clone(), (drive_id, Some(item_id), name));
                let upload_url = format!("{}/upload-session/{sid}", s.base_url.trim_end_matches("/v1.0"));
                (StatusCode::OK, Json(json!({ "uploadUrl": upload_url, "expirationDateTime": "2026-11-20T09:00:00Z" }))).into_response()
            }),
        )
        .route(
            "/upload-session/{sid}",
            put(|State(s): State<SyncMock>, Path(sid): Path<String>, body: Bytes| async move {
                let req_num = {
                    let mut reqs = s.requests.lock().await;
                    reqs.push(("PUT".into(), format!("/upload-session/{sid}"), None));
                    reqs.len()
                };
                let Some((drive_id, existing_id, name)) = s.sessions.lock().await.get(&sid).cloned() else {
                    return StatusCode::NOT_FOUND.into_response();
                };
                let mut items = s.items.lock().await;
                let id = existing_id.unwrap_or_else(|| format!("item-sess-{req_num}"));
                let updated = json!({
                    "id": id,
                    "name": name,
                    "size": body.len(),
                    "eTag": format!("\"etag-sess-{req_num}\""),
                    "cTag": format!("\"ctag-sess-{req_num}\""),
                    "file": { "mimeType": "text/markdown" },
                    "parentReference": { "driveId": drive_id, "id": "root", "path": "/drive/root:" }
                });
                items.insert((drive_id.clone(), id.clone()), updated.clone());
                s.contents.lock().await.insert((drive_id, id), body.to_vec());
                (StatusCode::OK, Json(updated)).into_response()
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/{*tail}",
            get(|State(s): State<SyncMock>, Path((drive_id, tail)): Path<(String, String)>| async move {
                s.requests.lock().await.push(("GET".into(), format!("/v1.0/drives/{drive_id}/{tail}"), None));
                if tail.starts_with("root:/") && tail.ends_with(":/children") {
                    let rel = tail.trim_start_matches("root:/").trim_end_matches(":/children");
                    let want = format!("/drive/root:/{}", rel.trim_matches('/'));
                    let items = s.items.lock().await;
                    let out: Vec<Value> = items
                        .iter()
                        .filter(|((d, _), v)| {
                            d == &drive_id
                                && v["parentReference"]["path"]
                                    .as_str()
                                    .unwrap_or("")
                                    .eq_ignore_ascii_case(&want)
                        })
                        .map(|(_, v)| v.clone())
                        .collect();
                    return (StatusCode::OK, Json(json!({ "value": out }))).into_response();
                }
                if tail.starts_with("root:/") && tail.ends_with(':') {
                    let rel = tail.trim_start_matches("root:/").trim_end_matches(':');
                    let norm = format!("/{}", rel.trim_matches('/'));
                    let items = s.items.lock().await;
                    for ((d, _), v) in items.iter() {
                        if d != &drive_id {
                            continue;
                        }
                        let pp = v["parentReference"]["path"].as_str().unwrap_or("");
                        let ptail = pp.split("root:").nth(1).unwrap_or("").trim_end_matches('/');
                        let name = v["name"].as_str().unwrap_or("");
                        let full = if ptail.is_empty() {
                            format!("/{name}")
                        } else {
                            format!("{ptail}/{name}")
                        };
                        if full.eq_ignore_ascii_case(&norm) {
                            return (StatusCode::OK, Json(v.clone())).into_response();
                        }
                    }
                }
                (StatusCode::NOT_FOUND, Json(json!({ "error": { "code": "itemNotFound" } }))).into_response()
            })
            // Intentionally IGNORES If-Match/If-None-Match on PUT /root:/...:/content to mirror Graph's undocumented PUT behavior!
            .put(|State(s): State<SyncMock>, Path((drive_id, tail)): Path<(String, String)>, headers: HeaderMap, body: Bytes| async move {
                let if_match = headers
                    .get("if-match")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                let req_num = {
                    let mut reqs = s.requests.lock().await;
                    reqs.push(("PUT".into(), format!("/v1.0/drives/{drive_id}/{tail}"), if_match));
                    reqs.len()
                };
                if tail.starts_with("root:/") && tail.ends_with(":/content") {
                    let name = tail.trim_start_matches("root:/").trim_end_matches(":/content");
                    let mut items = s.items.lock().await;
                    let existing_key = items
                        .iter()
                        .find(|((d, _), v)| {
                            d == &drive_id
                                && v["name"].as_str().unwrap_or("").eq_ignore_ascii_case(name)
                                && v["parentReference"]["path"].as_str().unwrap_or("").ends_with("root:")
                        })
                        .map(|(k, _)| k.clone());
                    let id = existing_key
                        .as_ref()
                        .map(|(_, id)| id.clone())
                        .unwrap_or_else(|| format!("item-new-{}", items.len()));
                    let new_etag = format!("\"etag-up-{req_num}\"");
                    let updated = json!({
                        "id": id,
                        "name": name,
                        "size": body.len(),
                        "eTag": new_etag,
                        "cTag": format!("\"ctag-up-{req_num}\""),
                        "file": { "mimeType": "text/markdown" },
                        "parentReference": { "driveId": drive_id, "id": "root", "path": "/drive/root:" }
                    });
                    items.insert((drive_id.clone(), id.clone()), updated.clone());
                    s.contents.lock().await.insert((drive_id, id), body.to_vec());
                    return (StatusCode::OK, Json(updated)).into_response();
                }
                StatusCode::NOT_FOUND.into_response()
            })
            .post(|State(s): State<SyncMock>, Path((drive_id, tail)): Path<(String, String)>, Json(body): Json<Value>| async move {
                let req_num = {
                    let mut reqs = s.requests.lock().await;
                    reqs.push(("POST".into(), format!("/v1.0/drives/{drive_id}/{tail}"), None));
                    reqs.len()
                };
                if tail.starts_with("root:/") && tail.ends_with(":/createUploadSession") {
                    let name = tail
                        .trim_start_matches("root:/")
                        .trim_end_matches(":/createUploadSession")
                        .to_string();
                    let behavior = body["item"]["@microsoft.graph.conflictBehavior"]
                        .as_str()
                        .unwrap_or("replace");
                    let items = s.items.lock().await;
                    let existing = items
                        .iter()
                        .find(|((d, _), v)| {
                            d == &drive_id
                                && v["name"].as_str().unwrap_or("").eq_ignore_ascii_case(&name)
                                && v["parentReference"]["path"].as_str().unwrap_or("").ends_with("root:")
                        })
                        .map(|((_, id), _)| id.clone());
                    if existing.is_some() && behavior == "fail" {
                        return (
                            StatusCode::CONFLICT,
                            Json(json!({ "error": { "code": "nameAlreadyExists", "message": "exists" } })),
                        )
                            .into_response();
                    }
                    let sid = format!("sess-{req_num}");
                    s.sessions.lock().await.insert(sid.clone(), (drive_id, existing, name));
                    let upload_url = format!("{}/upload-session/{sid}", s.base_url.trim_end_matches("/v1.0"));
                    return (StatusCode::OK, Json(json!({ "uploadUrl": upload_url, "expirationDateTime": "2026-11-20T09:00:00Z" }))).into_response();
                }
                StatusCode::NOT_FOUND.into_response()
            }),
        )
        .layer(axum::extract::DefaultBodyLimit::max(16 * 1024 * 1024))
        .with_state(st.clone());

    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let (cfg, graph, storage) = make_graph_and_storage(&base_url);

    // S5: Cache /Finance/secret.md, then online resolve_item_for_write("secret.md") MUST fail with 0 deletes!
    let fin_items = cli::fetch_folder_items(&graph, &storage, "drv-me", Some("/Finance"), 100, false, false, false)
        .await
        .unwrap();
    assert_eq!(fin_items.len(), 1);
    assert!(resolver::resolve_item_for_write(&graph, &storage, "drv-me", "secret.md").await.is_err());
    assert!(
        st.items.lock().await.contains_key(&("drv-me".into(), "item-secret".into())),
        "unrelated /Finance/secret.md must remain untouched"
    );
    // Exact path and exact ID succeed
    assert_eq!(
        resolver::resolve_item_for_write(&graph, &storage, "drv-me", "/Finance/secret.md")
            .await
            .unwrap()
            .id,
        "item-secret"
    );
    assert_eq!(
        resolver::resolve_item_for_write(&graph, &storage, "drv-me", "item-secret")
            .await
            .unwrap()
            .id,
        "item-secret"
    );

    // S3: Full root listing caches 3 items; limit=1 listing MUST preserve all 3 cached items
    let all_root = cli::fetch_folder_items(&graph, &storage, "drv-me", None, 100, false, false, false)
        .await
        .unwrap();
    assert_eq!(all_root.len(), 3);
    let partial_root = cli::fetch_folder_items(&graph, &storage, "drv-me", None, 1, false, false, false)
        .await
        .unwrap();
    assert_eq!(partial_root.len(), 1);
    let offline_root = cli::fetch_folder_items(&graph, &storage, "drv-me", None, 100, false, true, false)
        .await
        .unwrap();
    assert_eq!(offline_root.len(), 3, "partial limit=1 fetch must not wipe cached siblings");

    // S6: Download doc.md (etag-v1), server edits to VERSION_2_EXTERNAL (etag-v2),
    // re-uploading edited downloaded file sends If-Match: "etag-v1" -> 412 conflict, preserves both remote and local!
    let tmp = tempdir().unwrap();
    let doc_item = resolver::resolve_item(&graph, &storage, "drv-me", "item-doc", false)
        .await
        .unwrap();
    let mut dl = graph
        .download_item_to_file("drv-me", "item-doc", Some("doc.md"), Some("text/markdown"), None, tmp.path())
        .await
        .unwrap();
    cli::record_download_baseline(&storage, &doc_item, &mut dl);
    assert_eq!(dl.e_tag.as_deref(), Some("\"etag-v1\""));

    // External server edit -> v2
    {
        let mut items = st.items.lock().await;
        let entry = items.get_mut(&("drv-me".into(), "item-doc".into())).unwrap();
        entry["eTag"] = json!("\"etag-v2\"");
        entry["size"] = json!(18);
        st.contents
            .lock()
            .await
            .insert(("drv-me".into(), "item-doc".into()), b"VERSION_2_EXTERNAL".to_vec());
    }

    // Edit downloaded file locally and attempt default re-upload (no --conflict replace)
    // Note: this mock IGNORES If-Match on PUT /content; client must block via up-front baseline comparison!
    std::fs::write(&dl.path, b"STALE_LOCAL_EDIT").unwrap();
    let auto_opts = cli::resolve_upload_options_for_local_file(
        &storage,
        "drv-me",
        None,
        Some(""),
        None,
        &dl.path,
        None,
        None,
    );
    assert_eq!(auto_opts.if_match.as_deref(), Some("\"etag-v1\""));
    assert_eq!(auto_opts.target_item_id.as_deref(), Some("item-doc"));
    let reqs_before_stale = st.requests.lock().await.len();
    let conflict_err = graph
        .upload_local_file_with_options("drv-me", None, None, &dl.path, &auto_opts)
        .await
        .expect_err("conditional upload with stale ETag must fail with 412 even when PUT ignores If-Match");
    assert!(
        format!("{conflict_err:#}").contains("412"),
        "expected 412 conflict error, got: {conflict_err:#}"
    );
    assert!(
        !st.requests.lock().await[reqs_before_stale..]
            .iter()
            .any(|(m, _, _)| m == "PUT" || m == "POST"),
        "stale baseline must abort before sending any PUT/POST write"
    );
    assert_eq!(
        st.contents.lock().await.get(&("drv-me".into(), "item-doc".into())).unwrap().as_slice(),
        b"VERSION_2_EXTERNAL",
        "latest remote content must be preserved on conflict"
    );
    assert_eq!(
        std::fs::read(&dl.path).unwrap().as_slice(),
        b"STALE_LOCAL_EDIT",
        "local file must be preserved on conflict"
    );

    // Zero-length conditional upload against already-changed server MUST ALSO fail up front (0 PUTs)
    std::fs::write(&dl.path, b"").unwrap();
    let reqs_before_zero_stale = st.requests.lock().await.len();
    let zero_conflict = graph
        .upload_local_file_with_options("drv-me", None, None, &dl.path, &auto_opts)
        .await
        .expect_err("zero-length conditional upload with stale ETag must fail with 412");
    assert!(format!("{zero_conflict:#}").contains("412"));
    assert!(
        !st.requests.lock().await[reqs_before_zero_stale..]
            .iter()
            .any(|(m, _, _)| m == "PUT" || m == "POST")
    );
    assert_eq!(
        st.contents.lock().await.get(&("drv-me".into(), "item-doc".into())).unwrap().as_slice(),
        b"VERSION_2_EXTERNAL"
    );

    // Race between preflight GET and POST /items/{id}/createUploadSession:
    // documented createUploadSession If-Match check rejects with 412 and uploads 0 chunks!
    std::fs::write(&dl.path, b"LOCAL_EDIT_V2").unwrap();
    *st.mutate_doc_etag_after_next_get.lock().await = Some("\"etag-v3-raced\"".into());
    let v2_opts = UploadOptions {
        conflict_behavior: None,
        if_match: Some("\"etag-v2\"".into()),
        expected_c_tag: None,
        target_item_id: Some("item-doc".into()),
    };
    let reqs_before_race = st.requests.lock().await.len();
    let race_err = graph
        .upload_local_file_with_options("drv-me", None, None, &dl.path, &v2_opts)
        .await
        .expect_err("createUploadSession must reject raced ETag with 412");
    assert!(format!("{race_err:#}").contains("412"));
    let race_reqs = st.requests.lock().await[reqs_before_race..].to_vec();
    assert!(
        race_reqs.iter().any(|(m, p, im)| m == "POST"
            && p == "/v1.0/drives/drv-me/items/item-doc/createUploadSession"
            && im.as_deref() == Some("\"etag-v2\"")),
        "expected documented POST /items/item-doc/createUploadSession with If-Match, got: {race_reqs:?}"
    );
    assert!(
        !race_reqs.iter().any(|(m, _, _)| m == "PUT"),
        "failed session creation must not upload any chunks"
    );
    assert_eq!(
        st.contents.lock().await.get(&("drv-me".into(), "item-doc".into())).unwrap().as_slice(),
        b"VERSION_RACED_BEFORE_SESSION"
    );

    // Clean non-empty conditional upload uses documented POST /items/{id}/createUploadSession with If-Match
    let v3_opts = UploadOptions {
        conflict_behavior: None,
        if_match: Some("\"etag-v3-raced\"".into()),
        expected_c_tag: None,
        target_item_id: Some("item-doc".into()),
    };
    let reqs_before_clean = st.requests.lock().await.len();
    let clean_up = graph
        .upload_local_file_with_options("drv-me", None, None, &dl.path, &v3_opts)
        .await
        .unwrap();
    let clean_reqs = st.requests.lock().await[reqs_before_clean..].to_vec();
    assert!(
        clean_reqs.iter().any(|(m, p, im)| m == "POST"
            && p == "/v1.0/drives/drv-me/items/item-doc/createUploadSession"
            && im.as_deref() == Some("\"etag-v3-raced\"")),
        "clean conditional upload must use createUploadSession with If-Match: {clean_reqs:?}"
    );
    assert_eq!(
        st.contents.lock().await.get(&("drv-me".into(), "item-doc".into())).unwrap().as_slice(),
        b"LOCAL_EDIT_V2"
    );

    // Clean zero-length conditional upload uses exact item-ID PUT /items/{id}/content (valid for 0-byte files)
    std::fs::write(&dl.path, b"").unwrap();
    let zero_ok_opts = UploadOptions {
        conflict_behavior: None,
        if_match: clean_up.e_tag.clone(),
        expected_c_tag: None,
        target_item_id: Some("item-doc".into()),
    };
    let reqs_before_zero_ok = st.requests.lock().await.len();
    let zero_up = graph
        .upload_local_file_with_options("drv-me", None, None, &dl.path, &zero_ok_opts)
        .await
        .unwrap();
    let zero_ok_reqs = st.requests.lock().await[reqs_before_zero_ok..].to_vec();
    assert!(
        zero_ok_reqs
            .iter()
            .any(|(m, p, im)| m == "PUT"
                && p == "/v1.0/drives/drv-me/items/item-doc/content"
                && im.as_deref() == clean_up.e_tag.as_deref()),
        "zero-length conditional upload must use exact item-ID PUT /content with If-Match: {zero_ok_reqs:?}"
    );
    assert_eq!(zero_up.size, 0);
    assert_eq!(
        st.contents.lock().await.get(&("drv-me".into(), "item-doc".into())).unwrap().as_slice(),
        b""
    );

    // Explicit replace workflow MUST still work!
    std::fs::write(&dl.path, b"EXPLICIT_REPLACE_DATA").unwrap();
    let replace_opts = cli::resolve_upload_options_for_local_file(
        &storage,
        "drv-me",
        None,
        Some(""),
        None,
        &dl.path,
        None,
        Some(ConflictBehavior::Replace),
    );
    assert_eq!(
        replace_opts,
        UploadOptions {
            conflict_behavior: Some(ConflictBehavior::Replace),
            if_match: None,
            expected_c_tag: None,
            target_item_id: None,
        }
    );
    let replaced = graph
        .upload_local_file_with_options("drv-me", None, None, &dl.path, &replace_opts)
        .await
        .unwrap();
    assert_eq!(replaced.id, "item-doc");
    assert_eq!(
        st.contents.lock().await.get(&("drv-me".into(), "item-doc".into())).unwrap().as_slice(),
        b"EXPLICIT_REPLACE_DATA"
    );

    // WI-1: CLI `upload --if-match ""` and MCP `upload_file(if_match: "")` must fail before sending PUT and keep remote content unchanged.
    {
        let empty_etag_file = tmp.path().join("empty-etag-upload.md");
        std::fs::write(&empty_etag_file, b"SHOULD_NOT_OVERWRITE_CLI").unwrap();
        let reqs_before_empty_cli = st.requests.lock().await.len();
        let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_sharepoint"));
        cmd.kill_on_drop(true);
        for (k, _) in std::env::vars_os() {
            let u = k.to_string_lossy().to_ascii_uppercase();
            if ["SHAREPOINT_", "ONEDRIVE_", "TEAMS_", "AZURE_"]
                .iter()
                .any(|p| u.starts_with(p))
            {
                cmd.env_remove(k);
            }
        }
        let cli_out = cmd
            .env("SHAREPOINT_GRAPH_BASE_URL", &base_url)
            .env("SHAREPOINT_ACCESS_TOKEN", "sync-empty-etag-token")
            .env("SHAREPOINT_DISABLE_KEYRING", "1")
            .env("SHAREPOINT_SHARE_M365_AUTH", "0")
            .env("SHAREPOINT_AUTO_DAEMON", "0")
            .env("SHAREPOINT_CONFIG_DIR", tmp.path().join("cfg_empty_etag"))
            .env("SHAREPOINT_DATA_DIR", tmp.path().join("data_empty_etag"))
            .env("NO_PROXY", "127.0.0.1,localhost")
            .args([
                "upload",
                &empty_etag_file.to_string_lossy(),
                "--drive",
                "drv-me",
                "--item-id",
                "item-doc",
                "--if-match",
                "",
                "--json",
            ])
            .output()
            .await
            .unwrap();
        assert_eq!(
            cli_out.status.code(),
            Some(1),
            "CLI upload --if-match \"\" must exit 1, stderr={}",
            String::from_utf8_lossy(&cli_out.stderr)
        );
        assert!(
            String::from_utf8_lossy(&cli_out.stderr).contains("If-Match requires a non-empty ETag"),
            "unexpected stderr: {}",
            String::from_utf8_lossy(&cli_out.stderr)
        );
        assert!(
            !st.requests.lock().await[reqs_before_empty_cli..]
                .iter()
                .any(|(m, _, _)| m == "PUT" || m == "POST"),
            "CLI upload --if-match \"\" must not send PUT/POST"
        );
        assert_eq!(
            st.contents.lock().await.get(&("drv-me".into(), "item-doc".into())).unwrap().as_slice(),
            b"EXPLICIT_REPLACE_DATA",
            "CLI upload --if-match \"\" must preserve remote content"
        );

        let reqs_before_empty_mcp = st.requests.lock().await.len();
        let mcp_empty = mcp::handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            &json!({
                "jsonrpc": "2.0",
                "id": 91,
                "method": "tools/call",
                "params": {
                    "name": "upload_file",
                    "arguments": {
                        "drive": "drv-me",
                        "item_id": "item-doc",
                        "file_name": "doc.md",
                        "content_text": "SHOULD_NOT_OVERWRITE_MCP",
                        "if_match": ""
                    }
                }
            })
            .to_string(),
        )
        .await
        .unwrap();
        assert_eq!(mcp_empty["result"]["isError"], true);
        assert!(
            mcp_empty["result"]["content"][0]["text"]
                .as_str()
                .unwrap_or("")
                .contains("If-Match requires a non-empty ETag"),
            "unexpected MCP error: {mcp_empty}"
        );
        assert!(
            !st.requests.lock().await[reqs_before_empty_mcp..]
                .iter()
                .any(|(m, _, _)| m == "PUT" || m == "POST"),
            "MCP upload_file(if_match:\"\") must not send PUT/POST"
        );
        assert_eq!(
            st.contents.lock().await.get(&("drv-me".into(), "item-doc".into())).unwrap().as_slice(),
            b"EXPLICIT_REPLACE_DATA",
            "MCP upload_file(if_match:\"\") must preserve remote content"
        );
    }

    // S4: Offline sites search sends 0 network requests + unfollow persists false
    let sites1 = cli::fetch_sites(&graph, &storage, None, 50, false, false, false).await.unwrap();
    assert_eq!(sites1.len(), 2);
    st.followed_sites.lock().await.clear();
    let sites2 = cli::fetch_sites(&graph, &storage, None, 50, true, false, false).await.unwrap();
    assert!(sites2.iter().all(|s| !s.is_followed));

    let reqs_before = st.requests.lock().await.len();
    let offline_eng = cli::fetch_sites(&graph, &storage, Some("eng"), 50, false, true, false)
        .await
        .unwrap();
    assert_eq!(st.requests.lock().await.len(), reqs_before, "offline sites search must make 0 network requests");
    assert_eq!(offline_eng.len(), 1);
    assert_eq!(offline_eng[0].id, "site-eng");
    assert!(!offline_eng[0].is_followed);

    server.abort();
}

#[tokio::test]
async fn regression_sharepoint_sp_02_small_and_large_upload_with_item_id_target_exact_item() {
    use sha2::{Digest, Sha256};
    use std::collections::HashMap;
    use tokio::process::Command;

    #[derive(Clone)]
    struct Sp02State {
        base_url: String,
        items: Arc<Mutex<HashMap<(String, String), Value>>>,
        contents: Arc<Mutex<HashMap<(String, String), Vec<u8>>>>,
        sessions: Arc<Mutex<HashMap<String, (String, String, Vec<u8>)>>>,
        requests: Arc<Mutex<Vec<(String, String, Option<String>)>>>,
    }

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}/v1.0");

    let mut init_items = HashMap::new();
    init_items.insert(
        ("drv-me".to_string(), "item-root-same".to_string()),
        json!({
            "id": "item-root-same",
            "name": "replacement.txt",
            "size": 21,
            "eTag": "\"etag-root-1\"",
            "file": { "mimeType": "text/plain" },
            "parentReference": { "driveId": "drv-me", "id": "root", "path": "/drive/root:" }
        }),
    );
    init_items.insert(
        ("drv-me".to_string(), "item-nested".to_string()),
        json!({
            "id": "item-nested",
            "name": "replacement.txt",
            "size": 15,
            "eTag": "\"etag-nested-1\"",
            "file": { "mimeType": "text/plain" },
            "parentReference": { "driveId": "drv-me", "id": "fld-sub", "path": "/drive/root:/Sub" }
        }),
    );

    let root_original = b"ORIGINAL_ROOT_CONTENT".to_vec();
    let root_original_hash = hex::encode(Sha256::digest(&root_original));
    let mut init_contents = HashMap::new();
    init_contents.insert(
        ("drv-me".to_string(), "item-root-same".to_string()),
        root_original.clone(),
    );
    init_contents.insert(
        ("drv-me".to_string(), "item-nested".to_string()),
        b"OLD_NESTED_DATA".to_vec(),
    );

    let st = Sp02State {
        base_url: base_url.clone(),
        items: Arc::new(Mutex::new(init_items)),
        contents: Arc::new(Mutex::new(init_contents)),
        sessions: Arc::new(Mutex::new(HashMap::new())),
        requests: Arc::new(Mutex::new(Vec::new())),
    };

    let router = Router::new()
        .route(
            "/v1.0/me/drive",
            get(|State(s): State<Sp02State>| async move {
                s.requests
                    .lock()
                    .await
                    .push(("GET".into(), "/v1.0/me/drive".into(), None));
                Json(json!({
                    "id": "drv-me",
                    "name": "OneDrive",
                    "driveType": "business"
                }))
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/items/{item_id}",
            get(|State(s): State<Sp02State>, Path((drive_id, item_id)): Path<(String, String)>| async move {
                s.requests.lock().await.push((
                    "GET".into(),
                    format!("/v1.0/drives/{drive_id}/items/{item_id}"),
                    None,
                ));
                match s.items.lock().await.get(&(drive_id, item_id)).cloned() {
                    Some(v) => (StatusCode::OK, Json(v)).into_response(),
                    None => (
                        StatusCode::NOT_FOUND,
                        Json(json!({ "error": { "code": "itemNotFound", "message": "Item does not exist" } })),
                    )
                        .into_response(),
                }
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/items/{item_id}/content",
            put(|State(s): State<Sp02State>, Path((drive_id, item_id)): Path<(String, String)>, headers: HeaderMap, body: Bytes| async move {
                let if_match = headers.get("if-match").and_then(|v| v.to_str().ok()).map(str::to_string);
                let req_num = {
                    let mut reqs = s.requests.lock().await;
                    reqs.push((
                        "PUT".into(),
                        format!("/v1.0/drives/{drive_id}/items/{item_id}/content"),
                        if_match,
                    ));
                    reqs.len()
                };
                let mut items = s.items.lock().await;
                let Some(existing) = items.get(&(drive_id.clone(), item_id.clone())).cloned() else {
                    return (
                        StatusCode::NOT_FOUND,
                        Json(json!({ "error": { "code": "itemNotFound", "message": "Item does not exist" } })),
                    )
                        .into_response();
                };
                let mut updated = existing;
                updated["size"] = json!(body.len());
                updated["eTag"] = json!(format!("\"etag-put-{req_num}\""));
                items.insert((drive_id.clone(), item_id.clone()), updated.clone());
                s.contents.lock().await.insert((drive_id, item_id), body.to_vec());
                (StatusCode::OK, Json(updated)).into_response()
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/items/{item_id}/createUploadSession",
            post(|State(s): State<Sp02State>, Path((drive_id, item_id)): Path<(String, String)>, headers: HeaderMap| async move {
                let if_match = headers.get("if-match").and_then(|v| v.to_str().ok()).map(str::to_string);
                let req_num = {
                    let mut reqs = s.requests.lock().await;
                    reqs.push((
                        "POST".into(),
                        format!("/v1.0/drives/{drive_id}/items/{item_id}/createUploadSession"),
                        if_match.clone(),
                    ));
                    reqs.len()
                };
                let items = s.items.lock().await;
                let Some(existing) = items.get(&(drive_id.clone(), item_id.clone())) else {
                    return (
                        StatusCode::NOT_FOUND,
                        Json(json!({ "error": { "code": "itemNotFound", "message": "Item does not exist" } })),
                    )
                        .into_response();
                };
                if let Some(ref expected) = if_match {
                    if existing["eTag"].as_str().unwrap_or("") != expected {
                        return (
                            StatusCode::PRECONDITION_FAILED,
                            Json(json!({ "error": { "code": "resourceModified" } })),
                        )
                            .into_response();
                    }
                }
                let sid = format!("sess-{req_num}");
                s.sessions
                    .lock()
                    .await
                    .insert(sid.clone(), (drive_id, item_id, Vec::new()));
                let upload_url = format!("{}/upload-session/{sid}", s.base_url.trim_end_matches("/v1.0"));
                (StatusCode::OK, Json(json!({ "uploadUrl": upload_url }))).into_response()
            }),
        )
        .route(
            "/upload-session/{sid}",
            put(|State(s): State<Sp02State>, Path(sid): Path<String>, headers: HeaderMap, body: Bytes| async move {
                let req_num = {
                    let mut reqs = s.requests.lock().await;
                    reqs.push(("PUT".into(), format!("/upload-session/{sid}"), None));
                    reqs.len()
                };
                let range = headers
                    .get("content-range")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                let total: usize = range
                    .split('/')
                    .nth(1)
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let mut sessions = s.sessions.lock().await;
                let Some((drive_id, item_id, buf)) = sessions.get_mut(&sid) else {
                    return StatusCode::NOT_FOUND.into_response();
                };
                buf.extend_from_slice(&body);
                if buf.len() >= total && total > 0 {
                    let final_bytes = buf.clone();
                    let d = drive_id.clone();
                    let id = item_id.clone();
                    drop(sessions);
                    let mut items = s.items.lock().await;
                    let mut updated = items.get(&(d.clone(), id.clone())).cloned().unwrap();
                    updated["size"] = json!(final_bytes.len());
                    updated["eTag"] = json!(format!("\"etag-sess-{req_num}\""));
                    items.insert((d.clone(), id.clone()), updated.clone());
                    s.contents.lock().await.insert((d, id), final_bytes);
                    (StatusCode::OK, Json(updated)).into_response()
                } else {
                    (StatusCode::ACCEPTED, Json(json!({ "nextExpectedRanges": [] }))).into_response()
                }
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/{*tail}",
            put(|State(s): State<Sp02State>, Path((drive_id, tail)): Path<(String, String)>, body: Bytes| async move {
                let req_num = {
                    let mut reqs = s.requests.lock().await;
                    reqs.push(("PUT".into(), format!("/v1.0/drives/{drive_id}/{tail}"), None));
                    reqs.len()
                };
                if tail.starts_with("root:/") && tail.ends_with(":/content") {
                    let name = tail.trim_start_matches("root:/").trim_end_matches(":/content");
                    let mut items = s.items.lock().await;
                    let existing_id = items
                        .iter()
                        .find(|((d, _), v)| {
                            d == &drive_id
                                && v["name"].as_str().unwrap_or("").eq_ignore_ascii_case(name)
                                && v["parentReference"]["path"].as_str().unwrap_or("").ends_with("root:")
                        })
                        .map(|((_, id), _)| id.clone());
                    let id = existing_id.unwrap_or_else(|| format!("item-new-{req_num}"));
                    let updated = json!({
                        "id": id,
                        "name": name,
                        "size": body.len(),
                        "eTag": format!("\"etag-path-{req_num}\""),
                        "file": { "mimeType": "text/plain" },
                        "parentReference": { "driveId": drive_id, "id": "root", "path": "/drive/root:" }
                    });
                    items.insert((drive_id.clone(), id.clone()), updated.clone());
                    s.contents.lock().await.insert((drive_id, id), body.to_vec());
                    return (StatusCode::OK, Json(updated)).into_response();
                }
                StatusCode::NOT_FOUND.into_response()
            }),
        )
        .layer(axum::extract::DefaultBodyLimit::max(16 * 1024 * 1024))
        .with_state(st.clone());

    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let profile = tempdir().unwrap();
    let workdir = tempdir().unwrap();
    let small_file = workdir.path().join("replacement.txt");
    let small_payload = b"0123456789abcdef";
    assert_eq!(small_payload.len(), 16);
    std::fs::write(&small_file, small_payload).unwrap();

    let run_cli = |args: Vec<String>| {
        let profile_path = profile.path().to_path_buf();
        let base_url = base_url.clone();
        async move {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_sharepoint"));
            cmd.kill_on_drop(true);
            for (k, _) in std::env::vars_os() {
                let u = k.to_string_lossy().to_ascii_uppercase();
                if ["SHAREPOINT_", "ONEDRIVE_", "TEAMS_", "AZURE_"]
                    .iter()
                    .any(|p| u.starts_with(p))
                {
                    cmd.env_remove(k);
                }
            }
            cmd.env("SHAREPOINT_GRAPH_BASE_URL", &base_url)
                .env("SHAREPOINT_ACCESS_TOKEN", "sp02-test-token")
                .env("SHAREPOINT_DISABLE_KEYRING", "1")
                .env("SHAREPOINT_SHARE_M365_AUTH", "0")
                .env("SHAREPOINT_AUTO_DAEMON", "0")
                .env("SHAREPOINT_CONFIG_DIR", profile_path.join("config"))
                .env("SHAREPOINT_DATA_DIR", profile_path.join("data"))
                .env("NO_PROXY", "127.0.0.1,localhost")
                .args(&args)
                .output()
                .await
                .unwrap()
        }
    };

    // 1. 16-byte upload with --item-id item-nested (without --if-match) MUST update item-nested only!
    let out_small = run_cli(vec![
        "upload".into(),
        small_file.to_string_lossy().into_owned(),
        "--drive".into(),
        "me".into(),
        "--item-id".into(),
        "item-nested".into(),
        "--json".into(),
    ])
    .await;
    assert!(
        out_small.status.success(),
        "small --item-id upload failed: stderr={}",
        String::from_utf8_lossy(&out_small.stderr)
    );
    let small_json: Value = serde_json::from_slice(&out_small.stdout).unwrap();
    assert_eq!(small_json["id"], "item-nested", "small upload must return nested item ID");
    assert_eq!(
        st.contents
            .lock()
            .await
            .get(&("drv-me".into(), "item-nested".into()))
            .unwrap()
            .as_slice(),
        small_payload,
        "nested item content must be updated by 16-byte upload"
    );
    let root_after_small = st
        .contents
        .lock()
        .await
        .get(&("drv-me".into(), "item-root-same".into()))
        .unwrap()
        .clone();
    assert_eq!(
        hex::encode(Sha256::digest(&root_after_small)),
        root_original_hash,
        "same-named root file hash must remain unchanged"
    );
    assert!(
        st.requests
            .lock()
            .await
            .iter()
            .any(|(m, p, _)| m == "PUT" && p == "/v1.0/drives/drv-me/items/item-nested/content"),
        "16-byte --item-id upload must PUT /drives/drv-me/items/item-nested/content"
    );

    // 2. >= 4 MiB upload with --item-id item-nested uses ID-targeted createUploadSession on same item
    let large_file = workdir.path().join("replacement.txt");
    let large_payload = vec![b'Z'; 4 * 1024 * 1024 + 1024];
    std::fs::write(&large_file, &large_payload).unwrap();
    let reqs_before_large = st.requests.lock().await.len();
    let out_large = run_cli(vec![
        "upload".into(),
        large_file.to_string_lossy().into_owned(),
        "--drive".into(),
        "me".into(),
        "--item-id".into(),
        "item-nested".into(),
        "--json".into(),
    ])
    .await;
    assert!(
        out_large.status.success(),
        "large --item-id upload failed: stderr={}",
        String::from_utf8_lossy(&out_large.stderr)
    );
    let large_json: Value = serde_json::from_slice(&out_large.stdout).unwrap();
    assert_eq!(large_json["id"], "item-nested");
    assert_eq!(
        st.contents
            .lock()
            .await
            .get(&("drv-me".into(), "item-nested".into()))
            .unwrap()
            .len(),
        large_payload.len()
    );
    assert!(
        st.requests.lock().await[reqs_before_large..]
            .iter()
            .any(|(m, p, _)| m == "POST"
                && p == "/v1.0/drives/drv-me/items/item-nested/createUploadSession"),
        "large --item-id upload must use items/item-nested/createUploadSession"
    );
    let root_after_large = st
        .contents
        .lock()
        .await
        .get(&("drv-me".into(), "item-root-same".into()))
        .unwrap()
        .clone();
    assert_eq!(
        hex::encode(Sha256::digest(&root_after_large)),
        root_original_hash,
        "root file hash must remain unchanged after large --item-id upload"
    );

    // 3. Regression control: nonexistent --item-id fails with 404 and does NOT create path-based file
    std::fs::write(&small_file, b"missing-id-bytes").unwrap();
    let items_before_missing = st.items.lock().await.len();
    let reqs_before_missing = st.requests.lock().await.len();
    let out_missing = run_cli(vec![
        "upload".into(),
        small_file.to_string_lossy().into_owned(),
        "--drive".into(),
        "me".into(),
        "--item-id".into(),
        "item-does-not-exist".into(),
        "--json".into(),
    ])
    .await;
    assert!(!out_missing.status.success(), "missing --item-id must fail");
    assert!(
        String::from_utf8_lossy(&out_missing.stderr).contains("404"),
        "expected 404 error, got: {}",
        String::from_utf8_lossy(&out_missing.stderr)
    );
    assert_eq!(
        st.items.lock().await.len(),
        items_before_missing,
        "missing --item-id must not create any new item"
    );
    assert!(
        !st.requests.lock().await[reqs_before_missing..]
            .iter()
            .any(|(m, p, _)| m == "PUT" && p.contains("root:/")),
        "missing --item-id must not fall back to path-based PUT"
    );

    // 4. Regression control: upload without --item-id still uses filename/parent path policy
    let fresh_file = workdir.path().join("fresh.txt");
    std::fs::write(&fresh_file, b"fresh-content").unwrap();
    let out_fresh = run_cli(vec![
        "upload".into(),
        fresh_file.to_string_lossy().into_owned(),
        "--drive".into(),
        "me".into(),
        "--json".into(),
    ])
    .await;
    assert!(out_fresh.status.success());
    let fresh_json: Value = serde_json::from_slice(&out_fresh.stdout).unwrap();
    assert_eq!(fresh_json["name"], "fresh.txt");
    assert!(
        st.requests
            .lock()
            .await
            .iter()
            .any(|(m, p, _)| m == "PUT" && p == "/v1.0/drives/drv-me/root:/fresh.txt:/content"),
        "upload without --item-id must still use path-based PUT"
    );

    // 5. Regression control: conditional --item-id + --if-match keeps upload-session preflight branch
    let current_nested_etag = st
        .items
        .lock()
        .await
        .get(&("drv-me".into(), "item-nested".into()))
        .unwrap()["eTag"]
        .as_str()
        .unwrap()
        .to_string();
    let reqs_before_cond = st.requests.lock().await.len();
    let out_cond = run_cli(vec![
        "upload".into(),
        small_file.to_string_lossy().into_owned(),
        "--drive".into(),
        "me".into(),
        "--item-id".into(),
        "item-nested".into(),
        "--if-match".into(),
        current_nested_etag.clone(),
        "--json".into(),
    ])
    .await;
    assert!(
        out_cond.status.success(),
        "conditional --item-id + --if-match upload failed: {}",
        String::from_utf8_lossy(&out_cond.stderr)
    );
    assert!(
        st.requests.lock().await[reqs_before_cond..]
            .iter()
            .any(|(m, p, im)| m == "POST"
                && p == "/v1.0/drives/drv-me/items/item-nested/createUploadSession"
                && im.as_deref() == Some(current_nested_etag.as_str())),
        "conditional upload must continue using createUploadSession with If-Match"
    );

    server.abort();
}

#[tokio::test]
async fn regression_sharepoint_sp_05_copy_upload_preserves_original_download_baseline() {
    use std::collections::HashMap;
    use tokio::process::Command;

    #[derive(Clone)]
    struct Sp05State {
        base_url: String,
        items: Arc<Mutex<HashMap<(String, String), Value>>>,
        contents: Arc<Mutex<HashMap<(String, String), Vec<u8>>>>,
        sessions: Arc<Mutex<HashMap<String, (String, String, Vec<u8>)>>>,
        requests: Arc<Mutex<Vec<(String, String, Option<String>)>>>,
        next_seq: Arc<AtomicUsize>,
    }

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}/v1.0");

    let mut init_items = HashMap::new();
    init_items.insert(
        ("drv-me".into(), "fld-a".into()),
        json!({
            "id": "fld-a",
            "name": "FolderA",
            "size": 0,
            "folder": { "childCount": 1 },
            "parentReference": { "driveId": "drv-me", "id": "root", "path": "/drive/root:" }
        }),
    );
    init_items.insert(
        ("drv-me".into(), "fld-b".into()),
        json!({
            "id": "fld-b",
            "name": "FolderB",
            "size": 0,
            "folder": { "childCount": 0 },
            "parentReference": { "driveId": "drv-me", "id": "root", "path": "/drive/root:" }
        }),
    );
    init_items.insert(
        ("drv-me".into(), "item-spec-orig".into()),
        json!({
            "id": "item-spec-orig",
            "name": "spec.md",
            "size": 11,
            "eTag": "\"spec-v1\"",
            "cTag": "\"spec-ctag-v1\"",
            "file": { "mimeType": "text/markdown" },
            "parentReference": { "driveId": "drv-me", "id": "fld-a", "path": "/drive/root:/FolderA" }
        }),
    );

    let mut init_contents = HashMap::new();
    init_contents.insert(
        ("drv-me".into(), "item-spec-orig".into()),
        b"ORIGINAL_V1".to_vec(),
    );

    let st = Sp05State {
        base_url: base_url.clone(),
        items: Arc::new(Mutex::new(init_items)),
        contents: Arc::new(Mutex::new(init_contents)),
        sessions: Arc::new(Mutex::new(HashMap::new())),
        requests: Arc::new(Mutex::new(Vec::new())),
        next_seq: Arc::new(AtomicUsize::new(1)),
    };

    let router = Router::new()
        .route(
            "/v1.0/me/drive",
            get(|| async {
                Json(json!({
                    "id": "drv-me",
                    "name": "OneDrive",
                    "driveType": "business"
                }))
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}",
            get(|Path(drive_id): Path<String>| async move {
                Json(json!({
                    "id": drive_id,
                    "name": "OtherLib",
                    "driveType": "documentLibrary"
                }))
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/items/{item_id}",
            get(|State(s): State<Sp05State>, Path((drive_id, item_id)): Path<(String, String)>| async move {
                s.requests.lock().await.push((
                    "GET".into(),
                    format!("/v1.0/drives/{drive_id}/items/{item_id}"),
                    None,
                ));
                match s.items.lock().await.get(&(drive_id, item_id)).cloned() {
                    Some(v) => (StatusCode::OK, Json(v)).into_response(),
                    None => (
                        StatusCode::NOT_FOUND,
                        Json(json!({ "error": { "code": "itemNotFound" } })),
                    )
                        .into_response(),
                }
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/items/{item_id}/content",
            get(|State(s): State<Sp05State>, Path((drive_id, item_id)): Path<(String, String)>| async move {
                match s.contents.lock().await.get(&(drive_id, item_id)).cloned() {
                    Some(data) => ([(header::CONTENT_TYPE, "text/markdown")], data).into_response(),
                    None => StatusCode::NOT_FOUND.into_response(),
                }
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/items/{item_id}/createUploadSession",
            post(|State(s): State<Sp05State>, Path((drive_id, item_id)): Path<(String, String)>, headers: HeaderMap| async move {
                let if_match = headers.get("if-match").and_then(|v| v.to_str().ok()).map(str::to_string);
                s.requests.lock().await.push((
                    "POST".into(),
                    format!("/v1.0/drives/{drive_id}/items/{item_id}/createUploadSession"),
                    if_match.clone(),
                ));
                let items = s.items.lock().await;
                let Some(existing) = items.get(&(drive_id.clone(), item_id.clone())) else {
                    return (StatusCode::NOT_FOUND, Json(json!({ "error": { "code": "itemNotFound" } }))).into_response();
                };
                if let Some(ref expected) = if_match {
                    if existing["eTag"].as_str().unwrap_or("") != expected {
                        return (
                            StatusCode::PRECONDITION_FAILED,
                            Json(json!({ "error": { "code": "resourceModified" } })),
                        )
                            .into_response();
                    }
                }
                let seq = s.next_seq.fetch_add(1, Ordering::SeqCst);
                let sid = format!("sess-{seq}");
                s.sessions
                    .lock()
                    .await
                    .insert(sid.clone(), (drive_id, item_id, Vec::new()));
                let upload_url = format!("{}/upload-session/{sid}", s.base_url.trim_end_matches("/v1.0"));
                (StatusCode::OK, Json(json!({ "uploadUrl": upload_url }))).into_response()
            }),
        )
        .route(
            "/upload-session/{sid}",
            put(|State(s): State<Sp05State>, Path(sid): Path<String>, body: Bytes| async move {
                let Some((drive_id, item_id, _)) = s.sessions.lock().await.remove(&sid) else {
                    return StatusCode::NOT_FOUND.into_response();
                };
                let seq = s.next_seq.fetch_add(1, Ordering::SeqCst);
                let mut items = s.items.lock().await;
                let mut updated = items.get(&(drive_id.clone(), item_id.clone())).cloned().unwrap();
                updated["size"] = json!(body.len());
                updated["eTag"] = json!(format!("\"spec-sess-{seq}\""));
                updated["cTag"] = json!(format!("\"spec-ctag-sess-{seq}\""));
                items.insert((drive_id.clone(), item_id.clone()), updated.clone());
                s.contents.lock().await.insert((drive_id, item_id), body.to_vec());
                (StatusCode::OK, Json(updated)).into_response()
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/{*tail}",
            get(|State(s): State<Sp05State>, Path((drive_id, tail)): Path<(String, String)>| async move {
                s.requests.lock().await.push((
                    "GET".into(),
                    format!("/v1.0/drives/{drive_id}/{tail}"),
                    None,
                ));
                if tail.starts_with("root:/") && tail.ends_with(':') {
                    let rel = tail.trim_start_matches("root:/").trim_end_matches(':');
                    let norm = format!("/{}", rel.trim_matches('/'));
                    let items = s.items.lock().await;
                    for ((d, _), v) in items.iter() {
                        if d != &drive_id {
                            continue;
                        }
                        let pp = v["parentReference"]["path"].as_str().unwrap_or("");
                        let ptail = pp.split("root:").nth(1).unwrap_or("").trim_end_matches('/');
                        let name = v["name"].as_str().unwrap_or("");
                        let full = if ptail.is_empty() {
                            format!("/{name}")
                        } else {
                            format!("{ptail}/{name}")
                        };
                        if full.eq_ignore_ascii_case(&norm) {
                            return (StatusCode::OK, Json(v.clone())).into_response();
                        }
                    }
                }
                (StatusCode::NOT_FOUND, Json(json!({ "error": { "code": "itemNotFound" } }))).into_response()
            })
            .put(|State(s): State<Sp05State>, Path((drive_id, tail)): Path<(String, String)>, body: Bytes| async move {
                s.requests.lock().await.push((
                    "PUT".into(),
                    format!("/v1.0/drives/{drive_id}/{tail}"),
                    None,
                ));
                let seq = s.next_seq.fetch_add(1, Ordering::SeqCst);
                if tail.starts_with("items/") && tail.ends_with(":/content") {
                    let inner = tail.trim_start_matches("items/").trim_end_matches(":/content");
                    if let Some((parent_id, name)) = inner.split_once(":/") {
                        let mut items = s.items.lock().await;
                        let parent_path = items
                            .get(&(drive_id.clone(), parent_id.to_string()))
                            .map(|p| {
                                let pp = p["parentReference"]["path"].as_str().unwrap_or("/drive/root:");
                                let pname = p["name"].as_str().unwrap_or("");
                                if pp.ends_with("root:") {
                                    format!("/drive/root:/{pname}")
                                } else {
                                    format!("{pp}/{pname}")
                                }
                            })
                            .unwrap_or_else(|| "/drive/root:".into());
                        let existing_id = items
                            .iter()
                            .find(|((d, _), v)| {
                                d == &drive_id
                                    && v["parentReference"]["id"].as_str() == Some(parent_id)
                                    && v["name"].as_str().unwrap_or("").eq_ignore_ascii_case(name)
                            })
                            .map(|((_, id), _)| id.clone());
                        let id = existing_id.unwrap_or_else(|| format!("item-copy-{seq}"));
                        let updated = json!({
                            "id": id,
                            "name": name,
                            "size": body.len(),
                            "eTag": format!("\"etag-put-{seq}\""),
                            "cTag": format!("\"ctag-put-{seq}\""),
                            "file": { "mimeType": "text/markdown" },
                            "parentReference": { "driveId": drive_id, "id": parent_id, "path": parent_path }
                        });
                        items.insert((drive_id.clone(), id.clone()), updated.clone());
                        s.contents.lock().await.insert((drive_id, id), body.to_vec());
                        return (StatusCode::CREATED, Json(updated)).into_response();
                    }
                }
                if tail.starts_with("root:/") && tail.ends_with(":/content") {
                    let name = tail.trim_start_matches("root:/").trim_end_matches(":/content");
                    let id = format!("item-other-drv-{seq}");
                    let updated = json!({
                        "id": id,
                        "name": name,
                        "size": body.len(),
                        "eTag": format!("\"etag-other-{seq}\""),
                        "file": { "mimeType": "text/markdown" },
                        "parentReference": { "driveId": drive_id, "id": "root", "path": "/drive/root:" }
                    });
                    s.items.lock().await.insert((drive_id.clone(), id.clone()), updated.clone());
                    s.contents.lock().await.insert((drive_id, id), body.to_vec());
                    return (StatusCode::CREATED, Json(updated)).into_response();
                }
                StatusCode::NOT_FOUND.into_response()
            }),
        )
        .layer(axum::extract::DefaultBodyLimit::max(16 * 1024 * 1024))
        .with_state(st.clone());

    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let profile = tempdir().unwrap();
    let dl_dir = tempdir().unwrap();
    let run_cli = |args: Vec<String>| {
        let profile_path = profile.path().to_path_buf();
        let base_url = base_url.clone();
        async move {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_sharepoint"));
            cmd.kill_on_drop(true);
            for (k, _) in std::env::vars_os() {
                let u = k.to_string_lossy().to_ascii_uppercase();
                if ["SHAREPOINT_", "ONEDRIVE_", "TEAMS_", "AZURE_"]
                    .iter()
                    .any(|p| u.starts_with(p))
                {
                    cmd.env_remove(k);
                }
            }
            cmd.env("SHAREPOINT_GRAPH_BASE_URL", &base_url)
                .env("SHAREPOINT_ACCESS_TOKEN", "sp05-test-token")
                .env("SHAREPOINT_DISABLE_KEYRING", "1")
                .env("SHAREPOINT_SHARE_M365_AUTH", "0")
                .env("SHAREPOINT_AUTO_DAEMON", "0")
                .env("SHAREPOINT_CONFIG_DIR", profile_path.join("config"))
                .env("SHAREPOINT_DATA_DIR", profile_path.join("data"))
                .env("NO_PROXY", "127.0.0.1,localhost")
                .args(&args)
                .output()
                .await
                .unwrap()
        }
    };

    // 1. Download original /FolderA/spec.md
    let out_dl = run_cli(vec![
        "download".into(),
        "/FolderA/spec.md".into(),
        "--drive".into(),
        "me".into(),
        "--dir".into(),
        dl_dir.path().to_string_lossy().into_owned(),
        "--json".into(),
    ])
    .await;
    assert!(out_dl.status.success(), "download failed: {}", String::from_utf8_lossy(&out_dl.stderr));
    let dl_json: Value = serde_json::from_slice(&out_dl.stdout).unwrap();
    let local_path = std::path::PathBuf::from(dl_json["path"].as_str().unwrap());
    assert_eq!(dl_json["item_id"], "item-spec-orig");

    // 2. Edit locally and upload copy to /FolderB, then another copy by name, then to another drive
    std::fs::write(&local_path, b"LOCAL_STALE_EDIT").unwrap();
    let out_copy_folder = run_cli(vec![
        "upload".into(),
        local_path.to_string_lossy().into_owned(),
        "/FolderB".into(),
        "--drive".into(),
        "me".into(),
        "--json".into(),
    ])
    .await;
    assert!(out_copy_folder.status.success());
    let copy_json: Value = serde_json::from_slice(&out_copy_folder.stdout).unwrap();
    let copy_id = copy_json["id"].as_str().unwrap().to_string();
    assert_ne!(copy_id, "item-spec-orig");

    let out_copy_name = run_cli(vec![
        "upload".into(),
        local_path.to_string_lossy().into_owned(),
        "/FolderA".into(),
        "--name".into(),
        "spec-copy.md".into(),
        "--drive".into(),
        "me".into(),
        "--json".into(),
    ])
    .await;
    assert!(out_copy_name.status.success());

    let out_copy_drive = run_cli(vec![
        "upload".into(),
        local_path.to_string_lossy().into_owned(),
        "--drive".into(),
        "drv-other".into(),
        "--json".into(),
    ])
    .await;
    assert!(out_copy_drive.status.success());

    // 3. Server colleague edits original /FolderA/spec.md
    {
        let mut items = st.items.lock().await;
        let orig = items.get_mut(&("drv-me".into(), "item-spec-orig".into())).unwrap();
        orig["eTag"] = json!("\"spec-v2-server\"");
        orig["cTag"] = json!("\"spec-ctag-v2-server\"");
        orig["size"] = json!(23);
        st.contents
            .lock()
            .await
            .insert(("drv-me".into(), "item-spec-orig".into()), b"SERVER EDIT BY COLLEAGUE".to_vec());
    }

    // 4. Re-upload local file to original location /FolderA -> MUST fail with 412 conflict!
    let out_reupload = run_cli(vec![
        "upload".into(),
        local_path.to_string_lossy().into_owned(),
        "/FolderA".into(),
        "--drive".into(),
        "me".into(),
        "--json".into(),
    ])
    .await;
    assert!(
        !out_reupload.status.success(),
        "re-upload after copy upload must detect 412 conflict instead of overwriting colleague edit; stdout={}",
        String::from_utf8_lossy(&out_reupload.stdout)
    );
    assert!(
        String::from_utf8_lossy(&out_reupload.stderr).contains("412"),
        "expected 412 conflict error, got: {}",
        String::from_utf8_lossy(&out_reupload.stderr)
    );
    // Server original preserved, copy has local content, baseline item_id & eTag unchanged
    assert_eq!(
        st.contents
            .lock()
            .await
            .get(&("drv-me".into(), "item-spec-orig".into()))
            .unwrap()
            .as_slice(),
        b"SERVER EDIT BY COLLEAGUE"
    );
    assert_eq!(
        st.contents
            .lock()
            .await
            .get(&("drv-me".into(), copy_id.clone()))
            .unwrap()
            .as_slice(),
        b"LOCAL_STALE_EDIT"
    );
    let db_path = profile.path().join("data").join("default").join("cache.db");
    let storage = Storage::open(&db_path).unwrap();
    let baseline = storage.get_download_baseline(&local_path).unwrap().unwrap();
    assert_eq!(baseline.drive_id, "drv-me");
    assert_eq!(baseline.item_id, "item-spec-orig");
    assert_eq!(baseline.e_tag.as_deref(), Some("\"spec-v1\""));

    // 5. Regression control: explicit re-download sets baseline to v2; normal updates advance eTag twice
    let dl_dir2 = tempdir().unwrap();
    let out_dl2 = run_cli(vec![
        "download".into(),
        "/FolderA/spec.md".into(),
        "--drive".into(),
        "me".into(),
        "--dir".into(),
        dl_dir2.path().to_string_lossy().into_owned(),
        "--json".into(),
    ])
    .await;
    assert!(out_dl2.status.success());
    let local_path2 = std::path::PathBuf::from(
        serde_json::from_slice::<Value>(&out_dl2.stdout).unwrap()["path"]
            .as_str()
            .unwrap(),
    );
    std::fs::write(&local_path2, b"MERGED_EDIT_1").unwrap();
    let out_ok1 = run_cli(vec![
        "upload".into(),
        local_path2.to_string_lossy().into_owned(),
        "/FolderA".into(),
        "--drive".into(),
        "me".into(),
        "--json".into(),
    ])
    .await;
    assert!(
        out_ok1.status.success(),
        "first clean update of original failed: {}",
        String::from_utf8_lossy(&out_ok1.stderr)
    );
    let ok1_etag = serde_json::from_slice::<Value>(&out_ok1.stdout).unwrap()["e_tag"]
        .as_str()
        .unwrap()
        .to_string();
    let baseline2 = storage.get_download_baseline(&local_path2).unwrap().unwrap();
    assert_eq!(baseline2.item_id, "item-spec-orig");
    assert_eq!(baseline2.e_tag.as_deref(), Some(ok1_etag.as_str()));

    std::fs::write(&local_path2, b"MERGED_EDIT_2").unwrap();
    let out_ok2 = run_cli(vec![
        "upload".into(),
        local_path2.to_string_lossy().into_owned(),
        "/FolderA".into(),
        "--drive".into(),
        "me".into(),
        "--json".into(),
    ])
    .await;
    assert!(
        out_ok2.status.success(),
        "second clean update of original using refreshed baseline eTag failed: {}",
        String::from_utf8_lossy(&out_ok2.stderr)
    );
    assert_eq!(
        st.contents
            .lock()
            .await
            .get(&("drv-me".into(), "item-spec-orig".into()))
            .unwrap()
            .as_slice(),
        b"MERGED_EDIT_2"
    );

    server.abort();
}

#[tokio::test]
async fn regression_sharepoint_sp_03_explicit_site_resolves_site_drives_named_personal_or_my() {
    use tokio::process::Command;

    #[derive(Clone)]
    struct Sp03State {
        me_drive_calls: Arc<AtomicUsize>,
        upload_paths: Arc<Mutex<Vec<String>>>,
    }

    let st = Sp03State {
        me_drive_calls: Arc::new(AtomicUsize::new(0)),
        upload_paths: Arc::new(Mutex::new(Vec::new())),
    };

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}/v1.0");

    let router = Router::new()
        .route(
            "/v1.0/me/drive",
            get(|State(s): State<Sp03State>| async move {
                s.me_drive_calls.fetch_add(1, Ordering::SeqCst);
                Json(json!({
                    "id": "drv-me",
                    "name": "OneDrive",
                    "driveType": "business"
                }))
            }),
        )
        .route(
            "/v1.0/me/followedSites",
            get(|| async {
                Json(json!({
                    "value": [{
                        "id": "site-acme",
                        "name": "acme-engineering",
                        "displayName": "Acme Engineering",
                        "webUrl": "https://synthetic.sharepoint.test/sites/acme-engineering"
                    }]
                }))
            }),
        )
        .route(
            "/v1.0/sites",
            get(|| async {
                Json(json!({
                    "value": [{
                        "id": "site-acme",
                        "name": "acme-engineering",
                        "displayName": "Acme Engineering",
                        "webUrl": "https://synthetic.sharepoint.test/sites/acme-engineering"
                    }]
                }))
            }),
        )
        .route(
            "/v1.0/sites/{site_id}/drives",
            get(|Path(site_id): Path<String>| async move {
                if site_id == "site-acme" {
                    Json(json!({
                        "value": [
                            {
                                "id": "drv-eng-personal",
                                "name": "Personal",
                                "driveType": "documentLibrary"
                            },
                            {
                                "id": "drv-eng-my",
                                "name": "My",
                                "driveType": "documentLibrary"
                            },
                            {
                                "id": "drv-eng-docs",
                                "name": "Documents",
                                "driveType": "documentLibrary"
                            }
                        ]
                    }))
                } else {
                    Json(json!({ "value": [] }))
                }
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/root/children",
            get(|Path(drive_id): Path<String>| async move {
                let name = match drive_id.as_str() {
                    "drv-eng-personal" => "site-personal-handbook.md",
                    "drv-eng-my" => "site-my-checklist.md",
                    "drv-me" => "my-private-budget.xlsx",
                    _ => "other.txt",
                };
                Json(json!({
                    "value": [{
                        "id": format!("item-{drive_id}"),
                        "name": name,
                        "size": 10,
                        "file": { "mimeType": "text/plain" },
                        "parentReference": { "driveId": drive_id, "id": "root", "path": "/drive/root:" }
                    }]
                }))
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/{*tail}",
            put(|State(s): State<Sp03State>, Path((drive_id, tail)): Path<(String, String)>, body: Bytes| async move {
                s.upload_paths
                    .lock()
                    .await
                    .push(format!("/v1.0/drives/{drive_id}/{tail}"));
                let name = tail.trim_start_matches("root:/").trim_end_matches(":/content");
                (
                    StatusCode::CREATED,
                    Json(json!({
                        "id": format!("up-{drive_id}"),
                        "name": name,
                        "size": body.len(),
                        "file": { "mimeType": "text/plain" },
                        "parentReference": { "driveId": drive_id, "id": "root", "path": "/drive/root:" }
                    })),
                )
            }),
        )
        .with_state(st.clone());

    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let profile = tempdir().unwrap();
    let workdir = tempdir().unwrap();
    let upload_file = workdir.path().join("note.txt");
    std::fs::write(&upload_file, b"hello site library").unwrap();

    let run_cli = |args: Vec<String>| {
        let profile_path = profile.path().to_path_buf();
        let base_url = base_url.clone();
        async move {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_sharepoint"));
            cmd.kill_on_drop(true);
            for (k, _) in std::env::vars_os() {
                let u = k.to_string_lossy().to_ascii_uppercase();
                if ["SHAREPOINT_", "ONEDRIVE_", "TEAMS_", "AZURE_"]
                    .iter()
                    .any(|p| u.starts_with(p))
                {
                    cmd.env_remove(k);
                }
            }
            cmd.env("SHAREPOINT_GRAPH_BASE_URL", &base_url)
                .env("SHAREPOINT_ACCESS_TOKEN", "sp03-test-token")
                .env("SHAREPOINT_DISABLE_KEYRING", "1")
                .env("SHAREPOINT_SHARE_M365_AUTH", "0")
                .env("SHAREPOINT_AUTO_DAEMON", "0")
                .env("SHAREPOINT_CONFIG_DIR", profile_path.join("config"))
                .env("SHAREPOINT_DATA_DIR", profile_path.join("data"))
                .env("NO_PROXY", "127.0.0.1,localhost")
                .args(&args)
                .output()
                .await
                .unwrap()
        }
    };

    // 1. --site "Acme Engineering" --drive Personal -> site library drv-eng-personal, 0 /me/drive calls
    let ls_personal = run_cli(vec![
        "ls".into(),
        "--site".into(),
        "Acme Engineering".into(),
        "--drive".into(),
        "Personal".into(),
        "--json".into(),
    ])
    .await;
    assert!(ls_personal.status.success(), "ls Personal failed: {}", String::from_utf8_lossy(&ls_personal.stderr));
    let ls_personal_json: Value = serde_json::from_slice(&ls_personal.stdout).unwrap();
    assert_eq!(ls_personal_json["drive"]["id"], "drv-eng-personal");
    assert_eq!(ls_personal_json["items"][0]["name"], "site-personal-handbook.md");
    assert_eq!(st.me_drive_calls.load(Ordering::SeqCst), 0, "must not query /me/drive when --site is given");

    // 2. --site "Acme Engineering" --drive My -> site library drv-eng-my, 0 /me/drive calls
    let ls_my = run_cli(vec![
        "ls".into(),
        "--site".into(),
        "Acme Engineering".into(),
        "--drive".into(),
        "My".into(),
        "--json".into(),
    ])
    .await;
    assert!(ls_my.status.success());
    let ls_my_json: Value = serde_json::from_slice(&ls_my.stdout).unwrap();
    assert_eq!(ls_my_json["drive"]["id"], "drv-eng-my");
    assert_eq!(ls_my_json["items"][0]["name"], "site-my-checklist.md");
    assert_eq!(st.me_drive_calls.load(Ordering::SeqCst), 0);

    // 3. upload with --site and --drive Personal / My -> targets site drives, 0 /me/drive calls
    let up_personal = run_cli(vec![
        "upload".into(),
        upload_file.to_string_lossy().into_owned(),
        "--site".into(),
        "Acme Engineering".into(),
        "--drive".into(),
        "Personal".into(),
        "--json".into(),
    ])
    .await;
    assert!(up_personal.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&up_personal.stdout).unwrap()["drive_id"],
        "drv-eng-personal"
    );

    let up_my = run_cli(vec![
        "upload".into(),
        upload_file.to_string_lossy().into_owned(),
        "--site".into(),
        "Acme Engineering".into(),
        "--drive".into(),
        "My".into(),
        "--json".into(),
    ])
    .await;
    assert!(up_my.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&up_my.stdout).unwrap()["drive_id"],
        "drv-eng-my"
    );
    assert_eq!(st.me_drive_calls.load(Ordering::SeqCst), 0);

    // 4. Unmatched alias inside explicit site (e.g. --site "Acme Engineering" --drive OneDrive) fails with 0 /me/drive calls
    let ls_missing = run_cli(vec![
        "ls".into(),
        "--site".into(),
        "Acme Engineering".into(),
        "--drive".into(),
        "OneDrive".into(),
        "--json".into(),
    ])
    .await;
    assert!(!ls_missing.status.success(), "non-existent library inside --site must fail");
    assert_eq!(st.me_drive_calls.load(Ordering::SeqCst), 0);

    // 5. Regression control: exact drive ID and unique prefix "Pers" inside site continue working
    let ls_id = run_cli(vec![
        "ls".into(),
        "--site".into(),
        "Acme Engineering".into(),
        "--drive".into(),
        "drv-eng-personal".into(),
        "--json".into(),
    ])
    .await;
    assert!(ls_id.status.success());
    assert_eq!(serde_json::from_slice::<Value>(&ls_id.stdout).unwrap()["drive"]["id"], "drv-eng-personal");

    let ls_prefix = run_cli(vec![
        "ls".into(),
        "--site".into(),
        "Acme Engineering".into(),
        "--drive".into(),
        "Pers".into(),
        "--json".into(),
    ])
    .await;
    assert!(ls_prefix.status.success());
    assert_eq!(serde_json::from_slice::<Value>(&ls_prefix.stdout).unwrap()["drive"]["id"], "drv-eng-personal");
    assert_eq!(st.me_drive_calls.load(Ordering::SeqCst), 0);

    // 6. Regression control: without --site, me / onedrive aliases still resolve personal OneDrive
    let ls_me = run_cli(vec!["ls".into(), "--drive".into(), "me".into(), "--json".into()]).await;
    assert!(ls_me.status.success());
    assert_eq!(serde_json::from_slice::<Value>(&ls_me.stdout).unwrap()["drive"]["id"], "drv-me");
    assert_eq!(st.me_drive_calls.load(Ordering::SeqCst), 1);

    let ls_od = run_cli(vec!["ls".into(), "--drive".into(), "onedrive".into(), "--json".into()]).await;
    assert!(ls_od.status.success());
    assert_eq!(serde_json::from_slice::<Value>(&ls_od.stdout).unwrap()["drive"]["id"], "drv-me");

    // 7. bitefoundry-onedrive entrypoint also resolves explicit --site library without touching /me/drive
    let me_calls_before_od = st.me_drive_calls.load(Ordering::SeqCst);
    let od_profile = tempdir().unwrap();
    let mut od_cmd = Command::new(env!("CARGO_BIN_EXE_bitefoundry-onedrive"));
    od_cmd.kill_on_drop(true);
    for (k, _) in std::env::vars_os() {
        let u = k.to_string_lossy().to_ascii_uppercase();
        if ["SHAREPOINT_", "ONEDRIVE_", "TEAMS_", "AZURE_"]
            .iter()
            .any(|p| u.starts_with(p))
        {
            od_cmd.env_remove(k);
        }
    }
    let od_site_out = od_cmd
        .env("SHAREPOINT_GRAPH_BASE_URL", &base_url)
        .env("SHAREPOINT_ACCESS_TOKEN", "sp03-od-token")
        .env("SHAREPOINT_DISABLE_KEYRING", "1")
        .env("SHAREPOINT_SHARE_M365_AUTH", "0")
        .env("SHAREPOINT_AUTO_DAEMON", "0")
        .env("SHAREPOINT_CONFIG_DIR", od_profile.path().join("config"))
        .env("SHAREPOINT_DATA_DIR", od_profile.path().join("data"))
        .env("NO_PROXY", "127.0.0.1,localhost")
        .args(["ls", "--site", "Acme Engineering", "--drive", "Personal", "--json"])
        .output()
        .await
        .unwrap();
    assert!(od_site_out.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&od_site_out.stdout).unwrap()["drive"]["id"],
        "drv-eng-personal"
    );
    assert_eq!(st.me_drive_calls.load(Ordering::SeqCst), me_calls_before_od);

    server.abort();
}

#[tokio::test]
async fn regression_sharepoint_sp_04_move_to_root_resolves_actual_root_id_in_cli_and_mcp() {
    use tokio::process::Command;

    #[derive(Clone)]
    struct Sp04State {
        fail_root_get: Arc<AtomicBool>,
        root_get_count: Arc<AtomicUsize>,
        patches: Arc<Mutex<Vec<(String, Value)>>>,
    }

    let st = Sp04State {
        fail_root_get: Arc::new(AtomicBool::new(false)),
        root_get_count: Arc::new(AtomicUsize::new(0)),
        patches: Arc::new(Mutex::new(Vec::new())),
    };

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}/v1.0");

    let router = Router::new()
        .route(
            "/v1.0/drives/{drive_id}",
            get(|Path(drive_id): Path<String>| async move {
                Json(json!({
                    "id": drive_id,
                    "name": "Documents",
                    "driveType": "documentLibrary"
                }))
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/root",
            get(|State(s): State<Sp04State>, Path(drive_id): Path<String>| async move {
                s.root_get_count.fetch_add(1, Ordering::SeqCst);
                if s.fail_root_get.load(Ordering::SeqCst) {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": { "code": "generalException", "message": "root lookup failed" } })),
                    )
                        .into_response();
                }
                (
                    StatusCode::OK,
                    Json(json!({
                        "id": "root-real",
                        "name": "root",
                        "size": 0,
                        "folder": { "childCount": 2 },
                        "parentReference": { "driveId": drive_id }
                    })),
                )
                    .into_response()
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/items/{item_id}",
            get(|Path((drive_id, item_id)): Path<(String, String)>| async move {
                if item_id == "fld-dest" {
                    return (
                        StatusCode::OK,
                        Json(json!({
                            "id": "fld-dest",
                            "name": "Dest",
                            "size": 0,
                            "folder": { "childCount": 0 },
                            "parentReference": { "driveId": drive_id, "id": "root-real", "path": "/drive/root:" }
                        })),
                    )
                        .into_response();
                }
                (
                    StatusCode::OK,
                    Json(json!({
                        "id": item_id,
                        "name": "doc.md",
                        "size": 12,
                        "file": { "mimeType": "text/markdown" },
                        "parentReference": { "driveId": drive_id, "id": "fld-sub", "path": "/drive/root:/Sub" }
                    })),
                )
                    .into_response()
            })
            .patch(|State(s): State<Sp04State>, Path((drive_id, item_id)): Path<(String, String)>, Json(body): Json<Value>| async move {
                s.patches.lock().await.push((item_id.clone(), body.clone()));
                if body.pointer("/parentReference/id").and_then(Value::as_str) == Some("root") {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({
                            "error": {
                                "code": "invalidRequest",
                                "message": "Cannot use literal 'root' as parentReference.id"
                            }
                        })),
                    )
                        .into_response();
                }
                let parent_id = body
                    .pointer("/parentReference/id")
                    .and_then(Value::as_str)
                    .unwrap_or("fld-sub");
                let parent_path = if parent_id == "root-real" {
                    "/drive/root:"
                } else {
                    "/drive/root:/Dest"
                };
                let name = body.get("name").and_then(Value::as_str).unwrap_or("doc.md");
                (
                    StatusCode::OK,
                    Json(json!({
                        "id": item_id,
                        "name": name,
                        "size": 12,
                        "file": { "mimeType": "text/markdown" },
                        "parentReference": { "driveId": drive_id, "id": parent_id, "path": parent_path }
                    })),
                )
                    .into_response()
            }),
        )
        .with_state(st.clone());

    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let profile = tempdir().unwrap();
    let run_cli = |args: Vec<String>| {
        let profile_path = profile.path().to_path_buf();
        let base_url = base_url.clone();
        async move {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_sharepoint"));
            cmd.kill_on_drop(true);
            for (k, _) in std::env::vars_os() {
                let u = k.to_string_lossy().to_ascii_uppercase();
                if ["SHAREPOINT_", "ONEDRIVE_", "TEAMS_", "AZURE_"]
                    .iter()
                    .any(|p| u.starts_with(p))
                {
                    cmd.env_remove(k);
                }
            }
            cmd.env("SHAREPOINT_GRAPH_BASE_URL", &base_url)
                .env("SHAREPOINT_ACCESS_TOKEN", "sp04-test-token")
                .env("SHAREPOINT_DISABLE_KEYRING", "1")
                .env("SHAREPOINT_SHARE_M365_AUTH", "0")
                .env("SHAREPOINT_AUTO_DAEMON", "0")
                .env("SHAREPOINT_CONFIG_DIR", profile_path.join("config"))
                .env("SHAREPOINT_DATA_DIR", profile_path.join("data"))
                .env("NO_PROXY", "127.0.0.1,localhost")
                .args(&args)
                .output()
                .await
                .unwrap()
        }
    };

    // 1. CLI `mv --parent /` fetches actual root ID ("root-real") and PATCHes with it
    let cli_mv = run_cli(vec![
        "mv".into(),
        "item-cli-1".into(),
        "--drive".into(),
        "drv-sp".into(),
        "--parent".into(),
        "/".into(),
        "--json".into(),
    ])
    .await;
    assert!(
        cli_mv.status.success(),
        "CLI mv --parent / failed: stderr={}",
        String::from_utf8_lossy(&cli_mv.stderr)
    );
    assert_eq!(st.root_get_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        st.patches.lock().await.last(),
        Some(&(
            "item-cli-1".to_string(),
            json!({ "parentReference": { "id": "root-real" } })
        ))
    );

    // 2. MCP `move_or_rename_item` with `new_parent_id: "root"` resolves "root-real" and PATCHes with it
    let (cfg, graph, storage) = make_graph_and_storage(&base_url);
    let mcp_req = json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "tools/call",
        "params": {
            "name": "move_or_rename_item",
            "arguments": {
                "drive": "drv-sp",
                "item_id": "item-mcp-1",
                "new_parent_id": "root"
            }
        }
    });
    let mcp_resp = mcp::handle_rpc_line(&cfg, &graph, &storage, &mcp_req.to_string())
        .await
        .unwrap();
    assert_eq!(
        mcp_resp["result"]["isError"],
        false,
        "MCP move_or_rename_item root failed: {mcp_resp}"
    );
    assert_eq!(st.root_get_count.load(Ordering::SeqCst), 2);
    assert_eq!(
        st.patches.lock().await.last(),
        Some(&(
            "item-mcp-1".to_string(),
            json!({ "parentReference": { "id": "root-real" } })
        ))
    );

    // 3. When GET /drives/{id}/root fails, 0 PATCH requests are sent (CLI and MCP)
    st.fail_root_get.store(true, Ordering::SeqCst);
    let patches_before_fail = st.patches.lock().await.len();
    let cli_fail = run_cli(vec![
        "mv".into(),
        "item-cli-fail".into(),
        "--drive".into(),
        "drv-sp".into(),
        "--parent".into(),
        "/".into(),
        "--json".into(),
    ])
    .await;
    assert!(!cli_fail.status.success());

    let mcp_fail_req = json!({
        "jsonrpc": "2.0",
        "id": 11,
        "method": "tools/call",
        "params": {
            "name": "move_or_rename_item",
            "arguments": {
                "drive": "drv-sp",
                "item_id": "item-mcp-fail",
                "new_parent_id": "root"
            }
        }
    });
    let mcp_fail = mcp::handle_rpc_line(&cfg, &graph, &storage, &mcp_fail_req.to_string())
        .await
        .unwrap();
    assert_eq!(mcp_fail["result"]["isError"], true);
    assert_eq!(
        st.patches.lock().await.len(),
        patches_before_fail,
        "failed root GET must send 0 PATCH requests"
    );

    // 4. Regression control: normal folder ID move and rename-only do NOT call GET /root
    st.fail_root_get.store(false, Ordering::SeqCst);
    let root_gets_before_ctrl = st.root_get_count.load(Ordering::SeqCst);
    let cli_folder = run_cli(vec![
        "mv".into(),
        "item-ctrl-1".into(),
        "--drive".into(),
        "drv-sp".into(),
        "--parent".into(),
        "fld-dest".into(),
        "--json".into(),
    ])
    .await;
    assert!(cli_folder.status.success());
    assert_eq!(
        st.patches.lock().await.last(),
        Some(&(
            "item-ctrl-1".to_string(),
            json!({ "parentReference": { "id": "fld-dest" } })
        ))
    );

    let cli_rename = run_cli(vec![
        "mv".into(),
        "item-ctrl-2".into(),
        "--drive".into(),
        "drv-sp".into(),
        "--name".into(),
        "renamed.md".into(),
        "--json".into(),
    ])
    .await;
    assert!(cli_rename.status.success());
    assert_eq!(
        st.patches.lock().await.last(),
        Some(&(
            "item-ctrl-2".to_string(),
            json!({ "name": "renamed.md" })
        ))
    );
    assert_eq!(
        st.root_get_count.load(Ordering::SeqCst),
        root_gets_before_ctrl,
        "normal folder move and rename-only must not call GET /root"
    );

    server.abort();
}

#[tokio::test]
async fn regression_sharepoint_sp_11_exact_limit_200_completeness_and_cache_pruning() {
    use sharepoint_cli::cli;

    #[derive(Clone)]
    struct Sp11State {
        base_url: String,
        mode: Arc<Mutex<String>>,
    }

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}/v1.0");
    let st = Sp11State {
        base_url: base_url.clone(),
        mode: Arc::new(Mutex::new("201_two_pages".into())),
    };

    let make_item = |i: usize| {
        json!({
            "id": format!("item-{i:03}"),
            "name": format!("file-{i:03}.txt"),
            "size": 10,
            "file": { "mimeType": "text/plain" },
            "parentReference": { "driveId": "drv-me", "id": "root", "path": "/drive/root:" }
        })
    };

    let router = Router::new()
        .route(
            "/v1.0/drives/{drive_id}/root/children",
            get(move |State(s): State<Sp11State>, Path(drive_id): Path<String>, Query(q): Query<std::collections::HashMap<String, String>>| async move {
                let mode = s.mode.lock().await.clone();
                let page = q.get("page").map(String::as_str).unwrap_or("1");
                match (mode.as_str(), page) {
                    ("201_two_pages", "1") => {
                        let vals: Vec<Value> = (1..=200).map(make_item).collect();
                        (
                            StatusCode::OK,
                            Json(json!({
                                "value": vals,
                                "@odata.nextLink": format!("{}/drives/{drive_id}/root/children?page=2", s.base_url)
                            })),
                        )
                            .into_response()
                    }
                    ("201_two_pages", "2") => (
                        StatusCode::OK,
                        Json(json!({ "value": [make_item(201)] })),
                    )
                        .into_response(),
                    ("200_final", _) => {
                        let vals: Vec<Value> = (1..=200).map(make_item).collect();
                        (StatusCode::OK, Json(json!({ "value": vals }))).into_response()
                    }
                    ("200_with_next", "1") => {
                        let vals: Vec<Value> = (1..=200).map(make_item).collect();
                        (
                            StatusCode::OK,
                            Json(json!({
                                "value": vals,
                                "@odata.nextLink": format!("{}/drives/{drive_id}/root/children?page=2", s.base_url)
                            })),
                        )
                            .into_response()
                    }
                    ("mid_page_5", _) => {
                        let vals: Vec<Value> = (1..=5).map(make_item).collect();
                        (StatusCode::OK, Json(json!({ "value": vals }))).into_response()
                    }
                    ("page2_fails", "1") => {
                        let vals: Vec<Value> = (1..=100).map(make_item).collect();
                        (
                            StatusCode::OK,
                            Json(json!({
                                "value": vals,
                                "@odata.nextLink": format!("{}/drives/{drive_id}/root/children?page=2", s.base_url)
                            })),
                        )
                            .into_response()
                    }
                    ("page2_fails", "2") => (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": { "code": "generalException", "message": "page 2 failed" } })),
                    )
                        .into_response(),
                    _ => (StatusCode::OK, Json(json!({ "value": [] }))).into_response(),
                }
            }),
        )
        .with_state(st.clone());

    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let (_cfg, graph, storage) = make_graph_and_storage(&base_url);

    // 1. Seed 201 items into cache
    let all_201 = cli::fetch_folder_items(&graph, &storage, "drv-me", None, 250, false, false, false)
        .await
        .unwrap();
    assert_eq!(all_201.len(), 201);
    assert!(storage.get_drive_item("drv-me", "item-201").unwrap().is_some());

    // 2. Server reduces to 200 items with NO nextLink; ls --limit 200 must prune deleted item-201!
    *st.mode.lock().await = "200_final".into();
    let page_200 = cli::fetch_folder_items(&graph, &storage, "drv-me", None, 200, false, false, false)
        .await
        .unwrap();
    assert_eq!(page_200.len(), 200);
    let offline_after_200 =
        cli::fetch_folder_items(&graph, &storage, "drv-me", None, 300, false, true, false)
            .await
            .unwrap();
    assert_eq!(
        offline_after_200.len(),
        200,
        "exact limit=200 without nextLink must prune deleted 201st item"
    );
    assert!(
        storage.get_drive_item("drv-me", "item-201").unwrap().is_none(),
        "deleted item-201 must be absent from cache"
    );

    // 3. Re-seed 201 items, then server returns 200 items WITH nextLink -> must preserve item-201
    *st.mode.lock().await = "201_two_pages".into();
    assert_eq!(
        cli::fetch_folder_items(&graph, &storage, "drv-me", None, 250, false, false, false)
            .await
            .unwrap()
            .len(),
        201
    );
    *st.mode.lock().await = "200_with_next".into();
    let partial_200 =
        cli::fetch_folder_items(&graph, &storage, "drv-me", None, 200, false, false, false)
            .await
            .unwrap();
    assert_eq!(partial_200.len(), 200);
    let offline_with_next =
        cli::fetch_folder_items(&graph, &storage, "drv-me", None, 300, false, true, false)
            .await
            .unwrap();
    assert_eq!(
        offline_with_next.len(),
        201,
        "200 items with nextLink must preserve unfetched 201st cached item"
    );

    // 4. Regression control: mid-page limit (3 of 5) preserves uncollected items
    *st.mode.lock().await = "mid_page_5".into();
    let mid = cli::fetch_folder_items(&graph, &storage, "drv-me", None, 3, false, false, false)
        .await
        .unwrap();
    assert_eq!(mid.len(), 3);
    assert_eq!(
        cli::fetch_folder_items(&graph, &storage, "drv-me", None, 300, false, true, false)
            .await
            .unwrap()
            .len(),
        201,
        "mid-page limit must not delete cached siblings"
    );

    // 5. Regression control: page 2 error preserves existing cache
    *st.mode.lock().await = "page2_fails".into();
    let page2_err = cli::fetch_folder_items(&graph, &storage, "drv-me", None, 200, false, false, false)
        .await
        .expect_err("page 2 failure must return error");
    assert!(format!("{page2_err:#}").contains("500"));
    assert_eq!(
        cli::fetch_folder_items(&graph, &storage, "drv-me", None, 300, false, true, false)
            .await
            .unwrap()
            .len(),
        201,
        "failed second page must preserve cached items"
    );

    server.abort();
}

#[tokio::test]
async fn regression_sharepoint_sp_06_resolve_item_distinguishes_auth_server_errors_from_not_found() {
    use tokio::process::Command;

    #[derive(Clone)]
    struct Sp06State {
        remote_requests: Arc<AtomicUsize>,
        write_requests: Arc<AtomicUsize>,
    }

    let st = Sp06State {
        remote_requests: Arc::new(AtomicUsize::new(0)),
        write_requests: Arc::new(AtomicUsize::new(0)),
    };

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}/v1.0");

    let error_for_key = |key: &str| -> axum::response::Response {
        match key {
            "err-403" | "Secret403" => (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": { "code": "accessDenied", "message": "Access denied to resource" } })),
            )
                .into_response(),
            "err-401" | "Secret401" => (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": { "code": "InvalidAuthenticationToken", "message": "Token expired" } })),
            )
                .into_response(),
            "err-500" | "Secret500" => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": { "code": "generalException", "message": "Upstream storage fault" } })),
            )
                .into_response(),
            _ => (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": { "code": "itemNotFound", "message": "Item not found" } })),
            )
                .into_response(),
        }
    };

    let router = Router::new()
        .route(
            "/v1.0/me/drive",
            get(|State(s): State<Sp06State>| async move {
                s.remote_requests.fetch_add(1, Ordering::SeqCst);
                Json(json!({ "id": "drv-me", "name": "OneDrive", "driveType": "business" }))
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/items/{item_id}",
            get(move |State(s): State<Sp06State>, Path((_drive_id, item_id)): Path<(String, String)>| async move {
                s.remote_requests.fetch_add(1, Ordering::SeqCst);
                error_for_key(&item_id)
            })
            .patch(|State(s): State<Sp06State>| async move {
                s.write_requests.fetch_add(1, Ordering::SeqCst);
                StatusCode::OK
            })
            .delete(|State(s): State<Sp06State>| async move {
                s.write_requests.fetch_add(1, Ordering::SeqCst);
                StatusCode::NO_CONTENT
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/{*tail}",
            get(move |State(s): State<Sp06State>, Path((drive_id, tail)): Path<(String, String)>| async move {
                s.remote_requests.fetch_add(1, Ordering::SeqCst);
                if tail.starts_with("root:/") && tail.ends_with(':') {
                    let rel = tail.trim_start_matches("root:/").trim_end_matches(':');
                    if rel == "bare-file.md" {
                        return (
                            StatusCode::OK,
                            Json(json!({
                                "id": "item-bare-real",
                                "name": "bare-file.md",
                                "size": 15,
                                "file": { "mimeType": "text/markdown" },
                                "parentReference": { "driveId": drive_id, "id": "root", "path": "/drive/root:" }
                            })),
                        )
                            .into_response();
                    }
                    return error_for_key(rel);
                }
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": { "code": "itemNotFound" } })),
                )
                    .into_response()
            }),
        )
        .with_state(st.clone());

    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let profile = tempdir().unwrap();
    let run_cli = |args: Vec<String>| {
        let profile_path = profile.path().to_path_buf();
        let base_url = base_url.clone();
        async move {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_sharepoint"));
            cmd.kill_on_drop(true);
            for (k, _) in std::env::vars_os() {
                let u = k.to_string_lossy().to_ascii_uppercase();
                if ["SHAREPOINT_", "ONEDRIVE_", "TEAMS_", "AZURE_"]
                    .iter()
                    .any(|p| u.starts_with(p))
                {
                    cmd.env_remove(k);
                }
            }
            cmd.env("SHAREPOINT_GRAPH_BASE_URL", &base_url)
                .env("SHAREPOINT_ACCESS_TOKEN", "sp06-test-token")
                .env("SHAREPOINT_DISABLE_KEYRING", "1")
                .env("SHAREPOINT_SHARE_M365_AUTH", "0")
                .env("SHAREPOINT_AUTO_DAEMON", "0")
                .env("SHAREPOINT_CONFIG_DIR", profile_path.join("config"))
                .env("SHAREPOINT_DATA_DIR", profile_path.join("data"))
                .env("NO_PROXY", "127.0.0.1,localhost")
                .args(&args)
                .output()
                .await
                .unwrap()
        }
    };

    let (cfg, graph, storage) = make_graph_and_storage(&base_url);

    // 1. CLI & MCP distinguish 403 accessDenied, 401, 500 vs real 404 on both ID and path lookups
    for (target, expected_status, expected_code) in [
        ("err-403", "403", "accessDenied"),
        ("/Secret403", "403", "accessDenied"),
        ("err-401", "401", "InvalidAuthenticationToken"),
        ("/Secret401", "401", "InvalidAuthenticationToken"),
        ("err-500", "500", "generalException"),
        ("/Secret500", "500", "generalException"),
    ] {
        let cli_out = run_cli(vec![
            "info".into(),
            target.into(),
            "--drive".into(),
            "me".into(),
            "--json".into(),
        ])
        .await;
        assert!(!cli_out.status.success(), "CLI info {target} must fail");
        let stderr = String::from_utf8_lossy(&cli_out.stderr);
        assert!(
            stderr.contains(expected_status) && stderr.contains(expected_code),
            "CLI info {target} must surface {expected_status}/{expected_code}, got: {stderr}"
        );
        assert!(
            !stderr.contains("Drive item not found"),
            "CLI info {target} must not disguise {expected_status} as not found: {stderr}"
        );

        let mcp_req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "get_file_info",
                "arguments": { "drive": "me", "item_id": target }
            }
        });
        let mcp_resp = mcp::handle_rpc_line(&cfg, &graph, &storage, &mcp_req.to_string())
            .await
            .unwrap();
        assert_eq!(mcp_resp["result"]["isError"], true);
        let err_text = mcp_resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            err_text.contains(expected_status) && err_text.contains(expected_code),
            "MCP get_file_info {target} must surface {expected_status}/{expected_code}, got: {err_text}"
        );
        assert!(
            !err_text.contains("Drive item not found"),
            "MCP get_file_info {target} must not mask {expected_status} as not found: {err_text}"
        );
    }

    // Genuine 404 ends with Drive item not found
    for missing in ["err-404", "/Missing404"] {
        let cli_404 = run_cli(vec![
            "info".into(),
            missing.into(),
            "--drive".into(),
            "me".into(),
            "--json".into(),
        ])
        .await;
        assert!(!cli_404.status.success());
        let stderr = String::from_utf8_lossy(&cli_404.stderr);
        assert!(
            stderr.contains("Drive item not found"),
            "genuine 404 for {missing} should report Drive item not found, got: {stderr}"
        );
    }

    // Failed write resolution sends 0 write requests
    let rm_403 = run_cli(vec![
        "rm".into(),
        "err-403".into(),
        "--drive".into(),
        "me".into(),
        "--yes".into(),
        "--json".into(),
    ])
    .await;
    assert!(!rm_403.status.success());
    assert!(String::from_utf8_lossy(&rm_403.stderr).contains("403"));
    assert_eq!(st.write_requests.load(Ordering::SeqCst), 0);

    // 2. Regression control: bare filename gets 404 on /items/{name}, succeeds on /root:/{name}:
    let bare = run_cli(vec![
        "info".into(),
        "bare-file.md".into(),
        "--drive".into(),
        "me".into(),
        "--json".into(),
    ])
    .await;
    assert!(
        bare.status.success(),
        "bare relative filename fallback failed: {}",
        String::from_utf8_lossy(&bare.stderr)
    );
    let bare_json: Value = serde_json::from_slice(&bare.stdout).unwrap();
    assert_eq!(bare_json["item"]["id"], "item-bare-real");

    // 3. Regression control: offline cache miss sends 0 remote requests
    let remote_before_offline = st.remote_requests.load(Ordering::SeqCst);
    let off_miss = run_cli(vec![
        "info".into(),
        "/does-not-exist-offline.md".into(),
        "--drive".into(),
        "me".into(),
        "--offline".into(),
        "--json".into(),
    ])
    .await;
    assert!(!off_miss.status.success());
    assert_eq!(
        st.remote_requests.load(Ordering::SeqCst),
        remote_before_offline,
        "offline cache miss must send 0 remote requests"
    );

    server.abort();
}

#[tokio::test]
async fn regression_sharepoint_sp_07_http_content_uses_decode_text_file_and_returns_415_on_binary() {
    use tokio::process::Command;

    let utf16_text = "상태: 승인 완료\r\n다음 단계: 배포";
    let utf16_bytes: Vec<u8> = [0xffu8, 0xfe]
        .into_iter()
        .chain(utf16_text.encode_utf16().flat_map(u16::to_le_bytes))
        .collect();
    let utf8_text = "상태: UTF-8 한글 정상\n두 번째 줄";
    let utf8_bytes = utf8_text.as_bytes().to_vec();
    let mut png_bytes = Vec::new();
    let img = image::RgbaImage::from_pixel(1, 1, image::Rgba([10, 20, 30, 255]));
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut png_bytes), image::ImageFormat::Png)
        .unwrap();
    let big_bytes = vec![b'A'; 200];

    #[derive(Clone)]
    struct Sp07State {
        utf16: Vec<u8>,
        utf8: Vec<u8>,
        png: Vec<u8>,
        big: Vec<u8>,
    }

    let st = Sp07State {
        utf16: utf16_bytes.clone(),
        utf8: utf8_bytes.clone(),
        png: png_bytes.clone(),
        big: big_bytes.clone(),
    };

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}/v1.0");

    let graph_app = Router::new()
        .route(
            "/v1.0/me/drive",
            get(|| async {
                Json(json!({ "id": "drv-me", "name": "OneDrive", "driveType": "business" }))
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/items/{item_id}",
            get(|State(s): State<Sp07State>, Path((drive_id, item_id)): Path<(String, String)>| async move {
                let (name, mime, size) = match item_id.as_str() {
                    "item-utf16" => ("korean-utf16.txt", "text/plain", s.utf16.len()),
                    "item-utf8" => ("korean-utf8.txt", "text/plain", s.utf8.len()),
                    "item-png" => ("logo.png", "image/png", s.png.len()),
                    "item-big" => ("big.txt", "text/plain", s.big.len()),
                    _ => return StatusCode::NOT_FOUND.into_response(),
                };
                (
                    StatusCode::OK,
                    Json(json!({
                        "id": item_id,
                        "name": name,
                        "size": size,
                        "eTag": "\"etag-sp07\"",
                        "file": { "mimeType": mime },
                        "parentReference": { "driveId": drive_id, "id": "root", "path": "/drive/root:" }
                    })),
                )
                    .into_response()
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/items/{item_id}/content",
            get(|State(s): State<Sp07State>, Path((_drive_id, item_id)): Path<(String, String)>| async move {
                match item_id.as_str() {
                    "item-utf16" => ([(header::CONTENT_TYPE, "text/plain")], s.utf16).into_response(),
                    "item-utf8" => ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], s.utf8).into_response(),
                    "item-png" => ([(header::CONTENT_TYPE, "image/png")], s.png).into_response(),
                    "item-big" => ([(header::CONTENT_TYPE, "text/plain")], s.big).into_response(),
                    _ => StatusCode::NOT_FOUND.into_response(),
                }
            }),
        )
        .with_state(st);

    let graph_server = tokio::spawn(async move {
        let _ = axum::serve(listener, graph_app).await;
    });

    let (cfg, graph, storage) = make_graph_and_storage(&base_url);
    // Keyless router that still accepts `dir`: local paths are an explicit opt-in.
    let api_router = serve::build_router_with(
        cfg,
        graph,
        storage,
        serve::RouterPolicy {
            allow_local_paths: true,
            ..Default::default()
        },
    );
    let api_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_addr = api_listener.local_addr().unwrap();
    let api_server = tokio::spawn(async move {
        let _ = axum::serve(api_listener, api_router).await;
    });

    let profile = tempdir().unwrap();
    let run_cli_cat = |item_id: &str| {
        let profile_path = profile.path().to_path_buf();
        let base_url = base_url.clone();
        let item_id = item_id.to_string();
        async move {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_sharepoint"));
            cmd.kill_on_drop(true);
            for (k, _) in std::env::vars_os() {
                let u = k.to_string_lossy().to_ascii_uppercase();
                if ["SHAREPOINT_", "ONEDRIVE_", "TEAMS_", "AZURE_"]
                    .iter()
                    .any(|p| u.starts_with(p))
                {
                    cmd.env_remove(k);
                }
            }
            cmd.env("SHAREPOINT_GRAPH_BASE_URL", &base_url)
                .env("SHAREPOINT_ACCESS_TOKEN", "sp07-test-token")
                .env("SHAREPOINT_DISABLE_KEYRING", "1")
                .env("SHAREPOINT_SHARE_M365_AUTH", "0")
                .env("SHAREPOINT_AUTO_DAEMON", "0")
                .env("SHAREPOINT_CONFIG_DIR", profile_path.join("config"))
                .env("SHAREPOINT_DATA_DIR", profile_path.join("data"))
                .env("NO_PROXY", "127.0.0.1,localhost")
                .args(["cat", &item_id, "--drive", "me", "--json"])
                .output()
                .await
                .unwrap()
        }
    };

    let http = reqwest::Client::new();

    // 1. BOM UTF-16LE Korean file matches CLI cat and reports original byte count
    let cli_utf16 = run_cli_cat("item-utf16").await;
    assert!(cli_utf16.status.success());
    let cli_utf16_json: Value = serde_json::from_slice(&cli_utf16.stdout).unwrap();

    let http_utf16_resp = http
        .get(format!("http://{api_addr}/v1/items/item-utf16/content?drive=me"))
        .send()
        .await
        .unwrap();
    assert_eq!(http_utf16_resp.status(), StatusCode::OK);
    let http_utf16: Value = http_utf16_resp.json().await.unwrap();
    assert_eq!(http_utf16["ok"], true);
    assert_eq!(http_utf16["content"], cli_utf16_json["content"]);
    assert_eq!(http_utf16["content"], utf16_text);
    assert_eq!(http_utf16["size_bytes"], utf16_bytes.len());

    // 2. UTF-8 Korean file matches CLI cat
    let cli_utf8 = run_cli_cat("item-utf8").await;
    assert!(cli_utf8.status.success());
    let cli_utf8_json: Value = serde_json::from_slice(&cli_utf8.stdout).unwrap();

    let http_utf8_resp = http
        .get(format!("http://{api_addr}/v1/items/item-utf8/content?drive=me"))
        .send()
        .await
        .unwrap();
    assert_eq!(http_utf8_resp.status(), StatusCode::OK);
    let http_utf8: Value = http_utf8_resp.json().await.unwrap();
    assert_eq!(http_utf8["content"], cli_utf8_json["content"]);
    assert_eq!(http_utf8["size_bytes"], utf8_bytes.len());

    // 3. PNG binary returns HTTP 415 Unsupported Media Type with standard error JSON
    let http_png_resp = http
        .get(format!("http://{api_addr}/v1/items/item-png/content?drive=me"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        http_png_resp.status(),
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "PNG preview must return HTTP 415"
    );
    let http_png_body: Value = http_png_resp.json().await.unwrap();
    assert!(http_png_body.get("error").and_then(Value::as_str).is_some());
    assert!(http_png_body.get("content").is_none());

    // 4. Regression control: max_bytes enforced, metadata route works, POST /v1/download returns raw PNG bytes
    let over_limit = http
        .get(format!("http://{api_addr}/v1/items/item-big/content?drive=me&max_bytes=50"))
        .send()
        .await
        .unwrap();
    assert!(!over_limit.status().is_success());

    let meta_resp = http
        .get(format!("http://{api_addr}/v1/items/item-png?drive=me"))
        .send()
        .await
        .unwrap();
    assert_eq!(meta_resp.status(), StatusCode::OK);
    let meta_json: Value = meta_resp.json().await.unwrap();
    assert_eq!(meta_json["item"]["mime_type"], "image/png");

    let tmp_dl = tempdir().unwrap();
    let dl_resp = http
        .post(format!("http://{api_addr}/v1/download"))
        .json(&json!({
            "drive": "me",
            "item_id": "item-png",
            "dir": tmp_dl.path().to_string_lossy()
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(dl_resp.status(), StatusCode::OK);
    let dl_json: Value = dl_resp.json().await.unwrap();
    assert_eq!(dl_json["ok"], true);
    assert_eq!(dl_json["is_image"], true);
    let downloaded_bytes = std::fs::read(dl_json["path"].as_str().unwrap()).unwrap();
    assert_eq!(downloaded_bytes, png_bytes);

    api_server.abort();
    graph_server.abort();
}

#[tokio::test]
async fn regression_sharepoint_sp_10_remote_search_fresh_results_ranking_and_error_propagation() {
    use sharepoint_cli::{cli, models::DriveItem};

    #[derive(Clone)]
    struct Sp10State {
        search_mode: Arc<Mutex<String>>,
        me_drive_mode: Arc<Mutex<String>>,
        requests: Arc<Mutex<Vec<(String, String)>>>,
    }

    let st = Sp10State {
        search_mode: Arc::new(Mutex::new("fresh_hits".into())),
        me_drive_mode: Arc::new(Mutex::new("ok".into())),
        requests: Arc::new(Mutex::new(Vec::new())),
    };

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}/v1.0");

    let router = Router::new()
        .route(
            "/v1.0/search/query",
            post(|State(s): State<Sp10State>| async move {
                s.requests
                    .lock()
                    .await
                    .push(("POST".into(), "/v1.0/search/query".into()));
                match s.search_mode.lock().await.as_str() {
                    "fresh_hits" => (
                        StatusCode::OK,
                        Json(json!({
                            "value": [{
                                "hitsContainers": [{
                                    "hits": [
                                        {
                                            "resource": {
                                                "id": "item-new-remote",
                                                "name": "report-brand-new.md",
                                                "size": 512,
                                                "eTag": "\"new-v1\"",
                                                "file": { "mimeType": "text/markdown" },
                                                "parentReference": { "driveId": "drv-me", "id": "root", "path": "/drive/root:" }
                                            }
                                        },
                                        {
                                            "resource": {
                                                "id": "item-1",
                                                "name": "report-alpha-fresh.md",
                                                "size": 999,
                                                "eTag": "\"fresh-v2\"",
                                                "file": { "mimeType": "text/markdown" },
                                                "parentReference": { "driveId": "drv-me", "id": "root", "path": "/drive/root:" }
                                            }
                                        }
                                    ]
                                }]
                            }]
                        })),
                    )
                        .into_response(),
                    "empty_ok" => (
                        StatusCode::OK,
                        Json(json!({ "value": [{ "hitsContainers": [{ "hits": [] }] }] })),
                    )
                        .into_response(),
                    "forbidden_403" => (
                        StatusCode::FORBIDDEN,
                        Json(json!({ "error": { "code": "accessDenied", "message": "Search API forbidden" } })),
                    )
                        .into_response(),
                    _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                }
            }),
        )
        .route(
            "/v1.0/me/drive",
            get(|State(s): State<Sp10State>| async move {
                s.requests
                    .lock()
                    .await
                    .push(("GET".into(), "/v1.0/me/drive".into()));
                match s.me_drive_mode.lock().await.as_str() {
                    "ok" => (
                        StatusCode::OK,
                        Json(json!({ "id": "drv-me", "name": "OneDrive", "driveType": "business" })),
                    )
                        .into_response(),
                    _ => (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": { "code": "generalException", "message": "Fallback drive error" } })),
                    )
                        .into_response(),
                }
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/{*tail}",
            get(|State(s): State<Sp10State>, Path((drive_id, tail)): Path<(String, String)>| async move {
                s.requests
                    .lock()
                    .await
                    .push(("GET".into(), format!("/v1.0/drives/{drive_id}/{tail}")));
                if tail.contains("search(q=") {
                    return (
                        StatusCode::OK,
                        Json(json!({
                            "value": [{
                                "id": "item-fallback-1",
                                "name": "report-fallback.md",
                                "size": 77,
                                "eTag": "\"fb-v1\"",
                                "file": { "mimeType": "text/markdown" },
                                "parentReference": { "driveId": drive_id, "id": "root", "path": "/drive/root:" }
                            }]
                        })),
                    )
                        .into_response();
                }
                StatusCode::NOT_FOUND.into_response()
            }),
        )
        .with_state(st.clone());

    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let (_cfg, graph, storage) = make_graph_and_storage(&base_url);

    // Seed 5 cached items including stale item-1
    let cached: Vec<DriveItem> = (1..=5)
        .map(|i| DriveItem {
            id: format!("item-{i}"),
            drive_id: "drv-me".into(),
            parent_id: Some("root".into()),
            parent_path: String::new(),
            name: if i == 1 {
                "report-alpha-stale.md".into()
            } else {
                format!("report-cached-{i}.md")
            },
            size: 10,
            e_tag: Some("\"stale-v1\"".into()),
            last_modified_at: Some(format!("2026-11-1{i}T00:00:00Z")),
            ..Default::default()
        })
        .collect();
    storage.upsert_drive_items(&cached).unwrap();

    // 1. First remote search with limit=5 returns fresh item-1 fields AND new remote item immediately
    let hits = cli::search_items_combined(&graph, &storage, "report", None, true, 5)
        .await
        .unwrap();
    assert_eq!(hits.len(), 5);
    assert_eq!(
        hits[0].id, "item-new-remote",
        "new remote item must appear in first limit=5 response: {hits:?}"
    );
    assert_eq!(hits[1].id, "item-1");
    assert_eq!(hits[1].name, "report-alpha-fresh.md");
    assert_eq!(hits[1].size, 999);
    assert_eq!(hits[1].e_tag.as_deref(), Some("\"fresh-v2\""));
    assert_eq!(
        hits.iter().filter(|i| i.id == "item-1").count(),
        1,
        "duplicate (drive_id, item_id) must appear only once with fresh remote object"
    );

    // 2. Cross-site 403 + personal fallback 500 MUST return error with both contexts (NOT Ok([]))
    *st.search_mode.lock().await = "forbidden_403".into();
    *st.me_drive_mode.lock().await = "error_500".into();
    let err = cli::search_items_combined(&graph, &storage, "report", None, true, 5)
        .await
        .expect_err("double failure (403 + 500) must fail instead of returning empty list");
    let err_str = format!("{err:#}");
    assert!(
        err_str.contains("403") && err_str.contains("500"),
        "error must preserve both initial 403 context and fallback 500 error, got: {err_str}"
    );

    // 3. Regression control: cross-site 403 + working personal drive fallback succeeds
    *st.me_drive_mode.lock().await = "ok".into();
    let fb_hits = cli::search_items_combined(&graph, &storage, "report", None, true, 5)
        .await
        .unwrap();
    assert_eq!(fb_hits[0].id, "item-fallback-1");

    // 4. Regression control: remote 200 with 0 hits succeeds without invoking /me/drive fallback
    *st.search_mode.lock().await = "empty_ok".into();
    let reqs_before_empty = st.requests.lock().await.len();
    let empty_remote = cli::search_items_combined(&graph, &storage, "nomatchxyz", None, true, 5)
        .await
        .unwrap();
    assert!(empty_remote.is_empty());
    assert!(
        !st.requests.lock().await[reqs_before_empty..]
            .iter()
            .any(|(_, p)| p == "/v1.0/me/drive"),
        "successful 0-hit cross-site search must not trigger personal drive fallback"
    );

    // 5. Regression control: local-only search sends 0 network requests
    let reqs_before_local = st.requests.lock().await.len();
    let local_hits = cli::search_items_combined(&graph, &storage, "report", None, false, 5)
        .await
        .unwrap();
    assert!(!local_hits.is_empty());
    assert_eq!(
        st.requests.lock().await.len(),
        reqs_before_local,
        "local-only search must send 0 network requests"
    );

    server.abort();
}

#[tokio::test]
async fn regression_sharepoint_sp_12_tree_propagates_child_folder_errors_with_path_and_status() {
    use tokio::process::Command;

    #[derive(Clone)]
    struct Sp12State {
        restricted_forbidden: Arc<AtomicBool>,
        child_requests: Arc<Mutex<Vec<String>>>,
    }

    let st = Sp12State {
        restricted_forbidden: Arc::new(AtomicBool::new(true)),
        child_requests: Arc::new(Mutex::new(Vec::new())),
    };

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}/v1.0");

    let router = Router::new()
        .route(
            "/v1.0/me/drive",
            get(|| async {
                Json(json!({ "id": "drv-me", "name": "OneDrive", "driveType": "business" }))
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/root/children",
            get(|Path(drive_id): Path<String>| async move {
                Json(json!({
                    "value": [
                        {
                            "id": "fld-empty",
                            "name": "EmptyFolder",
                            "size": 0,
                            "folder": { "childCount": 0 },
                            "parentReference": { "driveId": drive_id, "id": "root", "path": "/drive/root:" }
                        },
                        {
                            "id": "fld-restricted",
                            "name": "Restricted",
                            "size": 0,
                            "folder": { "childCount": 2 },
                            "parentReference": { "driveId": drive_id, "id": "root", "path": "/drive/root:" }
                        }
                    ]
                }))
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/items/{item_id}/children",
            get(|State(s): State<Sp12State>, Path((drive_id, item_id)): Path<(String, String)>| async move {
                s.child_requests.lock().await.push(item_id.clone());
                match item_id.as_str() {
                    "fld-empty" => (StatusCode::OK, Json(json!({ "value": [] }))).into_response(),
                    "fld-restricted" => {
                        if s.restricted_forbidden.load(Ordering::SeqCst) {
                            (
                                StatusCode::FORBIDDEN,
                                Json(json!({ "error": { "code": "accessDenied", "message": "Access denied" } })),
                            )
                                .into_response()
                        } else {
                            (
                                StatusCode::OK,
                                Json(json!({
                                    "value": [{
                                        "id": "fld-deep",
                                        "name": "DeepSub",
                                        "size": 0,
                                        "folder": { "childCount": 1 },
                                        "parentReference": { "driveId": drive_id, "id": "fld-restricted", "path": "/drive/root:/Restricted" }
                                    }]
                                })),
                            )
                                .into_response()
                        }
                    }
                    _ => (StatusCode::OK, Json(json!({ "value": [] }))).into_response(),
                }
            }),
        )
        .with_state(st.clone());

    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let profile = tempdir().unwrap();
    let run_cli = |args: Vec<String>| {
        let profile_path = profile.path().to_path_buf();
        let base_url = base_url.clone();
        async move {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_sharepoint"));
            cmd.kill_on_drop(true);
            for (k, _) in std::env::vars_os() {
                let u = k.to_string_lossy().to_ascii_uppercase();
                if ["SHAREPOINT_", "ONEDRIVE_", "TEAMS_", "AZURE_"]
                    .iter()
                    .any(|p| u.starts_with(p))
                {
                    cmd.env_remove(k);
                }
            }
            cmd.env("SHAREPOINT_GRAPH_BASE_URL", &base_url)
                .env("SHAREPOINT_ACCESS_TOKEN", "sp12-test-token")
                .env("SHAREPOINT_DISABLE_KEYRING", "1")
                .env("SHAREPOINT_SHARE_M365_AUTH", "0")
                .env("SHAREPOINT_AUTO_DAEMON", "0")
                .env("SHAREPOINT_CONFIG_DIR", profile_path.join("config"))
                .env("SHAREPOINT_DATA_DIR", profile_path.join("data"))
                .env("NO_PROXY", "127.0.0.1,localhost")
                .args(&args)
                .output()
                .await
                .unwrap()
        }
    };

    // 1. Depth 2 with Restricted returning 403 -> exit code 1, stderr has Restricted + 403, no partial success tree
    for extra in [vec!["--json"], vec![]] {
        let is_json = !extra.is_empty();
        let mut args = vec![
            "tree".into(),
            "--drive".into(),
            "me".into(),
            "--depth".into(),
            "2".into(),
        ];
        args.extend(extra.into_iter().map(str::to_string));
        let out = run_cli(args).await;
        assert_eq!(out.status.code(), Some(1), "tree with 403 child must exit with code 1");
        if is_json {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let lines: Vec<&str> = stdout.lines().collect();
            assert_eq!(lines.len(), 1, "expected single-line JSON error on stdout, got: {stdout}");
            let err_json: Value = serde_json::from_str(lines[0]).unwrap();
            assert_eq!(err_json["ok"], false);
            assert_eq!(err_json["error"]["code"], "forbidden");
            assert_eq!(err_json["error"]["http_status"], 403);
            assert!(err_json.get("tree").is_none());
        } else {
            assert!(
                out.stdout.is_empty(),
                "failed tree must not print partial/empty-children success tree, stdout={}",
                String::from_utf8_lossy(&out.stdout)
            );
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("Restricted") && stderr.contains("403"),
            "stderr must contain failing folder 'Restricted' and status '403', got: {stderr}"
        );
    }

    // 2. Regression control: depth 1 does NOT query subfolders beyond requested depth
    st.child_requests.lock().await.clear();
    let d1 = run_cli(vec![
        "tree".into(),
        "--drive".into(),
        "me".into(),
        "--depth".into(),
        "1".into(),
        "--json".into(),
    ])
    .await;
    assert!(d1.status.success());
    assert!(
        st.child_requests.lock().await.is_empty(),
        "depth 1 must not query child folders"
    );

    // 3. Regression control: truly empty folder succeeds with empty children, depth 2 doesn't query depth 3
    st.restricted_forbidden.store(false, Ordering::SeqCst);
    st.child_requests.lock().await.clear();
    let d2_ok = run_cli(vec![
        "tree".into(),
        "--drive".into(),
        "me".into(),
        "--depth".into(),
        "2".into(),
        "--json".into(),
    ])
    .await;
    assert!(d2_ok.status.success());
    let tree_json: Value = serde_json::from_slice(&d2_ok.stdout).unwrap();
    let nodes = tree_json["tree"].as_array().unwrap();
    let empty_node = nodes
        .iter()
        .find(|n| n["item"]["id"] == "fld-empty")
        .unwrap();
    assert!(empty_node["children"].as_array().unwrap().is_empty());
    assert!(
        !st.child_requests.lock().await.iter().any(|id| id == "fld-deep"),
        "depth 2 must not fetch children of depth-2 folder fld-deep"
    );

    server.abort();
}

#[tokio::test]
async fn regression_sharepoint_sp_15_limit_zero_returns_empty_and_preserves_cache() {
    use tokio::process::Command;

    #[derive(Clone)]
    struct Sp15State {
        list_calls: Arc<AtomicUsize>,
    }

    let st = Sp15State {
        list_calls: Arc::new(AtomicUsize::new(0)),
    };

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}/v1.0");

    let router = Router::new()
        .route(
            "/v1.0/me/drive",
            get(|| async {
                Json(json!({ "id": "drv-me", "name": "OneDrive", "driveType": "business" }))
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/root/children",
            get(|State(s): State<Sp15State>, Path(drive_id): Path<String>| async move {
                s.list_calls.fetch_add(1, Ordering::SeqCst);
                Json(json!({
                    "value": [
                        {
                            "id": "item-ls-1",
                            "name": "alpha-report.md",
                            "size": 12,
                            "file": { "mimeType": "text/markdown" },
                            "parentReference": { "driveId": drive_id, "id": "root", "path": "/drive/root:" }
                        },
                        {
                            "id": "item-ls-2",
                            "name": "beta-report.md",
                            "size": 24,
                            "file": { "mimeType": "text/markdown" },
                            "parentReference": { "driveId": drive_id, "id": "root", "path": "/drive/root:" }
                        }
                    ]
                }))
            }),
        )
        .route(
            "/v1.0/me/drive/recent",
            get(|State(s): State<Sp15State>| async move {
                s.list_calls.fetch_add(1, Ordering::SeqCst);
                Json(json!({
                    "value": [{
                        "id": "item-rec-1",
                        "name": "recent-report.md",
                        "size": 30,
                        "file": { "mimeType": "text/markdown" },
                        "parentReference": { "driveId": "drv-me", "id": "root", "path": "/drive/root:" }
                    }]
                }))
            }),
        )
        .route(
            "/v1.0/me/drive/sharedWithMe",
            get(|State(s): State<Sp15State>| async move {
                s.list_calls.fetch_add(1, Ordering::SeqCst);
                Json(json!({
                    "value": [{
                        "id": "item-sh-1",
                        "name": "shared-report.md",
                        "size": 40,
                        "file": { "mimeType": "text/markdown" },
                        "parentReference": { "driveId": "drv-me", "id": "root", "path": "/drive/root:" }
                    }]
                }))
            }),
        )
        .route(
            "/v1.0/search/query",
            post(|State(s): State<Sp15State>| async move {
                s.list_calls.fetch_add(1, Ordering::SeqCst);
                Json(json!({
                    "value": [{
                        "hitsContainers": [{
                            "hits": [{
                                "resource": {
                                    "id": "item-ls-1",
                                    "name": "alpha-report.md",
                                    "size": 12,
                                    "file": { "mimeType": "text/markdown" },
                                    "parentReference": { "driveId": "drv-me", "id": "root", "path": "/drive/root:" }
                                }
                            }]
                        }]
                    }]
                }))
            }),
        )
        .with_state(st.clone());

    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let profile = tempdir().unwrap();
    let run_cli = |args: &[&str]| {
        let profile_path = profile.path().to_path_buf();
        let base_url = base_url.clone();
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        async move {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_sharepoint"));
            cmd.kill_on_drop(true);
            for (k, _) in std::env::vars_os() {
                let u = k.to_string_lossy().to_ascii_uppercase();
                if ["SHAREPOINT_", "ONEDRIVE_", "TEAMS_", "AZURE_"]
                    .iter()
                    .any(|p| u.starts_with(p))
                {
                    cmd.env_remove(k);
                }
            }
            cmd.env("SHAREPOINT_GRAPH_BASE_URL", &base_url)
                .env("SHAREPOINT_ACCESS_TOKEN", "sp15-test-token")
                .env("SHAREPOINT_DISABLE_KEYRING", "1")
                .env("SHAREPOINT_SHARE_M365_AUTH", "0")
                .env("SHAREPOINT_AUTO_DAEMON", "0")
                .env("SHAREPOINT_CONFIG_DIR", profile_path.join("config"))
                .env("SHAREPOINT_DATA_DIR", profile_path.join("data"))
                .env("NO_PROXY", "127.0.0.1,localhost")
                .args(&args)
                .output()
                .await
                .unwrap()
        }
    };

    // Warm cache with positive/default limits
    let ls_warm = run_cli(&["ls", "--drive", "me", "--json"]).await;
    assert!(ls_warm.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&ls_warm.stdout).unwrap()["items"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let rec_warm = run_cli(&["recent", "--json"]).await;
    assert!(rec_warm.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&rec_warm.stdout)
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let sh_warm = run_cli(&["shared", "--json"]).await;
    assert!(sh_warm.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&sh_warm.stdout)
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // Run --limit 0 online and offline for ls, recent, shared, search -> ALL return length 0 with 0 page fetches!
    let calls_before_zero = st.list_calls.load(Ordering::SeqCst);

    for args in [
        &["ls", "--drive", "me", "--limit", "0", "--json"][..],
        &["ls", "--drive", "me", "--limit", "0", "--offline", "--json"][..],
    ] {
        let out = run_cli(args).await;
        assert!(out.status.success(), "{args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v["items"].as_array().unwrap().len(), 0, "{args:?} must return 0 items");
    }

    for args in [
        &["recent", "--limit", "0", "--json"][..],
        &["recent", "--limit", "0", "--offline", "--json"][..],
        &["shared", "--limit", "0", "--json"][..],
        &["shared", "--limit", "0", "--offline", "--json"][..],
        &["search", "report", "--limit", "0", "--remote", "--json"][..],
        &["search", "report", "--limit", "0", "--json"][..],
    ] {
        let out = run_cli(args).await;
        assert!(out.status.success(), "{args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 0, "{args:?} must return 0 items");
    }

    assert_eq!(
        st.list_calls.load(Ordering::SeqCst),
        calls_before_zero,
        "limit=0 requests must not start remote page collection"
    );

    // After limit=0, limit=1 offline MUST still see the original first cached item for every command!
    let ls_1 = run_cli(&["ls", "--drive", "me", "--limit", "1", "--offline", "--json"]).await;
    assert!(ls_1.status.success());
    let ls_1_json: Value = serde_json::from_slice(&ls_1.stdout).unwrap();
    assert_eq!(ls_1_json["items"].as_array().unwrap().len(), 1);
    assert_eq!(ls_1_json["items"][0]["id"], "item-ls-1");

    let rec_1 = run_cli(&["recent", "--limit", "1", "--offline", "--json"]).await;
    assert!(rec_1.status.success());
    let rec_1_json: Value = serde_json::from_slice(&rec_1.stdout).unwrap();
    assert_eq!(rec_1_json.as_array().unwrap().len(), 1);
    assert_eq!(rec_1_json[0]["id"], "item-rec-1");

    let sh_1 = run_cli(&["shared", "--limit", "1", "--offline", "--json"]).await;
    assert!(sh_1.status.success());
    let sh_1_json: Value = serde_json::from_slice(&sh_1.stdout).unwrap();
    assert_eq!(sh_1_json.as_array().unwrap().len(), 1);
    assert_eq!(sh_1_json[0]["id"], "item-sh-1");

    let search_1 = run_cli(&["search", "report", "--limit", "1", "--json"]).await;
    assert!(search_1.status.success());
    let search_1_json: Value = serde_json::from_slice(&search_1.stdout).unwrap();
    assert_eq!(search_1_json.as_array().unwrap().len(), 1);

    // bitefoundry-onedrive entrypoint also returns empty list for --limit 0 without touching cache
    let mut od_cmd = Command::new(env!("CARGO_BIN_EXE_bitefoundry-onedrive"));
    od_cmd.kill_on_drop(true);
    for (k, _) in std::env::vars_os() {
        let u = k.to_string_lossy().to_ascii_uppercase();
        if ["SHAREPOINT_", "ONEDRIVE_", "TEAMS_", "AZURE_"]
            .iter()
            .any(|p| u.starts_with(p))
        {
            od_cmd.env_remove(k);
        }
    }
    let od_zero = od_cmd
        .env("SHAREPOINT_GRAPH_BASE_URL", &base_url)
        .env("SHAREPOINT_ACCESS_TOKEN", "sp15-od-token")
        .env("SHAREPOINT_DISABLE_KEYRING", "1")
        .env("SHAREPOINT_SHARE_M365_AUTH", "0")
        .env("SHAREPOINT_AUTO_DAEMON", "0")
        .env("SHAREPOINT_CONFIG_DIR", profile.path().join("config"))
        .env("SHAREPOINT_DATA_DIR", profile.path().join("data"))
        .env("NO_PROXY", "127.0.0.1,localhost")
        .args(["ls", "--limit", "0", "--json"])
        .output()
        .await
        .unwrap();
    assert!(od_zero.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&od_zero.stdout).unwrap()["items"]
            .as_array()
            .unwrap()
            .len(),
        0
    );

    server.abort();
}
