use std::{
    collections::{HashMap, HashSet},
    path::Path,
    process::{Output, Stdio},
    sync::Arc,
};

use axum::{
    extract::{Path as RoutePath, Query, State},
    http::{StatusCode, Uri},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use serde_json::{json, Value};
use tokio::{io::AsyncWriteExt, net::TcpListener, process::Command, sync::Mutex};

#[derive(Clone)]
struct SiteSpec {
    id: String,
    name: String,
    display_name: String,
}

struct MockState {
    followed_sites: Vec<SiteSpec>,
    discovered_sites: Vec<SiteSpec>,
    failing_site_drives: HashSet<String>,
    fail_site_discovery: bool,
    requests: Vec<String>,
}

impl Default for MockState {
    fn default() -> Self {
        Self {
            followed_sites: make_sites("s", 12),
            discovered_sites: Vec::new(),
            failing_site_drives: HashSet::new(),
            fail_site_discovery: false,
            requests: Vec::new(),
        }
    }
}

fn make_sites(prefix: &str, count: usize) -> Vec<SiteSpec> {
    (1..=count)
        .map(|i| SiteSpec {
            id: format!("{prefix}{i:02}"),
            name: format!("site-{i:02}"),
            display_name: format!("Site {i:02}"),
        })
        .collect()
}

#[derive(Clone)]
struct MockGraph {
    base: String,
    state: Arc<Mutex<MockState>>,
}

fn file_item_json(base: &str, drive_id: &str, id: &str, name: &str) -> Value {
    json!({
        "id": id,
        "name": name,
        "size": 1234,
        "cTag": format!("\"{id}-c\""),
        "eTag": format!("\"{id}-e\""),
        "createdDateTime": "2026-01-01T00:00:00Z",
        "createdBy": { "user": { "displayName": "Test User" } },
        "lastModifiedDateTime": "2026-02-02T00:00:00Z",
        "lastModifiedBy": { "user": { "displayName": "Test User" } },
        "file": { "mimeType": "text/plain" },
        "@microsoft.graph.downloadUrl": format!("{base}/v1.0/dl/{id}?tempauth=SYNTH"),
        "parentReference": { "driveId": drive_id, "id": "root", "path": "/drive/root:" },
        "webUrl": format!("https://contoso.example/{name}")
    })
}

fn folder_item_json(
    drive_id: &str,
    id: &str,
    name: &str,
    child_count: u32,
    parent_id: &str,
    parent_path: &str,
) -> Value {
    json!({
        "id": id,
        "name": name,
        "size": 0,
        "eTag": format!("\"{id}-e\""),
        "lastModifiedDateTime": "2026-02-02T00:00:00Z",
        "folder": { "childCount": child_count },
        "parentReference": { "driveId": drive_id, "id": parent_id, "path": parent_path },
        "webUrl": format!("https://contoso.example/{name}")
    })
}

fn paginate(items: Vec<Value>, top: usize, offset: usize, next_prefix: &str) -> Value {
    let end = (offset + top).min(items.len());
    let slice = if offset < items.len() {
        items[offset..end].to_vec()
    } else {
        Vec::new()
    };
    let mut body = json!({ "value": slice });
    if end < items.len() {
        body["@odata.nextLink"] = json!(format!("{next_prefix}?$top={top}&$skiptoken={end}"));
    }
    body
}

impl MockGraph {
    async fn record(&self, uri: &Uri) {
        self.state.lock().await.requests.push(uri.to_string());
    }

    async fn me_drive(State(mock): State<Self>, uri: Uri) -> Json<Value> {
        mock.record(&uri).await;
        Json(json!({
            "id": "d1",
            "name": "OneDrive",
            "driveType": "personal",
            "webUrl": "https://contoso.example/me"
        }))
    }

    async fn get_drive(
        State(mock): State<Self>,
        uri: Uri,
        RoutePath(drive_id): RoutePath<String>,
    ) -> Json<Value> {
        mock.record(&uri).await;
        let drive_type = if drive_id == "d1" {
            "personal"
        } else {
            "documentLibrary"
        };
        Json(json!({
            "id": drive_id,
            "name": format!("Drive {drive_id}"),
            "driveType": drive_type,
            "webUrl": format!("https://contoso.example/drives/{drive_id}")
        }))
    }

    async fn root_children(
        State(mock): State<Self>,
        uri: Uri,
        RoutePath(drive_id): RoutePath<String>,
        Query(query): Query<HashMap<String, String>>,
    ) -> Json<Value> {
        mock.record(&uri).await;
        let top = query
            .get("$top")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(200);
        let offset = query
            .get("$skiptoken")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);

        let items: Vec<Value> = match drive_id.as_str() {
            "d1" => (0..120)
                .map(|i| {
                    file_item_json(
                        &mock.base,
                        "d1",
                        &format!("f{i:03}"),
                        &format!("file{i:03}.txt"),
                    )
                })
                .collect(),
            "dx" => (0..100)
                .map(|i| {
                    file_item_json(
                        &mock.base,
                        "dx",
                        &format!("x{i:03}"),
                        &format!("exact{i:03}.txt"),
                    )
                })
                .collect(),
            "ds" => (0..2)
                .map(|i| {
                    file_item_json(
                        &mock.base,
                        "ds",
                        &format!("s{i:03}"),
                        &format!("small{i:03}.txt"),
                    )
                })
                .collect(),
            "de" => Vec::new(),
            "dt" => vec![
                folder_item_json("dt", "big", "Big", 250, "root", "/drive/root:"),
                folder_item_json("dt", "l1", "L1", 1, "root", "/drive/root:"),
            ],
            _ => Vec::new(),
        };

        let next_prefix = format!("{}/v1.0/drives/{drive_id}/root/children", mock.base);
        Json(paginate(items, top, offset, &next_prefix))
    }

    async fn get_item(
        State(mock): State<Self>,
        uri: Uri,
        RoutePath((drive_id, item_id)): RoutePath<(String, String)>,
    ) -> impl IntoResponse {
        mock.record(&uri).await;
        if drive_id == "dt" {
            let item = match item_id.as_str() {
                "big" => folder_item_json("dt", "big", "Big", 250, "root", "/drive/root:"),
                "l1" => folder_item_json("dt", "l1", "L1", 1, "root", "/drive/root:"),
                "l2" => folder_item_json("dt", "l2", "L2", 1, "l1", "/drive/root:/L1"),
                _ => folder_item_json("dt", &item_id, &item_id, 0, "root", "/drive/root:"),
            };
            return (StatusCode::OK, Json(item)).into_response();
        }
        let name = if let Some(num) = item_id.strip_prefix('f') {
            format!("file{num}.txt")
        } else {
            format!("{item_id}.txt")
        };
        (
            StatusCode::OK,
            Json(file_item_json(&mock.base, &drive_id, &item_id, &name)),
        )
            .into_response()
    }

    async fn item_children(
        State(mock): State<Self>,
        uri: Uri,
        RoutePath((drive_id, item_id)): RoutePath<(String, String)>,
        Query(query): Query<HashMap<String, String>>,
    ) -> Json<Value> {
        mock.record(&uri).await;
        let top = query
            .get("$top")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(200);
        let offset = query
            .get("$skiptoken")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);

        let items: Vec<Value> = match (drive_id.as_str(), item_id.as_str()) {
            ("dt", "big") => (0..250)
                .map(|i| {
                    let id = format!("big-{i:03}");
                    let name = format!("big-{i:03}.txt");
                    json!({
                        "id": id,
                        "name": name,
                        "size": 10,
                        "file": { "mimeType": "text/plain" },
                        "parentReference": { "driveId": "dt", "id": "big", "path": "/drive/root:/Big" }
                    })
                })
                .collect(),
            ("dt", "l1") => vec![folder_item_json("dt", "l2", "L2", 1, "l1", "/drive/root:/L1")],
            ("dt", "l2") => vec![folder_item_json("dt", "l3", "L3", 1, "l2", "/drive/root:/L1/L2")],
            ("dt", "l3") => vec![folder_item_json("dt", "l4", "L4", 1, "l3", "/drive/root:/L1/L2/L3")],
            ("dt", "l4") => vec![folder_item_json("dt", "l5", "L5", 1, "l4", "/drive/root:/L1/L2/L3/L4")],
            ("dt", "l5") => vec![folder_item_json("dt", "l6", "L6", 1, "l5", "/drive/root:/L1/L2/L3/L4/L5")],
            ("dt", "l6") => vec![folder_item_json("dt", "l7", "L7", 0, "l6", "/drive/root:/L1/L2/L3/L4/L5/L6")],
            _ => Vec::new(),
        };

        let next_prefix = format!(
            "{}/v1.0/drives/{drive_id}/items/{item_id}/children",
            mock.base
        );
        Json(paginate(items, top, offset, &next_prefix))
    }

    async fn item_versions(
        State(mock): State<Self>,
        uri: Uri,
        RoutePath((drive_id, item_id)): RoutePath<(String, String)>,
        Query(query): Query<HashMap<String, String>>,
    ) -> Json<Value> {
        mock.record(&uri).await;
        let offset = query
            .get("$skiptoken")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);
        let versions: Vec<Value> = match item_id.as_str() {
            "f000" => vec![
                json!({
                    "id": "3.0",
                    "lastModifiedDateTime": "2026-02-02T00:00:00Z",
                    "lastModifiedBy": { "user": { "displayName": "Test User" } },
                    "size": 300
                }),
                json!({
                    "id": "2.0",
                    "lastModifiedDateTime": "2026-02-01T00:00:00Z",
                    "lastModifiedBy": { "user": { "displayName": "Test User" } },
                    "size": 200
                }),
                json!({
                    "id": "1.0",
                    "lastModifiedDateTime": "2026-01-31T00:00:00Z",
                    "lastModifiedBy": { "user": { "displayName": "Test User" } },
                    "size": 100
                }),
            ],
            "f001" => Vec::new(),
            _ => Vec::new(),
        };
        let next_prefix = format!(
            "{}/v1.0/drives/{drive_id}/items/{item_id}/versions",
            mock.base
        );
        Json(paginate(versions, 2, offset, &next_prefix))
    }

    async fn followed_sites(State(mock): State<Self>, uri: Uri) -> impl IntoResponse {
        mock.record(&uri).await;
        let state = mock.state.lock().await;
        if state.fail_site_discovery {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": { "code": "generalException", "message": "Site discovery unavailable" } })),
            )
                .into_response();
        }
        let values: Vec<Value> = state
            .followed_sites
            .iter()
            .map(|s| {
                json!({
                    "id": s.id,
                    "name": s.name,
                    "displayName": s.display_name,
                    "webUrl": format!("https://contoso.example/sites/{}", s.name)
                })
            })
            .collect();
        (StatusCode::OK, Json(json!({ "value": values }))).into_response()
    }

    async fn sites(
        State(mock): State<Self>,
        uri: Uri,
        Query(query): Query<HashMap<String, String>>,
    ) -> impl IntoResponse {
        mock.record(&uri).await;
        let state = mock.state.lock().await;
        if state.fail_site_discovery {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": { "code": "generalException", "message": "Site discovery unavailable" } })),
            )
                .into_response();
        }
        let top = query
            .get("$top")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(200);
        let offset = query
            .get("$skiptoken")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);
        let values: Vec<Value> = state
            .discovered_sites
            .iter()
            .map(|s| {
                json!({
                    "id": s.id,
                    "name": s.name,
                    "displayName": s.display_name,
                    "webUrl": format!("https://contoso.example/sites/{}", s.name)
                })
            })
            .collect();
        let next_prefix = format!("{}/v1.0/sites", mock.base);
        (
            StatusCode::OK,
            Json(paginate(values, top, offset, &next_prefix)),
        )
            .into_response()
    }

    async fn site_drives(
        State(mock): State<Self>,
        uri: Uri,
        RoutePath(site_id): RoutePath<String>,
    ) -> impl IntoResponse {
        mock.record(&uri).await;
        let state = mock.state.lock().await;
        if state.failing_site_drives.contains(&site_id) {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": { "code": "accessDenied", "message": "Access denied to site drives" } })),
            )
                .into_response();
        }
        (
            StatusCode::OK,
            Json(json!({
                "value": [{
                    "id": format!("lib-{site_id}"),
                    "name": format!("Documents {site_id}"),
                    "driveType": "documentLibrary",
                    "webUrl": format!("https://contoso.example/sites/{site_id}/Shared%20Documents")
                }]
            })),
        )
            .into_response()
    }
}

struct Client<'a> {
    exe: &'a Path,
    profile: &'a Path,
    base: &'a str,
}

impl<'a> Client<'a> {
    fn new(exe: &'a Path, profile: &'a Path, base: &'a str) -> Self {
        Self { exe, profile, base }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(self.exe);
        command.kill_on_drop(true);
        for (key, _) in std::env::vars_os() {
            let upper = key.to_string_lossy().to_ascii_uppercase();
            if ["SHAREPOINT_", "ONEDRIVE_", "TEAMS_", "AZURE_"]
                .iter()
                .any(|prefix| upper.starts_with(prefix))
            {
                command.env_remove(key);
            }
        }
        command
            .env("SHAREPOINT_GRAPH_BASE_URL", format!("{}/v1.0", self.base))
            .env("SHAREPOINT_ACCESS_TOKEN", "synthetic-token")
            .env("SHAREPOINT_DISABLE_KEYRING", "1")
            .env("SHAREPOINT_SHARE_M365_AUTH", "0")
            .env("SHAREPOINT_AUTO_DAEMON", "0")
            .env("SHAREPOINT_CONFIG_DIR", self.profile.join("config"))
            .env("SHAREPOINT_DATA_DIR", self.profile.join("data"))
            .env("NO_PROXY", "127.0.0.1,localhost");
        command
    }

    async fn cli(&self, args: &[&str]) -> Output {
        self.command().args(args).output().await.unwrap()
    }

    async fn mcp_call(&self, name: &str, args: Value) -> Value {
        let replies = self
            .mcp_exchange(&[
                json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                    "protocolVersion": "2025-06-18", "capabilities": {},
                    "clientInfo": { "name": "compact-paging-e2e", "version": "1" }
                }}),
                json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
                json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": name, "arguments": args } }),
            ])
            .await;
        replies[1]["result"].clone()
    }

    async fn mcp_exchange(&self, requests: &[Value]) -> Vec<Value> {
        let mut child = self
            .command()
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        for req in requests {
            stdin
                .write_all(format!("{req}\n").as_bytes())
                .await
                .unwrap();
        }
        drop(stdin);
        let output = child.wait_with_output().await.unwrap();
        assert!(
            output.status.success(),
            "mcp process failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        std::str::from_utf8(&output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

fn cli_json(output: Output) -> Value {
    assert!(
        output.status.success(),
        "command failed (code={:?}): {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

async fn spawn_mock(
    state: MockState,
) -> (
    String,
    Arc<Mutex<MockState>>,
    tokio::task::JoinHandle<()>,
    tempfile::TempDir,
    std::ffi::OsString,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let state = Arc::new(Mutex::new(state));
    let app = Router::new()
        .route("/v1.0/me/drive", get(MockGraph::me_drive))
        .route("/v1.0/drives/{d}", get(MockGraph::get_drive))
        .route(
            "/v1.0/drives/{d}/root/children",
            get(MockGraph::root_children),
        )
        .route("/v1.0/drives/{d}/items/{id}", get(MockGraph::get_item))
        .route(
            "/v1.0/drives/{d}/items/{id}/children",
            get(MockGraph::item_children),
        )
        .route(
            "/v1.0/drives/{d}/items/{id}/versions",
            get(MockGraph::item_versions),
        )
        .route("/v1.0/me/followedSites", get(MockGraph::followed_sites))
        .route("/v1.0/sites", get(MockGraph::sites))
        .route(
            "/v1.0/sites/{s}",
            get(|State(mock): State<MockGraph>, uri: Uri| async move {
                mock.record(&uri).await;
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": { "code": "itemNotFound", "message": "Site not found" } })),
                )
            }),
        )
        .route("/v1.0/sites/{s}/drives", get(MockGraph::site_drives))
        .with_state(MockGraph {
            base: base.clone(),
            state: state.clone(),
        });
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let profile = tempfile::tempdir().unwrap();
    let exe = env!("CARGO_BIN_EXE_sharepoint").into();
    (base, state, server, profile, exe)
}

#[tokio::test]
async fn info_json_defaults_to_compact_with_omitted_and_hint() {
    let (base, _state, server, profile, exe) = spawn_mock(MockState::default()).await;
    let client = Client::new(Path::new(&exe), profile.path(), &base);

    let out = cli_json(
        client
            .cli(&["info", "f000", "--drive", "d1", "--json"])
            .await,
    );
    assert!(out.get("versions").is_none());
    assert!(out.get("versions_has_more").is_none());

    let item = out["item"].as_object().unwrap();
    let keys: Vec<&str> = item.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        vec![
            "drive_id",
            "e_tag",
            "hint",
            "id",
            "is_folder",
            "last_modified_at",
            "name",
            "omitted",
            "parent_path",
            "size",
        ]
    );
    assert_eq!(item["drive_id"], "d1");
    assert_eq!(item["id"], "f000");
    assert_eq!(item["name"], "file000.txt");
    assert_eq!(item["parent_path"], "");
    assert_eq!(item["e_tag"], "\"f000-e\"");
    assert_eq!(item["is_folder"], false);
    assert_eq!(item["size"], 1234);
    assert_eq!(item["last_modified_at"], "2026-02-02T00:00:00Z");
    assert_eq!(
        item["omitted"],
        json!({
            "fields": [
                "c_tag",
                "created_at",
                "created_by",
                "download_url",
                "last_modified_by",
                "mime_type",
                "parent_id",
                "web_url"
            ],
            "versions": null
        })
    );
    assert_eq!(
        item["hint"],
        "sharepoint info 'f000' --drive 'd1' --json --full"
    );

    server.abort();
}

#[tokio::test]
async fn info_compact_omitted_fields_skip_null_values() {
    let (base, _state, server, profile, exe) = spawn_mock(MockState::default()).await;
    let client = Client::new(Path::new(&exe), profile.path(), &base);

    let out = cli_json(
        client
            .cli(&["info", "big", "--drive", "dt", "--json"])
            .await,
    );
    let omitted: Vec<&str> = out["item"]["omitted"]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(omitted.contains(&"child_count"));
    assert!(!omitted.contains(&"mime_type"));
    assert!(!omitted.contains(&"download_url"));
    assert!(!omitted.contains(&"c_tag"));

    server.abort();
}

#[tokio::test]
async fn info_full_returns_all_fields_and_flag_conflicts() {
    let (base, _state, server, profile, exe) = spawn_mock(MockState::default()).await;
    let client = Client::new(Path::new(&exe), profile.path(), &base);

    let default_out = cli_json(
        client
            .cli(&["info", "f000", "--drive", "d1", "--json"])
            .await,
    );
    let alias_out = cli_json(
        client
            .cli(&["info", "f000", "--drive", "d1", "--json", "--compact"])
            .await,
    );
    assert_eq!(alias_out, default_out);

    let full_out = cli_json(
        client
            .cli(&["info", "f000", "--drive", "d1", "--json", "--full"])
            .await,
    );
    let item = full_out["item"].as_object().unwrap();
    assert_eq!(item.len(), 20);
    assert!(!item.contains_key("omitted"));
    assert!(!item.contains_key("hint"));
    assert!(full_out.get("versions").is_none());

    let conflict = client
        .cli(&[
            "info",
            "f000",
            "--drive",
            "d1",
            "--json",
            "--compact",
            "--full",
        ])
        .await;
    assert_eq!(conflict.status.code(), Some(2));
    assert!(conflict.stdout.is_empty());
    assert!(String::from_utf8_lossy(&conflict.stderr)
        .contains("the argument '--compact' cannot be used with '--full'"));

    let no_json = client
        .cli(&["info", "f000", "--drive", "d1", "--full"])
        .await;
    assert_eq!(no_json.status.code(), Some(2));
    assert!(no_json.stdout.is_empty());
    assert!(String::from_utf8_lossy(&no_json.stderr).contains("--json"));

    // WI-4: runtime error with --json writes a single-line {ok:false,error} JSON to stdout; without --json stdout stays empty
    let missing_json = client
        .cli(&["info", "/does-not-exist.md", "--drive", "d1", "--json"])
        .await;
    assert_eq!(missing_json.status.code(), Some(1));
    let missing_stdout = String::from_utf8_lossy(&missing_json.stdout);
    let missing_lines: Vec<&str> = missing_stdout.lines().collect();
    assert_eq!(
        missing_lines.len(),
        1,
        "expected single-line JSON error on stdout, got: {missing_stdout}"
    );
    let missing_val: Value = serde_json::from_str(missing_lines[0]).unwrap();
    assert_eq!(missing_val["ok"], false);
    assert_eq!(missing_val["error"]["code"], "not_found");
    assert!(missing_val["error"]["message"]
        .as_str()
        .unwrap()
        .contains("/does-not-exist.md"));

    let missing_plain = client
        .cli(&["info", "/does-not-exist.md", "--drive", "d1"])
        .await;
    assert_eq!(missing_plain.status.code(), Some(1));
    assert!(missing_plain.stdout.is_empty());

    server.abort();
}

#[tokio::test]
async fn info_versions_offline_is_usage_error() {
    let (base, state, server, profile, exe) = spawn_mock(MockState::default()).await;
    let client = Client::new(Path::new(&exe), profile.path(), &base);

    // Seed local cache first.
    let seeded = cli_json(
        client
            .cli(&["info", "f000", "--drive", "d1", "--json"])
            .await,
    );
    assert_eq!(seeded["item"]["id"], "f000");

    state.lock().await.requests.clear();
    let bad = client
        .cli(&[
            "info",
            "f000",
            "--drive",
            "d1",
            "--versions",
            "--offline",
            "--json",
        ])
        .await;
    assert_eq!(bad.status.code(), Some(2));
    assert!(bad.stdout.is_empty());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("cannot be used with '--offline'"));
    assert!(state.lock().await.requests.is_empty());

    // Positive control: --offline without --versions still works from cache.
    let offline = cli_json(
        client
            .cli(&["info", "f000", "--drive", "d1", "--offline", "--json"])
            .await,
    );
    assert_eq!(offline["item"]["id"], "f000");
    assert_eq!(offline["item"]["omitted"]["versions"], Value::Null);
    assert!(offline.get("versions").is_none());
    assert!(state.lock().await.requests.is_empty());

    server.abort();
}

#[tokio::test]
async fn info_versions_reports_has_more_at_boundaries() {
    let (base, state, server, profile, exe) = spawn_mock(MockState::default()).await;
    let client = Client::new(Path::new(&exe), profile.path(), &base);

    // Full history (3 versions across 2 pages)
    let all = cli_json(
        client
            .cli(&["info", "f000", "--drive", "d1", "--versions", "--json"])
            .await,
    );
    assert_eq!(all["versions"].as_array().unwrap().len(), 3);
    assert_eq!(all["versions_has_more"], false);
    assert!(all["item"]["omitted"].get("versions").is_none());

    // Limit 1 (more remain on same page + nextLink)
    let lim1 = cli_json(
        client
            .cli(&[
                "info",
                "f000",
                "--drive",
                "d1",
                "--versions",
                "--versions-limit",
                "1",
                "--json",
            ])
            .await,
    );
    assert_eq!(lim1["versions"].as_array().unwrap().len(), 1);
    assert_eq!(lim1["versions_has_more"], true);

    // Limit 2 (exact page boundary, nextLink still exists)
    let lim2 = cli_json(
        client
            .cli(&[
                "info",
                "f000",
                "--drive",
                "d1",
                "--versions",
                "--versions-limit",
                "2",
                "--json",
            ])
            .await,
    );
    assert_eq!(lim2["versions"].as_array().unwrap().len(), 2);
    assert_eq!(lim2["versions_has_more"], true);

    // Limit 3 (exact total count, no nextLink)
    let lim3 = cli_json(
        client
            .cli(&[
                "info",
                "f000",
                "--drive",
                "d1",
                "--versions",
                "--versions-limit",
                "3",
                "--json",
            ])
            .await,
    );
    assert_eq!(lim3["versions"].as_array().unwrap().len(), 3);
    assert_eq!(lim3["versions_has_more"], false);

    // Limit 0 (no version request made, has_more is null)
    state.lock().await.requests.clear();
    let lim0 = cli_json(
        client
            .cli(&[
                "info",
                "f000",
                "--drive",
                "d1",
                "--versions",
                "--versions-limit",
                "0",
                "--json",
            ])
            .await,
    );
    assert_eq!(lim0["versions"], json!([]));
    assert_eq!(lim0["versions_has_more"], Value::Null);
    assert!(
        !state
            .lock()
            .await
            .requests
            .iter()
            .any(|req| req.contains("/versions")),
        "limit 0 must not request /versions"
    );

    // Confirmed 0 versions (f001)
    let empty = cli_json(
        client
            .cli(&["info", "f001", "--drive", "d1", "--versions", "--json"])
            .await,
    );
    assert_eq!(empty["versions"], json!([]));
    assert_eq!(empty["versions_has_more"], false);

    // Text mode prints truncation note when more versions exist.
    let text_lim1 = client
        .cli(&[
            "info",
            "f000",
            "--drive",
            "d1",
            "--versions",
            "--versions-limit",
            "1",
        ])
        .await;
    assert!(text_lim1.status.success());
    assert!(String::from_utf8_lossy(&text_lim1.stdout)
        .contains("(more versions exist; raise --versions-limit)"));

    server.abort();
}

#[tokio::test]
async fn mcp_get_file_info_compact_default_and_full() {
    let (base, _state, server, profile, exe) = spawn_mock(MockState::default()).await;
    let client = Client::new(Path::new(&exe), profile.path(), &base);

    let compact = client
        .mcp_call("get_file_info", json!({"drive": "d1", "item_id": "f000"}))
        .await;
    assert_eq!(compact["isError"], false);
    let item = &compact["structuredContent"]["item"];
    assert_eq!(item.as_object().unwrap().len(), 10);
    assert_eq!(
        item["hint"],
        "Call get_file_info with item_id \"f000\", drive \"d1\" and fields: [...] for specific fields or compact:false for every field."
    );
    assert_eq!(item["omitted"]["versions"], Value::Null);

    let full = client
        .mcp_call(
            "get_file_info",
            json!({"drive": "d1", "item_id": "f000", "compact": false}),
        )
        .await;
    assert_eq!(full["isError"], false);
    let full_item = full["structuredContent"]["item"].as_object().unwrap();
    assert_eq!(full_item.len(), 20);
    assert!(!full_item.contains_key("omitted"));
    assert!(!full_item.contains_key("hint"));

    let bad = client
        .mcp_call(
            "get_file_info",
            json!({"drive": "d1", "item_id": "f000", "compact": "yes"}),
        )
        .await;
    assert_eq!(bad["isError"], true);
    assert_eq!(bad["content"][0]["text"], "compact must be a boolean");
    assert!(bad.get("structuredContent").is_none());

    server.abort();
}

#[tokio::test]
async fn mcp_list_versions_reports_returned_and_has_more() {
    let (base, _state, server, profile, exe) = spawn_mock(MockState::default()).await;
    let client = Client::new(Path::new(&exe), profile.path(), &base);

    let lim1 = client
        .mcp_call(
            "list_versions",
            json!({"drive": "d1", "item_id": "f000", "limit": 1}),
        )
        .await;
    assert_eq!(lim1["isError"], false);
    assert_eq!(lim1["structuredContent"]["returned"], 1);
    assert_eq!(lim1["structuredContent"]["has_more"], true);

    let all = client
        .mcp_call("list_versions", json!({"drive": "d1", "item_id": "f000"}))
        .await;
    assert_eq!(all["isError"], false);
    assert_eq!(all["structuredContent"]["returned"], 3);
    assert_eq!(all["structuredContent"]["has_more"], false);

    let lim0 = client
        .mcp_call(
            "list_versions",
            json!({"drive": "d1", "item_id": "f000", "limit": 0}),
        )
        .await;
    assert_eq!(lim0["isError"], false);
    assert_eq!(lim0["structuredContent"]["returned"], 0);
    assert_eq!(lim0["structuredContent"]["has_more"], Value::Null);

    server.abort();
}

#[tokio::test]
async fn ls_reports_returned_has_more_and_source() {
    let (base, state, server, profile, exe) = spawn_mock(MockState::default()).await;
    let client = Client::new(Path::new(&exe), profile.path(), &base);

    // d1 (120 items), default limit 100 -> (100, true, remote)
    let d1_default = cli_json(client.cli(&["ls", "--drive", "d1", "--json"]).await);
    assert_eq!(d1_default["returned"], 100);
    assert_eq!(d1_default["has_more"], true);
    assert_eq!(d1_default["source"], "remote");
    assert_eq!(d1_default["path"], "/");
    assert!(d1_default.get("drive").is_some());
    assert_eq!(d1_default["items"].as_array().unwrap().len(), 100);
    assert!(
        d1_default["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|it| it.get("download_url").is_none()),
        "ls --json items must omit download_url"
    );

    // dx (exact 100 items), default limit 100 -> (100, false, remote)
    let dx_default = cli_json(client.cli(&["ls", "--drive", "dx", "--json"]).await);
    assert_eq!(dx_default["returned"], 100);
    assert_eq!(dx_default["has_more"], false);
    assert_eq!(dx_default["source"], "remote");

    // d1 --limit 200 -> (120, false, remote)
    let d1_200 = cli_json(
        client
            .cli(&["ls", "--drive", "d1", "--limit", "200", "--json"])
            .await,
    );
    assert_eq!(d1_200["returned"], 120);
    assert_eq!(d1_200["has_more"], false);
    assert_eq!(d1_200["source"], "remote");

    // ds (2 items) -> (2, false, remote)
    let ds = cli_json(client.cli(&["ls", "--drive", "ds", "--json"]).await);
    assert_eq!(ds["returned"], 2);
    assert_eq!(ds["has_more"], false);
    assert_eq!(ds["source"], "remote");

    // de (0 items) -> (0, false, remote)
    let de = cli_json(client.cli(&["ls", "--drive", "de", "--json"]).await);
    assert_eq!(de["returned"], 0);
    assert_eq!(de["has_more"], false);
    assert_eq!(de["source"], "remote");

    // --limit 0 -> (0, null, remote) with no children request
    state.lock().await.requests.clear();
    let lim0 = cli_json(
        client
            .cli(&["ls", "--drive", "d1", "--limit", "0", "--json"])
            .await,
    );
    assert_eq!(lim0["returned"], 0);
    assert_eq!(lim0["has_more"], Value::Null);
    assert_eq!(lim0["source"], "remote");
    assert!(!state
        .lock()
        .await
        .requests
        .iter()
        .any(|r| r.contains("/children")));

    // --offline on cached d1 -> source: "cache" and has_more reflects cached count
    let d1_off_100 = cli_json(
        client
            .cli(&["ls", "--drive", "d1", "--offline", "--json"])
            .await,
    );
    assert_eq!(d1_off_100["returned"], 100);
    assert_eq!(d1_off_100["has_more"], true);
    assert_eq!(d1_off_100["source"], "cache");

    let d1_off_200 = cli_json(
        client
            .cli(&[
                "ls",
                "--drive",
                "d1",
                "--offline",
                "--limit",
                "200",
                "--json",
            ])
            .await,
    );
    assert_eq!(d1_off_200["returned"], 120);
    assert_eq!(d1_off_200["has_more"], false);
    assert_eq!(d1_off_200["source"], "cache");

    // Fresh profile with --cached (ds): falls back to remote -> (2, false, remote)
    let fresh_cached_profile = tempfile::tempdir().unwrap();
    let fresh_cached = Client::new(Path::new(&exe), fresh_cached_profile.path(), &base);
    let ds_cached_miss = cli_json(
        fresh_cached
            .cli(&["ls", "--drive", "ds", "--cached", "--json"])
            .await,
    );
    assert_eq!(ds_cached_miss["returned"], 2);
    assert_eq!(ds_cached_miss["has_more"], false);
    assert_eq!(ds_cached_miss["source"], "remote");

    // Fresh profile with --offline (ds): seed drive metadata only via --limit 0, no children cached -> (0, false, cache)
    let fresh_offline_profile = tempfile::tempdir().unwrap();
    let fresh_offline = Client::new(Path::new(&exe), fresh_offline_profile.path(), &base);
    let _ = cli_json(
        fresh_offline
            .cli(&["ls", "--drive", "ds", "--limit", "0", "--json"])
            .await,
    );
    state.lock().await.requests.clear();
    let ds_offline_empty = cli_json(
        fresh_offline
            .cli(&["ls", "--drive", "ds", "--offline", "--json"])
            .await,
    );
    assert_eq!(ds_offline_empty["items"], json!([]));
    assert_eq!(ds_offline_empty["returned"], 0);
    assert_eq!(ds_offline_empty["has_more"], false);
    assert_eq!(ds_offline_empty["source"], "cache");
    assert!(state.lock().await.requests.is_empty());

    // MCP list_files matches metadata values without "path"
    let mcp_d1 = client
        .mcp_call(
            "list_files",
            json!({"drive": "d1", "compact": true, "limit": 2}),
        )
        .await;
    assert_eq!(mcp_d1["isError"], false);
    let sc = &mcp_d1["structuredContent"];
    assert_eq!(sc["drive"], json!({ "id": "d1" }));
    assert_eq!(sc["returned"], 2);
    assert_eq!(sc["has_more"], true);
    assert_eq!(sc["source"], "remote");
    assert!(sc.get("path").is_none());

    let mcp_dx = client.mcp_call("list_files", json!({"drive": "dx"})).await;
    assert_eq!(mcp_dx["structuredContent"]["returned"], 100);
    assert_eq!(mcp_dx["structuredContent"]["has_more"], false);
    assert_eq!(mcp_dx["structuredContent"]["source"], "remote");

    // Text mode emits stderr notice when more items exist.
    let text_ls = client.cli(&["ls", "--drive", "d1"]).await;
    assert!(text_ls.status.success());
    assert!(String::from_utf8_lossy(&text_ls.stderr)
        .contains("More items exist; rerun with a larger --limit (now 100)."));

    server.abort();
}

#[tokio::test]
async fn drives_scans_sites_after_the_tenth() {
    let (base, state, server, profile, exe) = spawn_mock(MockState::default()).await;
    let client = Client::new(Path::new(&exe), profile.path(), &base);

    let out = client.cli(&["drives", "--json"]).await;
    assert!(out.status.success());
    assert!(
        out.stderr.is_empty(),
        "expected empty stderr, got: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let drives: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(drives.len(), 13);
    let ids: Vec<&str> = drives.iter().map(|d| d["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"d1"));
    assert!(ids.contains(&"lib-s11"));
    assert!(ids.contains(&"lib-s12"));

    let site_drive_reqs = state
        .lock()
        .await
        .requests
        .iter()
        .filter(|r| r.starts_with("/v1.0/sites/") && r.ends_with("/drives"))
        .count();
    assert_eq!(site_drive_reqs, 12);

    let mcp = client.mcp_call("list_drives", json!({})).await;
    assert_eq!(mcp["isError"], false);
    let sc = &mcp["structuredContent"];
    assert_eq!(sc["returned"], 13);
    assert_eq!(sc["sites_scanned"], 12);
    assert_eq!(sc["sites_failed"], 0);
    assert_eq!(sc["has_more"], false);

    server.abort();
}

#[tokio::test]
async fn drives_reports_failed_sites_and_site_cap() {
    // (a) 12 sites, 1 returns 403 on /drives
    let mut state_a = MockState::default();
    state_a.failing_site_drives.insert("s12".to_string());
    let (base_a, _st_a, server_a, profile_a, exe) = spawn_mock(state_a).await;
    let client_a = Client::new(Path::new(&exe), profile_a.path(), &base_a);

    let out_a = client_a.cli(&["drives", "--json"]).await;
    assert!(out_a.status.success());
    let drives_a: Vec<Value> = serde_json::from_slice(&out_a.stdout).unwrap();
    assert_eq!(drives_a.len(), 12);
    assert!(
        String::from_utf8_lossy(&out_a.stderr).contains("could not list libraries for 1 site(s)")
    );

    let mcp_a = client_a.mcp_call("list_drives", json!({})).await;
    assert_eq!(mcp_a["isError"], false);
    assert_eq!(mcp_a["structuredContent"]["returned"], 12);
    assert_eq!(mcp_a["structuredContent"]["sites_scanned"], 12);
    assert_eq!(mcp_a["structuredContent"]["sites_failed"], 1);
    assert_eq!(mcp_a["structuredContent"]["has_more"], false);
    server_a.abort();

    // (b) 201 sites returned from /sites -> scans first 200 and reports cap
    let state_b = MockState {
        followed_sites: Vec::new(),
        discovered_sites: make_sites("d", 201),
        failing_site_drives: HashSet::new(),
        fail_site_discovery: false,
        requests: Vec::new(),
    };
    let (base_b, st_b, server_b, profile_b, exe_b) = spawn_mock(state_b).await;
    let client_b = Client::new(Path::new(&exe_b), profile_b.path(), &base_b);

    let out_b = client_b.cli(&["drives", "--json"]).await;
    assert!(out_b.status.success());
    let drives_b: Vec<Value> = serde_json::from_slice(&out_b.stdout).unwrap();
    assert_eq!(drives_b.len(), 201); // personal d1 + 200 site libraries
    assert!(String::from_utf8_lossy(&out_b.stderr).contains("scanned the first 200"));
    let site_drive_reqs = st_b
        .lock()
        .await
        .requests
        .iter()
        .filter(|r| r.starts_with("/v1.0/sites/") && r.ends_with("/drives"))
        .count();
    assert_eq!(site_drive_reqs, 200);

    let mcp_b = client_b.mcp_call("list_drives", json!({})).await;
    assert_eq!(mcp_b["isError"], false);
    assert_eq!(mcp_b["structuredContent"]["sites_scanned"], 200);
    assert_eq!(mcp_b["structuredContent"]["has_more"], true);
    server_b.abort();
}

#[tokio::test]
async fn drives_site_and_offline_paths_unchanged() {
    let (base, _state, server, profile, exe) = spawn_mock(MockState::default()).await;
    let client = Client::new(Path::new(&exe), profile.path(), &base);

    // Specific site lookup
    let one_site = cli_json(client.cli(&["drives", "--site", "Site 11", "--json"]).await);
    let arr = one_site.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"], "lib-s11");

    // Full online scan populates cache, then --offline reads all 13 without warning
    let _ = cli_json(client.cli(&["drives", "--json"]).await);
    let off = client.cli(&["drives", "--offline", "--json"]).await;
    assert!(off.status.success());
    assert!(
        off.stderr.is_empty(),
        "offline drives must not emit discovery warning: {}",
        String::from_utf8_lossy(&off.stderr)
    );
    let cached_drives: Vec<Value> = serde_json::from_slice(&off.stdout).unwrap();
    assert_eq!(cached_drives.len(), 13);

    let mcp_off = client
        .mcp_call("list_drives", json!({"offline": true}))
        .await;
    assert_eq!(mcp_off["isError"], false);
    assert_eq!(mcp_off["structuredContent"]["returned"], 13);
    assert_eq!(mcp_off["structuredContent"]["sites_scanned"], Value::Null);
    assert_eq!(mcp_off["structuredContent"]["has_more"], Value::Null);

    server.abort();
}

#[tokio::test]
async fn drives_site_prefix_ambiguity_preserves_candidates_without_404_fallback() {
    let state = MockState {
        followed_sites: vec![
            SiteSpec {
                id: "site-acme-ops".into(),
                name: "acme-ops".into(),
                display_name: "Acme Operations".into(),
            },
            SiteSpec {
                id: "site-acme-fin".into(),
                name: "acme-fin".into(),
                display_name: "Acme Finance".into(),
            },
        ],
        discovered_sites: Vec::new(),
        failing_site_drives: HashSet::new(),
        fail_site_discovery: false,
        requests: Vec::new(),
    };
    let (base, st, server, profile, exe) = spawn_mock(state).await;
    let client = Client::new(Path::new(&exe), profile.path(), &base);

    let out = client.cli(&["drives", "--site", "Acme", "--json"]).await;
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("site-acme-ops") && stderr.contains("site-acme-fin"),
        "expected both ambiguous site IDs in stderr, got: {stderr}"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "expected single-line JSON error, got: {stdout}");
    let err_json: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(err_json["ok"], false);
    assert_eq!(err_json["error"]["code"], "ambiguous");
    assert_eq!(err_json["error"]["candidates"].as_array().unwrap().len(), 2);
    assert!(
        !st.lock()
            .await
            .requests
            .iter()
            .any(|req| req.starts_with("/v1.0/sites/Acme")),
        "ambiguous site prefix must not fall back to GET /sites/Acme: {:?}",
        st.lock().await.requests
    );

    server.abort();
}

#[tokio::test]
async fn mcp_starts_unauthenticated_and_serves_initialize_tools_list_and_whoami() {
    let profile = tempfile::tempdir().unwrap();
    let exe = Path::new(env!("CARGO_BIN_EXE_sharepoint"));
    let mut cmd = Command::new(exe);
    cmd.kill_on_drop(true);
    for (key, _) in std::env::vars_os() {
        let upper = key.to_string_lossy().to_ascii_uppercase();
        if ["SHAREPOINT_", "ONEDRIVE_", "TEAMS_", "AZURE_"]
            .iter()
            .any(|prefix| upper.starts_with(prefix))
        {
            cmd.env_remove(key);
        }
    }
    let mut child = cmd
        .env("SHAREPOINT_DISABLE_KEYRING", "1")
        .env("SHAREPOINT_SHARE_M365_AUTH", "0")
        .env("SHAREPOINT_AUTO_DAEMON", "0")
        .env("SHAREPOINT_CONFIG_DIR", profile.path().join("config"))
        .env("SHAREPOINT_DATA_DIR", profile.path().join("data"))
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let requests = [
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": { "name": "unauth-check", "version": "1" }
        }}),
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "whoami", "arguments": {} } }),
        json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": { "name": "list_sites", "arguments": {} } }),
    ];
    let mut stdin = child.stdin.take().unwrap();
    for req in requests {
        stdin
            .write_all(format!("{req}\n").as_bytes())
            .await
            .unwrap();
    }
    drop(stdin);
    let output = child.wait_with_output().await.unwrap();
    assert!(
        output.status.success(),
        "unauthenticated mcp failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let replies: Vec<Value> = std::str::from_utf8(&output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(replies.len(), 4);
    assert_eq!(replies[0]["id"], 1);
    assert_eq!(replies[0]["result"]["serverInfo"]["name"], "sharepoint");
    assert_eq!(replies[1]["id"], 2);
    assert_eq!(replies[1]["result"]["tools"].as_array().unwrap().len(), 20);
    assert_eq!(replies[2]["id"], 3);
    assert_eq!(replies[2]["result"]["isError"], false);
    assert_eq!(replies[2]["result"]["structuredContent"]["authenticated"], false);
    assert_eq!(replies[3]["id"], 4);
    assert_eq!(replies[3]["result"]["isError"], true);
    assert!(replies[3]["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("Not logged in"));
}

#[tokio::test]
async fn tree_depth_range_and_folder_cap() {
    let (base, _state, server, profile, exe) = spawn_mock(MockState::default()).await;
    let client = Client::new(Path::new(&exe), profile.path(), &base);

    // Out-of-range depths: exit 2, stdout empty
    for bad_depth in ["8", "0"] {
        let out = client
            .cli(&["tree", "--drive", "dt", "--depth", bad_depth, "--json"])
            .await;
        assert_eq!(out.status.code(), Some(2));
        assert!(out.stdout.is_empty());
        assert!(String::from_utf8_lossy(&out.stderr).contains("is not in 1..=5"));
    }

    // Depth 5 succeeds and reaches 5 levels (L1 -> L2 -> L3 -> L4 -> L5)
    let d5 = cli_json(
        client
            .cli(&["tree", "--drive", "dt", "--depth", "5", "--json"])
            .await,
    );
    assert_eq!(d5["has_more"], false);
    let l1_d5 = d5["tree"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["item"]["id"] == "l1")
        .unwrap();
    let l5 = &l1_d5["children"][0]["children"][0]["children"][0]["children"][0];
    assert_eq!(l5["item"]["id"], "l5");
    assert_eq!(l5["children"], json!([]));
    assert_eq!(l5["has_more"], true);

    // Depth 2 JSON: root has_more=false, Big has 200 children + has_more=true, unexpanded L2 has [] + has_more=true
    let d2 = cli_json(
        client
            .cli(&["tree", "--drive", "dt", "--depth", "2", "--json"])
            .await,
    );
    assert_eq!(d2["has_more"], false);
    let nodes = d2["tree"].as_array().unwrap();
    let big = nodes.iter().find(|n| n["item"]["id"] == "big").unwrap();
    assert_eq!(big["children"].as_array().unwrap().len(), 200);
    assert_eq!(big["has_more"], true);
    let l1 = nodes.iter().find(|n| n["item"]["id"] == "l1").unwrap();
    assert_eq!(l1["has_more"], false);
    let l2 = &l1["children"][0];
    assert_eq!(l2["item"]["id"], "l2");
    assert_eq!(l2["children"], json!([]));
    assert_eq!(l2["has_more"], true);

    // Depth 2 text output includes truncation marker
    let d2_text = client.cli(&["tree", "--drive", "dt", "--depth", "2"]).await;
    assert!(d2_text.status.success());
    let stdout = String::from_utf8_lossy(&d2_text.stdout);
    assert!(
        stdout.contains("more items not shown (200 per folder)"),
        "expected truncation line in tree text output, got: {stdout}"
    );

    server.abort();
}

#[tokio::test]
async fn mcp_text_is_minified_and_matches_structured_content() {
    let (base, _state, server, profile, exe) = spawn_mock(MockState::default()).await;
    let client = Client::new(Path::new(&exe), profile.path(), &base);

    let ok_res = client.mcp_call("list_files", json!({"drive": "ds"})).await;
    assert_eq!(ok_res["isError"], false);
    let text = ok_res["content"][0]["text"].as_str().unwrap();
    assert!(
        !text.contains('\n'),
        "MCP text payload must be single-line minified JSON"
    );
    let parsed: Value = serde_json::from_str(text).unwrap();
    assert_eq!(parsed, ok_res["structuredContent"]);

    let err_res = client
        .mcp_call(
            "get_file_info",
            json!({"drive": "ds", "item_id": "s000", "compact": "bad"}),
        )
        .await;
    assert_eq!(err_res["isError"], true);
    assert!(err_res.get("structuredContent").is_none());

    // WI-3 (F-08): negative integer limit is rejected rather than silently defaulted
    let neg_limit = client
        .mcp_call("list_files", json!({"drive": "ds", "limit": -5}))
        .await;
    assert_eq!(neg_limit["isError"], true);
    assert_eq!(
        neg_limit["content"][0]["text"],
        "limit must be a non-negative integer"
    );
    assert!(neg_limit.get("structuredContent").is_none());

    server.abort();
}

#[tokio::test]
async fn mcp_tool_catalog_and_site_discovery_failure_contract() {
    let state = MockState {
        fail_site_discovery: true,
        ..MockState::default()
    };
    let (base, _st, server, profile, exe) = spawn_mock(state).await;
    let authed = Client::new(Path::new(&exe), profile.path(), &base);

    // Site discovery failure emits stderr warning and still returns personal drive
    let drives_out = authed.cli(&["drives", "--json"]).await;
    assert!(drives_out.status.success());
    let drives: Vec<Value> = serde_json::from_slice(&drives_out.stdout).unwrap();
    assert_eq!(drives.len(), 1);
    assert_eq!(drives[0]["id"], "d1");
    assert!(
        String::from_utf8_lossy(&drives_out.stderr).contains("SharePoint site discovery failed")
    );

    // MCP tools/list descriptions match spec
    let replies = authed
        .mcp_exchange(&[
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-06-18", "capabilities": {},
                "clientInfo": { "name": "catalog-check", "version": "1" }
            }}),
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        ])
        .await;
    let tools = replies[1]["result"]["tools"].as_array().unwrap();
    let find_tool = |name: &str| {
        tools
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("missing tool {name}"))
    };

    assert!(find_tool("list_drives")["description"]
        .as_str()
        .unwrap()
        .contains("200"));
    assert!(
        find_tool("list_files")["inputSchema"]["properties"]["limit"]["description"]
            .as_str()
            .unwrap()
            .contains("has_more")
    );
    assert!(
        find_tool("get_file_info")["inputSchema"]["properties"]["compact"]["description"]
            .as_str()
            .unwrap()
            .contains("Default true")
    );
    let dl_desc = find_tool("download_file")["description"].as_str().unwrap();
    for key in [
        "ok",
        "path",
        "file_name",
        "mime_type",
        "size_bytes",
        "is_image",
        "drive_id",
        "item_id",
        "e_tag",
        "c_tag",
    ] {
        assert!(dl_desc.contains(key), "download_file missing {key}");
    }
    assert!(
        find_tool("search_files")["inputSchema"]["properties"]["remote"]["description"]
            .as_str()
            .unwrap()
            .contains("Default true")
    );
    assert!(
        find_tool("list_versions")["inputSchema"]["properties"]["limit"]["description"]
            .as_str()
            .unwrap()
            .contains("has_more")
    );
    assert!(find_tool("list_items")["description"]
        .as_str()
        .unwrap()
        .contains("one page"));
    assert!(find_tool("list_items")["inputSchema"]["properties"]
        .get("limit")
        .is_some());
    assert!(find_tool("list_items")["inputSchema"]["properties"]
        .get("cursor")
        .is_some());
    assert_eq!(
        find_tool("create_share_link")["inputSchema"]["properties"]["type"]["enum"],
        json!(["view", "edit", "embed"])
    );
    assert_eq!(
        find_tool("create_share_link")["inputSchema"]["properties"]["scope"]["enum"],
        json!(["organization", "anonymous"])
    );
    assert!(find_tool("list_lists")["inputSchema"]["properties"]
        .get("site_id")
        .is_some());

    server.abort();
}
