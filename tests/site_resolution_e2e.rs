//! Regression tests for `resolver::resolve_site` (SP-01): the resolver searches Graph for
//! the query itself instead of matching only against the first page of `search=*`.

use std::{collections::HashMap, sync::Arc};

use axum::{
    extract::{Path, Query, State},
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
    graph::GraphClient,
    resolver,
    storage::Storage,
};

#[derive(Clone, Default)]
struct SiteMock {
    /// Sites returned for `search=*`.
    wildcard: Arc<Vec<Value>>,
    /// Sites returned only when the search text equals the key (case-insensitive).
    by_query: Arc<HashMap<String, Vec<Value>>>,
    /// Status the query search answers with instead of results (the wildcard is unaffected).
    query_search_status: Option<u16>,
    /// Status of `GET /sites/{id}`.
    direct_status: u16,
    requests: Arc<Mutex<Vec<String>>>,
}

fn site(id: &str, name: &str, display: &str) -> Value {
    json!({
        "id": id,
        "name": name,
        "displayName": display,
        "webUrl": format!("https://contoso.sharepoint.com/sites/{name}")
    })
}

fn graph_error(status: u16, code: &str, message: &str) -> axum::response::Response {
    (
        StatusCode::from_u16(status).unwrap(),
        Json(json!({ "error": { "code": code, "message": message } })),
    )
        .into_response()
}

async fn start(mock: SiteMock) -> (GraphClient, Storage, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1.0", listener.local_addr().unwrap());
    let app = Router::new()
        .route(
            "/v1.0/me/followedSites",
            get(|State(m): State<SiteMock>| async move {
                m.requests.lock().await.push("followedSites".to_string());
                Json(json!({ "value": [] }))
            }),
        )
        .route(
            "/v1.0/sites",
            get(
                |State(m): State<SiteMock>, Query(q): Query<HashMap<String, String>>| async move {
                    let search = q.get("search").cloned().unwrap_or_else(|| "*".to_string());
                    m.requests.lock().await.push(format!("search={search}"));
                    if search == "*" {
                        return Json(json!({ "value": *m.wildcard })).into_response();
                    }
                    if let Some(status) = m.query_search_status {
                        return graph_error(status, "accessDenied", "search refused by the mock");
                    }
                    let hits = m
                        .by_query
                        .get(&search.to_lowercase())
                        .cloned()
                        .unwrap_or_default();
                    Json(json!({ "value": hits })).into_response()
                },
            ),
        )
        .route(
            "/v1.0/sites/{id}",
            get(|State(m): State<SiteMock>, Path(id): Path<String>| async move {
                m.requests.lock().await.push(format!("direct={id}"));
                graph_error(
                    m.direct_status,
                    "invalidRequest",
                    "RAW-GRAPH-BODY: provided identifier is malformed",
                )
            }),
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
    (graph, Storage::open_in_memory().unwrap(), server)
}

fn filler_sites(n: usize) -> Vec<Value> {
    (0..n)
        .map(|i| site(&format!("site-filler-{i}"), &format!("filler{i}"), &format!("Filler {i}")))
        .collect()
}

#[tokio::test]
async fn resolve_site_searches_for_query_beyond_first_100() {
    // The tenant has more sites than one wildcard page: the target is only reachable by
    // searching for it.
    let mock = SiteMock {
        wildcard: Arc::new(filler_sites(100)),
        by_query: Arc::new(HashMap::from([(
            "zeta finance".to_string(),
            vec![site("site-zeta", "zetafinance", "Zeta Finance")],
        )])),
        direct_status: 400,
        ..Default::default()
    };
    let requests = mock.requests.clone();
    let (graph, storage, server) = start(mock).await;

    let found = resolver::resolve_site(&graph, &storage, "Zeta Finance", false)
        .await
        .expect("a site outside the first wildcard page must resolve by searching for it");
    assert_eq!(found.id, "site-zeta");

    let reqs = requests.lock().await.clone();
    assert!(
        reqs.iter().any(|r| r == "search=Zeta Finance"),
        "the query itself must be searched: {reqs:?}"
    );
    assert!(
        !reqs.iter().any(|r| r.starts_with("direct=")),
        "a resolved search must not fall through to /sites/{{name}}: {reqs:?}"
    );
    assert!(
        storage.list_sites().unwrap().iter().any(|s| s.id == "site-zeta"),
        "search results must populate the site cache"
    );
    server.abort();
}

#[tokio::test]
async fn resolve_site_substring_still_resolves_via_wildcard_fallback() {
    // Graph search is keyword-based and finds nothing for a mid-word fragment; the local
    // substring match over the wildcard listing must keep resolving it.
    let mock = SiteMock {
        wildcard: Arc::new(vec![
            site("site-eng", "engineering", "Engineering Hub"),
            site("site-mkt", "marketing", "Marketing Portal"),
        ]),
        direct_status: 400,
        ..Default::default()
    };
    let requests = mock.requests.clone();
    let (graph, storage, server) = start(mock).await;

    let found = resolver::resolve_site(&graph, &storage, "ineer", false)
        .await
        .unwrap();
    assert_eq!(found.id, "site-eng");
    let reqs = requests.lock().await.clone();
    let query_pos = reqs.iter().position(|r| r == "search=ineer").unwrap();
    let wildcard_pos = reqs.iter().position(|r| r == "search=*").unwrap();
    assert!(query_pos < wildcard_pos, "query search comes first: {reqs:?}");
    server.abort();
}

#[tokio::test]
async fn resolve_site_not_found_is_actionable() {
    for direct_status in [400u16, 404] {
        let mock = SiteMock {
            wildcard: Arc::new(vec![site("site-eng", "engineering", "Engineering Hub")]),
            direct_status,
            ..Default::default()
        };
        let (graph, storage, server) = start(mock).await;
        let err = resolver::resolve_site(&graph, &storage, "No Such Site", false)
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("SharePoint site not found: No Such Site"), "{msg}");
        assert!(msg.contains("sharepoint sites"), "{msg}");
        assert!(!msg.contains("RAW-GRAPH-BODY"), "raw Graph body leaked: {msg}");
        server.abort();
    }
}

#[tokio::test]
async fn resolve_site_propagates_search_auth_error() {
    // The search is refused (403): that is the answer, not "site not found".
    let mock = SiteMock {
        query_search_status: Some(403),
        direct_status: 400,
        ..Default::default()
    };
    let (graph, storage, server) = start(mock).await;
    let err = resolver::resolve_site(&graph, &storage, "Zeta Finance", false)
        .await
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("403"), "{msg}");
    assert!(!msg.contains("site not found"), "{msg}");
    server.abort();

    // A direct lookup that is itself refused or failing propagates unchanged.
    for direct_status in [403u16, 500] {
        let mock = SiteMock {
            direct_status,
            ..Default::default()
        };
        let (graph, storage, server) = start(mock).await;
        let err = resolver::resolve_site(&graph, &storage, "Zeta Finance", false)
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains(&direct_status.to_string()), "{msg}");
        assert!(!msg.contains("site not found"), "{msg}");
        server.abort();
    }
}

#[tokio::test]
async fn resolve_site_keeps_cache_and_ambiguity_behaviour() {
    // Two sites share a prefix: still an ambiguity error, not a silent pick or a lookup.
    let mock = SiteMock {
        by_query: Arc::new(HashMap::from([(
            "fin".to_string(),
            vec![
                site("site-fin-a", "finance", "Finance"),
                site("site-fin-b", "finops", "FinOps"),
            ],
        )])),
        direct_status: 400,
        ..Default::default()
    };
    let requests = mock.requests.clone();
    let (graph, storage, server) = start(mock).await;
    let err = resolver::resolve_site(&graph, &storage, "fin", false)
        .await
        .unwrap_err();
    assert!(err.downcast_ref::<resolver::AmbiguousMatch>().is_some(), "{err:#}");

    // Both candidates are cached now: an exact name resolves without any request.
    let before = requests.lock().await.len();
    let cached = resolver::resolve_site(&graph, &storage, "FinOps", false)
        .await
        .unwrap();
    assert_eq!(cached.id, "site-fin-b");
    assert_eq!(requests.lock().await.len(), before);

    // prefer_cache (offline) never reaches Graph, even on a miss.
    assert!(resolver::resolve_site(&graph, &storage, "nowhere", true)
        .await
        .is_err());
    assert_eq!(requests.lock().await.len(), before);
    server.abort();
}
