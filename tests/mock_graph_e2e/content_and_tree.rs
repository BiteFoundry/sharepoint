//! HTTP content text/binary decoding, remote search ranking, tree error propagation, and limit=0 tests.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::routing::{get, post};
use axum::response::IntoResponse;
use axum::{Json, Router};
use serde_json::{json, Value};
use tempfile::tempdir;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use sharepoint_cli::serve;
use crate::server::{make_graph_and_storage, onedrive_alias_exe};

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
    let od_exe = onedrive_alias_exe(profile.path());
    let mut od_cmd = Command::new(&od_exe);
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
