//! Real-time sync integration tests against isolated loopback mock Microsoft Graph +
//! Engine.IO v4 / Socket.IO v4 WebSocket servers:
//! - Bootstrap gap closure (`?token=latest` -> `/children` -> delta catch-up during initial load)
//! - Multi-page delta cursor atomicity & idempotent retry on mid-pagination failure
//! - HTTP `410 Gone` resync (with `Location` header AND without `Location`/overflow) including root changes
//! - Direct Socket.IO v4 WebSocket notifications -> delta -> `FolderItemsUpdated` (<1s)
//! - Disconnect fallback polling -> reconnect catch-up from saved `sync_cursors` + `FocusGained`

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::{
    net::TcpListener,
    sync::{broadcast, mpsc, Mutex},
};
use tokio_tungstenite::tungstenite::Message;

use sharepoint_cli::{
    auth::{AuthManager, StoredTokens},
    config::AppConfig,
    graph::GraphClient,
    models::CoreEvent,
    storage::Storage,
    sync::{
        bootstrap_drive_with_delta, drive_cursor_scope_key, spawn_sync_engine,
        sync_drive_delta_once, DriveSyncRuntime, FocusTarget, FocusViewKind, SyncCommand,
        FOLDER_PAGE_SIZE,
    },
};

fn build_graph_client(base_url: String) -> GraphClient {
    let mut cfg = AppConfig::default();
    cfg.graph_base_url = base_url;
    let auth = AuthManager::from_tokens(
        cfg.clone(),
        StoredTokens {
            access_token: "synthetic-realtime-token".into(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: cfg.client_id.clone(),
            tenant_id: cfg.tenant_id.clone(),
            user_id: Some("user-rt".into()),
            user_display_name: Some("Realtime Tester".into()),
            user_email: Some("rt@synthetic.test".into()),
            auth_mode: "access_token".into(),
        },
    );
    GraphClient::new(cfg, auth).unwrap()
}

/// Serves a drive root whose size is the number in the drive id (`drv-250` holds 250 files),
/// 200 per page, plus a delta feed that reports an edit of `file-001` on every pull from `t1` on.
async fn spawn_sized_folder_graph() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}/v1.0");
    let base = base_url.clone();

    fn sized_item(drive_id: &str, i: usize) -> serde_json::Value {
        json!({
            "id": format!("item-{i:03}"),
            "name": format!("file-{i:03}.txt"),
            "size": 10,
            "file": { "mimeType": "text/plain" },
            "parentReference": { "driveId": drive_id, "id": "real-root-id", "path": "/drive/root:" }
        })
    }

    let app = Router::new()
        .route(
            "/v1.0/drives/{drive_id}/root/children",
            get({
                let base = base.clone();
                move |Path(drive_id): Path<String>, Query(q): Query<HashMap<String, String>>| {
                    let base = base.clone();
                    async move {
                        let total: usize = drive_id.trim_start_matches("drv-").parse().unwrap();
                        let page: usize = q.get("page").and_then(|p| p.parse().ok()).unwrap_or(1);
                        let first = (page - 1) * 200 + 1;
                        let last = (page * 200).min(total);
                        let value: Vec<serde_json::Value> =
                            (first..=last).map(|i| sized_item(&drive_id, i)).collect();
                        let mut body = json!({ "value": value });
                        if last < total {
                            body["@odata.nextLink"] = json!(format!(
                                "{base}/drives/{drive_id}/root/children?page={}",
                                page + 1
                            ));
                        }
                        Json(body)
                    }
                }
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/root/delta",
            get({
                let base = base.clone();
                move |Path(drive_id): Path<String>, Query(q): Query<HashMap<String, String>>| {
                    let base = base.clone();
                    async move {
                        let link = |t: &str| format!("{base}/drives/{drive_id}/root/delta?token={t}");
                        let token = q.get("token").map(String::as_str).unwrap_or("");
                        let round: usize = token.trim_start_matches('t').parse().unwrap_or(0);
                        match token {
                            "latest" => Json(json!({ "value": [], "@odata.deltaLink": link("t0") })),
                            "t0" => Json(json!({ "value": [], "@odata.deltaLink": link("t1") })),
                            // Every later pull reports one more edit of the first file.
                            _ => Json(json!({
                                "value": [
                                    { "id": "real-root-id", "root": {} },
                                    {
                                        "id": "item-001",
                                        "name": "file-001.txt",
                                        "size": 98 + round,
                                        "file": { "mimeType": "text/plain" },
                                        "parentReference": { "driveId": drive_id, "id": "real-root-id" }
                                    }
                                ],
                                "@odata.deltaLink": link(&format!("t{}", round + 1))
                            })),
                        }
                    }
                }
            }),
        );

    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (base_url, server)
}

fn last_folder_event(rx: &mut mpsc::UnboundedReceiver<CoreEvent>) -> (usize, bool) {
    let mut last = None;
    while let Ok(ev) = rx.try_recv() {
        if let CoreEvent::FolderItemsUpdated { items, truncated, .. } = ev {
            last = Some((items.len(), truncated));
        }
    }
    last.expect("a FolderItemsUpdated event")
}

#[tokio::test]
async fn folder_items_updated_reports_truncation() {
    let (base_url, server) = spawn_sized_folder_graph().await;
    let graph = build_graph_client(base_url);
    let storage = Storage::open_in_memory().unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel::<CoreEvent>();
    let mut rt = DriveSyncRuntime::default();

    // 250 files on the server: the first load shows 200 and says there are more.
    bootstrap_drive_with_delta(&graph, &storage, &tx, &mut rt, "drv-250", None, FOLDER_PAGE_SIZE)
        .await
        .unwrap();
    assert_eq!(last_folder_event(&mut rx), (200, true));
    // The cache holds exactly one page, so the row count alone would say "complete".
    assert_eq!(storage.list_folder_children("drv-250", None, None, 1000).unwrap().len(), 200);

    // A delta-driven refresh reads the cache and must keep the mark.
    sync_drive_delta_once(&graph, &storage, &tx, &mut rt, "drv-250", None, FOLDER_PAGE_SIZE)
        .await
        .unwrap();
    assert_eq!(last_folder_event(&mut rx), (200, true));
    assert_eq!(storage.get_drive_item("drv-250", "item-001").unwrap().unwrap().size, 99);

    // A truncated listing never prunes: a cached row beyond the first page survives it.
    storage
        .upsert_drive_items(&[sharepoint_cli::models::DriveItem {
            id: "item-240".into(),
            drive_id: "drv-250".into(),
            name: "file-240.txt".into(),
            parent_id: Some("real-root-id".into()),
            ..Default::default()
        }])
        .unwrap();
    rt.delta_unsupported_drives.insert("drv-250".to_string());
    sync_drive_delta_once(&graph, &storage, &tx, &mut rt, "drv-250", None, FOLDER_PAGE_SIZE)
        .await
        .unwrap();
    assert_eq!(last_folder_event(&mut rx), (200, true));
    assert!(storage.get_drive_item("drv-250", "item-240").unwrap().is_some());

    // Exactly 200 files: everything is shown and nothing is marked.
    bootstrap_drive_with_delta(&graph, &storage, &tx, &mut rt, "drv-200", None, FOLDER_PAGE_SIZE)
        .await
        .unwrap();
    assert_eq!(last_folder_event(&mut rx), (200, false));

    server.abort();
}

#[tokio::test]
async fn raised_folder_limit_lists_the_whole_folder_and_replaces_the_cache() {
    let (base_url, server) = spawn_sized_folder_graph().await;
    let graph = build_graph_client(base_url);
    let storage = Storage::open_in_memory().unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel::<CoreEvent>();
    let mut rt = DriveSyncRuntime::default();

    bootstrap_drive_with_delta(&graph, &storage, &tx, &mut rt, "drv-250", None, FOLDER_PAGE_SIZE)
        .await
        .unwrap();
    assert_eq!(last_folder_event(&mut rx), (200, true));

    // A cached row the server no longer has: only a complete listing may drop it.
    storage
        .upsert_drive_items(&[sharepoint_cli::models::DriveItem {
            id: "item-stale".into(),
            drive_id: "drv-250".into(),
            name: "stale.txt".into(),
            parent_id: Some("real-root-id".into()),
            ..Default::default()
        }])
        .unwrap();
    rt.delta_unsupported_drives.insert("drv-250".to_string());
    sync_drive_delta_once(&graph, &storage, &tx, &mut rt, "drv-250", None, FOLDER_PAGE_SIZE)
        .await
        .unwrap();
    assert_eq!(last_folder_event(&mut rx), (200, true));
    assert!(storage.get_drive_item("drv-250", "item-stale").unwrap().is_some());

    // One more page asked for: 250 rows, nothing left on the server, cache made exact.
    sync_drive_delta_once(&graph, &storage, &tx, &mut rt, "drv-250", None, 400)
        .await
        .unwrap();
    assert_eq!(last_folder_event(&mut rx), (250, false));
    assert!(storage.get_drive_item("drv-250", "item-stale").unwrap().is_none());
    assert_eq!(storage.list_folder_children("drv-250", None, None, 1000).unwrap().len(), 250);

    // Back at one page the folder is cut short again, with all 250 rows still cached.
    sync_drive_delta_once(&graph, &storage, &tx, &mut rt, "drv-250", None, FOLDER_PAGE_SIZE)
        .await
        .unwrap();
    assert_eq!(last_folder_event(&mut rx), (200, true));
    assert_eq!(storage.list_folder_children("drv-250", None, None, 1000).unwrap().len(), 250);

    server.abort();
}

#[tokio::test]
async fn sync_engine_honours_the_folder_limit_from_command_and_focus() {
    async fn next_folder_event(
        rx: &mut mpsc::UnboundedReceiver<CoreEvent>,
        wanted: Option<(usize, bool)>,
    ) -> (usize, bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let ev = tokio::time::timeout(left, rx.recv())
                .await
                .unwrap_or_else(|_| panic!("no folder event matching {wanted:?}"))
                .expect("engine alive");
            if let CoreEvent::FolderItemsUpdated { items, truncated, .. } = ev {
                let got = (items.len(), truncated);
                if wanted.is_none_or(|w| w == got) {
                    return got;
                }
            }
        }
    }

    let (base_url, server) = spawn_sized_folder_graph().await;
    let cfg = AppConfig {
        graph_base_url: base_url.clone(),
        ..Default::default()
    };
    let graph = build_graph_client(base_url);
    let storage = Storage::open_in_memory().unwrap();
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<CoreEvent>();
    let ctrl = spawn_sync_engine(cfg, graph, storage, ev_tx);

    let focus = |active_limit: Option<usize>| FocusTarget {
        active_drive_id: Some("drv-250".into()),
        active_parent_id: None,
        active_site_id: None,
        view_kind: FocusViewKind::Folder,
        active_limit,
    };
    let sync_folder = |limit: Option<usize>| SyncCommand::SyncFolder {
        drive_id: "drv-250".into(),
        parent_id: None,
        reuse_startup_root: false,
        limit,
    };

    // A client that sends no limit gets one page, as before.
    ctrl.set_focus(focus(None));
    ctrl.request(sync_folder(None));
    assert_eq!(next_folder_event(&mut ev_rx, None).await, (200, true));

    // "Load more": the command lists with the raised limit.
    ctrl.set_focus(focus(Some(400)));
    ctrl.request(sync_folder(Some(400)));
    next_folder_event(&mut ev_rx, Some((250, false))).await;

    // From here every refresh, delta-driven ones included, must use the focus limit.
    ctrl.request(SyncCommand::FocusGained);
    assert_eq!(next_folder_event(&mut ev_rx, None).await, (250, false));
    ctrl.request(SyncCommand::FocusGained);
    assert_eq!(next_folder_event(&mut ev_rx, None).await, (250, false));

    ctrl.request(SyncCommand::Shutdown);
    server.abort();
}

#[tokio::test]
async fn bootstrap_gap_and_multipage_delta_cursor_atomicity_and_idempotent_retry() {
    #[derive(Clone)]
    struct DeltaTestState {
        base_url: String,
        fail_page2_once: Arc<AtomicBool>,
        page2_attempts: Arc<AtomicUsize>,
        bootstrap_gap_injected: Arc<AtomicBool>,
    }

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}/v1.0");
    let st = DeltaTestState {
        base_url: base_url.clone(),
        fail_page2_once: Arc::new(AtomicBool::new(true)),
        page2_attempts: Arc::new(AtomicUsize::new(0)),
        bootstrap_gap_injected: Arc::new(AtomicBool::new(false)),
    };

    let app = Router::new()
        .route(
            "/v1.0/drives/{drive_id}/root/children",
            get(|State(st): State<DeltaTestState>, Path(drive_id): Path<String>| async move {
                // Mark that an external edit happened during initial folder fetch (between T0 and T1)
                st.bootstrap_gap_injected.store(true, Ordering::SeqCst);
                Json(json!({
                    "value": [
                        {
                            "id": "item-doc",
                            "name": "doc.md",
                            "size": 10,
                            "eTag": "\"etag-v1\"",
                            "cTag": "\"ctag-v1\"",
                            "file": { "mimeType": "text/markdown" },
                            "parentReference": { "driveId": drive_id, "id": "real-root-id", "path": "/drive/root:" }
                        },
                        {
                            "id": "item-gone",
                            "name": "gone.txt",
                            "size": 5,
                            "eTag": "\"etag-g1\"",
                            "cTag": "\"ctag-g1\"",
                            "file": { "mimeType": "text/plain" },
                            "parentReference": { "driveId": drive_id, "id": "real-root-id", "path": "/drive/root:" }
                        }
                    ]
                }))
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/root/delta",
            get(|State(st): State<DeltaTestState>, Path(drive_id): Path<String>, Query(q): Query<HashMap<String, String>>| async move {
                let token = q.get("token").map(String::as_str).unwrap_or("");
                match token {
                    "latest" => {
                        Json(json!({
                            "value": [],
                            "@odata.deltaLink": format!("{}/drives/{drive_id}/root/delta?token=t0", st.base_url)
                        })).into_response()
                    }
                    "t0" => {
                        // Catch-up for edit that occurred DURING initial folder load!
                        assert!(st.bootstrap_gap_injected.load(Ordering::SeqCst));
                        Json(json!({
                            "value": [
                                { "id": "real-root-id", "root": {} },
                                {
                                    "id": "item-doc",
                                    "name": "doc.md",
                                    "size": 22,
                                    "eTag": "\"etag-v2-gap\"",
                                    "cTag": "\"ctag-v2-gap\"",
                                    "file": { "mimeType": "text/markdown" },
                                    // Omitted parentReference.path as in real Graph delta!
                                    "parentReference": { "driveId": drive_id, "id": "real-root-id" }
                                }
                            ],
                            "@odata.deltaLink": format!("{}/drives/{drive_id}/root/delta?token=t1", st.base_url)
                        })).into_response()
                    }
                    "t1" => {
                        // Multi-page delta: page 1 returns rename + subfolder + nextLink to page 2
                        Json(json!({
                            "value": [
                                {
                                    "id": "fld-sub",
                                    "name": "Sub",
                                    "folder": { "childCount": 1 },
                                    "parentReference": { "driveId": drive_id, "id": "real-root-id" }
                                },
                                {
                                    "id": "item-doc",
                                    "name": "doc-renamed.md",
                                    "size": 22,
                                    "eTag": "\"etag-v3-ren\"",
                                    "cTag": "\"ctag-v2-gap\"",
                                    "file": { "mimeType": "text/markdown" },
                                    "parentReference": { "driveId": drive_id, "id": "real-root-id" }
                                }
                            ],
                            "@odata.nextLink": format!("{}/drives/{drive_id}/root/delta?token=t1_p2", st.base_url)
                        })).into_response()
                    }
                    "t1_p2" => {
                        st.page2_attempts.fetch_add(1, Ordering::SeqCst);
                        if st.fail_page2_once.swap(false, Ordering::SeqCst) {
                            return (StatusCode::INTERNAL_SERVER_ERROR, "transient page 2 failure").into_response();
                        }
                        Json(json!({
                            "value": [
                                {
                                    "id": "item-sub-file",
                                    "name": "nested.txt",
                                    "size": 15,
                                    "file": { "mimeType": "text/plain" },
                                    // Omitted parentReference.path -> must resolve to /Sub and NOT leak into root!
                                    "parentReference": { "driveId": drive_id, "id": "fld-sub" }
                                },
                                {
                                    "id": "item-gone",
                                    "deleted": { "state": "deleted" }
                                }
                            ],
                            "@odata.deltaLink": format!("{}/drives/{drive_id}/root/delta?token=t2", st.base_url)
                        })).into_response()
                    }
                    _ => (StatusCode::BAD_REQUEST, "unexpected token").into_response(),
                }
            }),
        )
        .with_state(st.clone());

    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let graph = build_graph_client(base_url.clone());
    let storage = Storage::open_in_memory().unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel::<CoreEvent>();
    let mut rt = DriveSyncRuntime::default();

    // 1. Bootstrap with gap closure: edit during initial load must be reflected AND cursor persisted at t1
    bootstrap_drive_with_delta(&graph, &storage, &tx, &mut rt, "drv-1", None, FOLDER_PAGE_SIZE)
        .await
        .unwrap();
    let doc = storage.get_drive_item("drv-1", "item-doc").unwrap().unwrap();
    assert_eq!(doc.c_tag.as_deref(), Some("\"ctag-v2-gap\""));
    assert_eq!(doc.size, 22);
    let cursor_t1 = storage
        .get_sync_cursor(&drive_cursor_scope_key("drv-1"))
        .unwrap()
        .unwrap();
    assert!(cursor_t1.delta_link.as_deref().unwrap().ends_with("token=t1"));

    // 2. Multi-page delta where Page 2 fails on attempt 1 -> cursor MUST NOT advance!
    let err = sync_drive_delta_once(&graph, &storage, &tx, &mut rt, "drv-1", None, FOLDER_PAGE_SIZE)
        .await
        .expect_err("page 2 transient 500 must surface error");
    assert!(format!("{err:#}").contains("500"));
    let cursor_after_fail = storage
        .get_sync_cursor(&drive_cursor_scope_key("drv-1"))
        .unwrap()
        .unwrap();
    assert_eq!(
        cursor_after_fail.delta_link, cursor_t1.delta_link,
        "cursor must NOT advance when intermediate/final page fails"
    );

    // 3. Retry replays from t1 idempotently on same IDs and advances cursor to t2 after page 2 commits
    sync_drive_delta_once(&graph, &storage, &tx, &mut rt, "drv-1", None, FOLDER_PAGE_SIZE)
        .await
        .unwrap();
    assert_eq!(st.page2_attempts.load(Ordering::SeqCst), 2);
    let cursor_t2 = storage
        .get_sync_cursor(&drive_cursor_scope_key("drv-1"))
        .unwrap()
        .unwrap();
    assert!(cursor_t2.delta_link.as_deref().unwrap().ends_with("token=t2"));

    // Verify root children: doc-renamed.md + Sub (gone.txt deleted; nested.txt stayed in /Sub, NOT leaked to root!)
    let root_items = storage.list_folder_children("drv-1", None, None, 50).unwrap();
    let root_names: Vec<&str> = root_items.iter().map(|i| i.name.as_str()).collect();
    assert!(root_names.contains(&"doc-renamed.md"), "{root_names:?}");
    assert!(root_names.contains(&"Sub"), "{root_names:?}");
    assert!(!root_names.contains(&"gone.txt"), "{root_names:?}");
    assert!(
        !root_names.contains(&"nested.txt"),
        "subfolder delta item with omitted parentReference.path must never leak into root: {root_names:?}"
    );
    let sub_items = storage
        .list_folder_children("drv-1", Some("fld-sub"), None, 50)
        .unwrap();
    assert_eq!(sub_items.len(), 1);
    assert_eq!(sub_items[0].name, "nested.txt");
    assert_eq!(sub_items[0].parent_path, "/Sub");

    // Drain events to confirm FolderItemsUpdated was emitted
    let mut saw_renamed_event = false;
    while let Ok(ev) = rx.try_recv() {
        if let CoreEvent::FolderItemsUpdated { items, .. } = ev {
            if items.iter().any(|i| i.name == "doc-renamed.md") {
                saw_renamed_event = true;
            }
        }
    }
    assert!(saw_renamed_event);

    server.abort();
}

#[tokio::test]
async fn delta_410_gone_with_location_and_without_location_reconciles_root_deletions() {
    #[derive(Clone)]
    struct GoneState {
        base_url: String,
        use_location: Arc<AtomicBool>,
    }

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}/v1.0");
    let st = GoneState {
        base_url: base_url.clone(),
        use_location: Arc::new(AtomicBool::new(true)),
    };

    let app = Router::new()
        .route(
            "/v1.0/drives/{drive_id}/root/delta",
            get(|State(st): State<GoneState>, Path(drive_id): Path<String>, Query(q): Query<HashMap<String, String>>| async move {
                let token = q.get("token").map(String::as_str).unwrap_or("");
                match token {
                    "expired" => {
                        if st.use_location.load(Ordering::SeqCst) {
                            let loc = format!("{}/drives/{drive_id}/root/delta?token=resync_loc", st.base_url);
                            (StatusCode::GONE, [(header::LOCATION, loc)], Json(json!({"error": {"code": "resyncRequired"}}))).into_response()
                        } else {
                            (StatusCode::GONE, Json(json!({"error": {"code": "syncStateNotFound"}}))).into_response()
                        }
                    }
                    "resync_loc" => {
                        // Authoritative full resync: root-old is gone, root-fresh exists
                        Json(json!({
                            "value": [
                                { "id": "real-root", "root": {} },
                                {
                                    "id": "root-fresh",
                                    "name": "fresh-after-410.md",
                                    "size": 19,
                                    "file": { "mimeType": "text/markdown" },
                                    "parentReference": { "driveId": drive_id, "id": "real-root" }
                                }
                            ],
                            "@odata.deltaLink": format!("{}/drives/{drive_id}/root/delta?token=post_410_loc", st.base_url)
                        })).into_response()
                    }
                    "latest" => {
                        Json(json!({
                            "value": [],
                            "@odata.deltaLink": format!("{}/drives/{drive_id}/root/delta?token=post_410_noloc", st.base_url)
                        })).into_response()
                    }
                    "post_410_noloc" => {
                        Json(json!({
                            "value": [],
                            "@odata.deltaLink": format!("{}/drives/{drive_id}/root/delta?token=post_410_noloc_done", st.base_url)
                        })).into_response()
                    }
                    _ => (StatusCode::BAD_REQUEST, "unexpected token").into_response(),
                }
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/root/children",
            get(|Path(drive_id): Path<String>| async move {
                Json(json!({
                    "value": [
                        {
                            "id": "fld-sub",
                            "name": "Sub",
                            "folder": { "childCount": 1 },
                            "parentReference": { "driveId": drive_id, "id": "real-root", "path": "/drive/root:" }
                        },
                        {
                            "id": "root-after-fallback",
                            "name": "root-reconciled.md",
                            "size": 11,
                            "file": { "mimeType": "text/markdown" },
                            "parentReference": { "driveId": drive_id, "id": "real-root", "path": "/drive/root:" }
                        }
                    ]
                }))
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/items/{item_id}/children",
            get(|Path((drive_id, item_id)): Path<(String, String)>| async move {
                Json(json!({
                    "value": [
                        {
                            "id": "sub-live",
                            "name": "sub-live.md",
                            "size": 9,
                            "file": { "mimeType": "text/markdown" },
                            "parentReference": { "driveId": drive_id, "id": item_id, "path": "/drive/root:/Sub" }
                        }
                    ]
                }))
            }),
        )
        .with_state(st.clone());

    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let graph = build_graph_client(base_url.clone());
    let storage = Storage::open_in_memory().unwrap();
    let (tx, _rx) = mpsc::unbounded_channel::<CoreEvent>();
    let mut rt = DriveSyncRuntime::default();

    // Seed stale root item + expired cursor
    storage
        .upsert_drive_items(&[sharepoint_cli::models::DriveItem {
            id: "root-old".into(),
            drive_id: "drv-1".into(),
            parent_id: Some("real-root".into()),
            parent_path: "".into(),
            name: "deleted-before-410.md".into(),
            size: 5,
            ..Default::default()
        }])
        .unwrap();
    storage
        .upsert_sync_cursor(&sharepoint_cli::models::SyncCursor {
            scope_key: drive_cursor_scope_key("drv-1"),
            delta_link: Some(format!("{base_url}/drives/drv-1/root/delta?token=expired")),
            history_next_link: Some("root_id:real-root".into()),
            last_synced_at: "2026-11-20T00:00:00Z".into(),
        })
        .unwrap();

    // Case A: 410 Gone WITH Location header prunes root-old and installs root-fresh
    sync_drive_delta_once(&graph, &storage, &tx, &mut rt, "drv-1", None, FOLDER_PAGE_SIZE)
        .await
        .unwrap();
    assert!(storage.get_drive_item("drv-1", "root-old").unwrap().is_none());
    assert!(storage.get_drive_item("drv-1", "root-fresh").unwrap().is_some());
    assert!(storage
        .get_sync_cursor(&drive_cursor_scope_key("drv-1"))
        .unwrap()
        .unwrap()
        .delta_link
        .unwrap()
        .ends_with("token=post_410_loc"));

    // Case B: 410 Gone WITHOUT Location header while focused inside subfolder `fld-sub`:
    // MUST still reconcile root (pruning root-fresh, adding root-after-fallback) AND reconcile fld-sub!
    st.use_location.store(false, Ordering::SeqCst);
    storage
        .upsert_sync_cursor(&sharepoint_cli::models::SyncCursor {
            scope_key: drive_cursor_scope_key("drv-1"),
            delta_link: Some(format!("{base_url}/drives/drv-1/root/delta?token=expired")),
            history_next_link: Some("root_id:real-root".into()),
            last_synced_at: "2026-11-20T00:00:00Z".into(),
        })
        .unwrap();

    sync_drive_delta_once(&graph, &storage, &tx, &mut rt, "drv-1", Some("fld-sub"), FOLDER_PAGE_SIZE)
        .await
        .unwrap();
    assert!(
        storage.get_drive_item("drv-1", "root-fresh").unwrap().is_none(),
        "stale root item must be pruned on 410 fallback even while focused in subfolder"
    );
    assert!(storage
        .get_drive_item("drv-1", "root-after-fallback")
        .unwrap()
        .is_some());
    assert!(storage.get_drive_item("drv-1", "sub-live").unwrap().is_some());
    assert!(storage
        .get_sync_cursor(&drive_cursor_scope_key("drv-1"))
        .unwrap()
        .unwrap()
        .delta_link
        .unwrap()
        .ends_with("token=post_410_noloc_done"));

    server.abort();
}

#[tokio::test]
async fn direct_socketio_push_delta_fallback_reconnect_and_focus_gained() {
    #[derive(Clone)]
    struct LiveMockState {
        base_url: String,
        ws_url: String,
        socket_enabled: Arc<AtomicBool>,
        delta_log: Arc<Mutex<Vec<Value>>>,
        items: Arc<Mutex<Vec<Value>>>,
        delta_http_calls: Arc<AtomicUsize>,
    }

    // 1. Start isolated Engine.IO v4 / Socket.IO v4 WebSocket server
    let ws_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_addr = ws_listener.local_addr().unwrap();
    let (push_tx, _) = broadcast::channel::<String>(32);
    let (drop_tx, _) = broadcast::channel::<()>(8);
    let ws_accept_count = Arc::new(AtomicUsize::new(0));

    let ws_push_tx = push_tx.clone();
    let ws_drop_tx = drop_tx.clone();
    let ws_accepts = Arc::clone(&ws_accept_count);
    let ws_server = tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = ws_listener.accept().await else {
                break;
            };
            ws_accepts.fetch_add(1, Ordering::SeqCst);
            let mut rx_push = ws_push_tx.subscribe();
            let mut rx_drop = ws_drop_tx.subscribe();
            tokio::spawn(async move {
                let Ok(mut ws) = tokio_tungstenite::accept_async(tcp).await else {
                    return;
                };
                // Engine.IO v4 OPEN
                let _ = ws
                    .send(Message::Text(
                        r#"0{"sid":"sio-live","upgrades":[],"pingInterval":25000,"pingTimeout":20000}"#
                            .into(),
                    ))
                    .await;
                // Expect Socket.IO v4 custom namespace CONNECT
                if let Some(Ok(Message::Text(txt))) = ws.next().await {
                    if txt.as_str() == "40/zbaehwg/callback," {
                        let _ = ws
                            .send(Message::Text(
                                r#"40/zbaehwg/callback,{"sid":"ns-live"}"#.into(),
                            ))
                            .await;
                    }
                }
                loop {
                    tokio::select! {
                        Ok(pkt) = rx_push.recv() => {
                            if ws.send(Message::Text(pkt.into())).await.is_err() {
                                break;
                            }
                        }
                        Ok(()) = rx_drop.recv() => {
                            let _ = ws.close(None).await;
                            break;
                        }
                        msg = ws.next() => {
                            match msg {
                                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                                _ => {}
                            }
                        }
                    }
                }
            });
        }
    });

    // 2. Start isolated mock Graph HTTP server
    let http_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_addr = http_listener.local_addr().unwrap();
    let base_url = format!("http://{http_addr}/v1.0");
    let st = LiveMockState {
        base_url: base_url.clone(),
        ws_url: format!("http://{ws_addr}/zbaehwg/callback?snthgk=live-tok"),
        socket_enabled: Arc::new(AtomicBool::new(true)),
        delta_log: Arc::new(Mutex::new(Vec::new())),
        items: Arc::new(Mutex::new(vec![json!({
            "id": "item-doc",
            "name": "doc.md",
            "size": 10,
            "eTag": "\"etag-v1\"",
            "cTag": "\"ctag-v1\"",
            "file": { "mimeType": "text/markdown" },
            "parentReference": { "driveId": "drv-me", "id": "root", "path": "/drive/root:" }
        })])),
        delta_http_calls: Arc::new(AtomicUsize::new(0)),
    };

    let router = Router::new()
        .route(
            "/v1.0/drives/{drive_id}/root/subscriptions/socketIo",
            get(|State(st): State<LiveMockState>| async move {
                if !st.socket_enabled.load(Ordering::SeqCst) {
                    return (
                        StatusCode::PROXY_AUTHENTICATION_REQUIRED,
                        "proxy blocked websocket endpoint",
                    )
                        .into_response();
                }
                Json(json!({
                    "id": "sub-live-1",
                    "notificationUrl": st.ws_url
                }))
                .into_response()
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/root/children",
            get(|State(st): State<LiveMockState>| async move {
                let items = st.items.lock().await.clone();
                Json(json!({ "value": items }))
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/root/delta",
            get(|State(st): State<LiveMockState>, Path(drive_id): Path<String>, Query(q): Query<HashMap<String, String>>| async move {
                st.delta_http_calls.fetch_add(1, Ordering::SeqCst);
                let token = q.get("token").map(String::as_str).unwrap_or("0");
                let log = st.delta_log.lock().await;
                if token == "latest" {
                    let cur = log.len();
                    return Json(json!({
                        "value": [],
                        "@odata.deltaLink": format!("{}/drives/{drive_id}/root/delta?token={cur}", st.base_url)
                    }));
                }
                let start: usize = token.parse().unwrap_or(0).min(log.len());
                let slice: Vec<Value> = log[start..].to_vec();
                let next_cur = log.len();
                Json(json!({
                    "value": slice,
                    "@odata.deltaLink": format!("{}/drives/{drive_id}/root/delta?token={next_cur}", st.base_url)
                }))
            }),
        )
        .with_state(st.clone());

    let http_server = tokio::spawn(async move {
        let _ = axum::serve(http_listener, router).await;
    });

    let mut cfg = AppConfig::default();
    cfg.graph_base_url = base_url.clone();
    cfg.active_poll_ms = 1_000; // fast fallback for test
    let graph = build_graph_client(base_url.clone());
    let storage = Storage::open_in_memory().unwrap();
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<CoreEvent>();

    let ctrl = spawn_sync_engine(cfg, graph, storage, ev_tx);
    ctrl.set_focus(FocusTarget {
        active_drive_id: Some("drv-me".into()),
        active_parent_id: None,
        active_site_id: None,
        view_kind: FocusViewKind::Folder,
        active_limit: None,
    });

    // Wait for Socket.IO connection + initial folder bootstrap
    let mut saw_live = false;
    let mut saw_v1 = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !(saw_live && saw_v1) && Instant::now() < deadline {
        if let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(500), ev_rx.recv()).await {
            match ev {
                CoreEvent::SyncStateChanged { summary, .. } if summary.contains("Socket.IO") => {
                    saw_live = true;
                }
                CoreEvent::FolderItemsUpdated { items, .. }
                    if items.iter().any(|i| i.name == "doc.md") =>
                {
                    saw_v1 = true;
                }
                _ => {}
            }
        }
    }
    assert!(saw_live, "expected Socket.IO Live state");
    assert!(saw_v1, "expected initial bootstrap folder items");

    // 3. Measure direct Socket.IO push -> delta -> FolderItemsUpdated latency (must be < 1000 ms!)
    let t_push = Instant::now();
    {
        let mut log = st.delta_log.lock().await;
        log.push(json!({
            "id": "item-doc",
            "name": "doc-pushed.md",
            "size": 33,
            "eTag": "\"etag-v2\"",
            "cTag": "\"ctag-v2\"",
            "file": { "mimeType": "text/markdown" },
            "parentReference": { "driveId": "drv-me", "id": "root" }
        }));
    }
    let _ = push_tx.send(r#"42/zbaehwg/callback,["notification",{"subscriptionId":"sub-live-1"}]"#.to_string());

    let mut pushed_ms = None;
    while t_push.elapsed() < Duration::from_secs(3) {
        if let Ok(Some(CoreEvent::FolderItemsUpdated { items, .. })) =
            tokio::time::timeout(Duration::from_millis(200), ev_rx.recv()).await
        {
            if items.iter().any(|i| i.name == "doc-pushed.md" && i.size == 33) {
                pushed_ms = Some(t_push.elapsed().as_millis());
                break;
            }
        }
    }
    let pushed_ms = pushed_ms.expect("expected push-triggered FolderItemsUpdated");
    assert!(
        pushed_ms < 1000,
        "Socket.IO push -> event must be < 1000 ms, took {pushed_ms} ms"
    );

    // 4. FocusGained triggers immediate delta catch-up without needing a Socket.IO notification
    {
        let mut log = st.delta_log.lock().await;
        log.push(json!({
            "id": "item-focus",
            "name": "focus-added.txt",
            "size": 7,
            "file": { "mimeType": "text/plain" },
            "parentReference": { "driveId": "drv-me", "id": "root" }
        }));
    }
    ctrl.request(SyncCommand::FocusGained);
    let mut saw_focus = false;
    let t_focus = Instant::now();
    while t_focus.elapsed() < Duration::from_secs(2) {
        if let Ok(Some(CoreEvent::FolderItemsUpdated { items, .. })) =
            tokio::time::timeout(Duration::from_millis(200), ev_rx.recv()).await
        {
            if items.iter().any(|i| i.name == "focus-added.txt") {
                saw_focus = true;
                break;
            }
        }
    }
    assert!(saw_focus, "FocusGained must trigger immediate delta catch-up");

    // 5. Simulate proxy block / disconnect -> automatic delta polling fallback catches up!
    st.socket_enabled.store(false, Ordering::SeqCst);
    let _ = drop_tx.send(());
    tokio::time::sleep(Duration::from_millis(150)).await;
    {
        let mut log = st.delta_log.lock().await;
        log.push(json!({
            "id": "item-fallback",
            "name": "fallback-poll.txt",
            "size": 12,
            "file": { "mimeType": "text/plain" },
            "parentReference": { "driveId": "drv-me", "id": "root" }
        }));
    }
    let mut saw_fallback = false;
    let t_fb = Instant::now();
    while t_fb.elapsed() < Duration::from_secs(4) {
        if let Ok(Some(CoreEvent::FolderItemsUpdated { items, .. })) =
            tokio::time::timeout(Duration::from_millis(300), ev_rx.recv()).await
        {
            if items.iter().any(|i| i.name == "fallback-poll.txt") {
                saw_fallback = true;
                break;
            }
        }
    }
    assert!(saw_fallback, "disconnect/proxy block must fall back to active delta polling");

    // 6. Re-enable Socket.IO -> FocusGained wakes reconnect and catches up queued edit from saved cursor!
    let accepts_before = ws_accept_count.load(Ordering::SeqCst);
    st.socket_enabled.store(true, Ordering::SeqCst);
    {
        let mut log = st.delta_log.lock().await;
        log.push(json!({
            "id": "item-doc",
            "name": "doc-reconnected.md",
            "size": 44,
            "eTag": "\"etag-v4\"",
            "cTag": "\"ctag-v4\"",
            "file": { "mimeType": "text/markdown" },
            "parentReference": { "driveId": "drv-me", "id": "root" }
        }));
    }
    ctrl.request(SyncCommand::FocusGained);
    let mut saw_reconnected = false;
    let t_rec = Instant::now();
    while t_rec.elapsed() < Duration::from_secs(4) {
        if let Ok(Some(CoreEvent::FolderItemsUpdated { items, .. })) =
            tokio::time::timeout(Duration::from_millis(200), ev_rx.recv()).await
        {
            if items.iter().any(|i| i.name == "doc-reconnected.md") {
                saw_reconnected = true;
                break;
            }
        }
    }
    assert!(saw_reconnected, "reconnect must catch up from saved cursor");
    // Wait up to 2s for Socket.IO manager to complete reconnect handshake
    let reconn_deadline = Instant::now() + Duration::from_secs(2);
    while ws_accept_count.load(Ordering::SeqCst) <= accepts_before
        && Instant::now() < reconn_deadline
    {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        ws_accept_count.load(Ordering::SeqCst) > accepts_before,
        "Socket.IO manager must reconnect once endpoint is reachable again"
    );

    ctrl.request(SyncCommand::Shutdown);
    http_server.abort();
    ws_server.abort();
}
