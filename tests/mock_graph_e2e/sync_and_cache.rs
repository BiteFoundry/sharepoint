//! Sync write targeting, conditional upload, partial limit, offline isolation, and cache pruning tests.

use std::sync::Arc;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::routing::{get, post, put};
use axum::response::IntoResponse;
use axum::{Json, Router};
use serde_json::{json, Value};
use tempfile::tempdir;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use sharepoint_cli::mcp;
use crate::server::make_graph_and_storage;

#[tokio::test]
async fn e2e_sync_fixes_write_targeting_conditional_upload_partial_limit_and_offline_isolation() {
    use sharepoint_cli::{
        cli,
        graph::drives::{ConflictBehavior, UploadOptions},
        resolver,
    };

    type ItemMap = std::collections::HashMap<(String, String), Value>;
    type ContentMap = std::collections::HashMap<(String, String), Vec<u8>>;
    type SessionMap = std::collections::HashMap<String, (String, Option<String>, String)>;
    type RequestLog = Vec<(String, String, Option<String>)>;

    #[derive(Clone)]
    struct SyncMock {
        base_url: String,
        items: Arc<Mutex<ItemMap>>,
        contents: Arc<Mutex<ContentMap>>,
        sessions: Arc<Mutex<SessionMap>>,
        followed_sites: Arc<Mutex<Vec<Value>>>,
        requests: Arc<Mutex<RequestLog>>,
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

