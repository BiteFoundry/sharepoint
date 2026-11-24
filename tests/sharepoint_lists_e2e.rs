use std::{
    collections::HashMap,
    path::Path,
    process::{Output, Stdio},
    sync::Arc,
};

use axum::{
    extract::{Path as RoutePath, Query, State},
    http::{header, HeaderMap, StatusCode, Uri},
    response::IntoResponse,
    routing::{get, patch},
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde_json::{json, Value};
use tokio::{io::AsyncWriteExt, net::TcpListener, process::Command, sync::Mutex};

const SITE: &str = "contoso.sharepoint.com,11111111-1111-1111-1111-111111111111,22222222-2222-2222-2222-222222222222";
const LIST: &str = "33333333-3333-3333-3333-333333333333";
const ITEM: &str = "17";

#[derive(Clone)]
struct Fixture {
    base: String,
    state: Arc<Mutex<ListState>>,
}

struct ListState {
    revision: u32,
    fields: Value,
    reads: Vec<(String, Option<String>, Option<String>, Option<String>)>,
    writes: Vec<(String, Option<String>, Value)>,
}

impl Fixture {
    async fn lists(
        State(fixture): State<Self>,
        RoutePath(site): RoutePath<String>,
        Query(query): Query<HashMap<String, String>>,
    ) -> Json<Value> {
        assert_eq!(site.as_str(), SITE);
        let selection = query.get("$select");
        assert_eq!(
            selection.map(String::as_str),
            Some("id,name,displayName,webUrl,list")
        );
        fixture.state.lock().await.reads.push((
            "lists".into(),
            selection.cloned(),
            None,
            None,
        ));
        Json(json!({ "value": [
            {
                "id": LIST,
                "name": "ReviewTracker",
                "displayName": "Review Tracker",
                "webUrl": "https://contoso.sharepoint.com/sites/eng/Lists/ReviewTracker",
                "list": { "template": "genericList" }
            },
            {
                "id": "list-docs-0002",
                "name": "Shared Documents",
                "displayName": "Documents",
                "webUrl": "https://contoso.sharepoint.com/sites/eng/Shared%20Documents",
                "list": { "template": "documentLibrary" }
            }
        ] }))
    }

    async fn items(
        State(fixture): State<Self>,
        RoutePath((site, list)): RoutePath<(String, String)>,
        Query(query): Query<HashMap<String, String>>,
    ) -> Json<Value> {
        assert_eq!((site.as_str(), list.as_str()), (SITE, LIST));
        let mut state = fixture.state.lock().await;
        let expand = query.get("expand").expect("expanded list fields");
        let top = query.get("$top");
        let token = query.get("$skiptoken");
        state.reads.push((
            "items".into(),
            Some(expand.clone()),
            top.cloned(),
            token.cloned(),
        ));
        if let Some(token) = token {
            assert_eq!(token, "opaque+step/2");
            return Json(json!({ "value": [{
                "id": "28", "@odata.etag": "\"row-28\"", "fields": { "Title": "Second page", "Quantity": 4 }
            }] }));
        }
        let top_param = top
            .map(|value| format!("&%24top={}", urlencoding::encode(value)))
            .unwrap_or_default();
        let next = format!(
            "{}/v1.0/sites/{SITE}/lists/{LIST}/items?expand={}{top_param}&%24skiptoken=opaque%2Bstep%2F2",
            fixture.base,
            urlencoding::encode(expand),
        );
        Json(json!({ "value": [{
            "id": ITEM, "eTag": etag(state.revision), "@odata.etag": etag(state.revision),
            "fields": state.fields,
        }], "@odata.nextLink": next }))
    }

    async fn columns(
        State(fixture): State<Self>,
        RoutePath((site, list)): RoutePath<(String, String)>,
        Query(query): Query<HashMap<String, String>>,
    ) -> Json<Value> {
        assert_eq!((site.as_str(), list.as_str()), (SITE, LIST));
        let selection = query.get("$select");
        let token = query.get("$skiptoken");
        assert!(
            selection.is_none(),
            "list_columns should not narrow $select so Graph facets/description/required/choices are returned: {selection:?}"
        );
        fixture.state.lock().await.reads.push((
            "columns".into(),
            selection.cloned(),
            None,
            token.cloned(),
        ));
        if token.is_some() {
            return Json(json!({ "value": [
                { "id": "col-2", "name": "Complete", "displayName": "Completed", "readOnly": false },
                { "id": "col-3", "name": "Quantity", "displayName": "Quantity", "readOnly": false },
                { "id": "col-4", "name": "Created", "displayName": "Created", "readOnly": true },
                { "id": "col-5", "name": "OptionalNote", "displayName": "Note", "readOnly": false }
            ] }));
        }
        Json(json!({ "value": [
            { "id": "col-1", "name": "ReviewStatus", "displayName": "Review status", "readOnly": false }
        ], "@odata.nextLink": format!(
            "{}/v1.0/sites/{SITE}/lists/{LIST}/columns?%24skiptoken=next",
            fixture.base,
        ) }))
    }

    async fn update(
        State(fixture): State<Self>,
        RoutePath((site, list, item)): RoutePath<(String, String, String)>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> impl IntoResponse {
        assert_eq!(
            (site.as_str(), list.as_str(), item.as_str()),
            (SITE, LIST, ITEM)
        );
        let tag = headers
            .get(header::IF_MATCH)
            .map(|value| value.to_str().unwrap().to_owned());
        let mut state = fixture.state.lock().await;
        state.writes.push((item, tag.clone(), body.clone()));
        assert_eq!(body.as_object().unwrap().len(), 1);
        if body["ReviewStatus"] == "Denied" {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": {
                    "code": "accessDenied", "message": "Synthetic list update denied"
                }})),
            )
                .into_response();
        }
        if tag.is_some_and(|tag| tag != etag(state.revision)) {
            return (
                StatusCode::PRECONDITION_FAILED,
                Json(json!({ "error": {
                    "code": "preconditionFailed", "message": "Synthetic stale row ETag"
                }})),
            )
                .into_response();
        }
        for (field, value) in body.as_object().unwrap() {
            assert!(state.fields.get(field).is_some());
            state.fields[field] = value.clone();
        }
        state.revision += 1;
        let mut fields = state.fields.clone();
        match state.revision {
            2 => ([(header::ETAG, etag(state.revision))], Json(fields)).into_response(),
            3 => {
                fields["@odata.etag"] = json!(etag(state.revision));
                Json(fields).into_response()
            }
            _ => Json(fields).into_response(),
        }
    }
}

fn etag(revision: u32) -> String {
    format!("\"row-17:{revision}\"")
}

struct Client<'a> {
    exe: &'a Path,
    profile: &'a Path,
    base: &'a str,
}

impl Client<'_> {
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
            .env("SHAREPOINT_ACCESS_TOKEN", "synthetic-list-token")
            .env("SHAREPOINT_DISABLE_KEYRING", "1")
            .env("SHAREPOINT_SHARE_M365_AUTH", "0")
            .env("SHAREPOINT_AUTO_DAEMON", "0")
            .env("SHAREPOINT_CONFIG_DIR", self.profile.join("config"))
            .env("SHAREPOINT_DATA_DIR", self.profile.join("data"))
            .env("NO_PROXY", "127.0.0.1,localhost");
        command
    }

    async fn cli(&self, action: &str, args: &[&str]) -> Output {
        self.cli_for(SITE, LIST, action, args).await
    }

    async fn cli_for(&self, site: &str, list: &str, action: &str, args: &[&str]) -> Output {
        self.command()
            .args([
                "lists",
                action,
                "--site-id",
                site,
                "--list-id",
                list,
                "--json",
            ])
            .args(args)
            .output()
            .await
            .unwrap()
    }

    async fn mcp(&self, name: &str, mut args: Value) -> Value {
        args["site_id"] = json!(SITE);
        args["list_id"] = json!(LIST);
        self.mcp_raw(name, args).await
    }

    async fn mcp_raw(&self, name: &str, args: Value) -> Value {
        let requests = [
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-06-18", "capabilities": {},
                "clientInfo": {"name": "list-regression", "version": "1"}
            }}),
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": name, "arguments": args } }),
        ];
        let mut child = self
            .command()
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        for request in requests {
            stdin
                .write_all(format!("{request}\n").as_bytes())
                .await
                .unwrap();
        }
        drop(stdin);
        let output = child.wait_with_output().await.unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let replies: Vec<Value> = std::str::from_utf8(&output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(replies[0]["id"], 1);
        assert_eq!(replies[1]["id"], 2);
        let result = replies[1]["result"].clone();
        if result["isError"] == false {
            let text: Value =
                serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
            assert_eq!(text, result["structuredContent"]);
        }
        result
    }
}

fn cli_json(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[tokio::test]
async fn cli_and_stdio_mcp_read_two_pages_and_update_one_typed_field() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let original = json!({ "Title": "Keep this title", "ReviewStatus": "Draft", "Complete": true, "Quantity": 5, "Created": "Keep this date", "OptionalNote": "Temporary" });
    let state = Arc::new(Mutex::new(ListState {
        revision: 1,
        fields: original.clone(),
        reads: vec![],
        writes: vec![],
    }));
    let app = Router::new()
        .route("/v1.0/sites/{site}/lists", get(Fixture::lists))
        .route("/v1.0/sites/{site}/lists/{list}/items", get(Fixture::items))
        .route(
            "/v1.0/sites/{site}/lists/{list}/columns",
            get(Fixture::columns),
        )
        .route(
            "/v1.0/sites/{site}/lists/{list}/items/{item}/fields",
            patch(Fixture::update),
        )
        .with_state(Fixture {
            base: base.clone(),
            state: state.clone(),
        });
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let profile = tempfile::tempdir().unwrap();
    let exe = std::env::var_os("SHAREPOINT_LISTS_TEST_EXE")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_sharepoint").into());
    let client = Client {
        exe: Path::new(&exe),
        profile: profile.path(),
        base: &base,
    };

    let cli_lists = cli_json(
        client
            .command()
            .args(["lists", "ls", "--site-id", SITE, "--json"])
            .output()
            .await
            .unwrap(),
    );
    let mcp_lists = client.mcp_raw("list_lists", json!({ "site_id": SITE })).await;
    assert_eq!(mcp_lists["isError"], false);
    assert_eq!(mcp_lists["structuredContent"], cli_lists);
    assert_eq!(cli_lists["returned"], 2);
    assert_eq!(cli_lists["lists"][0]["id"], LIST);
    assert_eq!(cli_lists["lists"][0]["display_name"], "Review Tracker");
    assert_eq!(cli_lists["lists"][0]["template"], "genericList");
    assert_eq!(cli_lists["lists"][1]["template"], "documentLibrary");
    let discovered_list_id = cli_lists["lists"][0]["id"].as_str().unwrap();

    let columns = cli_json(client.cli_for(SITE, discovered_list_id, "columns", &[]).await);
    assert_eq!(
        client.mcp("list_columns", json!({})).await["structuredContent"],
        columns
    );
    assert_eq!(columns["columns"][0]["name"], "ReviewStatus");
    assert_eq!(columns["columns"][0]["display_name"], "Review status");
    assert_eq!(columns["columns"][3]["read_only"], true);
    let projection_p1 = cli_json(
        client
            .cli("items", &["--field", "Title", "--field", "Quantity"])
            .await,
    );
    assert_eq!(
        client
            .mcp("list_items", json!({"fields": ["Title", "Quantity"]}))
            .await["structuredContent"],
        projection_p1
    );
    assert_eq!(projection_p1["returned"], 1);
    assert_eq!(projection_p1["has_more"], true);
    assert_eq!(
        projection_p1["items"],
        json!([
            { "id": ITEM, "e_tag": etag(1), "fields": { "Title": "Keep this title", "Quantity": 5 } }
        ])
    );
    let cursor = projection_p1["next_cursor"]
        .as_str()
        .expect("opaque next_cursor")
        .to_string();
    let projection_p2 = cli_json(
        client
            .cli(
                "items",
                &["--field", "Title", "--field", "Quantity", "--cursor", &cursor],
            )
            .await,
    );
    assert_eq!(
        client
            .mcp(
                "list_items",
                json!({"fields": ["Title", "Quantity"], "cursor": cursor}),
            )
            .await["structuredContent"],
        projection_p2
    );
    assert_eq!(
        projection_p2,
        json!({
            "items": [
                { "id": "28", "e_tag": "\"row-28\"", "fields": { "Title": "Second page", "Quantity": 4 } }
            ],
            "returned": 1,
            "has_more": false,
            "next_cursor": null
        })
    );
    let reads = state.lock().await.reads.clone();
    assert_eq!(
        reads
            .iter()
            .filter(|(kind, expand, _, _)| kind == "items"
                && expand.as_deref() == Some("fields(select=Quantity,Title)"))
            .count(),
        4
    );
    assert_eq!(
        reads
            .iter()
            .filter(|(kind, _, top, token)| kind == "items"
                && top.as_deref() == Some("50")
                && token.is_none())
            .count(),
        2
    );
    assert_eq!(
        reads
            .iter()
            .filter(|(_, _, _, token)| token.as_deref() == Some("opaque+step/2"))
            .count(),
        2
    );

    let before = cli_json(client.cli("items", &[]).await);
    assert_eq!(before["items"][0]["fields"], original);
    let before_p2 = cli_json(
        client
            .cli("items", &["--cursor", before["next_cursor"].as_str().unwrap()])
            .await,
    );
    let first = cli_json(
        client
            .cli(
                "set-field",
                &[
                    "--item-id",
                    ITEM,
                    "--field",
                    "ReviewStatus",
                    "--value-json",
                    "\"Ready\"",
                    "--if-match",
                    &etag(1),
                ],
            )
            .await,
    );
    assert_eq!(first["item_id"], ITEM);
    assert_eq!(first["e_tag"], etag(2));
    assert_eq!(first["fields"]["ReviewStatus"], "Ready");
    let second = client
        .mcp(
            "update_list_item_field",
            json!({"item_id": ITEM, "field": "Complete", "value": false, "if_match": etag(2)}),
        )
        .await;
    assert_eq!(second["structuredContent"]["e_tag"], etag(3));
    let third = cli_json(
        client
            .cli(
                "set-field",
                &[
                    "--item-id",
                    ITEM,
                    "--field",
                    "Quantity",
                    "--value-json",
                    "0",
                ],
            )
            .await,
    );
    assert_eq!(third["e_tag"], Value::Null);
    assert_eq!(third["fields"]["Quantity"], 0);

    for (field, input, value) in [
        ("Complete", "false", json!(false)),
        ("OptionalNote", "null", Value::Null),
    ] {
        let update = cli_json(
            client
                .cli(
                    "set-field",
                    &["--item-id", ITEM, "--field", field, "--value-json", input],
                )
                .await,
        );
        assert_eq!(update["fields"].get(field), Some(&value));
    }

    let unknown = client
        .cli(
            "set-field",
            &[
                "--item-id",
                ITEM,
                "--field",
                "Review status",
                "--value-json",
                "\"Wrong\"",
            ],
        )
        .await;
    assert!(!unknown.status.success());
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("Unknown internal column name"));
    let unknown = client
        .mcp(
            "update_list_item_field",
            json!({"item_id": ITEM, "field": "MissingColumn", "value": "Wrong"}),
        )
        .await;
    assert_eq!(unknown["isError"], true);
    assert!(unknown["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("Unknown internal column name"));
    let read_only = client
        .mcp(
            "update_list_item_field",
            json!({"item_id": ITEM, "field": "Created", "value": "Wrong"}),
        )
        .await;
    assert_eq!(read_only["isError"], true);
    assert!(read_only["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("read-only"));
    assert_eq!(state.lock().await.writes.len(), 5);
    let stale = client
        .mcp(
            "update_list_item_field",
            json!({"item_id": ITEM, "field": "Quantity", "value": 4, "if_match": etag(2)}),
        )
        .await;
    assert_eq!(stale["isError"], true);
    assert!(stale["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("412"));
    let denied = client
        .cli(
            "set-field",
            &[
                "--item-id",
                ITEM,
                "--field",
                "ReviewStatus",
                "--value-json",
                "\"Denied\"",
            ],
        )
        .await;
    assert!(!denied.status.success());
    let error = String::from_utf8_lossy(&denied.stderr);
    assert!(error.contains("403") && error.contains("Synthetic list update denied"));
    let denied = client
        .mcp(
            "update_list_item_field",
            json!({"item_id": ITEM, "field": "ReviewStatus", "value": "Denied"}),
        )
        .await;
    assert_eq!(denied["isError"], true);
    let error = denied["content"][0]["text"].as_str().unwrap();
    assert!(error.contains("403") && error.contains("Synthetic list update denied"));

    let after = client.mcp("list_items", json!({})).await;
    let mut expected = original;
    expected["ReviewStatus"] = json!("Ready");
    expected["Complete"] = json!(false);
    expected["Quantity"] = json!(0);
    expected["OptionalNote"] = Value::Null;
    assert_eq!(
        after["structuredContent"]["items"][0],
        json!({"id": ITEM, "e_tag": etag(6), "fields": expected})
    );
    let after_cursor = after["structuredContent"]["next_cursor"]
        .as_str()
        .unwrap()
        .to_string();
    let after_p2 = client
        .mcp("list_items", json!({ "cursor": after_cursor }))
        .await;
    assert_eq!(
        after_p2["structuredContent"]["items"][0],
        before_p2["items"][0]
    );
    assert_eq!(
        state.lock().await.writes,
        vec![
            (ITEM.into(), Some(etag(1)), json!({"ReviewStatus": "Ready"})),
            (ITEM.into(), Some(etag(2)), json!({"Complete": false})),
            (ITEM.into(), None, json!({"Quantity": 0})),
            (ITEM.into(), None, json!({"Complete": false})),
            (ITEM.into(), None, json!({"OptionalNote": null})),
            (ITEM.into(), Some(etag(2)), json!({"Quantity": 4})),
            (ITEM.into(), None, json!({"ReviewStatus": "Denied"})),
            (ITEM.into(), None, json!({"ReviewStatus": "Denied"})),
        ]
    );
    server.abort();
}

#[derive(Clone)]
struct PagingFixture {
    base: String,
    state: Arc<Mutex<PagingState>>,
}

struct PagingRequest {
    raw_uri: String,
    expand: Option<String>,
    top: Option<String>,
}

struct PagingState {
    rows: Vec<Value>,
    encode_site_commas: bool,
    last_next_links: Vec<String>,
    requests: Vec<PagingRequest>,
}

impl PagingFixture {
    async fn items(
        State(fixture): State<Self>,
        uri: Uri,
        RoutePath((site, list)): RoutePath<(String, String)>,
        Query(query): Query<HashMap<String, String>>,
    ) -> Json<Value> {
        let mut state = fixture.state.lock().await;
        let expand = query.get("expand").cloned();
        let top = query.get("$top").cloned();
        let skiptoken = query.get("$skiptoken").cloned();
        state.requests.push(PagingRequest {
            raw_uri: uri.to_string(),
            expand: expand.clone(),
            top: top.clone(),
        });

        let offset = skiptoken
            .as_deref()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);
        let page_size = top
            .as_deref()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(50);
        let end = (offset + page_size).min(state.rows.len());
        let slice = if offset < state.rows.len() {
            state.rows[offset..end].to_vec()
        } else {
            Vec::new()
        };

        let mut body = json!({ "value": slice });
        if end < state.rows.len() {
            let site_segment = if state.encode_site_commas {
                site.replace(',', "%2C")
            } else {
                site
            };
            let expand_val = expand.unwrap_or_else(|| "fields".to_string());
            let next = format!(
                "{}/v1.0/sites/{site_segment}/lists/{list}/items?expand={}&$top={page_size}&$skiptoken={end}",
                fixture.base,
                urlencoding::encode(&expand_val),
            );
            state.last_next_links.push(next.clone());
            body["@odata.nextLink"] = json!(next);
        }
        Json(body)
    }
}

fn sample_rows(count: usize) -> Vec<Value> {
    (1..=count)
        .map(|idx| {
            let etag = format!("\"row{idx},1\"");
            json!({
                "id": idx.to_string(),
                "eTag": etag,
                "@odata.etag": etag,
                "fields": {
                    "@odata.etag": etag,
                    "Title": format!("Row {idx}"),
                    "Quantity": idx * 10,
                    "ReviewStatus": "Open",
                    "id": idx.to_string(),
                }
            })
        })
        .collect()
}

async fn spawn_paging_fixture(
    rows: Vec<Value>,
    encode_site_commas: bool,
) -> (
    String,
    Arc<Mutex<PagingState>>,
    tokio::task::JoinHandle<()>,
    tempfile::TempDir,
    std::ffi::OsString,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let state = Arc::new(Mutex::new(PagingState {
        rows,
        encode_site_commas,
        last_next_links: Vec::new(),
        requests: Vec::new(),
    }));
    let app = Router::new()
        .route(
            "/v1.0/sites/{site}/lists/{list}/items",
            get(PagingFixture::items),
        )
        .with_state(PagingFixture {
            base: base.clone(),
            state: state.clone(),
        });
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let profile = tempfile::tempdir().unwrap();
    let exe = std::env::var_os("SHAREPOINT_LISTS_TEST_EXE")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_sharepoint").into());
    (base, state, server, profile, exe)
}

#[tokio::test]
async fn list_items_pages_with_limit_and_cursor() {
    let (base, state, server, profile, exe) = spawn_paging_fixture(sample_rows(5), false).await;
    let client = Client {
        exe: Path::new(&exe),
        profile: profile.path(),
        base: &base,
    };

    let p1 = cli_json(
        client
            .cli("items", &["--field", "Title", "--limit", "2"])
            .await,
    );
    assert_eq!(state.lock().await.requests.len(), 1);
    assert_eq!(
        state.lock().await.requests[0].expand.as_deref(),
        Some("fields(select=Title)")
    );
    assert_eq!(state.lock().await.requests[0].top.as_deref(), Some("2"));
    assert_eq!(p1["returned"], 2);
    assert_eq!(p1["has_more"], true);
    let c1 = p1["next_cursor"].as_str().unwrap().to_string();

    let p2 = cli_json(
        client
            .cli("items", &["--field", "Title", "--cursor", &c1])
            .await,
    );
    assert_eq!(state.lock().await.requests.len(), 2);
    assert_eq!(p2["returned"], 2);
    assert_eq!(p2["has_more"], true);
    let c2 = p2["next_cursor"].as_str().unwrap().to_string();

    let p3 = cli_json(
        client
            .cli("items", &["--field", "Title", "--cursor", &c2])
            .await,
    );
    assert_eq!(state.lock().await.requests.len(), 3);
    assert_eq!(p3["returned"], 1);
    assert_eq!(p3["has_more"], false);
    assert_eq!(p3["next_cursor"], Value::Null);

    let ids: Vec<String> = [&p1, &p2, &p3]
        .iter()
        .flat_map(|page| {
            page["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["id"].as_str().unwrap().to_string())
        })
        .collect();
    assert_eq!(ids, vec!["1", "2", "3", "4", "5"]);

    // MCP produces identical structuredContent across all 3 pages (1 request per call).
    let m1 = client
        .mcp("list_items", json!({"fields": ["Title"], "limit": 2}))
        .await;
    assert_eq!(m1["structuredContent"], p1);
    assert_eq!(state.lock().await.requests.len(), 4);

    let m2 = client
        .mcp("list_items", json!({"fields": ["Title"], "cursor": c1}))
        .await;
    assert_eq!(m2["structuredContent"], p2);
    assert_eq!(state.lock().await.requests.len(), 5);

    let m3 = client
        .mcp("list_items", json!({"fields": ["Title"], "cursor": c2}))
        .await;
    assert_eq!(m3["structuredContent"], p3);
    assert_eq!(state.lock().await.requests.len(), 6);

    // Text mode prints cursor hint to stderr when more rows exist.
    let text_p1 = client
        .command()
        .args([
            "lists",
            "items",
            "--site-id",
            SITE,
            "--list-id",
            LIST,
            "--field",
            "Title",
            "--limit",
            "2",
        ])
        .output()
        .await
        .unwrap();
    assert!(text_p1.status.success());
    assert!(String::from_utf8_lossy(&text_p1.stderr)
        .contains(&format!("More rows: rerun with --cursor {c1}")));

    server.abort();
}

#[tokio::test]
async fn list_items_default_limit_is_50_and_strips_odata_etag() {
    let (base, state, server, profile, exe) = spawn_paging_fixture(sample_rows(2), false).await;
    let client = Client {
        exe: Path::new(&exe),
        profile: profile.path(),
        base: &base,
    };

    let page = cli_json(client.cli("items", &[]).await);
    assert_eq!(state.lock().await.requests[0].top.as_deref(), Some("50"));
    assert_eq!(page["returned"], 2);
    assert_eq!(page["has_more"], false);
    assert_eq!(page["next_cursor"], Value::Null);

    let row0 = &page["items"][0];
    assert_eq!(row0["e_tag"], "\"row1,1\"");
    let fields = row0["fields"].as_object().unwrap();
    assert!(!fields.contains_key("@odata.etag"));
    assert_eq!(fields.get("Title"), Some(&json!("Row 1")));
    assert_eq!(fields.get("Quantity"), Some(&json!(10)));
    assert_eq!(fields.get("ReviewStatus"), Some(&json!("Open")));
    assert_eq!(fields.get("id"), Some(&json!("1")));

    server.abort();
}

#[tokio::test]
async fn list_items_empty_list() {
    let (base, _state, server, profile, exe) = spawn_paging_fixture(Vec::new(), false).await;
    let client = Client {
        exe: Path::new(&exe),
        profile: profile.path(),
        base: &base,
    };

    let page = cli_json(client.cli("items", &[]).await);
    assert_eq!(
        page,
        json!({
            "has_more": false,
            "items": [],
            "next_cursor": null,
            "returned": 0
        })
    );

    server.abort();
}

#[tokio::test]
async fn list_items_rejects_bad_limit_and_malformed_cursor() {
    let (base, state, server, profile, exe) = spawn_paging_fixture(sample_rows(5), false).await;
    let client = Client {
        exe: Path::new(&exe),
        profile: profile.path(),
        base: &base,
    };

    let p1 = cli_json(client.cli("items", &["--limit", "2"]).await);
    let valid_cursor = p1["next_cursor"].as_str().unwrap().to_string();
    let raw_next_link = state.lock().await.last_next_links[0].clone();
    let v2_cursor = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&json!({
            "v": 2,
            "site_id": SITE.to_ascii_lowercase(),
            "list_id": LIST.to_ascii_lowercase(),
            "fields": [],
            "next": raw_next_link,
        }))
        .unwrap(),
    );

    let before_reqs = state.lock().await.requests.len();

    // CLI clap usage errors (exit 2)
    let lim0 = client.cli("items", &["--limit", "0"]).await;
    assert_eq!(lim0.status.code(), Some(2));
    assert!(lim0.stdout.is_empty());
    assert!(String::from_utf8_lossy(&lim0.stderr)
        .contains("invalid value '0' for '--limit <LIMIT>': 0 is not in 1..=200"));

    let lim201 = client.cli("items", &["--limit", "201"]).await;
    assert_eq!(lim201.status.code(), Some(2));
    assert!(lim201.stdout.is_empty());
    assert!(String::from_utf8_lossy(&lim201.stderr)
        .contains("invalid value '201' for '--limit <LIMIT>': 201 is not in 1..=200"));

    let combo = client
        .cli("items", &["--limit", "10", "--cursor", &valid_cursor])
        .await;
    assert_eq!(combo.status.code(), Some(2));
    assert!(combo.stdout.is_empty());
    assert!(String::from_utf8_lossy(&combo.stderr)
        .contains("the argument '--limit <LIMIT>' cannot be used with '--cursor <CURSOR>'"));

    // CLI malformed cursors (exit 1, 0 requests, single-line JSON error on stdout)
    for bad in ["not-a-token", raw_next_link.as_str(), v2_cursor.as_str()] {
        let out = client.cli("items", &["--cursor", bad]).await;
        assert_eq!(out.status.code(), Some(1));
        let stdout = String::from_utf8_lossy(&out.stdout);
        let lines: Vec<&str> = stdout.lines().collect();
        assert_eq!(lines.len(), 1, "expected single-line JSON error on stdout, got: {stdout}");
        let err_json: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(err_json["ok"], false);
        assert_eq!(err_json["error"]["code"], "error");
        assert!(err_json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Invalid cursor: pass the next_cursor value from the previous page"));
        assert!(String::from_utf8_lossy(&out.stderr)
            .contains("Invalid cursor: pass the next_cursor value from the previous page"));

        let mcp_bad = client.mcp("list_items", json!({"cursor": bad})).await;
        assert_eq!(mcp_bad["isError"], true);
        assert!(mcp_bad["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Invalid cursor: pass the next_cursor value from the previous page"));
    }

    // MCP bad limit / cursor types / limit+cursor
    for bad_limit in [json!(0), json!(201), json!("5")] {
        let res = client.mcp("list_items", json!({"limit": bad_limit})).await;
        assert_eq!(res["isError"], true);
        assert_eq!(
            res["content"][0]["text"],
            "limit must be an integer from 1 to 200"
        );
    }

    let mcp_combo = client
        .mcp("list_items", json!({"limit": 5, "cursor": valid_cursor}))
        .await;
    assert_eq!(mcp_combo["isError"], true);
    assert_eq!(
        mcp_combo["content"][0]["text"],
        "cursor and limit cannot be combined; a cursor keeps the first page's size"
    );

    let mcp_cursor_num = client.mcp("list_items", json!({"cursor": 7})).await;
    assert_eq!(mcp_cursor_num["isError"], true);
    assert_eq!(mcp_cursor_num["content"][0]["text"], "cursor must be a string");

    assert_eq!(
        state.lock().await.requests.len(),
        before_reqs,
        "no rejected limit/cursor call may hit Graph"
    );

    server.abort();
}

#[tokio::test]
async fn list_items_cursor_is_bound_to_site_list_and_fields() {
    let (base, state, server, profile, exe) = spawn_paging_fixture(sample_rows(5), false).await;
    let client = Client {
        exe: Path::new(&exe),
        profile: profile.path(),
        base: &base,
    };

    let page_a = cli_json(
        client
            .cli("items", &["--field", "Title", "--limit", "2"])
            .await,
    );
    let cursor_a = page_a["next_cursor"].as_str().unwrap().to_string();
    let page_b = cli_json(client.cli("items", &["--limit", "2"]).await);
    let cursor_b = page_b["next_cursor"].as_str().unwrap().to_string();

    let reqs_before = state.lock().await.requests.len();
    let other_site = "other.sharepoint.com,99999999-9999-9999-9999-999999999999,88888888-8888-8888-8888-888888888888";
    let other_list = "44444444-4444-4444-4444-444444444444";

    let cases: Vec<(&str, &str, Vec<&str>, Option<Vec<&str>>, &str, &str)> = vec![
        (
            SITE,
            other_list,
            vec!["--field", "Title", "--cursor", &cursor_a],
            Some(vec!["Title"]),
            &cursor_a,
            "different site_id or list_id",
        ),
        (
            other_site,
            LIST,
            vec!["--field", "Title", "--cursor", &cursor_a],
            Some(vec!["Title"]),
            &cursor_a,
            "different site_id or list_id",
        ),
        (
            SITE,
            LIST,
            vec!["--field", "Quantity", "--cursor", &cursor_a],
            Some(vec!["Quantity"]),
            &cursor_a,
            "issued for fields [Title]",
        ),
        (
            SITE,
            LIST,
            vec!["--cursor", &cursor_a],
            None,
            &cursor_a,
            "issued for fields [Title]",
        ),
        (
            SITE,
            LIST,
            vec!["--field", "Title", "--cursor", &cursor_b],
            Some(vec!["Title"]),
            &cursor_b,
            "issued for all fields",
        ),
    ];

    for (site, list, cli_args, mcp_fields, cursor, expected_err) in cases {
        let out = client.cli_for(site, list, "items", &cli_args).await;
        assert_eq!(out.status.code(), Some(1));
        let stdout = String::from_utf8_lossy(&out.stdout);
        let lines: Vec<&str> = stdout.lines().collect();
        assert_eq!(lines.len(), 1, "expected single-line JSON error on stdout, got: {stdout}");
        let err_json: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(err_json["ok"], false);
        assert!(err_json["error"]["message"]
            .as_str()
            .unwrap()
            .contains(expected_err));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains(expected_err),
            "expected stderr to contain {expected_err:?}, got: {stderr}"
        );

        let mut args = json!({
            "site_id": site,
            "list_id": list,
            "cursor": cursor,
        });
        if let Some(fields) = mcp_fields {
            args["fields"] = json!(fields);
        }
        let mcp = client.mcp_raw("list_items", args).await;
        assert_eq!(mcp["isError"], true);
        let text = mcp["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains(expected_err),
            "expected MCP text to contain {expected_err:?}, got: {text}"
        );
    }

    assert_eq!(
        state.lock().await.requests.len(),
        reqs_before,
        "cursor mismatches must fail before any Graph request"
    );

    server.abort();
}

#[tokio::test]
async fn list_items_cursor_rejects_foreign_host_and_shape() {
    let (base, state, server, profile, exe) = spawn_paging_fixture(sample_rows(5), false).await;
    let client = Client {
        exe: Path::new(&exe),
        profile: profile.path(),
        base: &base,
    };

    let make_token = |next: String| {
        URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&json!({
                "v": 1,
                "site_id": SITE.to_ascii_lowercase(),
                "list_id": LIST.to_ascii_lowercase(),
                "fields": [],
                "next": next,
            }))
            .unwrap(),
        )
    };

    let foreign = make_token("https://example.com/v1.0/sites/a/lists/b/items".to_string());
    let wrong_shape = make_token(format!("{base}/v1.0/drives/d1/root/children"));

    for (token, expected) in [
        (&foreign, "configured Graph endpoint"),
        (&wrong_shape, "not a SharePoint list items page"),
    ] {
        let out = client.cli("items", &["--cursor", token]).await;
        assert_eq!(out.status.code(), Some(1));
        let stdout = String::from_utf8_lossy(&out.stdout);
        let lines: Vec<&str> = stdout.lines().collect();
        assert_eq!(lines.len(), 1, "expected single-line JSON error on stdout, got: {stdout}");
        let err_json: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(err_json["ok"], false);
        assert!(err_json["error"]["message"]
            .as_str()
            .unwrap()
            .contains(expected));
        assert!(String::from_utf8_lossy(&out.stderr).contains(expected));

        let mcp = client.mcp("list_items", json!({"cursor": token})).await;
        assert_eq!(mcp["isError"], true);
        assert!(mcp["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains(expected));
    }

    assert_eq!(state.lock().await.requests.len(), 0);
    server.abort();
}

#[tokio::test]
async fn list_items_cursor_accepts_equivalent_spelling() {
    let (base, state, server, profile, exe) = spawn_paging_fixture(sample_rows(5), true).await;
    let client = Client {
        exe: Path::new(&exe),
        profile: profile.path(),
        base: &base,
    };

    let p1 = cli_json(
        client
            .cli(
                "items",
                &["--field", "Title", "--field", "Quantity", "--limit", "1"],
            )
            .await,
    );
    assert_eq!(
        state.lock().await.requests[0].expand.as_deref(),
        Some("fields(select=Quantity,Title)")
    );
    let cursor = p1["next_cursor"].as_str().unwrap().to_string();
    let issued_next_link = state.lock().await.last_next_links[0].clone();
    assert!(
        issued_next_link.contains("%2C"),
        "fixture must percent-encode commas in site_id"
    );

    // (a) Reordered fields
    let a = cli_json(
        client
            .cli(
                "items",
                &["--field", "Quantity", "--field", "Title", "--cursor", &cursor],
            )
            .await,
    );
    assert_eq!(a["items"][0]["id"], "2");

    // (b) Duplicate fields
    let b = cli_json(
        client
            .cli(
                "items",
                &[
                    "--field",
                    "Title",
                    "--field",
                    "Quantity",
                    "--field",
                    "Title",
                    "--cursor",
                    &cursor,
                ],
            )
            .await,
    );
    assert_eq!(b["items"][0]["id"], "2");

    // (c) Uppercased GUID in site_id
    let upper_site = SITE.to_ascii_uppercase();
    let c = cli_json(
        client
            .cli_for(
                &upper_site,
                LIST,
                "items",
                &["--field", "Quantity", "--field", "Title", "--cursor", &cursor],
            )
            .await,
    );
    assert_eq!(c["items"][0]["id"], "2");

    // (d) Leading/trailing whitespace in list_id
    let padded_list = format!("  {LIST}  ");
    let d = cli_json(
        client
            .cli_for(
                SITE,
                &padded_list,
                "items",
                &["--field", "Quantity", "--field", "Title", "--cursor", &cursor],
            )
            .await,
    );
    assert_eq!(d["items"][0]["id"], "2");

    // (e) MCP with reordered fields
    let m = client
        .mcp(
            "list_items",
            json!({"fields": ["Quantity", "Title"], "cursor": cursor}),
        )
        .await;
    assert_eq!(m["isError"], false);
    assert_eq!(m["structuredContent"]["items"][0]["id"], "2");

    // Every cursor call sent the exact byte-for-byte nextLink URL issued by the mock.
    let reqs = state.lock().await;
    assert_eq!(reqs.requests.len(), 6);
    for req in &reqs.requests[1..] {
        assert_eq!(format!("{base}{}", req.raw_uri), issued_next_link);
    }

    server.abort();
}

#[tokio::test]
async fn list_items_next_cursor_is_opaque_envelope() {
    let (base, state, server, profile, exe) = spawn_paging_fixture(sample_rows(5), true).await;
    let client = Client {
        exe: Path::new(&exe),
        profile: profile.path(),
        base: &base,
    };

    let p1 = cli_json(
        client
            .cli(
                "items",
                &["--field", "Title", "--field", "Quantity", "--limit", "2"],
            )
            .await,
    );
    let token = p1["next_cursor"].as_str().unwrap();
    assert!(!token.contains('='), "base64url token must omit padding");
    assert!(!token.contains('+'), "base64url token must not contain '+'");
    assert!(!token.contains('/'), "base64url token must not contain '/'");

    let decoded = URL_SAFE_NO_PAD.decode(token).unwrap();
    let raw_json = String::from_utf8(decoded).unwrap();
    let expected_next = state.lock().await.last_next_links[0].clone();
    assert_eq!(
        raw_json,
        format!(
            r#"{{"v":1,"site_id":"{}","list_id":"{}","fields":["Quantity","Title"],"next":{}}}"#,
            SITE.to_ascii_lowercase(),
            LIST.to_ascii_lowercase(),
            serde_json::to_string(&expected_next).unwrap(),
        )
    );

    server.abort();
}

#[tokio::test]
async fn list_columns_still_reads_every_page() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let state = Arc::new(Mutex::new(ListState {
        revision: 1,
        fields: json!({}),
        reads: vec![],
        writes: vec![],
    }));
    let app = Router::new()
        .route(
            "/v1.0/sites/{site}/lists/{list}/columns",
            get(Fixture::columns),
        )
        .with_state(Fixture {
            base: base.clone(),
            state: state.clone(),
        });
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let profile = tempfile::tempdir().unwrap();
    let exe = std::env::var_os("SHAREPOINT_LISTS_TEST_EXE")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_sharepoint").into());
    let client = Client {
        exe: Path::new(&exe),
        profile: profile.path(),
        base: &base,
    };

    let cols = cli_json(client.cli("columns", &[]).await);
    assert_eq!(cols["columns"].as_array().unwrap().len(), 5);
    assert_eq!(
        client.mcp("list_columns", json!({})).await["structuredContent"],
        cols
    );

    server.abort();
}
