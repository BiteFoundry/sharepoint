use std::{
    collections::HashMap,
    path::Path,
    process::Output,
    sync::Arc,
};

use axum::{
    body::Bytes,
    extract::{Path as RoutePath, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, patch},
    Json, Router,
};
use serde_json::{json, Value};
use tempfile::tempdir;
use tokio::{net::TcpListener, process::Command, sync::Mutex};

const SITE_ID: &str = "site-eng";
const LIST_ID: &str = "list-scores";
const LIST_ITEM_ID: &str = "17";
const DRIVE_ME: &str = "drv-me";
const DRIVE_SP: &str = "drv-site-eng-docs";

#[derive(Debug, Clone)]
struct RecordedRequest {
    method: String,
    path: String,
    body: Option<Value>,
    raw_bytes: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
struct MockItem {
    id: String,
    drive_id: String,
    parent_id: String,
    parent_path: String,
    name: String,
    is_folder: bool,
    size: u64,
    mime_type: Option<String>,
}

impl MockItem {
    fn full_path(&self) -> String {
        if self.parent_path.is_empty() {
            format!("/{}", self.name)
        } else {
            format!("{}/{}", self.parent_path, self.name)
        }
    }

    fn to_graph_json(&self) -> Value {
        let graph_parent_path = if self.parent_path.is_empty() {
            "/drive/root:".to_string()
        } else {
            format!("/drive/root:{}", self.parent_path)
        };
        let mut obj = json!({
            "id": self.id,
            "name": self.name,
            "size": self.size,
            "eTag": format!("\"etag-{}\"", self.id),
            "parentReference": {
                "driveId": self.drive_id,
                "id": self.parent_id,
                "path": graph_parent_path,
            }
        });
        if self.is_folder {
            obj["folder"] = json!({ "childCount": 0 });
        } else {
            obj["file"] = json!({
                "mimeType": self.mime_type.as_deref().unwrap_or("text/markdown")
            });
        }
        obj
    }
}

#[derive(Default)]
struct MockDriveState {
    items: HashMap<(String, String), MockItem>,
    file_bytes: HashMap<(String, String), Vec<u8>>,
    next_seq: usize,
    requests: Vec<RecordedRequest>,
    list_field_writes: Vec<Value>,
}

impl MockDriveState {
    fn seed_item(&mut self, item: MockItem) {
        self.items
            .insert((item.drive_id.clone(), item.id.clone()), item);
    }

    fn find_by_path(&self, drive_id: &str, path: &str) -> Option<MockItem> {
        let norm = format!("/{}", path.trim().trim_matches('/'));
        self.items
            .values()
            .find(|item| item.drive_id == drive_id && item.full_path().eq_ignore_ascii_case(&norm))
            .cloned()
    }

    fn children_of_path(&self, drive_id: &str, parent_path: &str) -> Vec<MockItem> {
        let norm = parent_path.trim().trim_end_matches('/');
        let norm = if norm.is_empty() || norm == "/" {
            String::new()
        } else if norm.starts_with('/') {
            norm.to_string()
        } else {
            format!("/{norm}")
        };
        let mut out: Vec<MockItem> = self
            .items
            .values()
            .filter(|item| {
                item.drive_id == drive_id && item.parent_path.eq_ignore_ascii_case(&norm)
            })
            .cloned()
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    fn children_of_id(&self, drive_id: &str, parent_id: &str) -> Vec<MockItem> {
        let mut out: Vec<MockItem> = self
            .items
            .values()
            .filter(|item| item.drive_id == drive_id && item.parent_id == parent_id)
            .cloned()
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    fn move_or_rename(
        &mut self,
        drive_id: &str,
        item_id: &str,
        new_name: Option<&str>,
        new_parent_id: Option<&str>,
    ) -> Option<MockItem> {
        let key = (drive_id.to_string(), item_id.to_string());
        let existing = self.items.get(&key)?.clone();
        let old_full = existing.full_path();

        let (next_parent_id, next_parent_path) = match new_parent_id {
            Some("root") => ("root".to_string(), String::new()),
            Some(pid) => {
                let parent = self.items.get(&(drive_id.to_string(), pid.to_string()))?;
                (parent.id.clone(), parent.full_path())
            }
            None => (existing.parent_id.clone(), existing.parent_path.clone()),
        };
        let next_name = new_name
            .map(str::to_string)
            .unwrap_or_else(|| existing.name.clone());

        let updated = MockItem {
            parent_id: next_parent_id,
            parent_path: next_parent_path,
            name: next_name,
            ..existing
        };
        let new_full = updated.full_path();
        self.items.insert(key, updated.clone());

        if updated.is_folder && old_full != new_full {
            let prefix_slash = format!("{old_full}/");
            for item in self.items.values_mut() {
                if item.drive_id != drive_id {
                    continue;
                }
                if item.parent_path.eq_ignore_ascii_case(&old_full) {
                    item.parent_path = new_full.clone();
                } else if item.parent_path.len() > prefix_slash.len()
                    && item.parent_path[..prefix_slash.len()].eq_ignore_ascii_case(&prefix_slash)
                {
                    let suffix = &item.parent_path[old_full.len()..];
                    item.parent_path = format!("{new_full}{suffix}");
                }
            }
        }
        Some(updated)
    }
}

#[derive(Clone)]
struct MockApp {
    state: Arc<Mutex<MockDriveState>>,
}

async fn start_mock_server(
    initial: MockDriveState,
) -> (String, Arc<Mutex<MockDriveState>>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}/v1.0");
    let state = Arc::new(Mutex::new(initial));

    let app = Router::new()
        .route(
            "/v1.0/me/drive",
            get(|State(mock): State<MockApp>| async move {
                mock.state.lock().await.requests.push(RecordedRequest {
                    method: "GET".into(),
                    path: "/v1.0/me/drive".into(),
                    body: None,
                    raw_bytes: None,
                });
                Json(json!({
                    "id": DRIVE_ME,
                    "name": "OneDrive",
                    "driveType": "business",
                    "webUrl": "https://synthetic-my.sharepoint.test/personal/user"
                }))
            }),
        )
        .route(
            "/v1.0/me/followedSites",
            get(|State(mock): State<MockApp>| async move {
                mock.state.lock().await.requests.push(RecordedRequest {
                    method: "GET".into(),
                    path: "/v1.0/me/followedSites".into(),
                    body: None,
                    raw_bytes: None,
                });
                Json(json!({
                    "value": [{
                        "id": SITE_ID,
                        "name": "engineering",
                        "displayName": "Engineering Hub",
                        "webUrl": "https://synthetic.sharepoint.test/sites/engineering"
                    }]
                }))
            }),
        )
        .route(
            "/v1.0/sites/{site_id}/drives",
            get(
                |State(mock): State<MockApp>, RoutePath(site_id): RoutePath<String>| async move {
                    mock.state.lock().await.requests.push(RecordedRequest {
                        method: "GET".into(),
                        path: format!("/v1.0/sites/{site_id}/drives"),
                        body: None,
                        raw_bytes: None,
                    });
                    Json(json!({
                        "value": [{
                            "id": DRIVE_SP,
                            "name": "Documents",
                            "driveType": "documentLibrary",
                            "webUrl": "https://synthetic.sharepoint.test/sites/engineering/Shared%20Documents"
                        }]
                    }))
                },
            ),
        )
        .route(
            "/v1.0/sites/{site_id}/lists/{list_id}/columns",
            get(
                |State(mock): State<MockApp>,
                 RoutePath((site_id, list_id)): RoutePath<(String, String)>| async move {
                    mock.state.lock().await.requests.push(RecordedRequest {
                        method: "GET".into(),
                        path: format!("/v1.0/sites/{site_id}/lists/{list_id}/columns"),
                        body: None,
                        raw_bytes: None,
                    });
                    Json(json!({
                        "value": [{
                            "id": "col-score",
                            "name": "score",
                            "displayName": "Score",
                            "readOnly": false
                        }]
                    }))
                },
            ),
        )
        .route(
            "/v1.0/sites/{site_id}/lists/{list_id}/items/{item_id}/fields",
            patch(
                |State(mock): State<MockApp>,
                 RoutePath((site_id, list_id, item_id)): RoutePath<(String, String, String)>,
                 Json(body): Json<Value>| async move {
                    let mut st = mock.state.lock().await;
                    st.requests.push(RecordedRequest {
                        method: "PATCH".into(),
                        path: format!("/v1.0/sites/{site_id}/lists/{list_id}/items/{item_id}/fields"),
                        body: Some(body.clone()),
                        raw_bytes: None,
                    });
                    st.list_field_writes.push(body.clone());
                    Json(body)
                },
            ),
        )
        .route(
            "/v1.0/drives/{drive_id}/root/children",
            get(
                |State(mock): State<MockApp>, RoutePath(drive_id): RoutePath<String>| async move {
                    let mut st = mock.state.lock().await;
                    st.requests.push(RecordedRequest {
                        method: "GET".into(),
                        path: format!("/v1.0/drives/{drive_id}/root/children"),
                        body: None,
                        raw_bytes: None,
                    });
                    let value: Vec<Value> = st
                        .children_of_path(&drive_id, "")
                        .into_iter()
                        .map(|it| it.to_graph_json())
                        .collect();
                    Json(json!({ "value": value }))
                },
            )
            .post(
                |State(mock): State<MockApp>,
                 RoutePath(drive_id): RoutePath<String>,
                 Json(body): Json<Value>| async move {
                    let mut st = mock.state.lock().await;
                    st.requests.push(RecordedRequest {
                        method: "POST".into(),
                        path: format!("/v1.0/drives/{drive_id}/root/children"),
                        body: Some(body.clone()),
                        raw_bytes: None,
                    });
                    st.next_seq += 1;
                    let id = format!("fld-created-{}", st.next_seq);
                    let name = body
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("New Folder")
                        .to_string();
                    let folder = MockItem {
                        id,
                        drive_id: drive_id.clone(),
                        parent_id: "root".into(),
                        parent_path: String::new(),
                        name,
                        is_folder: true,
                        size: 0,
                        mime_type: None,
                    };
                    let resp = folder.to_graph_json();
                    st.seed_item(folder);
                    (StatusCode::CREATED, Json(resp))
                },
            ),
        )
        .route(
            "/v1.0/drives/{drive_id}/items/{item_id}",
            get(
                |State(mock): State<MockApp>,
                 RoutePath((drive_id, item_id)): RoutePath<(String, String)>| async move {
                    let mut st = mock.state.lock().await;
                    st.requests.push(RecordedRequest {
                        method: "GET".into(),
                        path: format!("/v1.0/drives/{drive_id}/items/{item_id}"),
                        body: None,
                        raw_bytes: None,
                    });
                    match st.items.get(&(drive_id, item_id)) {
                        Some(item) => (StatusCode::OK, Json(item.to_graph_json())).into_response(),
                        None => (
                            StatusCode::NOT_FOUND,
                            Json(json!({ "error": { "code": "itemNotFound" } })),
                        )
                            .into_response(),
                    }
                },
            )
            .patch(
                |State(mock): State<MockApp>,
                 RoutePath((drive_id, item_id)): RoutePath<(String, String)>,
                 Json(body): Json<Value>| async move {
                    let mut st = mock.state.lock().await;
                    st.requests.push(RecordedRequest {
                        method: "PATCH".into(),
                        path: format!("/v1.0/drives/{drive_id}/items/{item_id}"),
                        body: Some(body.clone()),
                        raw_bytes: None,
                    });
                    let new_name = body.get("name").and_then(Value::as_str);
                    let new_parent_id = body
                        .get("parentReference")
                        .and_then(|p| p.get("id"))
                        .and_then(Value::as_str);
                    match st.move_or_rename(&drive_id, &item_id, new_name, new_parent_id) {
                        Some(updated) => {
                            (StatusCode::OK, Json(updated.to_graph_json())).into_response()
                        }
                        None => (
                            StatusCode::NOT_FOUND,
                            Json(json!({ "error": { "code": "itemNotFound" } })),
                        )
                            .into_response(),
                    }
                },
            ),
        )
        .route(
            "/v1.0/drives/{drive_id}/items/{item_id}/children",
            get(
                |State(mock): State<MockApp>,
                 RoutePath((drive_id, item_id)): RoutePath<(String, String)>| async move {
                    let mut st = mock.state.lock().await;
                    st.requests.push(RecordedRequest {
                        method: "GET".into(),
                        path: format!("/v1.0/drives/{drive_id}/items/{item_id}/children"),
                        body: None,
                        raw_bytes: None,
                    });
                    let value: Vec<Value> = st
                        .children_of_id(&drive_id, &item_id)
                        .into_iter()
                        .map(|it| it.to_graph_json())
                        .collect();
                    Json(json!({ "value": value }))
                },
            )
            .post(
                |State(mock): State<MockApp>,
                 RoutePath((drive_id, item_id)): RoutePath<(String, String)>,
                 Json(body): Json<Value>| async move {
                    let mut st = mock.state.lock().await;
                    st.requests.push(RecordedRequest {
                        method: "POST".into(),
                        path: format!("/v1.0/drives/{drive_id}/items/{item_id}/children"),
                        body: Some(body.clone()),
                        raw_bytes: None,
                    });
                    let Some(parent) = st.items.get(&(drive_id.clone(), item_id.clone())).cloned()
                    else {
                        return (
                            StatusCode::NOT_FOUND,
                            Json(json!({ "error": { "code": "itemNotFound" } })),
                        )
                            .into_response();
                    };
                    st.next_seq += 1;
                    let id = format!("fld-created-{}", st.next_seq);
                    let name = body
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("New Folder")
                        .to_string();
                    let folder = MockItem {
                        id,
                        drive_id,
                        parent_id: parent.id.clone(),
                        parent_path: parent.full_path(),
                        name,
                        is_folder: true,
                        size: 0,
                        mime_type: None,
                    };
                    let resp = folder.to_graph_json();
                    st.seed_item(folder);
                    (StatusCode::CREATED, Json(resp)).into_response()
                },
            ),
        )
        .route(
            "/v1.0/search/query",
            axum::routing::post(
                |State(mock): State<MockApp>, Json(body): Json<Value>| async move {
                    let mut st = mock.state.lock().await;
                    st.requests.push(RecordedRequest {
                        method: "POST".into(),
                        path: "/v1.0/search/query".into(),
                        body: Some(body.clone()),
                        raw_bytes: None,
                    });
                    let query = body["requests"][0]["query"]["queryString"]
                        .as_str()
                        .unwrap_or("")
                        .to_lowercase();
                    let hits: Vec<Value> = st
                        .items
                        .values()
                        .filter(|item| {
                            item.name.to_lowercase().contains(&query)
                                || item.parent_path.to_lowercase().contains(&query)
                        })
                        .map(|item| json!({ "resource": item.to_graph_json() }))
                        .collect();
                    Json(json!({
                        "value": [{
                            "hitsContainers": [{ "hits": hits }]
                        }]
                    }))
                },
            ),
        )
        .route(
            "/v1.0/drives/{drive_id}/{*tail}",
            get(
                |State(mock): State<MockApp>,
                 RoutePath((drive_id, tail)): RoutePath<(String, String)>,
                 Query(_q): Query<HashMap<String, String>>| async move {
                    let mut st = mock.state.lock().await;
                    st.requests.push(RecordedRequest {
                        method: "GET".into(),
                        path: format!("/v1.0/drives/{drive_id}/{tail}"),
                        body: None,
                        raw_bytes: None,
                    });
                    if let Some(after) = tail.strip_prefix("root/search(q='") {
                        let raw_q = after.strip_suffix("')").unwrap_or(after);
                        let decoded = urlencoding::decode(raw_q)
                            .map(|c| c.into_owned())
                            .unwrap_or_else(|_| raw_q.to_string())
                            .replace("''", "'")
                            .to_lowercase();
                        let value: Vec<Value> = st
                            .items
                            .values()
                            .filter(|item| {
                                item.drive_id == drive_id
                                    && (item.name.to_lowercase().contains(&decoded)
                                        || item.parent_path.to_lowercase().contains(&decoded))
                            })
                            .map(|item| item.to_graph_json())
                            .collect();
                        return (StatusCode::OK, Json(json!({ "value": value }))).into_response();
                    }
                    if tail.starts_with("root:/") && tail.ends_with(":/children") {
                        let rel = tail
                            .trim_start_matches("root:/")
                            .trim_end_matches(":/children");
                        let decoded = urlencoding::decode(rel)
                            .map(|c| c.into_owned())
                            .unwrap_or_else(|_| rel.to_string());
                        let value: Vec<Value> = st
                            .children_of_path(&drive_id, &decoded)
                            .into_iter()
                            .map(|it| it.to_graph_json())
                            .collect();
                        return (StatusCode::OK, Json(json!({ "value": value }))).into_response();
                    }
                    if tail.starts_with("root:/") && tail.ends_with(':') {
                        let rel = tail.trim_start_matches("root:/").trim_end_matches(':');
                        let decoded = urlencoding::decode(rel)
                            .map(|c| c.into_owned())
                            .unwrap_or_else(|_| rel.to_string());
                        if let Some(item) = st.find_by_path(&drive_id, &decoded) {
                            return (StatusCode::OK, Json(item.to_graph_json())).into_response();
                        }
                    }
                    (
                        StatusCode::NOT_FOUND,
                        Json(json!({ "error": { "code": "itemNotFound" } })),
                    )
                        .into_response()
                },
            )
            .put(
                |State(mock): State<MockApp>,
                 RoutePath((drive_id, tail)): RoutePath<(String, String)>,
                 body: Bytes| async move {
                    let mut st = mock.state.lock().await;
                    st.requests.push(RecordedRequest {
                        method: "PUT".into(),
                        path: format!("/v1.0/drives/{drive_id}/{tail}"),
                        body: None,
                        raw_bytes: Some(body.to_vec()),
                    });
                    if tail.starts_with("root:/") && tail.ends_with(":/content") {
                        let raw_name = tail
                            .trim_start_matches("root:/")
                            .trim_end_matches(":/content");
                        let name = urlencoding::decode(raw_name)
                            .map(|c| c.into_owned())
                            .unwrap_or_else(|_| raw_name.to_string());
                        st.next_seq += 1;
                        let id = format!("item-up-{}", st.next_seq);
                        let item = MockItem {
                            id: id.clone(),
                            drive_id: drive_id.clone(),
                            parent_id: "root".into(),
                            parent_path: String::new(),
                            name,
                            is_folder: false,
                            size: body.len() as u64,
                            mime_type: Some("text/markdown".into()),
                        };
                        let resp = item.to_graph_json();
                        st.seed_item(item);
                        st.file_bytes.insert((drive_id, id), body.to_vec());
                        return (StatusCode::CREATED, Json(resp)).into_response();
                    }
                    if tail.starts_with("items/") && tail.ends_with(":/content") {
                        let inner = tail
                            .trim_start_matches("items/")
                            .trim_end_matches(":/content");
                        if let Some((parent_id, raw_name)) = inner.split_once(":/") {
                            let Some(parent) = st
                                .items
                                .get(&(drive_id.clone(), parent_id.to_string()))
                                .cloned()
                            else {
                                return (
                                    StatusCode::NOT_FOUND,
                                    Json(json!({ "error": { "code": "itemNotFound" } })),
                                )
                                    .into_response();
                            };
                            let name = urlencoding::decode(raw_name)
                                .map(|c| c.into_owned())
                                .unwrap_or_else(|_| raw_name.to_string());
                            st.next_seq += 1;
                            let id = format!("item-up-{}", st.next_seq);
                            let item = MockItem {
                                id: id.clone(),
                                drive_id: drive_id.clone(),
                                parent_id: parent.id.clone(),
                                parent_path: parent.full_path(),
                                name,
                                is_folder: false,
                                size: body.len() as u64,
                                mime_type: Some("text/markdown".into()),
                            };
                            let resp = item.to_graph_json();
                            st.seed_item(item);
                            st.file_bytes.insert((drive_id, id), body.to_vec());
                            return (StatusCode::CREATED, Json(resp)).into_response();
                        }
                    }
                    (
                        StatusCode::NOT_FOUND,
                        Json(json!({ "error": { "code": "itemNotFound" } })),
                    )
                        .into_response()
                },
            ),
        )
        .with_state(MockApp {
            state: state.clone(),
        });

    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (base_url, state, server)
}

struct CliRunner<'a> {
    exe: &'a Path,
    profile: &'a Path,
    base_url: &'a str,
}

impl CliRunner<'_> {
    async fn run(&self, args: &[&str]) -> Output {
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
            .env("SHAREPOINT_GRAPH_BASE_URL", self.base_url)
            .env("SHAREPOINT_ACCESS_TOKEN", "synthetic-cli-test-token")
            .env("SHAREPOINT_DISABLE_KEYRING", "1")
            .env("SHAREPOINT_SHARE_M365_AUTH", "0")
            .env("SHAREPOINT_AUTO_DAEMON", "0")
            .env("SHAREPOINT_CONFIG_DIR", self.profile.join("config"))
            .env("SHAREPOINT_DATA_DIR", self.profile.join("data"))
            .env("NO_PROXY", "127.0.0.1,localhost")
            .args(args)
            .output()
            .await
            .unwrap()
    }

    async fn run_json(&self, args: &[&str]) -> Value {
        let out = self.run(args).await;
        assert!(
            out.status.success(),
            "CLI failed for {args:?} (code {:?})\nstdout: {}\nstderr: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap_or_else(|err| {
            panic!(
                "Invalid JSON for {args:?}: {err}\nstdout: {}",
                String::from_utf8_lossy(&out.stdout)
            )
        })
    }
}

#[tokio::test]
async fn cli_hyphen_filenames_search_and_lists_negative_number_on_both_binaries() {
    let mut initial = MockDriveState::default();
    for drive_id in [DRIVE_SP, DRIVE_ME] {
        initial.seed_item(MockItem {
            id: "item-rename-target".into(),
            drive_id: drive_id.into(),
            parent_id: "root".into(),
            parent_path: String::new(),
            name: "draft.md".into(),
            is_folder: false,
            size: 11,
            mime_type: Some("text/markdown".into()),
        });
        initial.seed_item(MockItem {
            id: "item-eq-target".into(),
            drive_id: drive_id.into(),
            parent_id: "root".into(),
            parent_path: String::new(),
            name: "flag-target.md".into(),
            is_folder: false,
            size: 9,
            mime_type: Some("text/markdown".into()),
        });
        initial.seed_item(MockItem {
            id: "fld-uploads".into(),
            drive_id: drive_id.into(),
            parent_id: "root".into(),
            parent_path: String::new(),
            name: "Uploads".into(),
            is_folder: true,
            size: 0,
            mime_type: None,
        });
    }

    let (base_url, state, server) = start_mock_server(initial).await;
    let tmp = tempdir().unwrap();
    let local_file = tmp.path().join("test.txt");
    std::fs::write(&local_file, b"hyphen payload bytes").unwrap();
    let local_file_str = local_file.to_string_lossy().into_owned();

    let binaries = [
        ("sharepoint", Path::new(env!("CARGO_BIN_EXE_sharepoint")), DRIVE_SP),
        (
            "bitefoundry-onedrive",
            Path::new(env!("CARGO_BIN_EXE_bitefoundry-onedrive")),
            DRIVE_ME,
        ),
    ];

    for (label, exe, expected_drive) in binaries {
        let profile = tempdir().unwrap();
        let cli = CliRunner {
            exe,
            profile: profile.path(),
            base_url: &base_url,
        };

        // 1. mkdir -report.md sends exact name in POST body.
        let mk = cli.run_json(&["mkdir", "-report.md", "--json"]).await;
        assert_eq!(mk["name"], "-report.md", "{label}: mkdir response");
        assert_eq!(mk["drive_id"], expected_drive, "{label}: mkdir drive");
        {
            let st = state.lock().await;
            let post_req = st
                .requests
                .iter()
                .rev()
                .find(|r| {
                    r.method == "POST"
                        && r.path == format!("/v1.0/drives/{expected_drive}/root/children")
                })
                .expect("expected POST root/children for mkdir");
            assert_eq!(
                post_req.body.as_ref().unwrap()["name"],
                "-report.md",
                "{label}: mkdir POST body name"
            );
        }

        // 2. upload test.txt --name -report.md sends exact upload path and bytes.
        let up = cli
            .run_json(&[
                "upload",
                &local_file_str,
                "/Uploads",
                "--name",
                "-report.md",
                "--json",
            ])
            .await;
        assert_eq!(up["name"], "-report.md", "{label}: upload response");
        assert_eq!(up["parent_path"], "/Uploads", "{label}: upload parent_path");
        {
            let st = state.lock().await;
            let put_req = st
                .requests
                .iter()
                .rev()
                .find(|r| r.method == "PUT" && r.path.contains(expected_drive))
                .expect("expected PUT for upload");
            assert_eq!(
                put_req.path,
                format!("/v1.0/drives/{expected_drive}/items/fld-uploads:/-report.md:/content"),
                "{label}: upload PUT path"
            );
            assert_eq!(
                put_req.raw_bytes.as_deref(),
                Some(b"hyphen payload bytes".as_slice()),
                "{label}: upload PUT payload"
            );
        }

        // 3. mv item-rename-target --name -report.md sends exact PATCH body.
        let mv = cli
            .run_json(&["mv", "item-rename-target", "--name", "-report.md", "--json"])
            .await;
        assert_eq!(mv["name"], "-report.md", "{label}: mv response");
        {
            let st = state.lock().await;
            let patch_req = st
                .requests
                .iter()
                .rev()
                .find(|r| {
                    r.method == "PATCH"
                        && r.path
                            == format!("/v1.0/drives/{expected_drive}/items/item-rename-target")
                })
                .expect("expected PATCH for mv");
            assert_eq!(
                patch_req.body.as_ref().unwrap(),
                &json!({ "name": "-report.md" }),
                "{label}: mv PATCH body"
            );
        }

        // 4. search -report.md finds the items in local cache and remote Graph search.
        let local_hits = cli.run_json(&["search", "-report.md", "--json"]).await;
        let local_arr = local_hits.as_array().unwrap();
        assert!(
            local_arr
                .iter()
                .any(|it| it["id"] == "item-rename-target" && it["name"] == "-report.md"),
            "{label}: local search must find renamed -report.md, got {local_hits}"
        );
        assert!(
            local_arr
                .iter()
                .any(|it| it["parent_path"] == "/Uploads" && it["name"] == "-report.md"),
            "{label}: local search must find uploaded -report.md, got {local_hits}"
        );
        assert!(
            local_arr
                .iter()
                .any(|it| it["is_folder"] == true && it["name"] == "-report.md"),
            "{label}: local search must find created -report.md folder, got {local_hits}"
        );

        let remote_hits = cli
            .run_json(&["search", "-report.md", "--drive", expected_drive, "--remote", "--json"])
            .await;
        assert!(
            remote_hits
                .as_array()
                .unwrap()
                .iter()
                .any(|it| it["name"] == "-report.md"),
            "{label}: remote search must find -report.md"
        );
        {
            let st = state.lock().await;
            assert!(
                st.requests.iter().any(|r| {
                    r.method == "GET"
                        && r.path
                            == format!("/v1.0/drives/{expected_drive}/root/search(q='-report.md')")
                }),
                "{label}: remote drive search must pass -report.md query"
            );
        }

        // 5. Korean hyphen query ("- 목록 항목") works through mkdir + local/remote search.
        let mk_ko = cli.run_json(&["mkdir", "- 목록 항목", "--json"]).await;
        assert_eq!(mk_ko["name"], "- 목록 항목", "{label}: Korean mkdir");
        let ko_hits = cli.run_json(&["search", "- 목록 항목", "--json"]).await;
        assert!(
            ko_hits
                .as_array()
                .unwrap()
                .iter()
                .any(|it| it["name"] == "- 목록 항목"),
            "{label}: local search must find Korean hyphen folder, got {ko_hits}"
        );
        let ko_remote = cli
            .run_json(&["search", "- 목록 항목", "--remote", "--json"])
            .await;
        assert!(
            ko_remote
                .as_array()
                .unwrap()
                .iter()
                .any(|it| it["name"] == "- 목록 항목"),
            "{label}: cross-site remote search must receive '- 목록 항목'"
        );
        {
            let st = state.lock().await;
            let search_req = st
                .requests
                .iter()
                .rev()
                .find(|r| r.method == "POST" && r.path == "/v1.0/search/query")
                .expect("expected POST /v1.0/search/query");
            assert_eq!(
                search_req.body.as_ref().unwrap()["requests"][0]["query"]["queryString"],
                "- 목록 항목"
            );
        }

        // 6. Control case: lists set-field --value-json -4 sends JSON number -4.
        let list_res = cli
            .run_json(&[
                "lists",
                "set-field",
                "--site-id",
                SITE_ID,
                "--list-id",
                LIST_ID,
                "--item-id",
                LIST_ITEM_ID,
                "--field",
                "score",
                "--value-json",
                "-4",
                "--json",
            ])
            .await;
        assert_eq!(list_res["fields"]["score"], json!(-4));
        {
            let st = state.lock().await;
            assert_eq!(st.list_field_writes.last(), Some(&json!({ "score": -4 })));
        }

        // 7. Explicit flag-looking text works with "=" and positional "--".
        let mv_eq = cli
            .run_json(&["mv", "item-eq-target", "--name=--dry-run", "--json"])
            .await;
        assert_eq!(mv_eq["name"], "--dry-run");
        let mk_sep = cli
            .run_json(&["mkdir", "--json", "--", "--dry-run"])
            .await;
        assert_eq!(mk_sep["name"], "--dry-run");

        // 8. Missing values, real flags, unknown long flags, and --help perform 0 Graph requests.
        let reqs_before_guards = state.lock().await.requests.len();
        for bad_cmd in [
            vec!["mv", "item-rename-target", "--name", "--json"],
            vec!["mv", "item-rename-target", "--name", "--dry-run"],
            vec!["upload", &local_file_str, "--name", "--json"],
            vec!["upload", &local_file_str, "--name", "--dry-run"],
            vec!["mkdir", "--json"],
            vec!["mkdir", "--dry-run"],
            vec!["search", "--json"],
            vec!["search", "--dry-run"],
        ] {
            let out = cli.run(&bad_cmd).await;
            assert_eq!(
                out.status.code(),
                Some(2),
                "{label}: expected exit code 2 for {bad_cmd:?}, got {:?}\nstderr: {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr)
            );
        }

        for help_cmd in [
            vec!["mv", "id", "--name", "-report.md", "--help"],
            vec!["upload", "test.txt", "--name", "-report.md", "--help"],
            vec!["mkdir", "-report.md", "--help"],
            vec!["search", "- 목록 항목", "--help"],
            vec![
                "lists",
                "set-field",
                "--site-id",
                SITE_ID,
                "--list-id",
                LIST_ID,
                "--item-id",
                LIST_ITEM_ID,
                "--field",
                "score",
                "--value-json",
                "-4",
                "--help",
            ],
        ] {
            let out = cli.run(&help_cmd).await;
            assert!(
                out.status.success(),
                "{label}: expected exit 0 for {help_cmd:?}\nstderr: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(
                String::from_utf8_lossy(&out.stdout).contains("Usage:"),
                "{label}: expected help output for {help_cmd:?}"
            );
        }

        assert_eq!(
            state.lock().await.requests.len(),
            reqs_before_guards,
            "{label}: rejected flag invocations and --help must send 0 Graph requests"
        );
    }

    server.abort();
}

#[tokio::test]
async fn cli_folder_rename_and_parent_move_preserve_child_ids_and_offline_reads() {
    let mut initial = MockDriveState::default();
    // Case A: /Archive -> /Renamed (same parent), then /Renamed -> /Target/Renamed (parent change from root).
    initial.seed_item(MockItem {
        id: "fld-archive".into(),
        drive_id: DRIVE_SP.into(),
        parent_id: "root".into(),
        parent_path: String::new(),
        name: "Archive".into(),
        is_folder: true,
        size: 0,
        mime_type: None,
    });
    initial.seed_item(MockItem {
        id: "item-archive-plan".into(),
        drive_id: DRIVE_SP.into(),
        parent_id: "fld-archive".into(),
        parent_path: "/Archive".into(),
        name: "plan.md".into(),
        is_folder: false,
        size: 42,
        mime_type: Some("text/markdown".into()),
    });
    initial.seed_item(MockItem {
        id: "fld-archive-sub".into(),
        drive_id: DRIVE_SP.into(),
        parent_id: "fld-archive".into(),
        parent_path: "/Archive".into(),
        name: "Nested".into(),
        is_folder: true,
        size: 0,
        mime_type: None,
    });
    initial.seed_item(MockItem {
        id: "item-archive-deep".into(),
        drive_id: DRIVE_SP.into(),
        parent_id: "fld-archive-sub".into(),
        parent_path: "/Archive/Nested".into(),
        name: "deep.md".into(),
        is_folder: false,
        size: 19,
        mime_type: Some("text/markdown".into()),
    });
    // Case B: parent change between non-root folders (/OldArea/Box -> /NewArea/Box).
    initial.seed_item(MockItem {
        id: "fld-old-area".into(),
        drive_id: DRIVE_SP.into(),
        parent_id: "root".into(),
        parent_path: String::new(),
        name: "OldArea".into(),
        is_folder: true,
        size: 0,
        mime_type: None,
    });
    initial.seed_item(MockItem {
        id: "fld-new-area".into(),
        drive_id: DRIVE_SP.into(),
        parent_id: "root".into(),
        parent_path: String::new(),
        name: "NewArea".into(),
        is_folder: true,
        size: 0,
        mime_type: None,
    });
    initial.seed_item(MockItem {
        id: "fld-box".into(),
        drive_id: DRIVE_SP.into(),
        parent_id: "fld-old-area".into(),
        parent_path: "/OldArea".into(),
        name: "Box".into(),
        is_folder: true,
        size: 0,
        mime_type: None,
    });
    initial.seed_item(MockItem {
        id: "item-box-note".into(),
        drive_id: DRIVE_SP.into(),
        parent_id: "fld-box".into(),
        parent_path: "/OldArea/Box".into(),
        name: "ledger.md".into(),
        is_folder: false,
        size: 77,
        mime_type: Some("text/markdown".into()),
    });

    let (base_url, state, server) = start_mock_server(initial).await;
    let profile = tempdir().unwrap();
    let cli = CliRunner {
        exe: Path::new(env!("CARGO_BIN_EXE_sharepoint")),
        profile: profile.path(),
        base_url: &base_url,
    };

    // Warm SQLite cache via online ls calls.
    cli.run_json(&["ls", "/", "--json"]).await;
    cli.run_json(&["ls", "/Archive", "--json"]).await;
    cli.run_json(&["ls", "/Archive/Nested", "--json"]).await;
    cli.run_json(&["ls", "/OldArea", "--json"]).await;
    cli.run_json(&["ls", "/OldArea/Box", "--json"]).await;
    cli.run_json(&["ls", "/NewArea", "--json"]).await;

    // -----------------------------------------------------------------------
    // Part 1: Rename /Archive -> /Renamed (same parent)
    // -----------------------------------------------------------------------
    let renamed = cli
        .run_json(&["mv", "/Archive", "--name", "Renamed", "--json"])
        .await;
    assert_eq!(renamed["id"], "fld-archive");
    assert_eq!(renamed["name"], "Renamed");

    let reqs_after_rename = state.lock().await.requests.len();

    let plan_info = cli
        .run_json(&["info", "/Renamed/plan.md", "--offline", "--json"])
        .await;
    assert_eq!(plan_info["item"]["id"], "item-archive-plan");
    assert_eq!(plan_info["item"]["parent_path"], "/Renamed");

    let deep_info = cli
        .run_json(&["info", "/Renamed/Nested/deep.md", "--offline", "--json"])
        .await;
    assert_eq!(deep_info["item"]["id"], "item-archive-deep");
    assert_eq!(deep_info["item"]["parent_path"], "/Renamed/Nested");

    let renamed_ls = cli.run_json(&["ls", "/Renamed", "--offline", "--json"]).await;
    let renamed_ids: Vec<&str> = renamed_ls["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|it| it["id"].as_str().unwrap())
        .collect();
    assert!(renamed_ids.contains(&"item-archive-plan"));
    assert!(renamed_ids.contains(&"fld-archive-sub"));

    let nested_ls = cli
        .run_json(&["ls", "/Renamed/Nested", "--offline", "--json"])
        .await;
    assert_eq!(nested_ls["items"].as_array().unwrap().len(), 1);
    assert_eq!(nested_ls["items"][0]["id"], "item-archive-deep");

    let old_info = cli
        .run_json_or_err(&["info", "/Archive/plan.md", "--offline", "--json"])
        .await;
    assert!(old_info.is_err(), "old path /Archive/plan.md must not resolve offline");

    let old_ls = cli.run_json(&["ls", "/Archive", "--offline", "--json"]).await;
    assert!(
        old_ls["items"].as_array().unwrap().is_empty(),
        "old folder /Archive must have 0 offline children"
    );

    let old_search = cli.run_json(&["search", "Archive", "--json"]).await;
    assert!(
        old_search.as_array().unwrap().is_empty(),
        "old path search 'Archive' must return 0 stale items, got {old_search}"
    );
    let new_search = cli.run_json(&["search", "Renamed", "--json"]).await;
    let new_search_ids: Vec<&str> = new_search
        .as_array()
        .unwrap()
        .iter()
        .map(|it| it["id"].as_str().unwrap())
        .collect();
    assert!(new_search_ids.contains(&"fld-archive"));
    assert!(new_search_ids.contains(&"item-archive-plan"));
    assert!(new_search_ids.contains(&"item-archive-deep"));

    assert_eq!(
        state.lock().await.requests.len(),
        reqs_after_rename,
        "info/ls --offline and local search after rename must send 0 Graph requests"
    );

    // -----------------------------------------------------------------------
    // Part 2: Parent change in same drive (/OldArea/Box -> /NewArea/Box)
    // -----------------------------------------------------------------------
    let moved_box = cli
        .run_json(&["mv", "/OldArea/Box", "--parent", "/NewArea", "--json"])
        .await;
    assert_eq!(moved_box["id"], "fld-box");
    assert_eq!(moved_box["parent_id"], "fld-new-area");
    assert_eq!(moved_box["parent_path"], "/NewArea");

    let reqs_after_box_move = state.lock().await.requests.len();

    let box_note_info = cli
        .run_json(&["info", "/NewArea/Box/ledger.md", "--offline", "--json"])
        .await;
    assert_eq!(box_note_info["item"]["id"], "item-box-note");
    assert_eq!(box_note_info["item"]["parent_path"], "/NewArea/Box");

    let new_area_ls = cli.run_json(&["ls", "/NewArea", "--offline", "--json"]).await;
    assert_eq!(new_area_ls["items"].as_array().unwrap().len(), 1);
    assert_eq!(new_area_ls["items"][0]["id"], "fld-box");

    let new_box_ls = cli
        .run_json(&["ls", "/NewArea/Box", "--offline", "--json"])
        .await;
    assert_eq!(new_box_ls["items"].as_array().unwrap().len(), 1);
    assert_eq!(new_box_ls["items"][0]["id"], "item-box-note");

    let old_area_ls = cli.run_json(&["ls", "/OldArea", "--offline", "--json"]).await;
    assert!(
        old_area_ls["items"].as_array().unwrap().is_empty(),
        "old parent /OldArea must no longer list moved folder Box"
    );
    let old_box_ls = cli
        .run_json(&["ls", "/OldArea/Box", "--offline", "--json"])
        .await;
    assert!(old_box_ls["items"].as_array().unwrap().is_empty());

    assert!(
        cli.run_json_or_err(&["info", "/OldArea/Box/ledger.md", "--offline", "--json"])
            .await
            .is_err(),
        "old path /OldArea/Box/ledger.md must not resolve offline"
    );

    let old_box_search = cli.run_json(&["search", "/OldArea/Box", "--json"]).await;
    assert!(
        old_box_search.as_array().unwrap().is_empty(),
        "search for old path '/OldArea/Box' must return 0 results, got {old_box_search}"
    );
    let old_area_search = cli.run_json(&["search", "OldArea", "--json"]).await;
    let old_area_ids: Vec<&str> = old_area_search
        .as_array()
        .unwrap()
        .iter()
        .map(|it| it["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        old_area_ids,
        vec!["fld-old-area"],
        "search 'OldArea' must not return moved Box or ledger.md"
    );
    let new_box_search = cli.run_json(&["search", "/NewArea/Box", "--json"]).await;
    assert_eq!(new_box_search.as_array().unwrap().len(), 1);
    assert_eq!(new_box_search[0]["id"], "item-box-note");

    assert_eq!(
        state.lock().await.requests.len(),
        reqs_after_box_move,
        "info/ls --offline and local search after parent move must send 0 Graph requests"
    );

    // -----------------------------------------------------------------------
    // Part 3: Parent change from root into subfolder (/Renamed -> /NewArea/Renamed)
    // -----------------------------------------------------------------------
    let moved_renamed = cli
        .run_json(&["mv", "/Renamed", "--parent", "/NewArea", "--json"])
        .await;
    assert_eq!(moved_renamed["id"], "fld-archive");
    assert_eq!(moved_renamed["parent_path"], "/NewArea");

    let reqs_after_root_move = state.lock().await.requests.len();

    let moved_plan = cli
        .run_json(&["info", "/NewArea/Renamed/plan.md", "--offline", "--json"])
        .await;
    assert_eq!(moved_plan["item"]["id"], "item-archive-plan");
    assert_eq!(moved_plan["item"]["parent_path"], "/NewArea/Renamed");

    let moved_deep = cli
        .run_json(&[
            "info",
            "/NewArea/Renamed/Nested/deep.md",
            "--offline",
            "--json",
        ])
        .await;
    assert_eq!(moved_deep["item"]["id"], "item-archive-deep");
    assert_eq!(moved_deep["item"]["parent_path"], "/NewArea/Renamed/Nested");

    let root_ls = cli.run_json(&["ls", "/", "--offline", "--json"]).await;
    let root_ids: Vec<&str> = root_ls["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|it| it["id"].as_str().unwrap())
        .collect();
    assert!(
        !root_ids.contains(&"fld-archive"),
        "moved folder /Renamed must no longer appear in root offline listing: {root_ids:?}"
    );
    let old_renamed_ls = cli.run_json(&["ls", "/Renamed", "--offline", "--json"]).await;
    assert!(old_renamed_ls["items"].as_array().unwrap().is_empty());

    assert_eq!(
        state.lock().await.requests.len(),
        reqs_after_root_move,
        "offline verification after root-to-subfolder move must send 0 Graph requests"
    );

    server.abort();
}

impl CliRunner<'_> {
    async fn run_json_or_err(&self, args: &[&str]) -> Result<Value, String> {
        let out = self.run(args).await;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).into_owned());
        }
        serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())
    }
}
