use std::path::Path;

use clap::Parser;
use serde_json::json;

use crate::{
    auth::{AuthManager, StoredTokens},
    config::{self, AppConfig},
    graph::GraphClient,
    storage::Storage,
};

use super::*;

#[test]
fn compact_and_version_limit_require_their_output_options() {
    assert!(Cli::try_parse_from(["sharepoint", "ls", "--compact"]).is_err());
    assert!(Cli::try_parse_from(["sharepoint", "info", "file", "--versions-limit", "2"]).is_err());
    let parsed = Cli::try_parse_from([
        "onedrive", "info", "file", "--json", "--compact", "--versions", "--versions-limit", "2",
    ]).unwrap();
    assert!(matches!(parsed.command, Some(Command::Info {
        json: true, compact: true, versions: true, versions_limit: Some(2), ..
    })));
}

#[test]
fn parses_all_sharepoint_onedrive_cli_subcommands() {
    let c1 = Cli::try_parse_from(["sharepoint", "sites", "--json"]).unwrap();
    assert!(matches!(c1.command, Some(Command::Sites { json: true, .. })));

    let c2 = Cli::try_parse_from(["onedrive", "ls", "/Documents", "--json"]).unwrap();
    assert!(matches!(c2.command, Some(Command::Ls { json: true, .. })));

    let c3 = Cli::try_parse_from(["sp", "download", "item-1", "--dir", "/tmp", "--json"]).unwrap();
    assert!(matches!(c3.command, Some(Command::Download { json: true, .. })));

    let c4 = Cli::try_parse_from(["od", "rm", "item-1", "--yes", "--json"]).unwrap();
    assert!(matches!(c4.command, Some(Command::Rm { yes: true, json: true, .. })));

    let c5 = Cli::try_parse_from(["sharepoint", "serve"]).unwrap();
    assert!(matches!(
        c5.command,
        Some(Command::Serve {
            port: config::DEFAULT_SERVE_PORT,
            ..
        })
    ));
}

#[test]
fn parses_hyphen_filenames_search_and_negative_json_for_both_launchers() {
    for bin in ["sharepoint", "bitefoundry-onedrive"] {
        for sample in ["-report.md", "- 목록 항목", "- first\n- second"] {
            let mv = parse_cli_from([bin, "mv", "item-1", "--name", sample, "--json"], bin)
                .unwrap();
            assert!(matches!(
                mv.command,
                Some(Command::Mv {
                    ref item_or_path,
                    ref name,
                    json: true,
                    ..
                }) if item_or_path == "item-1" && name.as_deref() == Some(sample)
            ));

            for upload_cmd in ["upload", "up"] {
                let up = parse_cli_from(
                    [bin, upload_cmd, "test.txt", "--name", sample, "--json"],
                    bin,
                )
                .unwrap();
                assert!(matches!(
                    up.command,
                    Some(Command::Upload {
                        ref local_path,
                        ref name,
                        json: true,
                        ..
                    }) if local_path == Path::new("test.txt") && name.as_deref() == Some(sample)
                ));
            }

            let mk = parse_cli_from(
                [bin, "mkdir", sample, "--parent", "/Archive", "--json"],
                bin,
            )
            .unwrap();
            assert!(matches!(
                mk.command,
                Some(Command::Mkdir {
                    ref name,
                    ref parent,
                    json: true,
                    ..
                }) if name == sample && parent.as_deref() == Some("/Archive")
            ));

            let sr = parse_cli_from(
                [bin, "search", sample, "--remote", "--top", "15", "--json"],
                bin,
            )
            .unwrap();
            assert!(matches!(
                sr.command,
                Some(Command::Search {
                    ref query,
                    remote: true,
                    limit: 15,
                    json: true,
                    ..
                }) if query == sample
            ));

            let inf = parse_cli_from([bin, "info", sample, "--drive", "d1", "--json"], bin)
                .unwrap();
            assert!(matches!(
                inf.command,
                Some(Command::Info {
                    ref item_or_path,
                    json: true,
                    ..
                }) if item_or_path == sample
            ));

            let ct = parse_cli_from([bin, "cat", sample, "--max-bytes", "64", "--json"], bin)
                .unwrap();
            assert!(matches!(
                ct.command,
                Some(Command::Cat {
                    ref item_or_path,
                    max_bytes: 64,
                    json: true,
                    ..
                }) if item_or_path == sample
            ));

            let dl = parse_cli_from([bin, "dl", sample, "--dir", "out", "--json"], bin)
                .unwrap();
            assert!(matches!(
                dl.command,
                Some(Command::Download {
                    ref item_or_path,
                    json: true,
                    ..
                }) if item_or_path == sample
            ));

            let rm = parse_cli_from([bin, "rm", sample, "--yes", "--json"], bin).unwrap();
            assert!(matches!(
                rm.command,
                Some(Command::Rm {
                    ref item_or_path,
                    yes: true,
                    json: true,
                    ..
                }) if item_or_path == sample
            ));
        }

        // Control case: lists set-field --value-json -4 keeps JSON number -4.
        let list_num = parse_cli_from(
            [
                bin,
                "lists",
                "set-field",
                "--site-id",
                "site",
                "--list-id",
                "list",
                "--item-id",
                "item",
                "--field",
                "score",
                "--value-json",
                "-4",
                "--json",
            ],
            bin,
        )
        .unwrap();
        assert!(matches!(
            list_num.command,
            Some(Command::Lists {
                json: true,
                action: ListAction::SetField { ref value_json, .. },
            }) if value_json == &json!(-4)
        ));
    }
}

#[test]
fn preserves_equals_positional_separator_known_flags_missing_values_and_help() {
    use clap::error::ErrorKind;

    // Equals syntax accepts explicit flag-looking text.
    for literal in ["-report.md", "--json", "--dry-run", "--help", "-h"] {
        let eq = format!("--name={literal}");
        let mv = parse_cli_from(["sharepoint", "mv", "id", &eq], "sharepoint").unwrap();
        assert!(matches!(
            mv.command,
            Some(Command::Mv { ref name, json: false, .. }) if name.as_deref() == Some(literal)
        ));
        let up =
            parse_cli_from(["sharepoint", "upload", "test.txt", &eq], "sharepoint").unwrap();
        assert!(matches!(
            up.command,
            Some(Command::Upload { ref name, json: false, .. }) if name.as_deref() == Some(literal)
        ));
    }

    // Positional "--" accepts explicit flag-looking text.
    for literal in ["-report.md", "- 목록 항목", "--json", "--dry-run", "--help", "-h"] {
        let mk =
            parse_cli_from(["sharepoint", "mkdir", "--", literal], "sharepoint").unwrap();
        assert!(matches!(
            mk.command,
            Some(Command::Mkdir { ref name, json: false, .. }) if name == literal
        ));
        let sr =
            parse_cli_from(["sharepoint", "search", "--", literal], "sharepoint").unwrap();
        assert!(matches!(
            sr.command,
            Some(Command::Search { ref query, json: false, .. }) if query == literal
        ));
    }

    // Known flags and unknown long flags must NOT be swallowed as separate values.
    for bad_args in [
        vec!["sharepoint", "mv", "id", "--name"],
        vec!["sharepoint", "mv", "id", "--name", "--json"],
        vec!["sharepoint", "mv", "id", "--name", "--parent", "/Archive"],
        vec!["sharepoint", "mv", "id", "--name", "--dry-run"],
        vec!["sharepoint", "mv", "--parent", "--name", "-report.md", "id"],
        vec!["sharepoint", "mv", "id", "--parent", "-archive"],
        vec!["sharepoint", "upload", "test.txt", "--name"],
        vec!["sharepoint", "upload", "test.txt", "--name", "--json"],
        vec!["sharepoint", "upload", "test.txt", "--name", "--conflict", "replace"],
        vec!["sharepoint", "upload", "test.txt", "--name", "--dry-run"],
        vec!["sharepoint", "upload", "test.txt", "--conflict", "-bad"],
        vec!["sharepoint", "mkdir", "--json"],
        vec!["sharepoint", "mkdir", "--dry-run"],
        vec!["sharepoint", "mkdir", "-report.md", "--dry-run"],
        vec!["sharepoint", "mkdir", "--parent", "-bad", "GoodName"],
        vec!["sharepoint", "search", "--json"],
        vec!["sharepoint", "search", "--remote"],
        vec!["sharepoint", "search", "--dry-run"],
        vec!["sharepoint", "search", "--limit", "-1", "query"],
        vec![
            "sharepoint",
            "lists",
            "set-field",
            "--site-id",
            "s",
            "--list-id",
            "l",
            "--item-id",
            "i",
            "--field",
            "f",
            "--value-json",
            "--json",
        ],
    ] {
        let err = parse_cli_from(bad_args.clone(), "sharepoint")
            .expect_err(&format!("expected parse failure for {bad_args:?}"));
        assert_ne!(
            err.kind(),
            ErrorKind::DisplayHelp,
            "unexpected help exit for {bad_args:?}: {err}"
        );
    }

    // Missing value followed by -h / --help must never parse as a literal filename.
    for help_after_name in [
        vec!["sharepoint", "mv", "id", "--name", "--help"],
        vec!["sharepoint", "mv", "id", "--name", "-h"],
        vec!["sharepoint", "upload", "test.txt", "--name", "--help"],
        vec!["sharepoint", "upload", "test.txt", "--name", "-h"],
    ] {
        assert!(
            parse_cli_from(help_after_name.clone(), "sharepoint").is_err(),
            "must not swallow help flag in {help_after_name:?}"
        );
    }

    // Actual help invocations (standalone and trailing --help after hyphen values).
    for help_args in [
        vec!["sharepoint", "mv", "--help"],
        vec!["sharepoint", "mv", "-h"],
        vec!["sharepoint", "mv", "id", "--name", "-report.md", "--help"],
        vec!["sharepoint", "upload", "--help"],
        vec!["sharepoint", "upload", "-h"],
        vec!["sharepoint", "upload", "test.txt", "--name", "-report.md", "--help"],
        vec!["sharepoint", "mkdir", "--help"],
        vec!["sharepoint", "mkdir", "-h"],
        vec!["sharepoint", "mkdir", "-report.md", "--help"],
        vec!["sharepoint", "search", "--help"],
        vec!["sharepoint", "search", "-h"],
        vec!["sharepoint", "search", "- 목록 항목", "--help"],
        vec![
            "sharepoint",
            "lists",
            "set-field",
            "--site-id",
            "site",
            "--list-id",
            "list",
            "--item-id",
            "item",
            "--field",
            "score",
            "--value-json",
            "-4",
            "--help",
        ],
    ] {
        let err = parse_cli_from(help_args.clone(), "sharepoint")
            .expect_err(&format!("expected DisplayHelp for {help_args:?}"));
        assert_eq!(
            err.kind(),
            ErrorKind::DisplayHelp,
            "expected DisplayHelp for {help_args:?}, got: {err}"
        );
    }
}

#[tokio::test]
async fn fetch_sites_discover_drives_and_build_tree_run_concurrently() {
    use axum::{extract::Path as AxumPath, routing::get, Json, Router};
    use std::sync::Arc;
    use tokio::{net::TcpListener, sync::Barrier};

    // 1. fetch_sites overlaps /me/followedSites and /sites?search=*
    let sites_barrier = Arc::new(Barrier::new(2));
    let sb1 = Arc::clone(&sites_barrier);
    let sb2 = Arc::clone(&sites_barrier);
    let sites_router = Router::new()
        .route(
            "/v1.0/me/followedSites",
            get(move || {
                let b = Arc::clone(&sb1);
                async move {
                    b.wait().await;
                    Json(json!({
                        "value": [{ "id": "site-a", "name": "alpha", "displayName": "Alpha" }]
                    }))
                }
            }),
        )
        .route(
            "/v1.0/sites",
            get(move || {
                let b = Arc::clone(&sb2);
                async move {
                    b.wait().await;
                    Json(json!({
                        "value": [{ "id": "site-b", "name": "beta", "displayName": "Beta" }]
                    }))
                }
            }),
        );
    let listener1 = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base1 = format!("http://{}/v1.0", listener1.local_addr().unwrap());
    let srv1 = tokio::spawn(async move { axum::serve(listener1, sites_router).await.unwrap() });

    let make_graph = |base_url: String| {
        let cfg = AppConfig {
            graph_base_url: base_url,
            ..AppConfig::default()
        };
        let auth = AuthManager::from_tokens(
            cfg.clone(),
            StoredTokens {
                access_token: "test-token".into(),
                refresh_token: None,
                expires_at: chrono::Utc::now().timestamp() + 3600,
                client_id: "cid".into(),
                tenant_id: "tid".into(),
                user_id: Some("u1".into()),
                user_display_name: None,
                user_email: None,
                auth_mode: "test".into(),
            },
        );
        GraphClient::new(cfg, auth).unwrap()
    };

    let tmp = tempfile::tempdir().unwrap();
    let storage = Storage::open(&tmp.path().join("cli-conc.db")).unwrap();
    let g1 = make_graph(base1);

    let sites = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        fetch_sites(&g1, &storage, None, 20, true, false, false),
    )
    .await
    .expect("fetch_sites must overlap followedSites and sites discovery")
    .unwrap();
    assert_eq!(sites.len(), 2);
    srv1.abort();

    // 2. discover_drives overlaps /me/drive with fetch_sites, and build_tree overlaps sibling folders
    let disc_barrier = Arc::new(Barrier::new(3));
    let db1 = Arc::clone(&disc_barrier);
    let db2 = Arc::clone(&disc_barrier);
    let db3 = Arc::clone(&disc_barrier);
    let tree_barrier = Arc::new(Barrier::new(2));
    let tb = Arc::clone(&tree_barrier);

    let router2 = Router::new()
        .route(
            "/v1.0/me/drive",
            get(move || {
                let b = Arc::clone(&db1);
                async move {
                    b.wait().await;
                    Json(json!({ "id": "drv-me", "name": "OneDrive", "driveType": "business" }))
                }
            }),
        )
        .route(
            "/v1.0/me/followedSites",
            get(move || {
                let b = Arc::clone(&db2);
                async move {
                    b.wait().await;
                    Json(json!({
                        "value": [{ "id": "site-1", "name": "eng", "displayName": "Engineering" }]
                    }))
                }
            }),
        )
        .route(
            "/v1.0/sites",
            get(move || {
                let b = Arc::clone(&db3);
                async move {
                    b.wait().await;
                    Json(json!({ "value": [] }))
                }
            }),
        )
        .route(
            "/v1.0/sites/site-1/drives",
            get(|| async {
                Json(json!({
                    "value": [{ "id": "drv-eng", "name": "Documents", "driveType": "documentLibrary" }]
                }))
            }),
        )
        .route(
            "/v1.0/drives/drv-eng/root/children",
            get(|| async {
                Json(json!({
                    "value": [
                        { "id": "fld-a", "name": "Alpha", "folder": { "childCount": 1 } },
                        { "id": "fld-b", "name": "Beta", "folder": { "childCount": 1 } }
                    ]
                }))
            }),
        )
        .route(
            "/v1.0/drives/drv-eng/items/{item_id}/children",
            get(move |AxumPath(item_id): AxumPath<String>| {
                let b = Arc::clone(&tb);
                async move {
                    b.wait().await;
                    Json(json!({
                        "value": [{
                            "id": format!("file-in-{item_id}"),
                            "name": format!("{item_id}.txt"),
                            "size": 10,
                            "file": { "mimeType": "text/plain" }
                        }]
                    }))
                }
            }),
        );
    let listener2 = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base2 = format!("http://{}/v1.0", listener2.local_addr().unwrap());
    let srv2 = tokio::spawn(async move { axum::serve(listener2, router2).await.unwrap() });
    let g2 = make_graph(base2);

    let discovered = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        discover_drives(&g2, &storage, None, true, false, false),
    )
    .await
    .expect("discover_drives must overlap get_my_drive with fetch_sites")
    .unwrap();
    assert_eq!(discovered.drives.len(), 2);

    let (nodes, truncated) = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        build_tree(&g2, &storage, "drv-eng", None, 2),
    )
    .await
    .expect("build_tree must list sibling subfolders concurrently")
    .unwrap();
    assert!(!truncated);
    assert_eq!(nodes.len(), 2);
    assert_eq!(nodes[0].item.id, "fld-a");
    assert_eq!(nodes[0].children[0].item.id, "file-in-fld-a");
    assert_eq!(nodes[1].item.id, "fld-b");
    assert_eq!(nodes[1].children[0].item.id, "file-in-fld-b");

    srv2.abort();
}

#[tokio::test]
async fn build_tree_descends_shortcuts_with_effective_ids_and_marks_depth_cut_folders() {
    use axum::{http::StatusCode as AxumStatus, routing::get, Json, Router};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::net::TcpListener;

    // My drive holds a shortcut to a folder shared out of another drive (`remoteItem`).
    // Its children live at /drives/drv-remote/items/remote-fld; the shortcut's own id in
    // drv-me lists nothing the user can read.
    let wrong_calls = Arc::new(AtomicUsize::new(0));
    let wc = Arc::clone(&wrong_calls);
    let router = Router::new()
        .route(
            "/v1.0/drives/drv-me/root/children",
            get(|| async {
                Json(json!({
                    "value": [{
                        "id": "shortcut-1",
                        "name": "Team Share",
                        "parentReference": { "driveId": "drv-me", "id": "root-me" },
                        "remoteItem": {
                            "id": "remote-fld",
                            "folder": { "childCount": 2 },
                            "parentReference": { "driveId": "drv-remote" }
                        }
                    }]
                }))
            }),
        )
        .route(
            "/v1.0/drives/drv-me/items/shortcut-1/children",
            get(move || {
                let wc = Arc::clone(&wc);
                async move {
                    wc.fetch_add(1, Ordering::SeqCst);
                    (
                        AxumStatus::NOT_FOUND,
                        Json(json!({ "error": { "code": "itemNotFound", "message": "not here" } })),
                    )
                }
            }),
        )
        .route(
            "/v1.0/drives/drv-remote/items/remote-fld/children",
            get(|| async {
                Json(json!({
                    "value": [
                        {
                            "id": "deep-fld",
                            "name": "Deep",
                            "folder": { "childCount": 5 },
                            "parentReference": { "driveId": "drv-remote", "id": "remote-fld" }
                        },
                        {
                            "id": "plan-1",
                            "name": "plan.md",
                            "size": 3,
                            "file": { "mimeType": "text/markdown" },
                            "parentReference": { "driveId": "drv-remote", "id": "remote-fld" }
                        }
                    ]
                }))
            }),
        );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1.0", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let cfg = AppConfig {
        graph_base_url: base,
        ..AppConfig::default()
    };
    let auth = AuthManager::from_tokens(
        cfg.clone(),
        StoredTokens {
            access_token: "test-token".into(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: "cid".into(),
            tenant_id: "tid".into(),
            user_id: Some("u1".into()),
            user_display_name: None,
            user_email: None,
            auth_mode: "test".into(),
        },
    );
    let graph = GraphClient::new(cfg, auth).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let storage = Storage::open(&tmp.path().join("cli-tree-remote.db")).unwrap();

    let (nodes, truncated) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        build_tree(&graph, &storage, "drv-me", None, 2),
    )
    .await
    .expect("build_tree must not hang")
    .expect("a shortcut folder must be listed through its remote drive and item ids");
    assert!(!truncated);
    assert_eq!(wrong_calls.load(Ordering::SeqCst), 0, "the shortcut's local id was listed");

    assert_eq!(nodes.len(), 1);
    let share = &nodes[0];
    assert_eq!(share.item.effective_drive_id(), "drv-remote");
    assert_eq!(share.item.effective_item_id(), "remote-fld");
    assert_eq!(share.has_more, Some(false));
    let child_ids: Vec<&str> = share.children.iter().map(|n| n.item.id.as_str()).collect();
    assert_eq!(child_ids, ["deep-fld", "plan-1"]);
    let deep = &share.children[0];
    assert!(deep.children.is_empty(), "Deep sits past --depth 2");
    assert_eq!(deep.has_more, Some(true));

    let mut lines = Vec::new();
    tree_lines(&nodes, "", &mut lines);
    assert_eq!(
        lines,
        [
            "└── Team Share/",
            "    ├── Deep/ …",
            "    └── plan.md",
        ],
        "a folder cut by --depth that still holds items ends in …"
    );

    server.abort();
}
