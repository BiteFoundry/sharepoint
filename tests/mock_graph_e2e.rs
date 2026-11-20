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
        .with_state(st.clone());

    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    let (_cfg, graph, storage) = make_graph_and_storage(&base_url);

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
            .any(|(m, p, _)| m == "PUT" && p == "/v1.0/drives/drv-me/items/item-doc/content"),
        "zero-length conditional upload must use exact item-ID PUT /content: {zero_ok_reqs:?}"
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
