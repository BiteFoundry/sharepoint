//! Explicit site drive resolution, move-to-root, and resolve_item error classification tests.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, put};
use axum::response::IntoResponse;
use axum::{Json, Router};
use serde_json::{json, Value};
use tempfile::tempdir;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use sharepoint_cli::mcp;
use crate::server::{make_graph_and_storage, onedrive_alias_exe};

#[tokio::test]
async fn regression_sharepoint_sp_03_explicit_site_resolves_site_drives_named_personal_or_my() {
    use tokio::process::Command;

    #[derive(Clone)]
    struct Sp03State {
        me_drive_calls: Arc<AtomicUsize>,
        upload_paths: Arc<Mutex<Vec<String>>>,
    }

    let st = Sp03State {
        me_drive_calls: Arc::new(AtomicUsize::new(0)),
        upload_paths: Arc::new(Mutex::new(Vec::new())),
    };

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}/v1.0");

    let router = Router::new()
        .route(
            "/v1.0/me/drive",
            get(|State(s): State<Sp03State>| async move {
                s.me_drive_calls.fetch_add(1, Ordering::SeqCst);
                Json(json!({
                    "id": "drv-me",
                    "name": "OneDrive",
                    "driveType": "business"
                }))
            }),
        )
        .route(
            "/v1.0/me/followedSites",
            get(|| async {
                Json(json!({
                    "value": [{
                        "id": "site-acme",
                        "name": "acme-engineering",
                        "displayName": "Acme Engineering",
                        "webUrl": "https://synthetic.sharepoint.test/sites/acme-engineering"
                    }]
                }))
            }),
        )
        .route(
            "/v1.0/sites",
            get(|| async {
                Json(json!({
                    "value": [{
                        "id": "site-acme",
                        "name": "acme-engineering",
                        "displayName": "Acme Engineering",
                        "webUrl": "https://synthetic.sharepoint.test/sites/acme-engineering"
                    }]
                }))
            }),
        )
        .route(
            "/v1.0/sites/{site_id}/drives",
            get(|Path(site_id): Path<String>| async move {
                if site_id == "site-acme" {
                    Json(json!({
                        "value": [
                            {
                                "id": "drv-eng-personal",
                                "name": "Personal",
                                "driveType": "documentLibrary"
                            },
                            {
                                "id": "drv-eng-my",
                                "name": "My",
                                "driveType": "documentLibrary"
                            },
                            {
                                "id": "drv-eng-docs",
                                "name": "Documents",
                                "driveType": "documentLibrary"
                            }
                        ]
                    }))
                } else {
                    Json(json!({ "value": [] }))
                }
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/root/children",
            get(|Path(drive_id): Path<String>| async move {
                let name = match drive_id.as_str() {
                    "drv-eng-personal" => "site-personal-handbook.md",
                    "drv-eng-my" => "site-my-checklist.md",
                    "drv-me" => "my-private-budget.xlsx",
                    _ => "other.txt",
                };
                Json(json!({
                    "value": [{
                        "id": format!("item-{drive_id}"),
                        "name": name,
                        "size": 10,
                        "file": { "mimeType": "text/plain" },
                        "parentReference": { "driveId": drive_id, "id": "root", "path": "/drive/root:" }
                    }]
                }))
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/{*tail}",
            put(|State(s): State<Sp03State>, Path((drive_id, tail)): Path<(String, String)>, body: Bytes| async move {
                s.upload_paths
                    .lock()
                    .await
                    .push(format!("/v1.0/drives/{drive_id}/{tail}"));
                let name = tail.trim_start_matches("root:/").trim_end_matches(":/content");
                (
                    StatusCode::CREATED,
                    Json(json!({
                        "id": format!("up-{drive_id}"),
                        "name": name,
                        "size": body.len(),
                        "file": { "mimeType": "text/plain" },
                        "parentReference": { "driveId": drive_id, "id": "root", "path": "/drive/root:" }
                    })),
                )
            }),
        )
        .with_state(st.clone());

    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let profile = tempdir().unwrap();
    let workdir = tempdir().unwrap();
    let upload_file = workdir.path().join("note.txt");
    std::fs::write(&upload_file, b"hello site library").unwrap();

    let run_cli = |args: Vec<String>| {
        let profile_path = profile.path().to_path_buf();
        let base_url = base_url.clone();
        async move {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_sharepoint"));
            cmd.kill_on_drop(true);
            for (k, _) in std::env::vars_os() {
                let u = k.to_string_lossy().to_ascii_uppercase();
                if ["SHAREPOINT_", "ONEDRIVE_", "TEAMS_", "AZURE_"]
                    .iter()
                    .any(|p| u.starts_with(p))
                {
                    cmd.env_remove(k);
                }
            }
            cmd.env("SHAREPOINT_GRAPH_BASE_URL", &base_url)
                .env("SHAREPOINT_ACCESS_TOKEN", "sp03-test-token")
                .env("SHAREPOINT_DISABLE_KEYRING", "1")
                .env("SHAREPOINT_SHARE_M365_AUTH", "0")
                .env("SHAREPOINT_AUTO_DAEMON", "0")
                .env("SHAREPOINT_CONFIG_DIR", profile_path.join("config"))
                .env("SHAREPOINT_DATA_DIR", profile_path.join("data"))
                .env("NO_PROXY", "127.0.0.1,localhost")
                .args(&args)
                .output()
                .await
                .unwrap()
        }
    };

    // 1. --site "Acme Engineering" --drive Personal -> site library drv-eng-personal, 0 /me/drive calls
    let ls_personal = run_cli(vec![
        "ls".into(),
        "--site".into(),
        "Acme Engineering".into(),
        "--drive".into(),
        "Personal".into(),
        "--json".into(),
    ])
    .await;
    assert!(ls_personal.status.success(), "ls Personal failed: {}", String::from_utf8_lossy(&ls_personal.stderr));
    let ls_personal_json: Value = serde_json::from_slice(&ls_personal.stdout).unwrap();
    assert_eq!(ls_personal_json["drive"]["id"], "drv-eng-personal");
    assert_eq!(ls_personal_json["items"][0]["name"], "site-personal-handbook.md");
    assert_eq!(st.me_drive_calls.load(Ordering::SeqCst), 0, "must not query /me/drive when --site is given");

    // 2. --site "Acme Engineering" --drive My -> site library drv-eng-my, 0 /me/drive calls
    let ls_my = run_cli(vec![
        "ls".into(),
        "--site".into(),
        "Acme Engineering".into(),
        "--drive".into(),
        "My".into(),
        "--json".into(),
    ])
    .await;
    assert!(ls_my.status.success());
    let ls_my_json: Value = serde_json::from_slice(&ls_my.stdout).unwrap();
    assert_eq!(ls_my_json["drive"]["id"], "drv-eng-my");
    assert_eq!(ls_my_json["items"][0]["name"], "site-my-checklist.md");
    assert_eq!(st.me_drive_calls.load(Ordering::SeqCst), 0);

    // 3. upload with --site and --drive Personal / My -> targets site drives, 0 /me/drive calls
    let up_personal = run_cli(vec![
        "upload".into(),
        upload_file.to_string_lossy().into_owned(),
        "--site".into(),
        "Acme Engineering".into(),
        "--drive".into(),
        "Personal".into(),
        "--json".into(),
    ])
    .await;
    assert!(up_personal.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&up_personal.stdout).unwrap()["drive_id"],
        "drv-eng-personal"
    );

    let up_my = run_cli(vec![
        "upload".into(),
        upload_file.to_string_lossy().into_owned(),
        "--site".into(),
        "Acme Engineering".into(),
        "--drive".into(),
        "My".into(),
        "--json".into(),
    ])
    .await;
    assert!(up_my.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&up_my.stdout).unwrap()["drive_id"],
        "drv-eng-my"
    );
    assert_eq!(st.me_drive_calls.load(Ordering::SeqCst), 0);

    // 4. Unmatched alias inside explicit site (e.g. --site "Acme Engineering" --drive OneDrive) fails with 0 /me/drive calls
    let ls_missing = run_cli(vec![
        "ls".into(),
        "--site".into(),
        "Acme Engineering".into(),
        "--drive".into(),
        "OneDrive".into(),
        "--json".into(),
    ])
    .await;
    assert!(!ls_missing.status.success(), "non-existent library inside --site must fail");
    assert_eq!(st.me_drive_calls.load(Ordering::SeqCst), 0);

    // 5. Regression control: exact drive ID and unique prefix "Pers" inside site continue working
    let ls_id = run_cli(vec![
        "ls".into(),
        "--site".into(),
        "Acme Engineering".into(),
        "--drive".into(),
        "drv-eng-personal".into(),
        "--json".into(),
    ])
    .await;
    assert!(ls_id.status.success());
    assert_eq!(serde_json::from_slice::<Value>(&ls_id.stdout).unwrap()["drive"]["id"], "drv-eng-personal");

    let ls_prefix = run_cli(vec![
        "ls".into(),
        "--site".into(),
        "Acme Engineering".into(),
        "--drive".into(),
        "Pers".into(),
        "--json".into(),
    ])
    .await;
    assert!(ls_prefix.status.success());
    assert_eq!(serde_json::from_slice::<Value>(&ls_prefix.stdout).unwrap()["drive"]["id"], "drv-eng-personal");
    assert_eq!(st.me_drive_calls.load(Ordering::SeqCst), 0);

    // 6. Regression control: without --site, me / onedrive aliases still resolve personal OneDrive
    let ls_me = run_cli(vec!["ls".into(), "--drive".into(), "me".into(), "--json".into()]).await;
    assert!(ls_me.status.success());
    assert_eq!(serde_json::from_slice::<Value>(&ls_me.stdout).unwrap()["drive"]["id"], "drv-me");
    assert_eq!(st.me_drive_calls.load(Ordering::SeqCst), 1);

    let ls_od = run_cli(vec!["ls".into(), "--drive".into(), "onedrive".into(), "--json".into()]).await;
    assert!(ls_od.status.success());
    assert_eq!(serde_json::from_slice::<Value>(&ls_od.stdout).unwrap()["drive"]["id"], "drv-me");

    // 7. bitefoundry-onedrive entrypoint also resolves explicit --site library without touching /me/drive
    let me_calls_before_od = st.me_drive_calls.load(Ordering::SeqCst);
    let od_profile = tempdir().unwrap();
    let od_exe = onedrive_alias_exe(od_profile.path());
    let mut od_cmd = Command::new(&od_exe);
    od_cmd.kill_on_drop(true);
    for (k, _) in std::env::vars_os() {
        let u = k.to_string_lossy().to_ascii_uppercase();
        if ["SHAREPOINT_", "ONEDRIVE_", "TEAMS_", "AZURE_"]
            .iter()
            .any(|p| u.starts_with(p))
        {
            od_cmd.env_remove(k);
        }
    }
    let od_site_out = od_cmd
        .env("SHAREPOINT_GRAPH_BASE_URL", &base_url)
        .env("SHAREPOINT_ACCESS_TOKEN", "sp03-od-token")
        .env("SHAREPOINT_DISABLE_KEYRING", "1")
        .env("SHAREPOINT_SHARE_M365_AUTH", "0")
        .env("SHAREPOINT_AUTO_DAEMON", "0")
        .env("SHAREPOINT_CONFIG_DIR", od_profile.path().join("config"))
        .env("SHAREPOINT_DATA_DIR", od_profile.path().join("data"))
        .env("NO_PROXY", "127.0.0.1,localhost")
        .args(["ls", "--site", "Acme Engineering", "--drive", "Personal", "--json"])
        .output()
        .await
        .unwrap();
    assert!(od_site_out.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&od_site_out.stdout).unwrap()["drive"]["id"],
        "drv-eng-personal"
    );
    assert_eq!(st.me_drive_calls.load(Ordering::SeqCst), me_calls_before_od);

    server.abort();
}

#[tokio::test]
async fn regression_sharepoint_sp_04_move_to_root_resolves_actual_root_id_in_cli_and_mcp() {
    use tokio::process::Command;

    #[derive(Clone)]
    struct Sp04State {
        fail_root_get: Arc<AtomicBool>,
        root_get_count: Arc<AtomicUsize>,
        patches: Arc<Mutex<Vec<(String, Value)>>>,
    }

    let st = Sp04State {
        fail_root_get: Arc::new(AtomicBool::new(false)),
        root_get_count: Arc::new(AtomicUsize::new(0)),
        patches: Arc::new(Mutex::new(Vec::new())),
    };

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}/v1.0");

    let router = Router::new()
        .route(
            "/v1.0/drives/{drive_id}",
            get(|Path(drive_id): Path<String>| async move {
                Json(json!({
                    "id": drive_id,
                    "name": "Documents",
                    "driveType": "documentLibrary"
                }))
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/root",
            get(|State(s): State<Sp04State>, Path(drive_id): Path<String>| async move {
                s.root_get_count.fetch_add(1, Ordering::SeqCst);
                if s.fail_root_get.load(Ordering::SeqCst) {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({ "error": { "code": "generalException", "message": "root lookup failed" } })),
                    )
                        .into_response();
                }
                (
                    StatusCode::OK,
                    Json(json!({
                        "id": "root-real",
                        "name": "root",
                        "size": 0,
                        "folder": { "childCount": 2 },
                        "parentReference": { "driveId": drive_id }
                    })),
                )
                    .into_response()
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/items/{item_id}",
            get(|Path((drive_id, item_id)): Path<(String, String)>| async move {
                if item_id == "fld-dest" {
                    return (
                        StatusCode::OK,
                        Json(json!({
                            "id": "fld-dest",
                            "name": "Dest",
                            "size": 0,
                            "folder": { "childCount": 0 },
                            "parentReference": { "driveId": drive_id, "id": "root-real", "path": "/drive/root:" }
                        })),
                    )
                        .into_response();
                }
                (
                    StatusCode::OK,
                    Json(json!({
                        "id": item_id,
                        "name": "doc.md",
                        "size": 12,
                        "file": { "mimeType": "text/markdown" },
                        "parentReference": { "driveId": drive_id, "id": "fld-sub", "path": "/drive/root:/Sub" }
                    })),
                )
                    .into_response()
            })
            .patch(|State(s): State<Sp04State>, Path((drive_id, item_id)): Path<(String, String)>, Json(body): Json<Value>| async move {
                s.patches.lock().await.push((item_id.clone(), body.clone()));
                if body.pointer("/parentReference/id").and_then(Value::as_str) == Some("root") {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({
                            "error": {
                                "code": "invalidRequest",
                                "message": "Cannot use literal 'root' as parentReference.id"
                            }
                        })),
                    )
                        .into_response();
                }
                let parent_id = body
                    .pointer("/parentReference/id")
                    .and_then(Value::as_str)
                    .unwrap_or("fld-sub");
                let parent_path = if parent_id == "root-real" {
                    "/drive/root:"
                } else {
                    "/drive/root:/Dest"
                };
                let name = body.get("name").and_then(Value::as_str).unwrap_or("doc.md");
                (
                    StatusCode::OK,
                    Json(json!({
                        "id": item_id,
                        "name": name,
                        "size": 12,
                        "file": { "mimeType": "text/markdown" },
                        "parentReference": { "driveId": drive_id, "id": parent_id, "path": parent_path }
                    })),
                )
                    .into_response()
            }),
        )
        .with_state(st.clone());

    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let profile = tempdir().unwrap();
    let run_cli = |args: Vec<String>| {
        let profile_path = profile.path().to_path_buf();
        let base_url = base_url.clone();
        async move {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_sharepoint"));
            cmd.kill_on_drop(true);
            for (k, _) in std::env::vars_os() {
                let u = k.to_string_lossy().to_ascii_uppercase();
                if ["SHAREPOINT_", "ONEDRIVE_", "TEAMS_", "AZURE_"]
                    .iter()
                    .any(|p| u.starts_with(p))
                {
                    cmd.env_remove(k);
                }
            }
            cmd.env("SHAREPOINT_GRAPH_BASE_URL", &base_url)
                .env("SHAREPOINT_ACCESS_TOKEN", "sp04-test-token")
                .env("SHAREPOINT_DISABLE_KEYRING", "1")
                .env("SHAREPOINT_SHARE_M365_AUTH", "0")
                .env("SHAREPOINT_AUTO_DAEMON", "0")
                .env("SHAREPOINT_CONFIG_DIR", profile_path.join("config"))
                .env("SHAREPOINT_DATA_DIR", profile_path.join("data"))
                .env("NO_PROXY", "127.0.0.1,localhost")
                .args(&args)
                .output()
                .await
                .unwrap()
        }
    };

    // 1. CLI `mv --parent /` fetches actual root ID ("root-real") and PATCHes with it
    let cli_mv = run_cli(vec![
        "mv".into(),
        "item-cli-1".into(),
        "--drive".into(),
        "drv-sp".into(),
        "--parent".into(),
        "/".into(),
        "--json".into(),
    ])
    .await;
    assert!(
        cli_mv.status.success(),
        "CLI mv --parent / failed: stderr={}",
        String::from_utf8_lossy(&cli_mv.stderr)
    );
    assert_eq!(st.root_get_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        st.patches.lock().await.last(),
        Some(&(
            "item-cli-1".to_string(),
            json!({ "parentReference": { "id": "root-real" } })
        ))
    );

    // 2. MCP `move_or_rename_item` with `new_parent_id: "root"` resolves "root-real" and PATCHes with it
    let (cfg, graph, storage) = make_graph_and_storage(&base_url);
    let mcp_req = json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "tools/call",
        "params": {
            "name": "move_or_rename_item",
            "arguments": {
                "drive": "drv-sp",
                "item_id": "item-mcp-1",
                "new_parent_id": "root"
            }
        }
    });
    let mcp_resp = mcp::handle_rpc_line(&cfg, &graph, &storage, &mcp_req.to_string())
        .await
        .unwrap();
    assert_eq!(
        mcp_resp["result"]["isError"],
        false,
        "MCP move_or_rename_item root failed: {mcp_resp}"
    );
    assert_eq!(st.root_get_count.load(Ordering::SeqCst), 2);
    assert_eq!(
        st.patches.lock().await.last(),
        Some(&(
            "item-mcp-1".to_string(),
            json!({ "parentReference": { "id": "root-real" } })
        ))
    );

    // 3. When GET /drives/{id}/root fails, 0 PATCH requests are sent (CLI and MCP)
    st.fail_root_get.store(true, Ordering::SeqCst);
    let patches_before_fail = st.patches.lock().await.len();
    let cli_fail = run_cli(vec![
        "mv".into(),
        "item-cli-fail".into(),
        "--drive".into(),
        "drv-sp".into(),
        "--parent".into(),
        "/".into(),
        "--json".into(),
    ])
    .await;
    assert!(!cli_fail.status.success());

    let mcp_fail_req = json!({
        "jsonrpc": "2.0",
        "id": 11,
        "method": "tools/call",
        "params": {
            "name": "move_or_rename_item",
            "arguments": {
                "drive": "drv-sp",
                "item_id": "item-mcp-fail",
                "new_parent_id": "root"
            }
        }
    });
    let mcp_fail = mcp::handle_rpc_line(&cfg, &graph, &storage, &mcp_fail_req.to_string())
        .await
        .unwrap();
    assert_eq!(mcp_fail["result"]["isError"], true);
    assert_eq!(
        st.patches.lock().await.len(),
        patches_before_fail,
        "failed root GET must send 0 PATCH requests"
    );

    // 4. Regression control: normal folder ID move and rename-only do NOT call GET /root
    st.fail_root_get.store(false, Ordering::SeqCst);
    let root_gets_before_ctrl = st.root_get_count.load(Ordering::SeqCst);
    let cli_folder = run_cli(vec![
        "mv".into(),
        "item-ctrl-1".into(),
        "--drive".into(),
        "drv-sp".into(),
        "--parent".into(),
        "fld-dest".into(),
        "--json".into(),
    ])
    .await;
    assert!(cli_folder.status.success());
    assert_eq!(
        st.patches.lock().await.last(),
        Some(&(
            "item-ctrl-1".to_string(),
            json!({ "parentReference": { "id": "fld-dest" } })
        ))
    );

    let cli_rename = run_cli(vec![
        "mv".into(),
        "item-ctrl-2".into(),
        "--drive".into(),
        "drv-sp".into(),
        "--name".into(),
        "renamed.md".into(),
        "--json".into(),
    ])
    .await;
    assert!(cli_rename.status.success());
    assert_eq!(
        st.patches.lock().await.last(),
        Some(&(
            "item-ctrl-2".to_string(),
            json!({ "name": "renamed.md" })
        ))
    );
    assert_eq!(
        st.root_get_count.load(Ordering::SeqCst),
        root_gets_before_ctrl,
        "normal folder move and rename-only must not call GET /root"
    );

    server.abort();
}


#[tokio::test]
async fn regression_sharepoint_sp_06_resolve_item_distinguishes_auth_server_errors_from_not_found() {
    use tokio::process::Command;

    #[derive(Clone)]
    struct Sp06State {
        remote_requests: Arc<AtomicUsize>,
        write_requests: Arc<AtomicUsize>,
    }

    let st = Sp06State {
        remote_requests: Arc::new(AtomicUsize::new(0)),
        write_requests: Arc::new(AtomicUsize::new(0)),
    };

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}/v1.0");

    let error_for_key = |key: &str| -> axum::response::Response {
        match key {
            "err-403" | "Secret403" => (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": { "code": "accessDenied", "message": "Access denied to resource" } })),
            )
                .into_response(),
            "err-401" | "Secret401" => (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": { "code": "InvalidAuthenticationToken", "message": "Token expired" } })),
            )
                .into_response(),
            "err-500" | "Secret500" => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": { "code": "generalException", "message": "Upstream storage fault" } })),
            )
                .into_response(),
            _ => (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": { "code": "itemNotFound", "message": "Item not found" } })),
            )
                .into_response(),
        }
    };

    let router = Router::new()
        .route(
            "/v1.0/me/drive",
            get(|State(s): State<Sp06State>| async move {
                s.remote_requests.fetch_add(1, Ordering::SeqCst);
                Json(json!({ "id": "drv-me", "name": "OneDrive", "driveType": "business" }))
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/items/{item_id}",
            get(move |State(s): State<Sp06State>, Path((_drive_id, item_id)): Path<(String, String)>| async move {
                s.remote_requests.fetch_add(1, Ordering::SeqCst);
                error_for_key(&item_id)
            })
            .patch(|State(s): State<Sp06State>| async move {
                s.write_requests.fetch_add(1, Ordering::SeqCst);
                StatusCode::OK
            })
            .delete(|State(s): State<Sp06State>| async move {
                s.write_requests.fetch_add(1, Ordering::SeqCst);
                StatusCode::NO_CONTENT
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/{*tail}",
            get(move |State(s): State<Sp06State>, Path((drive_id, tail)): Path<(String, String)>| async move {
                s.remote_requests.fetch_add(1, Ordering::SeqCst);
                if tail.starts_with("root:/") && tail.ends_with(':') {
                    let rel = tail.trim_start_matches("root:/").trim_end_matches(':');
                    if rel == "bare-file.md" {
                        return (
                            StatusCode::OK,
                            Json(json!({
                                "id": "item-bare-real",
                                "name": "bare-file.md",
                                "size": 15,
                                "file": { "mimeType": "text/markdown" },
                                "parentReference": { "driveId": drive_id, "id": "root", "path": "/drive/root:" }
                            })),
                        )
                            .into_response();
                    }
                    return error_for_key(rel);
                }
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": { "code": "itemNotFound" } })),
                )
                    .into_response()
            }),
        )
        .with_state(st.clone());

    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let profile = tempdir().unwrap();
    let run_cli = |args: Vec<String>| {
        let profile_path = profile.path().to_path_buf();
        let base_url = base_url.clone();
        async move {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_sharepoint"));
            cmd.kill_on_drop(true);
            for (k, _) in std::env::vars_os() {
                let u = k.to_string_lossy().to_ascii_uppercase();
                if ["SHAREPOINT_", "ONEDRIVE_", "TEAMS_", "AZURE_"]
                    .iter()
                    .any(|p| u.starts_with(p))
                {
                    cmd.env_remove(k);
                }
            }
            cmd.env("SHAREPOINT_GRAPH_BASE_URL", &base_url)
                .env("SHAREPOINT_ACCESS_TOKEN", "sp06-test-token")
                .env("SHAREPOINT_DISABLE_KEYRING", "1")
                .env("SHAREPOINT_SHARE_M365_AUTH", "0")
                .env("SHAREPOINT_AUTO_DAEMON", "0")
                .env("SHAREPOINT_CONFIG_DIR", profile_path.join("config"))
                .env("SHAREPOINT_DATA_DIR", profile_path.join("data"))
                .env("NO_PROXY", "127.0.0.1,localhost")
                .args(&args)
                .output()
                .await
                .unwrap()
        }
    };

    let (cfg, graph, storage) = make_graph_and_storage(&base_url);

    // 1. CLI & MCP distinguish 403 accessDenied, 401, 500 vs real 404 on both ID and path lookups
    for (target, expected_status, expected_code) in [
        ("err-403", "403", "accessDenied"),
        ("/Secret403", "403", "accessDenied"),
        ("err-401", "401", "InvalidAuthenticationToken"),
        ("/Secret401", "401", "InvalidAuthenticationToken"),
        ("err-500", "500", "generalException"),
        ("/Secret500", "500", "generalException"),
    ] {
        let cli_out = run_cli(vec![
            "info".into(),
            target.into(),
            "--drive".into(),
            "me".into(),
            "--json".into(),
        ])
        .await;
        assert!(!cli_out.status.success(), "CLI info {target} must fail");
        let stderr = String::from_utf8_lossy(&cli_out.stderr);
        assert!(
            stderr.contains(expected_status) && stderr.contains(expected_code),
            "CLI info {target} must surface {expected_status}/{expected_code}, got: {stderr}"
        );
        assert!(
            !stderr.contains("Drive item not found"),
            "CLI info {target} must not disguise {expected_status} as not found: {stderr}"
        );

        let mcp_req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "get_file_info",
                "arguments": { "drive": "me", "item_id": target }
            }
        });
        let mcp_resp = mcp::handle_rpc_line(&cfg, &graph, &storage, &mcp_req.to_string())
            .await
            .unwrap();
        assert_eq!(mcp_resp["result"]["isError"], true);
        let err_text = mcp_resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            err_text.contains(expected_status) && err_text.contains(expected_code),
            "MCP get_file_info {target} must surface {expected_status}/{expected_code}, got: {err_text}"
        );
        assert!(
            !err_text.contains("Drive item not found"),
            "MCP get_file_info {target} must not mask {expected_status} as not found: {err_text}"
        );
    }

    // Genuine 404 ends with Drive item not found
    for missing in ["err-404", "/Missing404"] {
        let cli_404 = run_cli(vec![
            "info".into(),
            missing.into(),
            "--drive".into(),
            "me".into(),
            "--json".into(),
        ])
        .await;
        assert!(!cli_404.status.success());
        let stderr = String::from_utf8_lossy(&cli_404.stderr);
        assert!(
            stderr.contains("Drive item not found"),
            "genuine 404 for {missing} should report Drive item not found, got: {stderr}"
        );
    }

    // Failed write resolution sends 0 write requests
    let rm_403 = run_cli(vec![
        "rm".into(),
        "err-403".into(),
        "--drive".into(),
        "me".into(),
        "--yes".into(),
        "--json".into(),
    ])
    .await;
    assert!(!rm_403.status.success());
    assert!(String::from_utf8_lossy(&rm_403.stderr).contains("403"));
    assert_eq!(st.write_requests.load(Ordering::SeqCst), 0);

    // 2. Regression control: bare filename gets 404 on /items/{name}, succeeds on /root:/{name}:
    let bare = run_cli(vec![
        "info".into(),
        "bare-file.md".into(),
        "--drive".into(),
        "me".into(),
        "--json".into(),
    ])
    .await;
    assert!(
        bare.status.success(),
        "bare relative filename fallback failed: {}",
        String::from_utf8_lossy(&bare.stderr)
    );
    let bare_json: Value = serde_json::from_slice(&bare.stdout).unwrap();
    assert_eq!(bare_json["item"]["id"], "item-bare-real");

    // 3. Regression control: offline cache miss sends 0 remote requests
    let remote_before_offline = st.remote_requests.load(Ordering::SeqCst);
    let off_miss = run_cli(vec![
        "info".into(),
        "/does-not-exist-offline.md".into(),
        "--drive".into(),
        "me".into(),
        "--offline".into(),
        "--json".into(),
    ])
    .await;
    assert!(!off_miss.status.success());
    assert_eq!(
        st.remote_requests.load(Ordering::SeqCst),
        remote_before_offline,
        "offline cache miss must send 0 remote requests"
    );

    server.abort();
}

