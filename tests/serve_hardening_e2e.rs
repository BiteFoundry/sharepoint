//! HTTP-level regression tests for the `sharepoint serve` hardening (SP-04):
//! exact CORS origins, the `Host` allow-list, and the local-filesystem gate.

use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::State,
    http::{Method, StatusCode, Uri},
    response::IntoResponse,
    Json, Router,
};
use serde_json::{json, Value};
use tempfile::tempdir;
use tokio::{net::TcpListener, sync::Mutex};

use sharepoint_cli::{
    auth::{AuthManager, StoredTokens},
    config::AppConfig,
    graph::GraphClient,
    serve::{self, RouterPolicy},
    storage::Storage,
};

type Recorded = Arc<Mutex<Vec<(String, String)>>>;

/// Minimal mock Graph: one drive, one file, simple uploads. Records every request.
async fn mock_graph_handler(
    State(recorded): State<Recorded>,
    method: Method,
    uri: Uri,
    body: Bytes,
) -> axum::response::Response {
    let path = uri.path().to_string();
    recorded.lock().await.push((method.to_string(), path.clone()));
    let item = |id: &str, name: &str, size: usize| {
        json!({
            "id": id,
            "name": name,
            "size": size,
            "eTag": "\"etag-1\"",
            "cTag": "\"ctag-1\"",
            "file": { "mimeType": "text/plain" },
            "parentReference": { "driveId": "drv-me", "id": "root", "path": "/drive/root:" }
        })
    };
    match (method.as_str(), path.as_str()) {
        ("GET", "/v1.0/me/drive") => Json(json!({
            "id": "drv-me",
            "name": "OneDrive",
            "driveType": "business",
            "webUrl": "https://contoso-my.sharepoint.com/personal/jane"
        }))
        .into_response(),
        ("GET", "/v1.0/drives/drv-me/items/item-doc") => {
            Json(item("item-doc", "doc.txt", 5)).into_response()
        }
        ("GET", "/v1.0/drives/drv-me/items/item-doc/content") => "hello".into_response(),
        ("PUT", p) if p.ends_with(":/content") => {
            (StatusCode::CREATED, Json(item("item-new", "new.txt", body.len()))).into_response()
        }
        _ => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

struct Harness {
    api: String,
    recorded: Recorded,
    download_dir: tempfile::TempDir,
    _servers: Vec<tokio::task::JoinHandle<()>>,
}

impl Harness {
    async fn uploads(&self) -> usize {
        self.recorded
            .lock()
            .await
            .iter()
            .filter(|(m, _)| m == "PUT")
            .count()
    }
}

async fn start(policy: RouterPolicy) -> Harness {
    let recorded: Recorded = Arc::new(Mutex::new(Vec::new()));
    let graph_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1.0", graph_listener.local_addr().unwrap());
    let graph_app = Router::new()
        .fallback(mock_graph_handler)
        .with_state(recorded.clone());
    let graph_server = tokio::spawn(async move {
        let _ = axum::serve(graph_listener, graph_app).await;
    });

    let download_dir = tempdir().unwrap();
    let cfg = AppConfig {
        graph_base_url: base_url,
        download_dir: download_dir.path().to_string_lossy().into_owned(),
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
    let graph = GraphClient::new(cfg.clone(), auth)
        .unwrap()
        .with_my_user_id("user-1");
    let storage = Storage::open_in_memory().unwrap();

    let router = serve::build_router_with(cfg, graph, storage, policy);
    let api_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}", api_listener.local_addr().unwrap());
    let api_server = tokio::spawn(async move {
        let _ = axum::serve(api_listener, router).await;
    });
    Harness {
        api,
        recorded,
        download_dir,
        _servers: vec![graph_server, api_server],
    }
}

fn dir_entries(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir).map(|d| d.count()).unwrap_or(0)
}

async fn preflight(api: &str, origin: &str) -> reqwest::Response {
    reqwest::Client::new()
        .request(reqwest::Method::OPTIONS, format!("{api}/v1/items"))
        .header("Origin", origin)
        .header("Access-Control-Request-Method", "DELETE")
        .header("Access-Control-Request-Headers", "authorization")
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn cors_preflight_rejects_localhost_evil_com() {
    let h = start(RouterPolicy::from_api_key(Some("k".to_string()))).await;

    for hostile in [
        "http://localhost.evil.com",
        "http://127.0.0.1.evil.com",
        "http://localhost:1@evil.com",
        "https://app.example.com",
    ] {
        let resp = preflight(&h.api, hostile).await;
        assert!(
            resp.headers().get("access-control-allow-origin").is_none(),
            "preflight from {hostile} must not be granted CORS access"
        );
    }

    for local in ["http://localhost:5173", "http://127.0.0.1:3000", "http://[::1]:8080"] {
        let resp = preflight(&h.api, local).await;
        assert_eq!(
            resp.headers()
                .get("access-control-allow-origin")
                .and_then(|v| v.to_str().ok()),
            Some(local),
            "preflight from {local} must be echoed"
        );
    }

    // Plain cross-origin GET from the look-alike origin: no CORS grant either.
    let get = reqwest::Client::new()
        .get(format!("{}/health", h.api))
        .header("Origin", "http://localhost.evil.com")
        .send()
        .await
        .unwrap();
    assert!(get.headers().get("access-control-allow-origin").is_none());
}

#[tokio::test]
async fn cors_origin_opt_in_is_exact() {
    let h = start(RouterPolicy {
        cors_origins: vec!["https://app.example.com".to_string()],
        ..RouterPolicy::from_api_key(Some("k".to_string()))
    })
    .await;

    let ok = preflight(&h.api, "https://app.example.com").await;
    assert_eq!(
        ok.headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("https://app.example.com")
    );
    for hostile in [
        "https://app.example.com.evil.com",
        "http://app.example.com",
        "https://app.example.com:8443",
    ] {
        let resp = preflight(&h.api, hostile).await;
        assert!(
            resp.headers().get("access-control-allow-origin").is_none(),
            "{hostile} must not match the configured origin"
        );
    }
    // Loopback origins keep working next to the opt-in origin.
    let local = preflight(&h.api, "http://localhost:5173").await;
    assert!(local.headers().get("access-control-allow-origin").is_some());
}

#[tokio::test]
async fn host_header_allowlist_blocks_rebinding() {
    let h = start(RouterPolicy::default()).await;
    let http = reqwest::Client::new();
    let port = h.api.rsplit(':').next().unwrap().to_string();

    // A rebinding request reaches the socket but carries the attacker's host name.
    for route in ["/health", "/openapi.yaml", "/v1/whoami"] {
        let resp = http
            .get(format!("{}{route}", h.api))
            .header("Host", format!("attacker.example:{port}"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{route}");
        let body: Value = resp.json().await.unwrap();
        let msg = body["error"].as_str().unwrap();
        assert!(msg.contains("attacker.example"), "{msg}");
        assert!(msg.contains("--allowed-host"), "{msg}");
    }
    let lookalike = http
        .get(format!("{}/health", h.api))
        .header("Host", format!("localhost.attacker.example:{port}"))
        .send()
        .await
        .unwrap();
    assert_eq!(lookalike.status(), StatusCode::FORBIDDEN);

    for host in [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("LOCALHOST:{port}"),
        format!("[::1]:{port}"),
    ] {
        let resp = http
            .get(format!("{}/health", h.api))
            .header("Host", &host)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "Host: {host}");
    }

    // The same foreign name passes once the operator allowed it.
    let allowed = start(RouterPolicy {
        allowed_hosts: vec!["attacker.example".to_string()],
        ..Default::default()
    })
    .await;
    let port = allowed.api.rsplit(':').next().unwrap().to_string();
    let resp = http
        .get(format!("{}/health", allowed.api))
        .header("Host", format!("attacker.example:{port}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let other = http
        .get(format!("{}/health", allowed.api))
        .header("Host", format!("other.example:{port}"))
        .send()
        .await
        .unwrap();
    assert_eq!(other.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn keyless_server_rejects_local_path_and_dir() {
    let h = start(RouterPolicy::default()).await;
    let http = reqwest::Client::new();
    let scratch = tempdir().unwrap();
    let local_file = scratch.path().join("secret.txt");
    std::fs::write(&local_file, b"LOCAL SECRET").unwrap();
    let out_dir = scratch.path().join("out");
    std::fs::create_dir_all(&out_dir).unwrap();

    let upload = http
        .post(format!("{}/v1/items/upload", h.api))
        .json(&json!({ "drive": "me", "local_path": local_file.to_string_lossy() }))
        .send()
        .await
        .unwrap();
    assert_eq!(upload.status(), StatusCode::FORBIDDEN);
    let body: Value = upload.json().await.unwrap();
    assert!(body["error"].as_str().unwrap().contains("--allow-local-paths"));
    assert_eq!(h.uploads().await, 0, "a refused local_path must not reach Graph");
    assert!(
        h.recorded.lock().await.is_empty(),
        "a refused local_path must not cause any Graph request"
    );

    for key in ["dir", "out_dir"] {
        let download = http
            .post(format!("{}/v1/download", h.api))
            .json(&json!({ "drive": "me", "item_id": "item-doc", key: out_dir.to_string_lossy() }))
            .send()
            .await
            .unwrap();
        assert_eq!(download.status(), StatusCode::FORBIDDEN, "{key}");
        let body: Value = download.json().await.unwrap();
        assert!(body["error"].as_str().unwrap().contains("--allow-local-paths"));
    }
    let by_id = http
        .post(format!("{}/v1/items/item-doc/download", h.api))
        .json(&json!({ "drive": "me", "dir": out_dir.to_string_lossy() }))
        .send()
        .await
        .unwrap();
    assert_eq!(by_id.status(), StatusCode::FORBIDDEN);
    assert_eq!(dir_entries(&out_dir), 0, "nothing may be written to the refused dir");
    assert_eq!(dir_entries(h.download_dir.path()), 0);
    assert!(h.recorded.lock().await.is_empty());

    // Inline content and the configured download directory keep working without a key.
    let inline = http
        .post(format!("{}/v1/items/upload", h.api))
        .json(&json!({ "drive": "me", "file_name": "new.txt", "content_base64": "aGVsbG8=" }))
        .send()
        .await
        .unwrap();
    assert_eq!(inline.status(), StatusCode::OK);
    assert_eq!(h.uploads().await, 1);

    let text = http
        .post(format!("{}/v1/items/upload", h.api))
        .json(&json!({ "drive": "me", "file_name": "new.txt", "content_text": "hello" }))
        .send()
        .await
        .unwrap();
    assert_eq!(text.status(), StatusCode::OK);

    let download = http
        .post(format!("{}/v1/download", h.api))
        .json(&json!({ "drive": "me", "item_id": "item-doc" }))
        .send()
        .await
        .unwrap();
    assert_eq!(download.status(), StatusCode::OK);
    let body: Value = download.json().await.unwrap();
    let written = std::path::PathBuf::from(body["path"].as_str().unwrap());
    assert!(written.starts_with(h.download_dir.path()));
    assert_eq!(std::fs::read(&written).unwrap(), b"hello");
    assert_eq!(dir_entries(&out_dir), 0);
}

#[tokio::test]
async fn keyed_or_flagged_server_allows_local_paths() {
    let keyed = start(RouterPolicy::from_api_key(Some("secret".to_string()))).await;
    let flagged = start(RouterPolicy {
        allow_local_paths: true,
        ..Default::default()
    })
    .await;
    let http = reqwest::Client::new();

    for (h, key) in [(&keyed, Some("secret")), (&flagged, None)] {
        let scratch = tempdir().unwrap();
        let local_file = scratch.path().join("new.txt");
        std::fs::write(&local_file, b"payload").unwrap();
        let out_dir = scratch.path().join("out");

        let mut upload = http
            .post(format!("{}/v1/items/upload", h.api))
            .json(&json!({ "drive": "me", "local_path": local_file.to_string_lossy() }));
        let mut download = http
            .post(format!("{}/v1/download", h.api))
            .json(&json!({ "drive": "me", "item_id": "item-doc", "dir": out_dir.to_string_lossy() }));
        if let Some(key) = key {
            upload = upload.bearer_auth(key);
            download = download.bearer_auth(key);
        }
        let upload = upload.send().await.unwrap();
        assert_eq!(upload.status(), StatusCode::OK, "local_path upload (key: {key:?})");
        assert_eq!(h.uploads().await, 1);

        let download = download.send().await.unwrap();
        assert_eq!(download.status(), StatusCode::OK, "dir download (key: {key:?})");
        assert_eq!(std::fs::read(out_dir.join("doc.txt")).unwrap(), b"hello");
    }

    // The key still guards the keyed server: no key, no local path access.
    let scratch = tempdir().unwrap();
    let unauth = http
        .post(format!("{}/v1/download", keyed.api))
        .json(&json!({ "drive": "me", "item_id": "item-doc", "dir": scratch.path().to_string_lossy() }))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(dir_entries(scratch.path()), 0);
}
