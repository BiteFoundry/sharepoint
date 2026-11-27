//! Regression tests for file downloads (SP-15): no whole-transfer timeout, fallback from
//! a failing pre-authenticated URL, and bounded retries before the first body byte.
//!
//! The production deadlines are 60 s each; the tests scale them down with
//! `GraphClient::with_download_timeouts`.

use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use axum::{
    body::{Body, Bytes},
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
    routing::get,
    Router,
};
use tempfile::tempdir;
use tokio::{net::TcpListener, sync::Mutex};

use sharepoint_cli::{
    auth::{AuthManager, StoredTokens},
    config::AppConfig,
    graph::GraphClient,
};

const SLOW_CHUNKS: usize = 10;
const SLOW_CHUNK_LEN: usize = 1024;
const SLOW_CHUNK_GAP: Duration = Duration::from_millis(100);

#[derive(Clone)]
struct Mock {
    origin: String,
    /// Paths requested on the mock, in order.
    requests: Arc<Mutex<Vec<String>>>,
    /// `Authorization` header of each `/blob` request (the redirect hop).
    blob_auth: Arc<Mutex<Vec<Option<String>>>>,
    throttle_hits: Arc<AtomicUsize>,
}

fn slow_chunk(i: usize) -> Bytes {
    Bytes::from(vec![b'a' + i as u8; SLOW_CHUNK_LEN])
}

fn slow_payload() -> Vec<u8> {
    (0..SLOW_CHUNKS).flat_map(|i| slow_chunk(i).to_vec()).collect()
}

/// A body whose first chunk is sent at once and every later one after `gap`.
fn drip_body(chunks: usize, gap: Duration) -> Body {
    Body::from_stream(futures_util::stream::unfold(0usize, move |i| async move {
        if i >= chunks {
            return None;
        }
        if i > 0 {
            tokio::time::sleep(gap).await;
        }
        Some((Ok::<Bytes, std::io::Error>(slow_chunk(i)), i + 1))
    }))
}

async fn start_mock() -> (Mock, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let mock = Mock {
        origin: origin.clone(),
        requests: Arc::new(Mutex::new(Vec::new())),
        blob_auth: Arc::new(Mutex::new(Vec::new())),
        throttle_hits: Arc::new(AtomicUsize::new(0)),
    };
    let app = Router::new()
        // Graph `/content`: requires the Bearer token and redirects to the blob store.
        .route(
            "/v1.0/drives/{drive_id}/items/{item_id}/content",
            get(
                |State(m): State<Mock>, Path((_drive, item)): Path<(String, String)>, headers: HeaderMap| async move {
                    m.requests.lock().await.push(format!("content/{item}"));
                    if headers.get(header::AUTHORIZATION).is_none() {
                        return (StatusCode::UNAUTHORIZED, "missing bearer").into_response();
                    }
                    if item == "direct" {
                        // Some servers answer /content with the body instead of a redirect.
                        return drip_body(SLOW_CHUNKS, SLOW_CHUNK_GAP).into_response();
                    }
                    if item == "missing" {
                        return (StatusCode::NOT_FOUND, "itemNotFound").into_response();
                    }
                    (
                        StatusCode::FOUND,
                        [(header::LOCATION, format!("{}/blob/{item}", m.origin))],
                    )
                        .into_response()
                },
            ),
        )
        .route(
            "/blob/{item_id}",
            get(
                |State(m): State<Mock>, Path(item): Path<String>, headers: HeaderMap| async move {
                    m.requests.lock().await.push(format!("blob/{item}"));
                    m.blob_auth.lock().await.push(
                        headers
                            .get(header::AUTHORIZATION)
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string),
                    );
                    match item.as_str() {
                        "slow" => drip_body(SLOW_CHUNKS, SLOW_CHUNK_GAP).into_response(),
                        // One chunk, then silence far longer than the read timeout.
                        "stall" => drip_body(2, Duration::from_secs(30)).into_response(),
                        // Never answers within the header deadline.
                        "mute" => {
                            tokio::time::sleep(Duration::from_secs(30)).await;
                            "too late".into_response()
                        }
                        "throttle" => {
                            if m.throttle_hits.fetch_add(1, Ordering::SeqCst) == 0 {
                                (
                                    StatusCode::TOO_MANY_REQUESTS,
                                    [(header::RETRY_AFTER, "0")],
                                    "slow down",
                                )
                                    .into_response()
                            } else {
                                "after-throttle".into_response()
                            }
                        }
                        _ => "plain-body".into_response(),
                    }
                },
            ),
        )
        // Pre-authenticated download URLs.
        .route(
            "/preauth/{kind}",
            get(|State(m): State<Mock>, Path(kind): Path<String>| async move {
                m.requests.lock().await.push(format!("preauth/{kind}"));
                match kind.as_str() {
                    "expired" => (StatusCode::FORBIDDEN, "expired").into_response(),
                    _ => "preauth-body".into_response(),
                }
            }),
        )
        .with_state(mock.clone());
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (mock, server)
}

fn graph_for(mock: &Mock, header: Duration, read: Duration) -> GraphClient {
    let cfg = AppConfig {
        graph_base_url: format!("{}/v1.0", mock.origin),
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
    GraphClient::new(cfg, auth)
        .unwrap()
        .with_my_user_id("user-1")
        .with_download_timeouts(header, read)
        .unwrap()
}

fn dir_entries(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir).map(|d| d.count()).unwrap_or(0)
}

/// A port on which nothing listens.
async fn closed_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

#[tokio::test]
async fn slow_streamed_download_outlives_old_total_timeout() {
    let (mock, server) = start_mock().await;
    // Each phase may take 300 ms; the whole body takes about 900 ms. A whole-request
    // timeout of that size (the old 60 s cap, scaled down) cuts this transfer off.
    let limit = Duration::from_millis(300);
    let graph = graph_for(&mock, limit, limit);
    let dest = tempdir().unwrap();

    for item in ["slow", "direct"] {
        let started = Instant::now();
        let file = graph
            .download_item_to_file("drv", item, Some("big.bin"), None, None, dest.path())
            .await
            .unwrap_or_else(|err| panic!("{item}: slow download must succeed: {err:#}"));
        assert!(
            started.elapsed() > limit * 2,
            "{item}: the transfer must outlast the per-phase limit ({:?})",
            started.elapsed()
        );
        assert_eq!(file.size_bytes as usize, SLOW_CHUNKS * SLOW_CHUNK_LEN);
        assert_eq!(std::fs::read(&file.path).unwrap(), slow_payload(), "{item}");
    }

    // The in-memory path (previews, `cat`) streams through the same clients.
    let bytes = graph
        .download_item_bytes("drv", "slow", None, 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(bytes, slow_payload());
    server.abort();
}

#[tokio::test]
async fn stalled_stream_fails_and_removes_partial_file() {
    let (mock, server) = start_mock().await;
    let graph = graph_for(&mock, Duration::from_millis(300), Duration::from_millis(300));
    let dest = tempdir().unwrap();

    let started = Instant::now();
    let err = graph
        .download_item_to_file("drv", "stall", Some("big.bin"), None, None, dest.path())
        .await
        .expect_err("a body that goes silent must fail");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "must fail at the read timeout, not hang ({:?})",
        started.elapsed()
    );
    assert!(format!("{err:#}").contains("download"), "{err:#}");
    assert_eq!(dir_entries(dest.path()), 0, "the partial file must be removed");
    // A stalled body is not retried: the stream had already started.
    let reqs = mock.requests.lock().await.clone();
    assert_eq!(reqs.iter().filter(|r| *r == "blob/stall").count(), 1, "{reqs:?}");
    server.abort();
}

#[tokio::test]
async fn unresponsive_hop_fails_at_header_deadline() {
    let (mock, server) = start_mock().await;
    let graph = graph_for(&mock, Duration::from_millis(250), Duration::from_secs(5));
    let dest = tempdir().unwrap();

    let started = Instant::now();
    let err = graph
        .download_item_to_file("drv", "mute", Some("big.bin"), None, None, dest.path())
        .await
        .expect_err("a hop that never answers must fail");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "must fail at the header deadline ({:?})",
        started.elapsed()
    );
    assert!(format!("{err:#}").contains("no response within"), "{err:#}");
    assert_eq!(dir_entries(dest.path()), 0);
    // One retry before giving up, no more.
    let reqs = mock.requests.lock().await.clone();
    assert_eq!(reqs.iter().filter(|r| *r == "blob/mute").count(), 2, "{reqs:?}");
    server.abort();
}

#[tokio::test]
async fn preauth_transport_error_falls_back_to_content() {
    let (mock, server) = start_mock().await;
    let graph = graph_for(&mock, Duration::from_secs(5), Duration::from_secs(5));
    let dest = tempdir().unwrap();
    let dead_url = format!("http://127.0.0.1:{}/download?tempauth=x", closed_port().await);

    let file = graph
        .download_item_to_file("drv", "plain", Some("doc.txt"), None, Some(&dead_url), dest.path())
        .await
        .expect("an unreachable pre-authenticated URL must fall back to /content");
    assert_eq!(std::fs::read(&file.path).unwrap(), b"plain-body");
    let reqs = mock.requests.lock().await.clone();
    assert_eq!(reqs, vec!["content/plain".to_string(), "blob/plain".to_string()]);

    // When /content fails as well, the error names both failures and keeps its status.
    let err = graph
        .download_item_to_file("drv", "missing", Some("doc.txt"), None, Some(&dead_url), dest.path())
        .await
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("pre-authenticated download URL had failed first"), "{msg}");
    assert!(msg.contains("404"), "{msg}");
    server.abort();
}

#[tokio::test]
async fn preauth_expired_status_still_falls_back() {
    let (mock, server) = start_mock().await;
    let graph = graph_for(&mock, Duration::from_secs(5), Duration::from_secs(5));
    let dest = tempdir().unwrap();

    // A working pre-authenticated URL is used directly: /content is never asked.
    let ok_url = format!("{}/preauth/ok", mock.origin);
    let file = graph
        .download_item_to_file("drv", "plain", Some("a.txt"), None, Some(&ok_url), dest.path())
        .await
        .unwrap();
    assert_eq!(std::fs::read(&file.path).unwrap(), b"preauth-body");
    assert_eq!(*mock.requests.lock().await, vec!["preauth/ok".to_string()]);
    mock.requests.lock().await.clear();

    let expired_url = format!("{}/preauth/expired", mock.origin);
    let file = graph
        .download_item_to_file("drv", "plain", Some("b.txt"), None, Some(&expired_url), dest.path())
        .await
        .unwrap();
    assert_eq!(std::fs::read(&file.path).unwrap(), b"plain-body");
    assert_eq!(
        *mock.requests.lock().await,
        vec![
            "preauth/expired".to_string(),
            "content/plain".to_string(),
            "blob/plain".to_string()
        ]
    );
    server.abort();
}

#[tokio::test]
async fn redirect_hop_retries_on_429_with_retry_after() {
    let (mock, server) = start_mock().await;
    let graph = graph_for(&mock, Duration::from_secs(5), Duration::from_secs(5));
    let dest = tempdir().unwrap();

    let file = graph
        .download_item_to_file("drv", "throttle", Some("t.txt"), None, None, dest.path())
        .await
        .expect("a throttled redirect hop must be retried");
    assert_eq!(std::fs::read(&file.path).unwrap(), b"after-throttle");
    assert_eq!(mock.throttle_hits.load(Ordering::SeqCst), 2, "exactly one retry");
    assert_eq!(dir_entries(dest.path()), 1);
    server.abort();
}

#[tokio::test]
async fn redirect_hop_never_sends_authorization() {
    let (mock, server) = start_mock().await;
    let graph = graph_for(&mock, Duration::from_secs(5), Duration::from_secs(5));
    let dest = tempdir().unwrap();

    for item in ["plain", "throttle"] {
        graph
            .download_item_to_file("drv", item, Some("x.txt"), None, None, dest.path())
            .await
            .unwrap();
    }
    let seen = mock.blob_auth.lock().await.clone();
    assert_eq!(seen.len(), 3, "plain once, throttle twice");
    assert!(
        seen.iter().all(Option::is_none),
        "the redirect hop must never carry the Bearer token: {seen:?}"
    );
    server.abort();
}
