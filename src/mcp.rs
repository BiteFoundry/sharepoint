//! Model Context Protocol (MCP) server over stdio (newline-delimited JSON-RPC 2.0)
//! exposing OneDrive & SharePoint agent tools.

use std::{io::Write, path::PathBuf};

use anyhow::Result;
use base64::{engine::general_purpose::STANDARD as B64_STANDARD, Engine as _};
use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};

use crate::{
    auth::diagnose_access_token_scopes,
    cli,
    config::{AppConfig, Persona},
    graph::{
        drives::{explicit_if_match, ConflictBehavior, UploadOptions},
        media::decode_text_file,
        GraphClient,
    },
    resolver,
    storage::Storage,
};

const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];
const LATEST_PROTOCOL_VERSION: &str = "2025-06-18";
const SERVER_NAME: &str = "sharepoint";
const MAX_RPC_LINE_BYTES: usize = 8 * 1024 * 1024;

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;

enum BoundedLine {
    Eof,
    Line(String),
    TooLong,
}

async fn read_line_bounded<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
) -> std::io::Result<BoundedLine> {
    let mut buf = Vec::new();
    let mut oversized = false;

    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if oversized {
                return Ok(BoundedLine::TooLong);
            }
            if buf.is_empty() {
                return Ok(BoundedLine::Eof);
            }
            let s = String::from_utf8(buf)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            return Ok(BoundedLine::Line(s));
        }

        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            if !oversized {
                if buf.len() + pos > max_bytes {
                    oversized = true;
                    buf.clear();
                } else {
                    buf.extend_from_slice(&available[..pos]);
                }
            }
            reader.consume(pos + 1);
            if oversized {
                return Ok(BoundedLine::TooLong);
            }
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            let s = String::from_utf8(buf)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            return Ok(BoundedLine::Line(s));
        } else {
            if !oversized {
                if buf.len() + available.len() > max_bytes {
                    oversized = true;
                    buf.clear();
                } else {
                    buf.extend_from_slice(available);
                }
            }
            let len = available.len();
            reader.consume(len);
        }
    }
}

pub async fn run(config: AppConfig, graph: GraphClient, storage: Storage) -> Result<()> {
    eprintln!(
        "{SERVER_NAME} MCP server ready (protocol {LATEST_PROTOCOL_VERSION}, user_id={})",
        graph.my_user_id().unwrap_or("unknown")
    );

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut stdout = std::io::stdout();

    loop {
        match read_line_bounded(&mut reader, MAX_RPC_LINE_BYTES).await? {
            BoundedLine::Eof => break,
            BoundedLine::TooLong => {
                let err = error_response(
                    Value::Null,
                    INVALID_REQUEST,
                    format!(
                        "invalid request: line exceeds {} MiB limit",
                        MAX_RPC_LINE_BYTES / (1024 * 1024)
                    ),
                );
                stdout.write_all(encode_message(&err).as_bytes())?;
                stdout.flush()?;
            }
            BoundedLine::Line(line) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Some(response) = handle_rpc_line(&config, &graph, &storage, trimmed).await {
                    stdout.write_all(encode_message(&response).as_bytes())?;
                    stdout.flush()?;
                }
            }
        }
    }

    eprintln!("{SERVER_NAME} MCP server: stdin closed, shutting down");
    Ok(())
}

pub async fn handle_rpc_line(
    config: &AppConfig,
    graph: &GraphClient,
    storage: &Storage,
    line: &str,
) -> Option<Value> {
    let message = match parse_message(line) {
        Ok(msg) => msg,
        Err(err_resp) => return Some(err_resp),
    };

    let method = message
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let id = message.get("id").cloned().unwrap_or(Value::Null);
    let has_id = message.get("id").is_some();

    if method.starts_with("notifications/") || !has_id {
        return None;
    }

    match method {
        "initialize" => Some(success_response(id, initialize_result(&message))),
        "ping" => Some(success_response(id, json!({}))),
        "tools/list" => Some(success_response(id, json!({ "tools": tool_catalog() }))),
        "tools/call" => {
            let name = message
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let arguments = message
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));

            let outcome = call_tool(config, graph, storage, name, &arguments).await;
            Some(success_response(id, tool_result(outcome)))
        }
        _ => Some(error_response(
            id,
            METHOD_NOT_FOUND,
            format!("method not found: {method}"),
        )),
    }
}

async fn map_graph_err<T>(graph: &GraphClient, res: Result<T>) -> Result<T, String> {
    match res {
        Ok(val) => Ok(val),
        Err(err) => Err(format!(
            "{:#}",
            cli::wrap_graph_permission_error(graph, err).await
        )),
    }
}

async fn call_tool(
    config: &AppConfig,
    graph: &GraphClient,
    storage: &Storage,
    name: &str,
    args: &Value,
) -> Result<Value, String> {
    let str_arg = |k: &str| -> Option<String> {
        args.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let bool_arg = |k: &str| -> bool { args.get(k).and_then(Value::as_bool).unwrap_or(false) };
    let usize_arg = |k: &str, def: usize| -> Result<usize, String> {
        match args.get(k) {
            None | Some(Value::Null) => Ok(def),
            Some(v) => v
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| format!("{k} must be a non-negative integer")),
        }
    };
    let persona = Persona::detect();

    match name {
        "whoami" => {
            let tokens = graph.auth().current_tokens().await;
            let source = cli::auth_source_label(tokens.as_ref(), config);
            let Some(tok) = tokens else {
                return Ok(json!({ "authenticated": false, "persona": persona.as_str(), "auth_source": source }));
            };
            let diag = diagnose_access_token_scopes(&tok.access_token);
            Ok(json!({
                "authenticated": true,
                "persona": persona.as_str(),
                "user_id": tok.user_id,
                "display_name": tok.user_display_name,
                "email": tok.user_email,
                "auth_mode": tok.auth_mode,
                "auth_source": source,
                "scope_diagnostics": diag,
            }))
        }
        "list_sites" => {
            let search = str_arg("search");
            let limit = usize_arg("limit", 50)?;
            let offline = bool_arg("offline");
            let sites = map_graph_err(
                graph,
                cli::fetch_sites(graph, storage, search.as_deref(), limit, false, offline, false).await,
            )
            .await?;
            Ok(json!({ "sites": sites }))
        }
        "list_drives" => {
            let site = str_arg("site");
            let offline = bool_arg("offline");
            let found = map_graph_err(
                graph,
                cli::discover_drives(graph, storage, site.as_deref(), false, offline, false).await,
            )
            .await?;
            Ok(json!({
                "drives": found.drives,
                "returned": found.drives.len(),
                "sites_scanned": found.sites_scanned,
                "sites_failed": found.sites_failed,
                "has_more": found.has_more,
            }))
        }
        "list_lists" => {
            let site_id = str_arg("site_id").ok_or_else(|| "site_id is required".to_string())?;
            let lists = map_graph_err(graph, graph.list_lists(&site_id).await).await?;
            Ok(json!({
                "lists": lists,
                "returned": lists.len(),
            }))
        }
        "list_items" | "list_columns" | "update_list_item_field" => {
            let site_id = str_arg("site_id").ok_or_else(|| "site_id is required".to_string())?;
            let list_id = str_arg("list_id").ok_or_else(|| "list_id is required".to_string())?;
            match name {
                "list_items" => {
                    let fields = match args.get("fields") {
                        Some(fields) => serde_json::from_value::<Vec<String>>(fields.clone())
                            .map_err(|_| {
                                "fields must be an array of internal column names".to_string()
                            })?,
                        None => Vec::new(),
                    };
                    let cursor = match args.get("cursor") {
                        None => None,
                        Some(Value::String(cursor)) => Some(cursor.as_str()),
                        Some(_) => return Err("cursor must be a string".to_string()),
                    };
                    let limit = match args.get("limit") {
                        None => 50,
                        Some(_) if cursor.is_some() => {
                            return Err(
                                "cursor and limit cannot be combined; a cursor keeps the first page's size"
                                    .to_string(),
                            )
                        }
                        Some(value) => match value.as_u64() {
                            Some(limit @ 1..=200) => limit as usize,
                            _ => return Err("limit must be an integer from 1 to 200".to_string()),
                        },
                    };
                    let page = map_graph_err(
                        graph,
                        graph
                            .list_items_page(&site_id, &list_id, &fields, limit, cursor)
                            .await,
                    )
                    .await?;
                    Ok(json!({
                        "items": page.items,
                        "returned": page.items.len(),
                        "has_more": page.next_cursor.is_some(),
                        "next_cursor": page.next_cursor,
                    }))
                }
                "list_columns" => {
                    let columns =
                        map_graph_err(graph, graph.list_columns(&site_id, &list_id).await).await?;
                    Ok(json!({ "columns": columns }))
                }
                _ => {
                    let item_id =
                        str_arg("item_id").ok_or_else(|| "item_id is required".to_string())?;
                    let field = str_arg("field").ok_or_else(|| "field is required".to_string())?;
                    let value = args
                        .get("value")
                        .cloned()
                        .ok_or_else(|| "value is required".to_string())?;
                    let if_match = args
                        .get("if_match")
                        .map(|tag| {
                            tag.as_str()
                                .ok_or_else(|| "if_match must be an ETag string".to_string())
                        })
                        .transpose()?;
                    let update = map_graph_err(
                        graph,
                        graph
                            .update_list_item_field(
                                &site_id, &list_id, &item_id, &field, value, if_match,
                            )
                            .await,
                    )
                    .await?;
                    Ok(json!(update))
                }
            }
        }
        "list_files" => {
            let drive_q = str_arg("drive");
            let site_q = str_arg("site");
            let target = str_arg("parent_id").or_else(|| str_arg("path"));
            let limit = usize_arg("limit", 100)?;
            let offline = bool_arg("offline");
            let drive = map_graph_err(
                graph,
                resolver::resolve_target_drive(
                    graph,
                    storage,
                    drive_q.as_deref(),
                    site_q.as_deref(),
                    persona,
                    offline,
                )
                .await,
            )
            .await?;
            let listing = map_graph_err(
                graph,
                cli::fetch_folder_listing(
                    graph,
                    storage,
                    &drive.id,
                    target.as_deref(),
                    limit,
                    false,
                    offline,
                    false,
                )
                .await,
            )
            .await?;
            Ok(cli::folder_listing_json(&drive, &listing, bool_arg("compact")))
        }
        "get_file_info" => {
            let item_q = str_arg("item_id")
                .or_else(|| str_arg("path"))
                .ok_or_else(|| "item_id or path is required".to_string())?;
            let drive_q = str_arg("drive");
            let site_q = str_arg("site");
            let offline = bool_arg("offline");
            let drive = map_graph_err(
                graph,
                resolver::resolve_target_drive(
                    graph,
                    storage,
                    drive_q.as_deref(),
                    site_q.as_deref(),
                    persona,
                    offline,
                )
                .await,
            )
            .await?;
            let item = map_graph_err(
                graph,
                resolver::resolve_item(graph, storage, &drive.id, &item_q, offline).await,
            )
            .await?;
            let compact = match args.get("compact") {
                None => true,
                Some(Value::Bool(flag)) => *flag,
                Some(_) => return Err("compact must be a boolean".to_string()),
            };
            Ok(cli::file_info_json(&item, !compact, cli::DetailHint::Mcp, false))
        }
        "read_text_file" => {
            let item_q = str_arg("item_id")
                .or_else(|| str_arg("path"))
                .ok_or_else(|| "item_id or path is required".to_string())?;
            let drive_q = str_arg("drive");
            let site_q = str_arg("site");
            let max_bytes = usize_arg("max_bytes", cli::DEFAULT_TEXT_PREVIEW_MAX_BYTES)?;
            let drive = map_graph_err(
                graph,
                resolver::resolve_target_drive(
                    graph,
                    storage,
                    drive_q.as_deref(),
                    site_q.as_deref(),
                    persona,
                    false,
                )
                .await,
            )
            .await?;
            let item = map_graph_err(
                graph,
                resolver::resolve_item(graph, storage, &drive.id, &item_q, false).await,
            )
            .await?;
            if item.is_folder {
                return Err(format!("'{}' is a folder, not a file.", item.name));
            }
            let bytes = map_graph_err(
                graph,
                graph
                    .download_item_bytes(
                        item.effective_drive_id(),
                        item.effective_item_id(),
                        item.download_url.as_deref(),
                        max_bytes,
                    )
                    .await,
            )
            .await?;
            Ok(json!({
                "ok": true,
                "id": item.id,
                "drive_id": item.drive_id,
                "name": item.name,
                "mime_type": item.mime_type,
                "e_tag": item.e_tag,
                "c_tag": item.c_tag,
                "size_bytes": bytes.len(),
                "content": decode_text_file(&bytes).map_err(|err| format!("{err:#}"))?,
            }))
        }
        "download_file" => {
            let item_q = str_arg("item_id")
                .or_else(|| str_arg("path"))
                .ok_or_else(|| "item_id or path is required".to_string())?;
            let drive_q = str_arg("drive");
            let site_q = str_arg("site");
            let dir = str_arg("dir").or_else(|| str_arg("out_dir"));
            let drive = map_graph_err(
                graph,
                resolver::resolve_target_drive(
                    graph,
                    storage,
                    drive_q.as_deref(),
                    site_q.as_deref(),
                    persona,
                    false,
                )
                .await,
            )
            .await?;
            let item = map_graph_err(
                graph,
                resolver::resolve_item(graph, storage, &drive.id, &item_q, false).await,
            )
            .await?;
            let dest_dir = cli::normalize_download_dir(config, dir.as_deref());
            let mut downloaded = map_graph_err(
                graph,
                graph
                    .download_item_to_file(
                        item.effective_drive_id(),
                        item.effective_item_id(),
                        Some(&item.name),
                        item.mime_type.as_deref(),
                        item.download_url.as_deref(),
                        &dest_dir,
                    )
                    .await,
            )
            .await?;
            cli::record_download_baseline(storage, &item, &mut downloaded);
            serde_json::to_value(downloaded).map_err(|e| e.to_string())
        }
        "upload_file" => {
            let drive_q = str_arg("drive");
            let site_q = str_arg("site");
            let parent_id = str_arg("parent_id");
            let file_name = str_arg("file_name");
            let target_item_id = str_arg("item_id");
            let if_match = match args.get("if_match") {
                None | Some(Value::Null) => None,
                Some(Value::String(raw)) => {
                    explicit_if_match(Some(raw.as_str())).map_err(|e| e.to_string())?
                }
                Some(_) => return Err("if_match must be a string ETag".to_string()),
            };
            let explicit_conflict = match str_arg("conflict_behavior").or_else(|| str_arg("conflict")) {
                Some(raw) => Some(ConflictBehavior::parse_str(&raw).map_err(|e| e.to_string())?),
                None => None,
            };
            let drive = map_graph_err(
                graph,
                resolver::resolve_target_drive(
                    graph,
                    storage,
                    drive_q.as_deref(),
                    site_q.as_deref(),
                    persona,
                    false,
                )
                .await,
            )
            .await?;
            let (resolved_parent_id, resolved_parent_path) = match parent_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty() && *s != "/" && *s != "root")
            {
                Some(p) if p.starts_with('/') || p.contains('/') => {
                    let folder = map_graph_err(
                        graph,
                        resolver::resolve_item_for_write(graph, storage, &drive.id, p).await,
                    )
                    .await?;
                    (Some(folder.id.clone()), Some(folder.display_path()))
                }
                Some(pid) => (Some(pid.to_string()), None),
                None => (None, Some(String::new())),
            };

            let uploaded = if let Some(lp) = str_arg("local_path") {
                let path = PathBuf::from(lp);
                let mut opts = cli::resolve_upload_options_for_local_file(
                    storage,
                    &drive.id,
                    resolved_parent_id.as_deref(),
                    resolved_parent_path.as_deref(),
                    file_name.as_deref(),
                    &path,
                    if_match.as_deref(),
                    explicit_conflict,
                );
                if let Some(explicit_id) = target_item_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    if if_match
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .is_none()
                        && opts.target_item_id.as_deref() != Some(explicit_id)
                    {
                        opts.if_match = None;
                        opts.expected_c_tag = None;
                    }
                    opts.target_item_id = Some(explicit_id.to_string());
                }
                let item = map_graph_err(
                    graph,
                    graph
                        .upload_local_file_with_options(
                            &drive.id,
                            resolved_parent_id.as_deref(),
                            file_name.as_deref(),
                            &path,
                            &opts,
                        )
                        .await,
                )
                .await?;
                cli::refresh_download_baseline_after_upload(storage, &path, &item, &opts);
                item
            } else if let Some(b64) = str_arg("content_base64") {
                let name = file_name.ok_or_else(|| "file_name is required with content_base64".to_string())?;
                let bytes = B64_STANDARD
                    .decode(b64.trim())
                    .map_err(|e| format!("Invalid base64: {e}"))?;
                let opts = UploadOptions {
                    conflict_behavior: explicit_conflict,
                    if_match,
                    expected_c_tag: None,
                    target_item_id,
                };
                map_graph_err(
                    graph,
                    graph
                        .upload_bytes_with_options(
                            &drive.id,
                            resolved_parent_id.as_deref(),
                            &name,
                            bytes,
                            None,
                            &opts,
                        )
                        .await,
                )
                .await?
            } else if let Some(text) = args.get("content_text").and_then(Value::as_str) {
                let name = file_name.ok_or_else(|| "file_name is required with content_text".to_string())?;
                let opts = UploadOptions {
                    conflict_behavior: explicit_conflict,
                    if_match,
                    expected_c_tag: None,
                    target_item_id,
                };
                map_graph_err(
                    graph,
                    graph
                        .upload_bytes_with_options(
                            &drive.id,
                            resolved_parent_id.as_deref(),
                            &name,
                            text.as_bytes().to_vec(),
                            Some("text/plain; charset=utf-8"),
                            &opts,
                        )
                        .await,
                )
                .await?
            } else {
                return Err("Provide local_path, content_base64, or content_text".to_string());
            };
            let _ = storage.upsert_drive_items(std::slice::from_ref(&uploaded));
            Ok(json!({ "item": uploaded }))
        }
        "create_folder" => {
            let name = str_arg("name").ok_or_else(|| "name is required".to_string())?;
            let drive_q = str_arg("drive");
            let site_q = str_arg("site");
            let parent_id = str_arg("parent_id");
            let drive = map_graph_err(
                graph,
                resolver::resolve_target_drive(
                    graph,
                    storage,
                    drive_q.as_deref(),
                    site_q.as_deref(),
                    persona,
                    false,
                )
                .await,
            )
            .await?;
            let resolved_parent_id = match parent_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty() && *s != "/" && !s.eq_ignore_ascii_case("root"))
            {
                Some(p) if p.starts_with('/') || p.contains('/') => {
                    let folder = map_graph_err(
                        graph,
                        resolver::resolve_item_for_write(graph, storage, &drive.id, p).await,
                    )
                    .await?;
                    Some(folder.id)
                }
                Some(pid) => Some(pid.to_string()),
                None => None,
            };
            let folder = map_graph_err(
                graph,
                graph
                    .create_folder(&drive.id, resolved_parent_id.as_deref(), &name)
                    .await,
            )
            .await?;
            let _ = storage.upsert_drive_items(std::slice::from_ref(&folder));
            Ok(json!({ "item": folder }))
        }
        "move_or_rename_item" => {
            let item_q = str_arg("item_id")
                .or_else(|| str_arg("path"))
                .ok_or_else(|| "item_id or path is required".to_string())?;
            let new_name = str_arg("new_name").or_else(|| str_arg("name"));
            let new_parent_id = str_arg("new_parent_id").or_else(|| str_arg("parent_id"));
            let drive_q = str_arg("drive");
            let site_q = str_arg("site");
            let drive = map_graph_err(
                graph,
                resolver::resolve_target_drive(
                    graph,
                    storage,
                    drive_q.as_deref(),
                    site_q.as_deref(),
                    persona,
                    false,
                )
                .await,
            )
            .await?;
            let item = map_graph_err(
                graph,
                resolver::resolve_item_for_write(graph, storage, &drive.id, &item_q).await,
            )
            .await?;
            let resolved_parent_id = match new_parent_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                Some(p) if p == "/" || p.eq_ignore_ascii_case("root") => Some("root".to_string()),
                Some(p) if p.starts_with('/') || p.contains('/') => {
                    let folder = map_graph_err(
                        graph,
                        resolver::resolve_item_for_write(graph, storage, &drive.id, p).await,
                    )
                    .await?;
                    Some(folder.id)
                }
                Some(pid) => Some(pid.to_string()),
                None => None,
            };
            let updated = map_graph_err(
                graph,
                graph
                    .rename_or_move_item(
                        &drive.id,
                        &item.id,
                        new_name.as_deref(),
                        resolved_parent_id.as_deref(),
                    )
                    .await,
            )
            .await?;
            let _ = storage.upsert_drive_items(std::slice::from_ref(&updated));
            Ok(json!({ "item": updated }))
        }
        "delete_item" => {
            if !bool_arg("confirm") {
                return Err("delete_item requires confirm: true".to_string());
            }
            let item_q = str_arg("item_id")
                .or_else(|| str_arg("path"))
                .ok_or_else(|| "item_id or path is required".to_string())?;
            let drive_q = str_arg("drive");
            let site_q = str_arg("site");
            let drive = map_graph_err(
                graph,
                resolver::resolve_target_drive(
                    graph,
                    storage,
                    drive_q.as_deref(),
                    site_q.as_deref(),
                    persona,
                    false,
                )
                .await,
            )
            .await?;
            let item = map_graph_err(
                graph,
                resolver::resolve_item_for_write(graph, storage, &drive.id, &item_q).await,
            )
            .await?;
            map_graph_err(graph, graph.delete_item(&drive.id, &item.id).await).await?;
            let _ = storage.delete_drive_item(&drive.id, &item.id);
            Ok(json!({
                "ok": true,
                "deleted": true,
                "drive_id": drive.id,
                "item_id": item.id,
                "name": item.name,
            }))
        }
        "search_files" => {
            let query = str_arg("query").ok_or_else(|| "query is required".to_string())?;
            let drive_q = str_arg("drive");
            let site_q = str_arg("site");
            let remote = args.get("remote").and_then(Value::as_bool).unwrap_or(true);
            let limit = usize_arg("limit", 30)?;
            let drive_id = if drive_q.is_some() || site_q.is_some() {
                Some(
                    map_graph_err(
                        graph,
                        resolver::resolve_target_drive(
                            graph,
                            storage,
                            drive_q.as_deref(),
                            site_q.as_deref(),
                            persona,
                            !remote,
                        )
                        .await,
                    )
                    .await?
                    .id,
                )
            } else {
                None
            };
            let items = map_graph_err(
                graph,
                cli::search_items_combined(graph, storage, &query, drive_id.as_deref(), remote, limit)
                    .await,
            )
            .await?;
            Ok(json!({ "query": query, "items": items }))
        }
        "create_share_link" => {
            let item_q = str_arg("item_id")
                .or_else(|| str_arg("path"))
                .ok_or_else(|| "item_id or path is required".to_string())?;
            let drive_q = str_arg("drive");
            let site_q = str_arg("site");
            let link_type = str_arg("type").unwrap_or_else(|| "view".to_string());
            let scope = str_arg("scope").unwrap_or_else(|| "organization".to_string());
            let expires = str_arg("expires");
            let drive = map_graph_err(
                graph,
                resolver::resolve_target_drive(
                    graph,
                    storage,
                    drive_q.as_deref(),
                    site_q.as_deref(),
                    persona,
                    false,
                )
                .await,
            )
            .await?;
            let item = map_graph_err(
                graph,
                resolver::resolve_item(graph, storage, &drive.id, &item_q, false).await,
            )
            .await?;
            let link = map_graph_err(
                graph,
                graph
                    .create_share_link(
                        item.effective_drive_id(),
                        item.effective_item_id(),
                        &link_type,
                        &scope,
                        expires.as_deref(),
                    )
                    .await,
            )
            .await?;
            Ok(json!({ "link": link }))
        }
        "list_recent" => {
            let limit = usize_arg("limit", 30)?;
            let offline = bool_arg("offline");
            let items = if limit == 0 {
                Vec::new()
            } else if offline {
                storage.list_recent_items(limit).map_err(|e| e.to_string())?
            } else {
                let live = map_graph_err(graph, graph.list_recent_items(limit).await).await?;
                let _ = storage.replace_recent_items(&live);
                live
            };
            Ok(json!({ "items": items }))
        }
        "list_shared_with_me" => {
            let limit = usize_arg("limit", 30)?;
            let offline = bool_arg("offline");
            let items = if limit == 0 {
                Vec::new()
            } else if offline {
                storage.list_shared_items(limit).map_err(|e| e.to_string())?
            } else {
                let live = map_graph_err(graph, graph.list_shared_with_me(limit).await).await?;
                let _ = storage.replace_shared_items(&live);
                live
            };
            Ok(json!({ "items": items }))
        }
        "list_versions" => {
            let item_q = str_arg("item_id")
                .or_else(|| str_arg("path"))
                .ok_or_else(|| "item_id or path is required".to_string())?;
            let limit = args
                .get("limit")
                .map(|value| {
                    value
                        .as_u64()
                        .and_then(|limit| usize::try_from(limit).ok())
                        .ok_or_else(|| "limit must be a non-negative integer".to_string())
                })
                .transpose()?;
            let drive_q = str_arg("drive");
            let site_q = str_arg("site");
            let drive = map_graph_err(
                graph,
                resolver::resolve_target_drive(
                    graph,
                    storage,
                    drive_q.as_deref(),
                    site_q.as_deref(),
                    persona,
                    false,
                )
                .await,
            )
            .await?;
            let item = map_graph_err(
                graph,
                resolver::resolve_item(graph, storage, &drive.id, &item_q, false).await,
            )
            .await?;
            let page = map_graph_err(
                graph,
                graph
                    .list_item_versions_page(
                        item.effective_drive_id(),
                        item.effective_item_id(),
                        limit,
                    )
                    .await,
            )
            .await?;
            Ok(json!({
                "item_id": item.id,
                "drive_id": item.drive_id,
                "versions": page.versions,
                "returned": page.versions.len(),
                "has_more": page.has_more,
            }))
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

pub fn tool_catalog() -> Vec<Value> {
    vec![
        tool_def("whoami", "Show current Microsoft 365 user identity, persona, and OAuth scope diagnostics", json!({ "type": "object", "properties": {} })),
        tool_def("list_sites", "List followed and discovered SharePoint sites", json!({ "type": "object", "properties": { "search": { "type": "string" }, "limit": { "type": "integer" }, "offline": { "type": "boolean" } } })),
        tool_def("list_drives", "List Personal OneDrive and SharePoint document libraries. Without site, scans up to 200 followed and discovered sites.", json!({ "type": "object", "properties": { "site": { "type": "string" }, "offline": { "type": "boolean" } } })),
        tool_def("list_files", "List files and folders inside a OneDrive or SharePoint drive", json!({ "type": "object", "properties": { "drive": { "type": "string" }, "site": { "type": "string" }, "parent_id": { "type": "string" }, "path": { "type": "string" }, "limit": { "type": "integer", "description": "Maximum items (default 100). has_more reports whether the folder holds more." }, "offline": { "type": "boolean" }, "compact": { "type": "boolean", "description": "Return the drive ID and core item fields" } } })),
        tool_def("get_file_info", "Get detailed metadata for a file or folder by item_id or path", json!({ "type": "object", "properties": { "item_id": { "type": "string" }, "path": { "type": "string" }, "drive": { "type": "string" }, "site": { "type": "string" }, "offline": { "type": "boolean" }, "compact": { "type": "boolean", "description": "Default true: core item fields, eTag, omitted field names and a hint. Pass false for every field." } } })),
        tool_def("read_text_file", "Read UTF-8 or BOM-marked UTF-16 text up to the original byte cap. Use download_file for binary files.", json!({ "type": "object", "properties": { "item_id": { "type": "string" }, "path": { "type": "string" }, "drive": { "type": "string" }, "site": { "type": "string" }, "max_bytes": { "type": "integer" } } })),
        tool_def("download_file", "Download a OneDrive or SharePoint file to local disk, returning { ok, path, file_name, mime_type, size_bytes, is_image, drive_id, item_id, e_tag, c_tag }", json!({ "type": "object", "properties": { "item_id": { "type": "string" }, "path": { "type": "string" }, "drive": { "type": "string" }, "site": { "type": "string" }, "dir": { "type": "string" } } })),
        tool_def("upload_file", "Upload a local file, base64 bytes, or text content. Files at least 4 MiB and conditional replacements use an upload session.", json!({ "type": "object", "properties": { "local_path": { "type": "string" }, "file_name": { "type": "string" }, "content_base64": { "type": "string" }, "content_text": { "type": "string" }, "drive": { "type": "string" }, "site": { "type": "string" }, "parent_id": { "type": "string", "description": "Folder item ID, or a path starting with / such as /Team Docs" }, "item_id": { "type": "string" }, "if_match": { "type": "string", "description": "eTag precondition for replacement via an upload session at any size" }, "conflict_behavior": { "type": "string", "enum": ["replace", "fail", "rename"] } } })),
        tool_def("create_folder", "Create a new folder inside OneDrive or a SharePoint document library", json!({ "type": "object", "required": ["name"], "properties": { "name": { "type": "string" }, "drive": { "type": "string" }, "site": { "type": "string" }, "parent_id": { "type": "string", "description": "Folder item ID, or a path starting with / such as /Team Docs" } } })),
        tool_def("move_or_rename_item", "Rename and/or move a OneDrive or SharePoint item to another folder", json!({ "type": "object", "properties": { "item_id": { "type": "string" }, "path": { "type": "string" }, "new_name": { "type": "string" }, "new_parent_id": { "type": "string", "description": "Folder item ID, or a path starting with / such as /Team Docs" }, "drive": { "type": "string" }, "site": { "type": "string" } } })),
        tool_def("delete_item", "Delete a OneDrive or SharePoint file or folder (requires confirm: true)", json!({ "type": "object", "required": ["confirm"], "properties": { "item_id": { "type": "string" }, "path": { "type": "string" }, "drive": { "type": "string" }, "site": { "type": "string" }, "confirm": { "type": "boolean" } } })),
        tool_def("search_files", "Search files in local SQLite FTS5 cache and/or Microsoft Graph", json!({ "type": "object", "required": ["query"], "properties": { "query": { "type": "string" }, "drive": { "type": "string" }, "site": { "type": "string" }, "remote": { "type": "boolean", "description": "Default true: query Microsoft Graph and fill remaining slots from the local cache. false searches the local cache only." }, "limit": { "type": "integer" } } })),
        tool_def("create_share_link", "Create a SharePoint/OneDrive sharing link (view, edit, or embed)", json!({ "type": "object", "properties": { "item_id": { "type": "string" }, "path": { "type": "string" }, "drive": { "type": "string" }, "site": { "type": "string" }, "type": { "type": "string", "enum": ["view", "edit", "embed"] }, "scope": { "type": "string", "enum": ["organization", "anonymous"] }, "expires": { "type": "string" } } })),
        tool_def("list_recent", "List recently accessed OneDrive files", json!({ "type": "object", "properties": { "limit": { "type": "integer" }, "offline": { "type": "boolean" } } })),
        tool_def("list_shared_with_me", "List files shared with the signed-in user", json!({ "type": "object", "properties": { "limit": { "type": "integer" }, "offline": { "type": "boolean" } } })),
        tool_def("list_versions", "List version history for a OneDrive or SharePoint file", json!({ "type": "object", "properties": { "item_id": { "type": "string" }, "path": { "type": "string" }, "drive": { "type": "string" }, "site": { "type": "string" }, "limit": { "type": "integer", "minimum": 0, "description": "Maximum versions to fetch. Omit to read the full history. has_more reports whether older versions remain." } } })),
        tool_def("list_lists", "List SharePoint Lists in a site to discover list_id values", json!({
            "type": "object", "required": ["site_id"], "properties": {
                "site_id": { "type": "string" }
            }
        })),
        tool_def("list_items", "Read one page of rows and field values from a SharePoint List", json!({
            "type": "object", "required": ["site_id", "list_id"], "properties": {
                "site_id": { "type": "string" }, "list_id": { "type": "string" },
                "fields": { "type": "array", "items": { "type": "string" }, "description": "Internal column names to select" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 200, "description": "Rows in the first page (default 50)" },
                "cursor": { "type": "string", "description": "Opaque next_cursor from the previous page. Pass the same site_id, list_id and fields; do not pass limit." }
            }
        })),
        tool_def("list_columns", "Discover the internal column names for a SharePoint List", json!({
            "type": "object", "required": ["site_id", "list_id"], "properties": {
                "site_id": { "type": "string" }, "list_id": { "type": "string" }
            }
        })),
        tool_def("update_list_item_field", "Set one SharePoint List field by internal name with a JSON value and optional row ETag", json!({
            "type": "object", "required": ["site_id", "list_id", "item_id", "field", "value"], "properties": {
                "site_id": { "type": "string" }, "list_id": { "type": "string" }, "item_id": { "type": "string" },
                "field": { "type": "string", "description": "Exact internal column name from list_columns" },
                "value": { "description": "Typed JSON value, including false, zero, or null where the column permits it" },
                "if_match": { "type": "string", "description": "Apply only if this row ETag is current" }
            }
        })),
    ]
}

fn tool_def(name: &str, description: &str, input_schema: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": input_schema,
    })
}

fn initialize_result(req: &Value) -> Value {
    let client_ver = req
        .pointer("/params/protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(LATEST_PROTOCOL_VERSION);
    let negotiated = if SUPPORTED_PROTOCOL_VERSIONS.contains(&client_ver) {
        client_ver
    } else {
        LATEST_PROTOCOL_VERSION
    };
    json!({
        "protocolVersion": negotiated,
        "capabilities": {
            "tools": { "listChanged": false }
        },
        "serverInfo": {
            "name": SERVER_NAME,
            "version": env!("CARGO_PKG_VERSION"),
        }
    })
}

fn parse_message(line: &str) -> Result<Value, Value> {
    let val: Value = serde_json::from_str(line)
        .map_err(|e| error_response(Value::Null, PARSE_ERROR, format!("parse error: {e}")))?;
    if !val.is_object() {
        return Err(error_response(
            Value::Null,
            INVALID_REQUEST,
            "invalid request: expected JSON object",
        ));
    }
    Ok(val)
}

fn success_response(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

fn error_response(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message.into(),
        }
    })
}

fn tool_result(outcome: Result<Value, String>) -> Value {
    match outcome {
        Ok(val) => json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string(&val).unwrap_or_else(|_| val.to_string()),
            }],
            "structuredContent": val,
            "isError": false,
        }),
        Err(msg) => json!({
            "content": [{
                "type": "text",
                "text": msg,
            }],
            "isError": true,
        }),
    }
}

fn encode_message(val: &Value) -> String {
    let mut s = serde_json::to_string(val).unwrap_or_else(|_| "{}".to_string());
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_catalog_exposes_all_required_sharepoint_tools() {
        let tools = tool_catalog();
        assert!(tools.len() >= 15);
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t.get("name").and_then(Value::as_str))
            .collect();
        for required in [
            "list_sites",
            "list_drives",
            "list_files",
            "get_file_info",
            "read_text_file",
            "download_file",
            "upload_file",
            "create_folder",
            "move_or_rename_item",
            "delete_item",
            "search_files",
            "create_share_link",
            "list_recent",
            "list_shared_with_me",
            "list_versions",
            "list_lists",
            "list_items",
            "list_columns",
            "update_list_item_field",
        ] {
            assert!(names.contains(&required), "missing MCP tool: {required}");
        }
    }

    #[tokio::test]
    async fn unauthenticated_whoami_includes_persona() {
        let cfg = AppConfig::default();
        let auth = crate::auth::AuthManager::empty_for_test(cfg.clone());
        let graph = GraphClient::new(cfg.clone(), auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let reply = handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"whoami","arguments":{}}}"#,
        )
        .await
        .unwrap();
        let sc = &reply["result"]["structuredContent"];
        assert_eq!(reply["result"]["isError"], false);
        assert_eq!(sc["authenticated"], false);
        assert!(sc["persona"].is_string());
        assert!(sc["auth_source"].is_string());
    }
}
