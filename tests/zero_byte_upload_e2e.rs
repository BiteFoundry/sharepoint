//! Regression tests for zero-length uploads with an explicit conflict behavior (SP-12).
//!
//! A zero-length payload cannot use an upload session, so it goes through the simple
//! `PUT .../content`, which replaces an existing file. `--conflict rename` must therefore
//! pick a free name itself instead of overwriting.

use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{OriginalUri, Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use serde_json::{json, Value};
use tokio::{net::TcpListener, sync::Mutex};

use sharepoint_cli::{
    auth::{AuthManager, StoredTokens},
    config::AppConfig,
    graph::{
        drives::{ConflictBehavior, UploadOptions},
        GraphClient,
    },
};

#[derive(Clone, Debug, PartialEq)]
struct RemoteFile {
    id: String,
    /// `root` or a parent item ID.
    parent: String,
    name: String,
    content: Vec<u8>,
}

#[derive(Clone, Default)]
struct Mock {
    files: Arc<Mutex<Vec<RemoteFile>>>,
    /// Raw request targets (path + query) of every `PUT`.
    puts: Arc<Mutex<Vec<String>>>,
    /// A server that answers every upload with its first file, whatever was asked.
    always_return_first: bool,
}

fn item_json(f: &RemoteFile) -> Value {
    json!({
        "id": f.id,
        "name": f.name,
        "size": f.content.len(),
        "file": { "mimeType": "text/plain" },
        "parentReference": { "driveId": "drv", "id": f.parent, "path": "/drive/root:" }
    })
}

/// Splits `root:/{name}:`, `root:/{name}:/content`, `items/{pid}:/{name}:` and
/// `items/{pid}:/{name}:/content` into (parent, name, is_content).
fn parse_tail(tail: &str) -> Option<(String, String, bool)> {
    let (parent, rest) = if let Some(rest) = tail.strip_prefix("root:/") {
        ("root".to_string(), rest)
    } else {
        let rest = tail.strip_prefix("items/")?;
        let (pid, rest) = rest.split_once(":/")?;
        (pid.to_string(), rest)
    };
    if let Some(name) = rest.strip_suffix(":/content") {
        Some((parent, name.to_string(), true))
    } else {
        rest.strip_suffix(':').map(|name| (parent, name.to_string(), false))
    }
}

async fn start(mock: Mock) -> (GraphClient, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1.0", listener.local_addr().unwrap());
    let app = Router::new()
        .route(
            "/v1.0/drives/{drive_id}/{*tail}",
            get(
                |State(m): State<Mock>, Path((_drive, tail)): Path<(String, String)>| async move {
                    let Some((parent, name, false)) = parse_tail(&tail) else {
                        return (StatusCode::BAD_REQUEST, "unsupported lookup").into_response();
                    };
                    let files = m.files.lock().await;
                    match files.iter().find(|f| f.parent == parent && f.name == name) {
                        Some(f) => Json(item_json(f)).into_response(),
                        None => (
                            StatusCode::NOT_FOUND,
                            Json(json!({ "error": { "code": "itemNotFound", "message": "The resource could not be found." } })),
                        )
                            .into_response(),
                    }
                },
            )
            .put(
                |State(m): State<Mock>,
                 OriginalUri(uri): OriginalUri,
                 Path((_drive, tail)): Path<(String, String)>,
                 body: Bytes| async move {
                    m.puts.lock().await.push(
                        uri.path_and_query()
                            .map(|pq| pq.as_str().to_string())
                            .unwrap_or_default(),
                    );
                    let Some((parent, name, true)) = parse_tail(&tail) else {
                        return (StatusCode::BAD_REQUEST, "unsupported upload").into_response();
                    };
                    let mut files = m.files.lock().await;
                    if m.always_return_first {
                        if let Some(first) = files.first_mut() {
                            first.content = body.to_vec();
                            return Json(item_json(first)).into_response();
                        }
                    }
                    // Like Graph's simple upload: an existing name is replaced in place.
                    if let Some(f) = files.iter_mut().find(|f| f.parent == parent && f.name == name) {
                        f.content = body.to_vec();
                        return Json(item_json(f)).into_response();
                    }
                    let created = RemoteFile {
                        id: format!("item-new-{}", files.len() + 1),
                        parent,
                        name,
                        content: body.to_vec(),
                    };
                    files.push(created.clone());
                    (StatusCode::CREATED, Json(item_json(&created))).into_response()
                },
            ),
        )
        .with_state(mock);
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let cfg = AppConfig {
        graph_base_url: base_url,
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
    let graph = GraphClient::new(cfg, auth).unwrap().with_my_user_id("user-1");
    (graph, server)
}

fn file(id: &str, parent: &str, name: &str, content: &[u8]) -> RemoteFile {
    RemoteFile {
        id: id.to_string(),
        parent: parent.to_string(),
        name: name.to_string(),
        content: content.to_vec(),
    }
}

fn with_conflict(behavior: ConflictBehavior) -> UploadOptions {
    UploadOptions {
        conflict_behavior: Some(behavior),
        ..Default::default()
    }
}

#[tokio::test]
async fn zero_byte_rename_does_not_overwrite_existing_file() {
    let mock = Mock {
        files: Arc::new(Mutex::new(vec![file("item-orig", "root", "notes.txt", b"KEEP")])),
        ..Default::default()
    };
    let (graph, server) = start(mock.clone()).await;

    let uploaded = graph
        .upload_bytes_with_options(
            "drv",
            None,
            "notes.txt",
            Vec::new(),
            None,
            &with_conflict(ConflictBehavior::Rename),
        )
        .await
        .unwrap();

    assert_ne!(uploaded.id, "item-orig", "rename must create a new item");
    assert_eq!(uploaded.name, "notes 1.txt");
    let files = mock.files.lock().await.clone();
    assert_eq!(
        files[0],
        file("item-orig", "root", "notes.txt", b"KEEP"),
        "the existing file must keep its id and content"
    );
    assert_eq!(files.len(), 2);
    assert_eq!(files[1].name, "notes 1.txt");
    assert!(files[1].content.is_empty());
    let puts = mock.puts.lock().await.clone();
    assert_eq!(puts, vec!["/v1.0/drives/drv/root:/notes%201.txt:/content".to_string()]);
    server.abort();
}

#[tokio::test]
async fn zero_byte_rename_without_collision_keeps_name() {
    let mock = Mock::default();
    let (graph, server) = start(mock.clone()).await;

    let uploaded = graph
        .upload_bytes_with_options(
            "drv",
            None,
            "notes.txt",
            Vec::new(),
            None,
            &with_conflict(ConflictBehavior::Rename),
        )
        .await
        .unwrap();
    assert_eq!(uploaded.name, "notes.txt", "a free name needs no rename");
    assert_eq!(
        *mock.puts.lock().await,
        vec!["/v1.0/drives/drv/root:/notes.txt:/content".to_string()]
    );
    server.abort();
}

#[tokio::test]
async fn zero_byte_rename_skips_taken_candidates_in_a_folder() {
    // Parent given as an item ID, a name that needs encoding, and the first candidate taken.
    let mock = Mock {
        files: Arc::new(Mutex::new(vec![
            file("item-a", "folder-1", "보고서 v1.txt", b"A"),
            file("item-b", "folder-1", "보고서 v1 1.txt", b"B"),
            file("item-root", "root", "보고서 v1 2.txt", b"other folder"),
        ])),
        ..Default::default()
    };
    let (graph, server) = start(mock.clone()).await;

    let uploaded = graph
        .upload_bytes_with_options(
            "drv",
            Some("folder-1"),
            "보고서 v1.txt",
            Vec::new(),
            None,
            &with_conflict(ConflictBehavior::Rename),
        )
        .await
        .unwrap();
    assert_eq!(uploaded.name, "보고서 v1 2.txt");
    let files = mock.files.lock().await.clone();
    assert_eq!(files[0].content, b"A");
    assert_eq!(files[1].content, b"B");
    assert_eq!(files[2].content, b"other folder", "a same-named file elsewhere is untouched");
    assert_eq!(files.len(), 4);
    assert_eq!(files[3].parent, "folder-1");
    assert_eq!(mock.puts.lock().await.len(), 1);
    server.abort();
}

#[tokio::test]
async fn zero_byte_rename_errors_when_server_returns_existing_item() {
    // A server that answers the upload with the pre-existing item: that is not a rename
    // and must not be reported as one.
    let mock = Mock {
        files: Arc::new(Mutex::new(vec![file("item-orig", "root", "notes.txt", b"KEEP")])),
        always_return_first: true,
        ..Default::default()
    };
    let (graph, server) = start(mock).await;

    let err = graph
        .upload_bytes_with_options(
            "drv",
            None,
            "notes.txt",
            Vec::new(),
            None,
            &with_conflict(ConflictBehavior::Rename),
        )
        .await
        .expect_err("returning the existing item must be an error");
    let msg = format!("{err:#}");
    assert!(msg.contains("did not create a new file"), "{msg}");
    assert!(msg.contains("item-orig"), "{msg}");
    server.abort();
}

#[tokio::test]
async fn zero_byte_fail_still_rejected_up_front() {
    let mock = Mock {
        files: Arc::new(Mutex::new(vec![file("item-orig", "root", "notes.txt", b"KEEP")])),
        ..Default::default()
    };
    let (graph, server) = start(mock.clone()).await;

    let err = graph
        .upload_bytes_with_options(
            "drv",
            None,
            "notes.txt",
            Vec::new(),
            None,
            &with_conflict(ConflictBehavior::Fail),
        )
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("412"), "{err:#}");
    assert!(mock.puts.lock().await.is_empty(), "fail must not upload anything");
    assert_eq!(mock.files.lock().await[0].content, b"KEEP");

    // On a free name `fail` uploads normally.
    let created = graph
        .upload_bytes_with_options(
            "drv",
            None,
            "fresh.txt",
            Vec::new(),
            None,
            &with_conflict(ConflictBehavior::Fail),
        )
        .await
        .unwrap();
    assert_eq!(created.name, "fresh.txt");
    server.abort();
}

#[tokio::test]
async fn zero_byte_replace_url_unchanged() {
    let mock = Mock {
        files: Arc::new(Mutex::new(vec![file("item-orig", "root", "notes.txt", b"OLD")])),
        ..Default::default()
    };
    let (graph, server) = start(mock.clone()).await;

    for options in [with_conflict(ConflictBehavior::Replace), UploadOptions::default()] {
        let replaced = graph
            .upload_bytes_with_options("drv", None, "notes.txt", Vec::new(), None, &options)
            .await
            .unwrap();
        assert_eq!(replaced.id, "item-orig", "replace keeps targeting the existing file");
    }
    let puts = mock.puts.lock().await.clone();
    assert_eq!(
        puts,
        vec!["/v1.0/drives/drv/root:/notes.txt:/content".to_string(); 2],
        "replace must send the plain URL, with no query string"
    );
    assert!(mock.files.lock().await[0].content.is_empty());
    assert_eq!(mock.files.lock().await.len(), 1);
    server.abort();
}
