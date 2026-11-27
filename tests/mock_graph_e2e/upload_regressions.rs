//! Upload item_id targeting and copy-upload download baseline regression tests.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::routing::{get, post, put};
use axum::response::IntoResponse;
use axum::{Json, Router};
use serde_json::{json, Value};
use tempfile::tempdir;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use sharepoint_cli::storage::Storage;

#[tokio::test]
async fn regression_sharepoint_sp_02_small_and_large_upload_with_item_id_target_exact_item() {
    use sha2::{Digest, Sha256};
    use std::collections::HashMap;
    use tokio::process::Command;

    type ItemMap = HashMap<(String, String), Value>;
    type ContentMap = HashMap<(String, String), Vec<u8>>;
    type SessionMap = HashMap<String, (String, String, Vec<u8>)>;
    type RequestLog = Vec<(String, String, Option<String>)>;

    #[derive(Clone)]
    struct Sp02State {
        base_url: String,
        items: Arc<Mutex<ItemMap>>,
        contents: Arc<Mutex<ContentMap>>,
        sessions: Arc<Mutex<SessionMap>>,
        requests: Arc<Mutex<RequestLog>>,
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

    type ItemMap = HashMap<(String, String), Value>;
    type ContentMap = HashMap<(String, String), Vec<u8>>;
    type SessionMap = HashMap<String, (String, String, Vec<u8>)>;
    type RequestLog = Vec<(String, String, Option<String>)>;

    #[derive(Clone)]
    struct Sp05State {
        base_url: String,
        items: Arc<Mutex<ItemMap>>,
        contents: Arc<Mutex<ContentMap>>,
        sessions: Arc<Mutex<SessionMap>>,
        requests: Arc<Mutex<RequestLog>>,
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

