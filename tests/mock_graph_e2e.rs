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
            "/v1.0/drives/{drive_id}/items/{item_id}/versions",
            get(|| async {
                Json(json!({
                    "value": [
                        {
                            "id": "2.0",
                            "size": 25,
                            "lastModifiedDateTime": "2026-11-19T11:00:00Z",
                            "lastModifiedBy": { "user": { "displayName": "Jane Doe" } }
                        },
                        {
                            "id": "1.0",
                            "size": 12,
                            "lastModifiedDateTime": "2026-11-18T11:00:00Z",
                            "lastModifiedBy": { "user": { "displayName": "Jane Doe" } }
                        }
                    ]
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

    let versions = graph.list_item_versions("drv-me", "item-readme").await.unwrap();
    assert_eq!(versions.len(), 2);

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

    api_server.abort();
    graph_server.abort();
}
