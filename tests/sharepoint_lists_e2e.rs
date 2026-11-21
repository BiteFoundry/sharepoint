use std::{
    collections::HashMap,
    path::Path,
    process::{Output, Stdio},
    sync::Arc,
};

use axum::{
    extract::{Path as RoutePath, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, patch},
    Json, Router,
};
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
    reads: Vec<(String, Option<String>, Option<String>)>,
    writes: Vec<(String, Option<String>, Value)>,
}

impl Fixture {
    async fn items(
        State(fixture): State<Self>,
        RoutePath((site, list)): RoutePath<(String, String)>,
        Query(query): Query<HashMap<String, String>>,
    ) -> Json<Value> {
        assert_eq!((site.as_str(), list.as_str()), (SITE, LIST));
        let mut state = fixture.state.lock().await;
        let expand = query.get("expand").expect("expanded list fields");
        let token = query.get("$skiptoken");
        state
            .reads
            .push(("items".into(), Some(expand.clone()), token.cloned()));
        if let Some(token) = token {
            assert_eq!(token, "opaque+step/2");
            return Json(json!({ "value": [{
                "id": "28", "@odata.etag": "\"row-28\"", "fields": { "Title": "Second page", "Quantity": 4 }
            }] }));
        }
        let next = format!(
            "{}/v1.0/sites/{SITE}/lists/{LIST}/items?expand={}&%24skiptoken=opaque%2Bstep%2F2",
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
        assert_eq!(
            selection.map(String::as_str),
            Some("id,name,displayName,readOnly,hidden")
        );
        fixture.state.lock().await.reads.push((
            "columns".into(),
            selection.cloned(),
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
            "{}/v1.0/sites/{SITE}/lists/{LIST}/columns?%24select=id,name,displayName,readOnly,hidden&%24skiptoken=next",
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
        self.command()
            .args([
                "lists",
                action,
                "--site-id",
                SITE,
                "--list-id",
                LIST,
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

    let columns = cli_json(client.cli("columns", &[]).await);
    assert_eq!(
        client.mcp("list_columns", json!({})).await["structuredContent"],
        columns
    );
    assert_eq!(columns["columns"][0]["name"], "ReviewStatus");
    assert_eq!(columns["columns"][0]["display_name"], "Review status");
    assert_eq!(columns["columns"][3]["read_only"], true);
    let projection = cli_json(
        client
            .cli("items", &["--field", "Title", "--field", "Quantity"])
            .await,
    );
    assert_eq!(
        client
            .mcp("list_items", json!({"fields": ["Title", "Quantity"]}))
            .await["structuredContent"],
        projection
    );
    assert_eq!(
        projection,
        json!({"items": [
            { "id": ITEM, "e_tag": etag(1), "fields": { "Title": "Keep this title", "Quantity": 5 } },
            { "id": "28", "e_tag": "\"row-28\"", "fields": { "Title": "Second page", "Quantity": 4 } }
        ]})
    );
    let reads = state.lock().await.reads.clone();
    assert_eq!(
        reads
            .iter()
            .filter(|(kind, expand, _)| kind == "items"
                && expand.as_deref() == Some("fields(select=Title,Quantity)"))
            .count(),
        4
    );
    assert_eq!(
        reads
            .iter()
            .filter(|(_, _, token)| token.as_deref() == Some("opaque+step/2"))
            .count(),
        2
    );

    let before = cli_json(client.cli("items", &[]).await);
    assert_eq!(before["items"][0]["fields"], original);
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
    assert_eq!(after["structuredContent"]["items"][1], before["items"][1]);
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
