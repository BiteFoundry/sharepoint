//! Core OneDrive/SharePoint pagination, uploads/downloads, FTS5, HTTP API, and MCP parity tests.

use std::sync::atomic::Ordering;
use axum::http::StatusCode;
use serde_json::{json, Value};
use tempfile::tempdir;
use tokio::net::TcpListener;
use sharepoint_cli::mcp;
use sharepoint_cli::serve;
use crate::server::{make_graph_and_storage, start_mock_graph};

#[tokio::test]
async fn e2e_onedrive_sharepoint_pagination_uploads_downloads_and_fts5() {
    let (base_url, mock_state, server) = start_mock_graph().await;
    let (_cfg, graph, storage) = make_graph_and_storage(&base_url);

    // 1. OneDrive + Recent + Shared
    let my_drive = graph.get_my_drive().await.unwrap();
    assert_eq!(my_drive.id, "drv-me");
    assert!(my_drive.is_personal_onedrive());

    let recent = graph.list_recent_items(10).await.unwrap();
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].name, "recent-notes.md");

    let shared = graph.list_shared_with_me(10).await.unwrap();
    assert_eq!(shared.len(), 1);
    assert_eq!(shared[0].effective_drive_id(), "drv-remote-fin");
    assert_eq!(shared[0].effective_item_id(), "remote-item-99");

    // 2. SharePoint Sites + Drives
    let followed = graph.list_followed_sites().await.unwrap();
    assert_eq!(followed.len(), 1);
    assert!(followed[0].is_followed);

    let all_sites = graph.list_sites(Some("*"), 50).await.unwrap();
    assert_eq!(all_sites.len(), 2);

    let eng_drives = graph
        .list_site_drives("site-eng", Some("Engineering Hub"))
        .await
        .unwrap();
    assert_eq!(eng_drives.len(), 1);
    assert_eq!(eng_drives[0].id, "drv-site-eng-docs");

    // 3. Multi-page @odata.nextLink pagination + SQLite FTS5 + Korean <3 char LIKE fallback
    let children = graph.list_children("drv-me", None, None).await.unwrap();
    assert_eq!(children.len(), 3, "must follow @odata.nextLink across 2 pages");
    storage.upsert_drive_items(&children).unwrap();
    assert_eq!(storage.search_items("README", Some("drv-me"), 10).unwrap().len(), 1);
    assert_eq!(storage.search_items("기획", Some("drv-me"), 10).unwrap().len(), 1);

    // 4. Path-based lookup
    let by_path = graph.get_item_by_path("drv-me", "/Docs/spec.md").await.unwrap();
    assert_eq!(by_path.name, "spec.md");

    // 5. Download with 302 redirect Authorization stripping!
    let tmp = tempdir().unwrap();
    let dl = graph
        .download_item_to_file("drv-me", "item-img", Some("chart.png"), Some("image/png"), None, tmp.path())
        .await
        .unwrap();
    assert!(dl.ok);
    assert!(dl.is_image);
    assert_eq!(dl.mime_type, "image/png");
    assert!(dl.size_bytes > 20);
    assert!(dl.path.exists());
    assert!(
        !mock_state.redirect_saw_auth.load(Ordering::SeqCst),
        "Authorization header must be stripped on 302 redirect"
    );

    // 6. Simple upload (< 4 MiB)
    let simple = graph
        .upload_file_simple("drv-me", None, "hello.txt", b"hello sharepoint".to_vec(), None)
        .await
        .unwrap();
    assert_eq!(simple.id, "item-uploaded-simple");
    assert_eq!(simple.size, 16);

    // 7. Chunked upload session (2 chunks, verifying no Bearer header sent to uploadUrl)
    let payload = vec![b'A'; 700_000];
    let chunked = graph
        .upload_bytes_chunked("drv-me", None, "large.bin", &payload, 320 * 1024)
        .await
        .unwrap();
    assert_eq!(chunked.id, "item-uploaded-chunked");
    assert_eq!(chunked.size as usize, payload.len());
    assert!(mock_state.chunk_count.load(Ordering::SeqCst) >= 2);
    assert!(
        !mock_state.chunk_saw_auth.load(Ordering::SeqCst),
        "Pre-authenticated upload session URL must not receive Bearer auth"
    );

    // 8. mkdir, rename, versions, share link, delete, cross-site search
    let folder = graph.create_folder("drv-me", None, "Reports").await.unwrap();
    assert_eq!(folder.name, "Reports");
    assert!(folder.is_folder);

    let renamed = graph
        .rename_or_move_item("drv-me", "item-readme", Some("GUIDE.md"), Some("folder-proj"))
        .await
        .unwrap();
    assert_eq!(renamed.name, "GUIDE.md");

    let versions = graph
        .list_item_versions_with_limit("drv-me", "item-readme", Some(1))
        .await
        .unwrap();
    assert_eq!(versions.len(), 1);
    assert_eq!(versions[0].id, "2.0");
    assert_eq!(mock_state.version_pages.load(Ordering::SeqCst), 1);

    let versions = graph.list_item_versions("drv-me", "item-readme").await.unwrap();
    let ids: Vec<_> = versions.iter().map(|v| v.id.as_str()).collect();
    assert_eq!(ids, ["2.0", "1.0"]);
    assert_eq!(mock_state.version_pages.load(Ordering::SeqCst), 3);

    let share = graph
        .create_share_link("drv-me", "item-readme", "view", "organization", None)
        .await
        .unwrap();
    assert!(share.web_url.contains("share-xyz"));

    let cross = graph.search_across_sites("spec", 10).await.unwrap();
    assert_eq!(cross.len(), 1);
    assert_eq!(cross[0].id, "item-cross-1");

    graph.delete_item("drv-me", "item-readme").await.unwrap();
    assert_eq!(mock_state.deleted_items.lock().await.as_slice(), &["item-readme"]);

    server.abort();
}

#[tokio::test]
async fn e2e_http_api_and_mcp_tools_parity() {
    let (base_url, _mock_state, graph_server) = start_mock_graph().await;
    let (cfg, graph, storage) = make_graph_and_storage(&base_url);

    // --- HTTP API (`serve::build_router`) ---
    let router = serve::build_router(
        cfg.clone(),
        graph.clone(),
        storage.clone(),
        Some("secret-api-key".to_string()),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_addr = listener.local_addr().unwrap();
    let api_server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let http = reqwest::Client::new();
    let health: Value = http
        .get(format!("http://{api_addr}/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["ok"], true);
    assert_eq!(health["service"], "sharepoint");

    // Unauthorized without API key
    let unauth = http
        .get(format!("http://{api_addr}/v1/sites"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

    // Authorized requests
    let sites_resp: Value = http
        .get(format!("http://{api_addr}/v1/sites"))
        .bearer_auth("secret-api-key")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(sites_resp["sites"].as_array().unwrap().len(), 2);

    let tmp_dl = tempdir().unwrap();
    let dl_resp: Value = http
        .post(format!("http://{api_addr}/v1/download"))
        .bearer_auth("secret-api-key")
        .json(&json!({
            "drive": "me",
            "item_id": "item-img",
            "dir": tmp_dl.path().to_string_lossy()
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(dl_resp["ok"], true);
    assert_eq!(dl_resp["file_name"], "chart.png");
    assert_eq!(dl_resp["mime_type"], "image/png");
    assert_eq!(dl_resp["is_image"], true);

    // HTTP /v1/items/upload with empty if_match returns 400 Bad Request (WI-1)
    let bad_if_match = http
        .post(format!("http://{api_addr}/v1/items/upload"))
        .bearer_auth("secret-api-key")
        .json(&json!({
            "drive": "me",
            "file_name": "note.md",
            "content_text": "hello",
            "if_match": ""
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_if_match.status(), StatusCode::BAD_REQUEST);
    let bad_if_match_body: Value = bad_if_match.json().await.unwrap();
    assert!(bad_if_match_body["error"]
        .as_str()
        .unwrap_or("")
        .contains("If-Match requires a non-empty ETag"));

    // --- MCP Server (`mcp::handle_rpc_line`) ---
    let init = mcp::handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
    )
    .await
    .unwrap();
    assert_eq!(init["result"]["serverInfo"]["name"], "sharepoint");

    let list_tools = mcp::handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
    )
    .await
    .unwrap();
    assert!(list_tools["result"]["tools"].as_array().unwrap().len() >= 15);

    let compact_info = mcp::handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"get_file_info","arguments":{"drive":"me","item_id":"item-readme","compact":true}}}"#,
    )
    .await
    .unwrap();
    let compact = &compact_info["result"]["structuredContent"];
    assert_eq!(compact, &json!({ "item": {
        "drive_id": "drv-me",
        "id": "item-readme",
        "name": "README.md",
        "parent_path": "",
        "is_folder": false,
        "size": 25,
        "last_modified_at": null,
        "e_tag": "\"mock-etag\"",
        "omitted": {
            "fields": ["mime_type", "parent_id", "web_url"],
            "versions": null
        },
        "hint": "Call get_file_info with item_id \"item-readme\", drive \"drv-me\" and fields: [...] for specific fields or compact:false for every field."
    } }));
    let text = compact_info["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(&serde_json::from_str::<Value>(text).unwrap(), compact);

    // read_text_file
    let read_res = mcp::handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"read_text_file","arguments":{"drive":"me","item_id":"item-readme"}}}"#,
    )
    .await
    .unwrap();
    assert_eq!(read_res["result"]["isError"], false);
    assert!(read_res["result"]["structuredContent"]["content"]
        .as_str()
        .unwrap()
        .contains("SharePoint CLI"));

    // download_file returns { ok, path, file_name, mime_type, size_bytes, is_image }
    let mcp_dl_req = json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "download_file",
            "arguments": {
                "drive": "me",
                "item_id": "item-img",
                "dir": tmp_dl.path().to_string_lossy()
            }
        }
    });
    let mcp_dl = mcp::handle_rpc_line(&cfg, &graph, &storage, &mcp_dl_req.to_string())
        .await
        .unwrap();
    let sc = &mcp_dl["result"]["structuredContent"];
    assert_eq!(sc["ok"], true);
    assert_eq!(sc["mime_type"], "image/png");
    assert_eq!(sc["is_image"], true);
    assert!(sc["size_bytes"].as_u64().unwrap() > 0);

    // delete_item without confirm: true MUST be rejected
    let del_rejected = mcp::handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"delete_item","arguments":{"drive":"me","item_id":"item-readme"}}}"#,
    )
    .await
    .unwrap();
    assert_eq!(del_rejected["result"]["isError"], true);

    // delete_item with confirm: true succeeds
    let del_ok = mcp::handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"delete_item","arguments":{"drive":"me","item_id":"item-readme","confirm":true}}}"#,
    )
    .await
    .unwrap();
    assert_eq!(del_ok["result"]["isError"], false);
    assert_eq!(del_ok["result"]["structuredContent"]["deleted"], true);

    // WI-3 (F-06): create_folder accepts both a folder path and a folder ID
    let cf_by_path = mcp::handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"create_folder","arguments":{"drive":"me","parent_id":"/Team Docs","name":"Path Child"}}}"#,
    )
    .await
    .unwrap();
    assert_eq!(cf_by_path["result"]["isError"], false, "{cf_by_path}");
    assert_eq!(
        cf_by_path["result"]["structuredContent"]["item"]["name"],
        "Path Child"
    );
    assert_eq!(
        cf_by_path["result"]["structuredContent"]["item"]["parent_id"],
        "item-by-path"
    );

    let cf_by_id = mcp::handle_rpc_line(
        &cfg,
        &graph,
        &storage,
        r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"create_folder","arguments":{"drive":"me","parent_id":"folder-proj","name":"Id Child"}}}"#,
    )
    .await
    .unwrap();
    assert_eq!(cf_by_id["result"]["isError"], false, "{cf_by_id}");
    assert_eq!(
        cf_by_id["result"]["structuredContent"]["item"]["name"],
        "Id Child"
    );
    assert_eq!(
        cf_by_id["result"]["structuredContent"]["item"]["parent_id"],
        "folder-proj"
    );

    api_server.abort();
    graph_server.abort();
}

