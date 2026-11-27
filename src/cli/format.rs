//! JSON/human output formatters, tree rendering, and error classification helpers.

use std::{
    io::{self, Write},
    path::{Path, PathBuf},
};

use anyhow::Result;
use serde_json::{json, Value};

use crate::{
    auth::{explain_graph_permission_error, StoredTokens},
    config::{self, AppConfig},
    graph::GraphClient,
    img_preview,
    models::{DriveInfo, DriveItem, TreeNode},
    resolver,
    safe_text::sanitize_terminal_inline,
};

pub struct FolderListing {
    pub items: Vec<DriveItem>,
    pub has_more: Option<bool>,
    pub source: &'static str,
}

pub(crate) fn folder_listing_json(
    drive: &DriveInfo,
    listing: &FolderListing,
    compact: bool,
) -> Value {
    if compact {
        let items: Vec<_> = listing.items.iter().map(compact_drive_item).collect();
        json!({
            "drive": { "id": drive.id },
            "items": items,
            "returned": listing.items.len(),
            "has_more": listing.has_more,
            "source": listing.source,
        })
    } else {
        json!({
            "drive": drive,
            "items": listing.items,
            "returned": listing.items.len(),
            "has_more": listing.has_more,
            "source": listing.source,
        })
    }
}

pub(crate) enum DetailHint {
    Cli,
    Mcp,
}

pub(crate) fn file_info_json(
    item: &DriveItem,
    full: bool,
    hint: DetailHint,
    versions_included: bool,
) -> Value {
    let mut all = json!(item);
    all["download_url"] = json!(item.download_url);
    if full {
        return json!({ "item": all });
    }
    let mut summary = compact_drive_item(item);
    summary["e_tag"] = json!(item.e_tag);
    summary["parent_path"] = json!(item.parent_path);
    let omitted_fields: Vec<&str> = all
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(key, value)| {
            key.as_str() != "list_item_fields"
                && summary.get(key.as_str()).is_none()
                && !value.is_null()
        })
        .map(|(key, _)| key.as_str())
        .collect();
    let mut omitted = json!({ "fields": omitted_fields });
    if !item.list_item_fields.is_empty() {
        let custom_cols: Vec<&str> = item.list_item_fields.keys().map(String::as_str).collect();
        omitted["list_item_fields"] = json!(custom_cols);
    }
    if !versions_included {
        omitted["versions"] = Value::Null;
    }
    summary["omitted"] = omitted;
    summary["hint"] = json!(match hint {
        DetailHint::Cli => format!(
            "sharepoint info {} --drive {} --json --full",
            shell_quote(&item.id),
            shell_quote(&item.drive_id)
        ),
        DetailHint::Mcp => format!(
            "Call get_file_info with item_id {:?}, drive {:?} and fields: [...] for specific fields or compact:false for every field.",
            item.id, item.drive_id
        ),
    });
    json!({ "item": summary })
}

fn canonical_drive_item_field(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "id" => Some("id"),
        "drive_id" | "driveid" => Some("drive_id"),
        "parent_id" | "parentid" => Some("parent_id"),
        "parent_path" | "parentpath" => Some("parent_path"),
        "name" => Some("name"),
        "size" => Some("size"),
        "is_folder" | "isfolder" => Some("is_folder"),
        "child_count" | "childcount" => Some("child_count"),
        "mime_type" | "mimetype" => Some("mime_type"),
        "web_url" | "weburl" => Some("web_url"),
        "download_url" | "downloadurl" => Some("download_url"),
        "e_tag" | "etag" => Some("e_tag"),
        "c_tag" | "ctag" => Some("c_tag"),
        "created_at" | "createdat" | "createddatetime" => Some("created_at"),
        "last_modified_at" | "lastmodifiedat" | "lastmodifieddatetime" => Some("last_modified_at"),
        "created_by" | "createdby" => Some("created_by"),
        "last_modified_by" | "lastmodifiedby" => Some("last_modified_by"),
        "shared_scope" | "sharedscope" => Some("shared_scope"),
        "remote_drive_id" | "remotedriveid" => Some("remote_drive_id"),
        "remote_item_id" | "remoteitemid" => Some("remote_item_id"),
        "description" => Some("description"),
        "sha256_hash" | "sha256hash" | "sha256" => Some("sha256_hash"),
        "quick_xor_hash" | "quickxorhash" => Some("quick_xor_hash"),
        _ => None,
    }
}

pub(crate) fn file_info_fields_json(item: &DriveItem, requested_fields: &[String]) -> Value {
    let mut all = json!(item);
    all["download_url"] = json!(item.download_url);

    let mut summary = compact_drive_item(item);
    summary["e_tag"] = json!(item.e_tag);
    summary["parent_path"] = json!(item.parent_path);

    let mut include_all_list_fields = false;
    let mut selected_list_fields: std::collections::BTreeMap<String, Value> =
        std::collections::BTreeMap::new();
    let mut missing_fields: Vec<String> = Vec::new();
    let mut push_missing = |raw: &str| {
        if !missing_fields.iter().any(|m| m.eq_ignore_ascii_case(raw)) {
            missing_fields.push(raw.to_string());
        }
    };

    for raw in requested_fields {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let lower = trimmed.to_ascii_lowercase();
        if lower == "hashes" {
            let mut hashes = serde_json::Map::new();
            if let Some(ref h) = item.sha256_hash {
                hashes.insert("sha256_hash".to_string(), json!(h));
            }
            if let Some(ref h) = item.quick_xor_hash {
                hashes.insert("quick_xor_hash".to_string(), json!(h));
            }
            if hashes.is_empty() {
                push_missing(trimmed);
            } else {
                summary["hashes"] = Value::Object(hashes);
            }
            continue;
        }
        if lower == "list_item_fields" || lower == "listitemfields" {
            if item.list_item_fields.is_empty() {
                push_missing(trimmed);
            } else {
                include_all_list_fields = true;
            }
            continue;
        }
        if let Some(canon) = canonical_drive_item_field(trimmed) {
            if let Some(val) = all.get(canon).filter(|v| !v.is_null()) {
                summary[canon] = val.clone();
                continue;
            }
            if let Some((k, v)) = item
                .list_item_fields
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(trimmed))
            {
                selected_list_fields.insert(k.clone(), v.clone());
                continue;
            }
            push_missing(trimmed);
            continue;
        }
        if let Some((k, v)) = item
            .list_item_fields
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(trimmed))
        {
            selected_list_fields.insert(k.clone(), v.clone());
        } else {
            push_missing(trimmed);
        }
    }

    if include_all_list_fields {
        summary["list_item_fields"] = json!(item.list_item_fields);
    } else if !selected_list_fields.is_empty() {
        summary["list_item_fields"] = json!(selected_list_fields);
    }

    if !missing_fields.is_empty() {
        summary["missing_fields"] = json!(missing_fields);
    }

    json!({ "item": summary })
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn compact_drive_item(item: &DriveItem) -> Value {
    json!({
        "drive_id": item.drive_id,
        "id": item.id,
        "name": item.name,
        "is_folder": item.is_folder,
        "size": item.size,
        "last_modified_at": item.last_modified_at,
    })
}


pub(super) fn print_tree_nodes(nodes: &[TreeNode], prefix: &str) {
    let mut lines = Vec::new();
    tree_lines(nodes, prefix, &mut lines);
    for line in lines {
        println!("{line}");
    }
}

/// Renders `nodes` as box-drawing tree lines. A folder past `--depth` that still holds items
/// ends in `…`; an expanded folder cut at the per-folder limit gets a note line.
pub(super) fn tree_lines(nodes: &[TreeNode], prefix: &str, out: &mut Vec<String>) {
    for (idx, node) in nodes.iter().enumerate() {
        let is_last = idx + 1 == nodes.len();
        let branch = if is_last { "└── " } else { "├── " };
        let suffix = if node.item.is_folder { "/" } else { "" };
        let depth_capped =
            node.item.is_folder && node.children.is_empty() && node.has_more == Some(true);
        let cue = if depth_capped { " …" } else { "" };
        out.push(format!(
            "{prefix}{branch}{}{suffix}{cue}",
            sanitize_terminal_inline(&node.item.name)
        ));
        if !node.children.is_empty() {
            let next_prefix = format!("{prefix}{}", if is_last { "    " } else { "│   " });
            tree_lines(&node.children, &next_prefix, out);
            if node.has_more == Some(true) {
                out.push(format!("{next_prefix}… more items not shown (200 per folder)"));
            }
        }
    }
}

pub(super) fn render_image_to_stdout(
    config: &AppConfig,
    bytes: &[u8],
    label: &str,
    cols: Option<u16>,
    rows: Option<u16>,
    protocol: Option<&str>,
) -> Result<()> {
    let pref = protocol
        .and_then(config::ImageProtocolPreference::parse_str)
        .unwrap_or(config.image_protocol);
    let out = img_preview::render_ansi_stdout_with_pref(
        bytes,
        cols.unwrap_or(80),
        rows.unwrap_or(28),
        pref,
    )?;
    print!("{out}");
    let _ = io::stdout().flush();
    eprintln!("{}", sanitize_terminal_inline(label));
    Ok(())
}

pub(super) fn print_daemon_status(status: &crate::daemon::DaemonStatus, as_json: bool) -> Result<()> {
    if as_json {
        println!("{}", serde_json::to_string_pretty(status)?);
    } else if status.running {
        println!(
            "Daemon running (pid={:?}, transport={:?}, clients={}, summary={})",
            status.pid,
            status.transport,
            status.attached_clients,
            sanitize_terminal_inline(&status.sync_summary)
        );
    } else {
        println!("Daemon is not running.");
    }
    Ok(())
}

pub fn auth_source_label(tokens: Option<&StoredTokens>, config: &AppConfig) -> String {
    if std::env::var("SHAREPOINT_ACCESS_TOKEN")
        .ok()
        .is_some_and(|v| !v.trim().is_empty())
    {
        return "env:SHAREPOINT_ACCESS_TOKEN".to_string();
    }
    if let Some(tf) = config.resolved_token_file() {
        return format!("token_file:{}", tf.display());
    }
    match tokens {
        Some(t) => format!("stored:{}", t.auth_mode),
        None => "none".to_string(),
    }
}

pub fn is_forbidden_error(err: &anyhow::Error) -> bool {
    let lower = format!("{err:#}").to_ascii_lowercase();
    lower.contains("403")
        || lower.contains("forbidden")
        || lower.contains("accessdenied")
        || lower.contains("insufficient")
}

pub async fn wrap_graph_permission_error(
    graph: &GraphClient,
    err: anyhow::Error,
) -> anyhow::Error {
    let raw = format!("{err:#}");
    let tok = graph
        .auth()
        .current_tokens()
        .await
        .map(|t| t.access_token);
    if let Some(explained) = explain_graph_permission_error(&raw, tok.as_deref()) {
        return anyhow::anyhow!("{explained}\nCause: {raw}");
    }
    err
}

pub(super) fn json_error_value(err: &anyhow::Error) -> Value {
    let message = format!("{err:#}");
    let http_status = crate::graph::graph_error_status(err);
    let ambiguous = err.downcast_ref::<resolver::AmbiguousMatch>();

    let code = if message.starts_with("Not logged in") || http_status == Some(401) {
        "not_authenticated"
    } else if ambiguous.is_some() {
        "ambiguous"
    } else if message.contains("Upload conflict (412") || http_status == Some(412) {
        "precondition_failed"
    } else if http_status == Some(403) {
        "forbidden"
    } else if http_status == Some(404) || message.to_ascii_lowercase().contains("not found") {
        "not_found"
    } else {
        "error"
    };

    let mut error = serde_json::Map::new();
    error.insert("code".to_string(), json!(code));
    if let Some(status) = http_status {
        error.insert("http_status".to_string(), json!(status));
    }
    error.insert("message".to_string(), json!(message));
    if let Some(amb) = ambiguous {
        let candidates: Vec<Value> = amb
            .candidates
            .iter()
            .map(|(name, id)| json!({ "name": name, "id": id }))
            .collect();
        error.insert("candidates".to_string(), Value::Array(candidates));
    }

    json!({
        "ok": false,
        "error": Value::Object(error),
    })
}

pub fn normalize_download_dir(config: &AppConfig, override_dir: Option<&str>) -> PathBuf {
    match override_dir.map(str::trim).filter(|s| !s.is_empty()) {
        Some(d) => Path::new(d).to_path_buf(),
        None => config.resolved_download_dir(),
    }
}
