//! Full Microsoft Graph OneDrive & SharePoint API client:
//! - Personal OneDrive (`/me/drive`, `/me/drive/recent`, `/me/drive/sharedWithMe`)
//! - SharePoint Sites (`/sites?search=*`, `/me/followedSites`, `/sites/{siteId}/drives`)
//! - Folder listing with `@odata.nextLink` pagination + path-based addressing (`/drives/{driveId}/root:/{path}:`)
//! - Metadata, versions (`/versions`), sharing links (`/createLink`)
//! - Downloads (`/content` with redirect Authorization stripping + `@microsoft.graph.downloadUrl`)
//! - Simple upload (`< 4 MiB` `PUT .../content`) and resumable chunked upload session (`>= 4 MiB` `POST .../createUploadSession`)
//! - Folder creation (`mkdir`), rename/move (`PATCH`), delete (`DELETE`), drive search & cross-site search.

use std::{collections::BTreeMap, path::Path, time::Duration};

use anyhow::{anyhow, bail, Context, Result};
use futures::StreamExt;
use rand::Rng;
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{
    graph::{
        client::{parse_retry_after_secs, GraphGetOrGone, GraphListPage, MAX_THROTTLE_RETRIES},
        media::{deduplicate_dest_path, guess_mime_type, sanitize_filename},
        GraphClient,
    },
    models::{
        DownloadedFile, DriveInfo, DriveItem, DriveItemVersion, ShareLinkResult, SharePointSite,
        UploadSession, VirtualListAvailability, VirtualListKind, VirtualListStatus,
    },
};

/// Threshold above which Microsoft Graph requires a resumable upload session (`4 MiB`).
pub const SIMPLE_UPLOAD_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Default chunk size for resumable upload sessions (must be a multiple of 320 KiB: 10 * 320 KiB = 3.2 MiB).
pub const DEFAULT_UPLOAD_CHUNK_BYTES: usize = 10 * 320 * 1024;

/// Graph chunk alignment granularity (320 KiB).
const UPLOAD_CHUNK_GRANULARITY: usize = 320 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictBehavior {
    Replace,
    Fail,
    Rename,
}

impl ConflictBehavior {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Replace => "replace",
            Self::Fail => "fail",
            Self::Rename => "rename",
        }
    }

    pub fn parse_str(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "replace" => Ok(Self::Replace),
            "fail" => Ok(Self::Fail),
            "rename" => Ok(Self::Rename),
            other => bail!("Invalid conflict behavior '{other}'; expected one of: replace, fail, rename"),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UploadOptions {
    pub conflict_behavior: Option<ConflictBehavior>,
    /// Expected remote `eTag` (`If-Match`) for conditional replacement.
    pub if_match: Option<String>,
    /// Optional expected remote `cTag` checked up front alongside/when `eTag` is absent.
    pub expected_c_tag: Option<String>,
    /// Exact target item ID in `drive_id` when conditionally replacing an existing item.
    pub target_item_id: Option<String>,
}

impl UploadOptions {
    pub fn is_conditional(&self) -> bool {
        self.if_match
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
            || self
                .expected_c_tag
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty())
    }
}

pub fn explicit_if_match(raw: Option<&str>) -> Result<Option<String>> {
    match raw {
        None => Ok(None),
        Some(tag) => {
            let trimmed = tag.trim();
            if trimmed.is_empty() {
                bail!("If-Match requires a non-empty ETag");
            }
            Ok(Some(trimmed.to_string()))
        }
    }
}

#[derive(Debug, Clone)]
pub struct FolderChildrenPage {
    pub items: Vec<DriveItem>,
    /// True only when the listing reached the final page without truncation by `limit`
    /// or an unfollowed `@odata.nextLink`.
    pub complete: bool,
}

#[derive(Debug, Clone)]
pub struct VersionsPage {
    pub versions: Vec<DriveItemVersion>,
    /// None only when the caller asked for zero versions and nothing was requested.
    pub has_more: Option<bool>,
}

#[derive(Debug, Clone)]
pub enum DriveDeltaChange {
    Root {
        id: String,
    },
    Upserted {
        item: DriveItem,
        has_explicit_parent_path: bool,
    },
    Deleted {
        id: String,
    },
}

#[derive(Debug, Clone)]
pub struct DriveDeltaPage {
    pub changes: Vec<DriveDeltaChange>,
    pub next_link: Option<String>,
    pub delta_link: Option<String>,
}

#[derive(Debug, Clone)]
pub enum DriveDeltaPageResponse {
    Page(DriveDeltaPage),
    ResyncRequired {
        location: Option<String>,
        detail: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketIoSubscription {
    pub id: String,
    pub notification_url: String,
    pub expiration_date_time: Option<String>,
}

#[derive(Debug, Clone)]
pub struct VirtualListFetch {
    pub items: Vec<DriveItem>,
    pub status: VirtualListStatus,
}

pub fn is_virtual_list_retired_or_unsupported(err: &anyhow::Error) -> Option<String> {
    let raw = format!("{err:#}");
    let lower = raw.to_ascii_lowercase();
    if lower.contains("410 gone")
        || lower.contains("404 not found")
        || lower.contains("501 not implemented")
        || lower.contains("notsupported")
        || lower.contains("deprecated")
        || lower.contains("retired")
        || lower.contains("resourcenotfound")
        || lower.contains("operationnotfound")
        || lower.contains("itemnotfound")
        || lower.contains("not supported")
    {
        let summary = if lower.contains("410") || lower.contains("deprecated") || lower.contains("retired") {
            "Microsoft Graph retired this endpoint (no longer returns data)".to_string()
        } else if lower.contains("404") || lower.contains("501") || lower.contains("notsupported") || lower.contains("not supported") {
            "Endpoint unavailable or unsupported for this account".to_string()
        } else {
            "Virtual list unavailable from Microsoft Graph".to_string()
        };
        return Some(summary);
    }
    None
}

pub fn is_delta_unsupported_error(err: &anyhow::Error) -> bool {
    let lower = format!("{err:#}").to_ascii_lowercase();
    lower.contains("404 not found")
        || lower.contains("501 not implemented")
        || lower.contains("notsupported")
}

pub fn is_socketio_unsupported_error(err: &anyhow::Error) -> bool {
    let lower = format!("{err:#}").to_ascii_lowercase();
    lower.contains("404 not found")
        || lower.contains("501 not implemented")
        || lower.contains("403 forbidden")
        || lower.contains("notsupported")
        || lower.contains("accessdenied")
}

pub fn is_precondition_failed_error(err: &anyhow::Error) -> bool {
    let lower = format!("{err:#}").to_ascii_lowercase();
    lower.contains("412")
        || lower.contains("precondition failed")
        || lower.contains("resourcemodified")
        || lower.contains("itemalreadyexists")
        || lower.contains("namealreadyexists")
}

// ---------------------------------------------------------------------------
// Wire Deserialization Types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct GraphSite {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(rename = "displayName", default)]
    display_name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(rename = "webUrl", default)]
    web_url: Option<String>,
    #[serde(rename = "createdDateTime", default)]
    created_date_time: Option<String>,
    #[serde(rename = "lastModifiedDateTime", default)]
    last_modified_date_time: Option<String>,
}

impl GraphSite {
    fn into_model(self, is_followed: bool) -> SharePointSite {
        let name = self.name.unwrap_or_default();
        let display_name = self
            .display_name
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| name.clone());
        SharePointSite {
            id: self.id,
            name,
            display_name,
            description: self.description.filter(|s| !s.trim().is_empty()),
            web_url: self.web_url,
            is_followed,
            created_at: self.created_date_time,
            last_modified_at: self.last_modified_date_time,
        }
    }
}

#[derive(Debug, Deserialize)]
struct GraphIdentityUser {
    #[serde(rename = "displayName", default)]
    display_name: Option<String>,
    #[serde(default)]
    email: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GraphIdentitySet {
    #[serde(default)]
    user: Option<GraphIdentityUser>,
}

impl GraphIdentitySet {
    fn display_label(&self) -> Option<String> {
        let u = self.user.as_ref()?;
        u.display_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                u.email
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            })
    }
}

#[derive(Debug, Deserialize)]
struct GraphQuota {
    #[serde(default)]
    total: Option<u64>,
    #[serde(default)]
    used: Option<u64>,
    #[serde(default)]
    remaining: Option<u64>,
    #[serde(default)]
    state: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GraphDrive {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(rename = "driveType", default)]
    drive_type: Option<String>,
    #[serde(rename = "webUrl", default)]
    web_url: Option<String>,
    #[serde(default)]
    owner: Option<GraphIdentitySet>,
    #[serde(default)]
    quota: Option<GraphQuota>,
}

impl GraphDrive {
    fn into_model(self, site_id: Option<&str>, site_name: Option<&str>) -> DriveInfo {
        DriveInfo {
            id: self.id,
            name: self.name.unwrap_or_else(|| "Documents".to_string()),
            drive_type: self.drive_type.unwrap_or_else(|| "documentLibrary".to_string()),
            site_id: site_id.map(str::to_string),
            site_name: site_name.map(str::to_string),
            web_url: self.web_url,
            owner_name: self.owner.and_then(|o| o.display_label()),
            quota_total: self.quota.as_ref().and_then(|q| q.total),
            quota_used: self.quota.as_ref().and_then(|q| q.used),
            quota_remaining: self.quota.as_ref().and_then(|q| q.remaining),
            quota_state: self.quota.and_then(|q| q.state),
        }
    }
}

#[derive(Debug, Deserialize)]
struct GraphItemReference {
    #[serde(rename = "driveId", default)]
    drive_id: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GraphFolderFacet {
    #[serde(rename = "childCount", default)]
    child_count: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct GraphFileHashes {
    #[serde(rename = "sha256Hash", default)]
    sha256_hash: Option<String>,
    #[serde(rename = "quickXorHash", default)]
    quick_xor_hash: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GraphFileFacet {
    #[serde(rename = "mimeType", default)]
    mime_type: Option<String>,
    #[serde(default)]
    hashes: Option<GraphFileHashes>,
}

#[derive(Debug, Deserialize)]
struct GraphListItemFacet {
    #[serde(default)]
    fields: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Debug, Deserialize)]
struct GraphSharedFacet {
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GraphRemoteItem {
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "parentReference", default)]
    parent_reference: Option<GraphItemReference>,
    #[serde(default)]
    folder: Option<GraphFolderFacet>,
    #[serde(default)]
    file: Option<GraphFileFacet>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(rename = "webUrl", default)]
    web_url: Option<String>,
    #[serde(default)]
    shared: Option<GraphSharedFacet>,
}

#[derive(Debug, Deserialize)]
struct GraphDeletedFacet {
    #[serde(default)]
    #[allow(dead_code)]
    state: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GraphDeltaPageWire {
    #[serde(default)]
    value: Vec<GraphDriveItem>,
    #[serde(rename = "@odata.nextLink", default)]
    next_link: Option<String>,
    #[serde(rename = "@odata.deltaLink", default)]
    delta_link: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GraphSocketIoSubscriptionWire {
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "notificationUrl")]
    notification_url: String,
    #[serde(rename = "expirationDateTime", default)]
    expiration_date_time: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GraphDriveItem {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    folder: Option<GraphFolderFacet>,
    #[serde(default)]
    file: Option<GraphFileFacet>,
    #[serde(rename = "webUrl", default)]
    web_url: Option<String>,
    #[serde(rename = "@microsoft.graph.downloadUrl", default)]
    download_url: Option<String>,
    #[serde(rename = "eTag", default)]
    e_tag: Option<String>,
    #[serde(rename = "cTag", default)]
    c_tag: Option<String>,
    #[serde(rename = "createdDateTime", default)]
    created_date_time: Option<String>,
    #[serde(rename = "lastModifiedDateTime", default)]
    last_modified_date_time: Option<String>,
    #[serde(rename = "createdBy", default)]
    created_by: Option<GraphIdentitySet>,
    #[serde(rename = "lastModifiedBy", default)]
    last_modified_by: Option<GraphIdentitySet>,
    #[serde(rename = "parentReference", default)]
    parent_reference: Option<GraphItemReference>,
    #[serde(default)]
    shared: Option<GraphSharedFacet>,
    #[serde(rename = "remoteItem", default)]
    remote_item: Option<GraphRemoteItem>,
    #[serde(rename = "listItem", default)]
    list_item: Option<GraphListItemFacet>,
    #[serde(default)]
    deleted: Option<GraphDeletedFacet>,
    #[serde(default)]
    root: Option<serde_json::Value>,
}

fn is_empty_json_value(val: &serde_json::Value) -> bool {
    match val {
        serde_json::Value::Null => true,
        serde_json::Value::String(s) => s.trim().is_empty(),
        serde_json::Value::Array(arr) => arr.is_empty(),
        serde_json::Value::Object(obj) => obj.is_empty(),
        serde_json::Value::Bool(_) | serde_json::Value::Number(_) => false,
    }
}

fn extract_list_item_fields(
    facet: Option<GraphListItemFacet>,
) -> BTreeMap<String, serde_json::Value> {
    let mut out = BTreeMap::new();
    let Some(fields) = facet.and_then(|li| li.fields) else {
        return out;
    };
    for (key, value) in fields {
        let trimmed = key.trim();
        if trimmed.is_empty()
            || trimmed.starts_with("@odata.")
            || trimmed.contains("@odata.")
            || is_empty_json_value(&value)
        {
            continue;
        }
        out.insert(key, value);
    }
    out
}

impl GraphDriveItem {
    fn into_delta_change(self, fallback_drive_id: &str) -> DriveDeltaChange {
        if self.deleted.is_some() {
            return DriveDeltaChange::Deleted { id: self.id };
        }
        if self.root.is_some() {
            return DriveDeltaChange::Root { id: self.id };
        }
        let has_explicit_parent_path = self
            .parent_reference
            .as_ref()
            .and_then(|p| p.path.as_deref())
            .is_some_and(|s| !s.trim().is_empty());
        let item = self.into_model(fallback_drive_id);
        DriveDeltaChange::Upserted {
            item,
            has_explicit_parent_path,
        }
    }

    fn into_model(self, fallback_drive_id: &str) -> DriveItem {
        let remote_drive_id = self
            .remote_item
            .as_ref()
            .and_then(|r| r.parent_reference.as_ref())
            .and_then(|p| p.drive_id.clone());
        let remote_item_id = self.remote_item.as_ref().and_then(|r| r.id.clone());

        let drive_id = self
            .parent_reference
            .as_ref()
            .and_then(|p| p.drive_id.clone())
            .or_else(|| remote_drive_id.clone())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| fallback_drive_id.to_string());

        let parent_id = self
            .parent_reference
            .as_ref()
            .and_then(|p| p.id.clone())
            .filter(|s| !s.trim().is_empty());

        let parent_path = normalize_graph_parent_path(
            self.parent_reference
                .as_ref()
                .and_then(|p| p.path.as_deref())
                .unwrap_or(""),
        );

        let folder_facet = self
            .folder
            .as_ref()
            .or_else(|| self.remote_item.as_ref().and_then(|r| r.folder.as_ref()));
        let file_facet = self
            .file
            .as_ref()
            .or_else(|| self.remote_item.as_ref().and_then(|r| r.file.as_ref()));

        let is_folder = folder_facet.is_some();
        let child_count = folder_facet.and_then(|f| f.child_count);
        let mime_type = file_facet.and_then(|f| f.mime_type.clone());
        let sha256_hash = file_facet
            .and_then(|f| f.hashes.as_ref())
            .and_then(|h| h.sha256_hash.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let quick_xor_hash = file_facet
            .and_then(|f| f.hashes.as_ref())
            .and_then(|h| h.quick_xor_hash.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let description = self
            .description
            .as_deref()
            .or_else(|| self.remote_item.as_ref().and_then(|r| r.description.as_deref()))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let size = self
            .size
            .or_else(|| self.remote_item.as_ref().and_then(|r| r.size))
            .unwrap_or(0);
        let web_url = self
            .web_url
            .or_else(|| self.remote_item.as_ref().and_then(|r| r.web_url.clone()));
        let shared_scope = self
            .shared
            .and_then(|s| s.scope)
            .or_else(|| self.remote_item.and_then(|r| r.shared).and_then(|s| s.scope));
        let list_item_fields = extract_list_item_fields(self.list_item);

        DriveItem {
            id: self.id,
            drive_id,
            parent_id,
            parent_path,
            name: self.name.unwrap_or_else(|| "(unnamed)".to_string()),
            size,
            is_folder,
            child_count,
            mime_type,
            web_url,
            download_url: self.download_url,
            e_tag: self.e_tag,
            c_tag: self.c_tag,
            created_at: self.created_date_time,
            last_modified_at: self.last_modified_date_time,
            created_by: self.created_by.and_then(|i| i.display_label()),
            last_modified_by: self.last_modified_by.and_then(|i| i.display_label()),
            shared_scope,
            remote_drive_id,
            remote_item_id,
            description,
            sha256_hash,
            quick_xor_hash,
            list_item_fields,
        }
    }
}

#[derive(Debug, Deserialize)]
struct GraphVersion {
    id: String,
    #[serde(rename = "lastModifiedDateTime", default)]
    last_modified_date_time: Option<String>,
    #[serde(rename = "lastModifiedBy", default)]
    last_modified_by: Option<GraphIdentitySet>,
    #[serde(default)]
    size: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct GraphPermissionLink {
    #[serde(rename = "webUrl", default)]
    web_url: Option<String>,
    #[serde(rename = "type", default)]
    link_type: Option<String>,
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GraphPermission {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    link: Option<GraphPermissionLink>,
    #[serde(rename = "expirationDateTime", default)]
    expiration_date_time: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GraphUploadSession {
    #[serde(rename = "uploadUrl")]
    upload_url: String,
    #[serde(rename = "expirationDateTime", default)]
    expiration_date_time: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GraphSearchResponse {
    #[serde(default)]
    value: Vec<GraphSearchResultSet>,
}

#[derive(Debug, Deserialize)]
struct GraphSearchResultSet {
    #[serde(rename = "hitsContainers", default)]
    hits_containers: Vec<GraphHitsContainer>,
}

#[derive(Debug, Deserialize)]
struct GraphHitsContainer {
    #[serde(default)]
    hits: Vec<GraphSearchHit>,
}

#[derive(Debug, Deserialize)]
struct GraphSearchHit {
    #[serde(default)]
    resource: Option<GraphDriveItem>,
}

/// Strips `/drive/root:` or `/drives/{id}/root:` prefixes from Graph `parentReference.path`.
pub fn normalize_graph_parent_path(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let after_root = if let Some((_, tail)) = trimmed.split_once("root:") {
        tail
    } else {
        trimmed
    };
    let decoded = urlencoding::decode(after_root)
        .map(|cow| cow.into_owned())
        .unwrap_or_else(|_| after_root.to_string());
    let clean = decoded.trim().trim_end_matches('/');
    if clean.is_empty() || clean == "/" {
        String::new()
    } else if clean.starts_with('/') {
        clean.to_string()
    } else {
        format!("/{clean}")
    }
}

/// Encodes each path segment for Graph path-based addressing (`/drives/{id}/root:/{encoded}:`).
pub fn encode_graph_path(path: &str) -> String {
    path.trim()
        .trim_matches('/')
        .split('/')
        .filter(|seg| !seg.trim().is_empty())
        .map(|seg| urlencoding::encode(seg).into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

impl GraphClient {
    // -----------------------------------------------------------------------
    // Personal OneDrive
    // -----------------------------------------------------------------------

    pub async fn get_my_drive(&self) -> Result<DriveInfo> {
        let d: GraphDrive = self.get_json("/me/drive").await?;
        Ok(d.into_model(None, None))
    }

    pub async fn list_recent_items(&self, limit: usize) -> Result<Vec<DriveItem>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        let mut next: Option<String> = Some(format!("/me/drive/recent?$top={}", limit.clamp(1, 200)));
        while let Some(url) = next {
            let page: GraphListPage<GraphDriveItem> = self.get_json(&url).await?;
            for item in page.value {
                out.push(item.into_model("me"));
                if out.len() >= limit {
                    return Ok(out);
                }
            }
            next = page.next_link;
        }
        Ok(out)
    }

    pub async fn list_shared_with_me(&self, limit: usize) -> Result<Vec<DriveItem>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        let mut next: Option<String> =
            Some(format!("/me/drive/sharedWithMe?$top={}", limit.clamp(1, 200)));
        while let Some(url) = next {
            let page: GraphListPage<GraphDriveItem> = self.get_json(&url).await?;
            for item in page.value {
                out.push(item.into_model("shared"));
                if out.len() >= limit {
                    return Ok(out);
                }
            }
            next = page.next_link;
        }
        Ok(out)
    }

    /// Queries `/me/drive/recent` or `/me/drive/sharedWithMe` while classifying Microsoft Graph
    /// endpoint deprecation/retirement cleanly instead of letting root items or fake rankings intrude.
    pub async fn fetch_virtual_list(
        &self,
        kind: VirtualListKind,
        limit: usize,
    ) -> Result<VirtualListFetch> {
        let checked_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let res = match kind {
            VirtualListKind::Recent => self.list_recent_items(limit).await,
            VirtualListKind::SharedWithMe => self.list_shared_with_me(limit).await,
        };
        match res {
            Ok(items) => Ok(VirtualListFetch {
                items,
                status: VirtualListStatus {
                    availability: VirtualListAvailability::Available,
                    last_checked_at: checked_at,
                },
            }),
            Err(err) => {
                if let Some(reason) = is_virtual_list_retired_or_unsupported(&err) {
                    Ok(VirtualListFetch {
                        items: Vec::new(),
                        status: VirtualListStatus {
                            availability: VirtualListAvailability::Unavailable { reason },
                            last_checked_at: checked_at,
                        },
                    })
                } else {
                    Err(err)
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Per-Drive Delta Tracking & Socket.IO Change Notifications (Graph v1.0)
    // -----------------------------------------------------------------------

    /// Acquires the latest delta cursor (`@odata.deltaLink`) for a drive before an initial folder load.
    pub async fn get_drive_delta_latest_link(&self, drive_id: &str) -> Result<String> {
        let enc_drive = urlencoding::encode(drive_id.trim());
        let mut next = format!("/drives/{enc_drive}/root/delta?token=latest");
        for _ in 0..8 {
            let url = self.resolve_delta_url(&next)?;
            let wire: GraphDeltaPageWire = self.get_json(&url).await?;
            if let Some(delta_link) = wire.delta_link.filter(|s| !s.trim().is_empty()) {
                return Ok(delta_link);
            }
            if let Some(next_link) = wire.next_link.filter(|s| !s.trim().is_empty()) {
                next = next_link;
                continue;
            }
            break;
        }
        bail!("Graph drive delta (?token=latest) returned no @odata.deltaLink for drive '{drive_id}'")
    }

    /// Fetches a single page of `driveItem: delta`, preserving HTTP `410 Gone` and its `Location` header.
    pub async fn get_drive_delta_page(
        &self,
        drive_id: &str,
        url_or_path: &str,
    ) -> Result<DriveDeltaPageResponse> {
        match self.get_json_or_gone::<GraphDeltaPageWire>(url_or_path).await? {
            GraphGetOrGone::Gone { location, detail } => {
                Ok(DriveDeltaPageResponse::ResyncRequired { location, detail })
            }
            GraphGetOrGone::Ok(wire) => {
                let changes = wire
                    .value
                    .into_iter()
                    .map(|raw| raw.into_delta_change(drive_id))
                    .collect();
                Ok(DriveDeltaPageResponse::Page(DriveDeltaPage {
                    changes,
                    next_link: wire.next_link.filter(|s| !s.trim().is_empty()),
                    delta_link: wire.delta_link.filter(|s| !s.trim().is_empty()),
                }))
            }
        }
    }

    /// Acquires a delegated Socket.IO change-notification endpoint for the given drive root
    /// (`GET /drives/{driveId}/root/subscriptions/socketIo`, delegated `Files.Read` scope).
    pub async fn get_drive_socketio_subscription(
        &self,
        drive_id: &str,
    ) -> Result<SocketIoSubscription> {
        let enc_drive = urlencoding::encode(drive_id.trim());
        let wire: GraphSocketIoSubscriptionWire = self
            .get_json_with_headers(
                &format!("/drives/{enc_drive}/root/subscriptions/socketIo"),
                &[("Content-Type", "application/json")],
            )
            .await?;
        if wire.notification_url.trim().is_empty() {
            bail!("Graph Socket.IO subscription returned empty notificationUrl for drive '{drive_id}'");
        }
        Ok(SocketIoSubscription {
            id: wire.id.unwrap_or_default(),
            notification_url: wire.notification_url,
            expiration_date_time: wire.expiration_date_time,
        })
    }

    // -----------------------------------------------------------------------
    // SharePoint Sites & Document Libraries
    // -----------------------------------------------------------------------

    pub async fn list_followed_sites(&self) -> Result<Vec<SharePointSite>> {
        let mut out = Vec::new();
        let mut next: Option<String> = Some("/me/followedSites".to_string());
        while let Some(url) = next {
            let page: GraphListPage<GraphSite> = self.get_json(&url).await?;
            out.extend(page.value.into_iter().map(|s| s.into_model(true)));
            next = page.next_link;
        }
        Ok(out)
    }

    pub async fn list_sites(
        &self,
        search_query: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SharePointSite>> {
        let q = search_query
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("*");
        let encoded_q = urlencoding::encode(q);
        let mut out = Vec::new();
        let mut next: Option<String> = Some(format!(
            "/sites?search={encoded_q}&$top={}",
            limit.clamp(1, 200)
        ));
        while let Some(url) = next {
            let page: GraphListPage<GraphSite> = self.get_json(&url).await?;
            for s in page.value {
                out.push(s.into_model(false));
                if out.len() >= limit {
                    return Ok(out);
                }
            }
            next = page.next_link;
        }
        Ok(out)
    }

    pub async fn get_site(&self, site_id: &str) -> Result<SharePointSite> {
        let encoded = urlencoding::encode(site_id.trim());
        let s: GraphSite = self.get_json(&format!("/sites/{encoded}")).await?;
        Ok(s.into_model(false))
    }

    pub async fn list_site_drives(
        &self,
        site_id: &str,
        site_name: Option<&str>,
    ) -> Result<Vec<DriveInfo>> {
        let encoded = urlencoding::encode(site_id.trim());
        let mut out = Vec::new();
        let mut next: Option<String> = Some(format!("/sites/{encoded}/drives"));
        while let Some(url) = next {
            let page: GraphListPage<GraphDrive> = self.get_json(&url).await?;
            out.extend(
                page.value
                    .into_iter()
                    .map(|d| d.into_model(Some(site_id), site_name)),
            );
            next = page.next_link;
        }
        Ok(out)
    }

    pub async fn get_drive(&self, drive_id: &str) -> Result<DriveInfo> {
        let encoded = urlencoding::encode(drive_id.trim());
        let d: GraphDrive = self.get_json(&format!("/drives/{encoded}")).await?;
        Ok(d.into_model(None, None))
    }

    // -----------------------------------------------------------------------
    // Folder Children & Item Metadata
    // -----------------------------------------------------------------------

    pub async fn list_children(
        &self,
        drive_id: &str,
        parent_item_id: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<DriveItem>> {
        Ok(self
            .list_children_detailed(drive_id, parent_item_id, limit)
            .await?
            .items)
    }

    pub async fn list_children_detailed(
        &self,
        drive_id: &str,
        parent_item_id: Option<&str>,
        limit: Option<usize>,
    ) -> Result<FolderChildrenPage> {
        if limit == Some(0) {
            return Ok(FolderChildrenPage {
                items: Vec::new(),
                complete: false,
            });
        }
        let enc_drive = urlencoding::encode(drive_id.trim());
        let top = match limit {
            Some(cap) => cap.saturating_add(1).clamp(1, 200),
            None => 200,
        };
        let initial = match parent_item_id.map(str::trim).filter(|s| !s.is_empty() && *s != "root") {
            Some(pid) => {
                let enc_item = urlencoding::encode(pid);
                format!("/drives/{enc_drive}/items/{enc_item}/children?$top={top}")
            }
            None => format!("/drives/{enc_drive}/root/children?$top={top}"),
        };
        self.collect_children_pages(drive_id, initial, limit).await
    }

    pub async fn list_children_by_path(
        &self,
        drive_id: &str,
        path: &str,
        limit: Option<usize>,
    ) -> Result<Vec<DriveItem>> {
        Ok(self
            .list_children_by_path_detailed(drive_id, path, limit)
            .await?
            .items)
    }

    pub async fn list_children_by_path_detailed(
        &self,
        drive_id: &str,
        path: &str,
        limit: Option<usize>,
    ) -> Result<FolderChildrenPage> {
        if limit == Some(0) {
            return Ok(FolderChildrenPage {
                items: Vec::new(),
                complete: false,
            });
        }
        let encoded_path = encode_graph_path(path);
        if encoded_path.is_empty() {
            return self.list_children_detailed(drive_id, None, limit).await;
        }
        let enc_drive = urlencoding::encode(drive_id.trim());
        let top = match limit {
            Some(cap) => cap.saturating_add(1).clamp(1, 200),
            None => 200,
        };
        let initial = format!("/drives/{enc_drive}/root:/{encoded_path}:/children?$top={top}");
        self.collect_children_pages(drive_id, initial, limit).await
    }

    async fn collect_children_pages(
        &self,
        drive_id: &str,
        initial_url: String,
        limit: Option<usize>,
    ) -> Result<FolderChildrenPage> {
        let mut out = Vec::new();
        let mut next: Option<String> = Some(initial_url);
        let mut truncated = false;

        while let Some(url) = next {
            let page: GraphListPage<GraphDriveItem> = self.get_json(&url).await?;
            let page_len = page.value.len();
            for (idx, item) in page.value.into_iter().enumerate() {
                if let Some(cap) = limit {
                    if out.len() >= cap {
                        truncated = true;
                        break;
                    }
                }
                out.push(item.into_model(drive_id));
                if let Some(cap) = limit {
                    if out.len() >= cap {
                        if idx + 1 < page_len || page.next_link.is_some() {
                            truncated = true;
                        }
                        return Ok(FolderChildrenPage {
                            items: out,
                            complete: !truncated,
                        });
                    }
                }
            }
            if truncated {
                break;
            }
            next = page.next_link;
        }

        Ok(FolderChildrenPage {
            items: out,
            complete: !truncated,
        })
    }

    async fn get_drive_item_with_list_fields(&self, base_endpoint: &str) -> Result<GraphDriveItem> {
        let expanded = format!("{base_endpoint}?$expand=listItem($expand=fields)");
        match self.get_json::<GraphDriveItem>(&expanded).await {
            Ok(raw) => Ok(raw),
            Err(err) if is_list_item_expand_unsupported(&err) => {
                self.get_json::<GraphDriveItem>(base_endpoint).await
            }
            Err(err) => Err(err),
        }
    }

    pub async fn get_item(&self, drive_id: &str, item_id: &str) -> Result<DriveItem> {
        let enc_drive = urlencoding::encode(drive_id.trim());
        let enc_item = urlencoding::encode(item_id.trim());
        let endpoint = format!("/drives/{enc_drive}/items/{enc_item}");
        let raw = self.get_drive_item_with_list_fields(&endpoint).await?;
        Ok(raw.into_model(drive_id))
    }

    pub async fn get_item_by_path(&self, drive_id: &str, path: &str) -> Result<DriveItem> {
        let enc_drive = urlencoding::encode(drive_id.trim());
        let encoded_path = encode_graph_path(path);
        let endpoint = if encoded_path.is_empty() {
            format!("/drives/{enc_drive}/root")
        } else {
            format!("/drives/{enc_drive}/root:/{encoded_path}:")
        };
        let raw = self.get_drive_item_with_list_fields(&endpoint).await?;
        Ok(raw.into_model(drive_id))
    }

    async fn get_item_by_parent_and_name(
        &self,
        drive_id: &str,
        parent_item_id: Option<&str>,
        file_name: &str,
    ) -> Result<DriveItem> {
        let enc_drive = urlencoding::encode(drive_id.trim());
        let enc_name = urlencoding::encode(file_name.trim());
        let endpoint = match parent_item_id
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != "root")
        {
            Some(pid) => {
                let enc_parent = urlencoding::encode(pid);
                format!("/drives/{enc_drive}/items/{enc_parent}:/{enc_name}:")
            }
            None => format!("/drives/{enc_drive}/root:/{enc_name}:"),
        };
        let raw: GraphDriveItem = self.get_json(&endpoint).await?;
        Ok(raw.into_model(drive_id))
    }

    /// Verifies exact drive/item target and compares the stored baseline tag (`eTag`/`cTag`)
    /// against live server metadata up front before initiating any conditional write.
    ///
    /// Note: this detects already-changed files before upload starts and resolves the exact
    /// item ID needed for `POST /drives/{driveId}/items/{itemId}/createUploadSession` with
    /// `If-Match`. It does not provide commit-time locking across chunk transfer.
    async fn preflight_conditional_target(
        &self,
        drive_id: &str,
        parent_item_id: Option<&str>,
        file_name: &str,
        options: &UploadOptions,
    ) -> Result<DriveItem> {
        let current = match options
            .target_item_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(item_id) => self.get_item(drive_id, item_id).await.map_err(|err| {
                if is_not_found_error(&err) {
                    anyhow!(
                        "Upload conflict (412 Precondition Failed) for '{file_name}': target item '{item_id}' no longer exists in drive '{drive_id}'. Cause: {err:#}"
                    )
                } else {
                    err
                }
            })?,
            None => self
                .get_item_by_parent_and_name(drive_id, parent_item_id, file_name)
                .await
                .map_err(|err| {
                    if is_not_found_error(&err) {
                        anyhow!(
                            "Upload conflict (412 Precondition Failed) for '{file_name}': target file does not exist in drive '{drive_id}' for conditional replacement. Cause: {err:#}"
                        )
                    } else {
                        err
                    }
                })?,
        };

        if current.drive_id != drive_id && current.effective_drive_id() != drive_id {
            bail!(
                "Upload conflict (412 Precondition Failed) for '{file_name}': resolved item '{}' belongs to drive '{}' rather than '{drive_id}'.",
                current.id,
                current.drive_id
            );
        }

        if let Some(expected_etag) = options
            .if_match
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let actual = current.e_tag.as_deref().map(str::trim).unwrap_or("");
            if actual != expected_etag {
                bail!(
                    "Upload conflict (412 Precondition Failed) for '{file_name}': remote file changed since expected version ({expected_etag}; server eTag is '{}'). Re-read/merge the latest remote file or pass --conflict replace to overwrite.",
                    if actual.is_empty() { "<none>" } else { actual }
                );
            }
        } else if let Some(expected_ctag) = options
            .expected_c_tag
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let actual = current.c_tag.as_deref().map(str::trim).unwrap_or("");
            if actual != expected_ctag {
                bail!(
                    "Upload conflict (412 Precondition Failed) for '{file_name}': remote file changed since expected cTag ({expected_ctag}; server cTag is '{}'). Re-read/merge the latest remote file or pass --conflict replace to overwrite.",
                    if actual.is_empty() { "<none>" } else { actual }
                );
            }
        }

        Ok(current)
    }

    async fn preflight_fail_if_exists(
        &self,
        drive_id: &str,
        parent_item_id: Option<&str>,
        file_name: &str,
    ) -> Result<()> {
        match self
            .get_item_by_parent_and_name(drive_id, parent_item_id, file_name)
            .await
        {
            Ok(existing) => bail!(
                "Upload conflict (412 Precondition Failed) for '{file_name}': target file already exists (id: {}) and conflict behavior is 'fail'. Pass --conflict replace or --conflict rename.",
                existing.id
            ),
            Err(err) if is_not_found_error(&err) => Ok(()),
            Err(err) => Err(err),
        }
    }

    pub async fn list_item_versions(
        &self,
        drive_id: &str,
        item_id: &str,
    ) -> Result<Vec<DriveItemVersion>> {
        self.list_item_versions_with_limit(drive_id, item_id, None)
            .await
    }

    pub async fn list_item_versions_with_limit(
        &self,
        drive_id: &str,
        item_id: &str,
        limit: Option<usize>,
    ) -> Result<Vec<DriveItemVersion>> {
        Ok(self
            .list_item_versions_page(drive_id, item_id, limit)
            .await?
            .versions)
    }

    pub async fn list_item_versions_page(
        &self,
        drive_id: &str,
        item_id: &str,
        limit: Option<usize>,
    ) -> Result<VersionsPage> {
        if limit == Some(0) {
            return Ok(VersionsPage {
                versions: Vec::new(),
                has_more: None,
            });
        }
        let enc_drive = urlencoding::encode(drive_id.trim());
        let enc_item = urlencoding::encode(item_id.trim());
        let limit = limit.unwrap_or(usize::MAX);
        let mut out = Vec::new();
        let mut next: Option<String> =
            Some(format!("/drives/{enc_drive}/items/{enc_item}/versions"));
        while let Some(url) = next {
            let page: GraphListPage<GraphVersion> = self.get_json(&url).await?;
            let remaining = limit - out.len();
            let page_len = page.value.len();
            out.extend(page.value.into_iter().take(remaining).map(|v| DriveItemVersion {
                id: v.id,
                last_modified_at: v.last_modified_date_time,
                last_modified_by: v.last_modified_by.and_then(|u| u.display_label()),
                size: v.size.unwrap_or(0),
            }));
            if out.len() >= limit {
                let has_more = page_len > remaining || page.next_link.is_some();
                return Ok(VersionsPage {
                    versions: out,
                    has_more: Some(has_more),
                });
            }
            next = page.next_link;
        }
        Ok(VersionsPage {
            versions: out,
            has_more: Some(false),
        })
    }

    pub async fn create_share_link(
        &self,
        drive_id: &str,
        item_id: &str,
        link_type: &str,
        scope: &str,
        expiration_iso: Option<&str>,
    ) -> Result<ShareLinkResult> {
        let enc_drive = urlencoding::encode(drive_id.trim());
        let enc_item = urlencoding::encode(item_id.trim());
        let mut body = json!({
            "type": link_type,
            "scope": scope,
        });
        if let Some(exp) = expiration_iso.map(str::trim).filter(|s| !s.is_empty()) {
            body["expirationDateTime"] = json!(exp);
        }
        let perm: GraphPermission = self
            .post_json(
                &format!("/drives/{enc_drive}/items/{enc_item}/createLink"),
                &body,
            )
            .await?;
        let link = perm
            .link
            .ok_or_else(|| anyhow!("Graph createLink response did not include a link URL"))?;
        let web_url = link
            .web_url
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow!("Graph createLink response returned an empty webUrl"))?;
        Ok(ShareLinkResult {
            id: perm.id.unwrap_or_default(),
            web_url,
            link_type: link.link_type.unwrap_or_else(|| link_type.to_string()),
            scope: link.scope.unwrap_or_else(|| scope.to_string()),
            expiration_date_time: perm.expiration_date_time,
        })
    }

    // -----------------------------------------------------------------------
    // Downloads (with explicit redirect Authorization header stripping!)
    // -----------------------------------------------------------------------

    /// Sends one download hop and returns its response once the headers have arrived.
    ///
    /// `build` creates the request afresh for each attempt; this helper never adds a
    /// token itself (pre-authenticated and redirect URLs must not carry one). Retries
    /// happen only here, before any body byte is read: up to `MAX_THROTTLE_RETRIES` on
    /// 429/502/503/504 honouring `Retry-After`, and once on a connect error or timeout.
    async fn send_download_hop<F>(&self, what: &str, build: F) -> Result<reqwest::Response>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        let header_timeout = self.download_header_timeout();
        let mut throttle_attempt = 0usize;
        let mut transport_retried = false;
        loop {
            let failure = match tokio::time::timeout(header_timeout, build().send()).await {
                Ok(Ok(resp)) => {
                    if matches!(
                        resp.status(),
                        StatusCode::TOO_MANY_REQUESTS
                            | StatusCode::SERVICE_UNAVAILABLE
                            | StatusCode::BAD_GATEWAY
                            | StatusCode::GATEWAY_TIMEOUT
                    ) && throttle_attempt < MAX_THROTTLE_RETRIES
                    {
                        let delay_secs = parse_retry_after_secs(resp.headers(), throttle_attempt);
                        throttle_attempt += 1;
                        let jitter_ms: u64 = rand::thread_rng().gen_range(0..=250);
                        tokio::time::sleep(
                            Duration::from_secs(delay_secs) + Duration::from_millis(jitter_ms),
                        )
                        .await;
                        continue;
                    }
                    return Ok(resp);
                }
                Ok(Err(err)) => {
                    let retryable = err.is_connect() || err.is_timeout();
                    let err = anyhow::Error::new(err).context(what.to_string());
                    (crate::net::annotate_tls_error(err), retryable)
                }
                Err(_elapsed) => (
                    anyhow!(
                        "{what}: no response within {:.0?}",
                        header_timeout
                    ),
                    true,
                ),
            };
            let (err, retryable) = failure;
            if retryable && !transport_retried {
                transport_retried = true;
                let jitter_ms: u64 = rand::thread_rng().gen_range(0..=250);
                tokio::time::sleep(Duration::from_millis(100 + jitter_ms)).await;
                continue;
            }
            return Err(err);
        }
    }

    async fn fetch_content_response(
        &self,
        drive_id: &str,
        item_id: &str,
        download_url: Option<&str>,
    ) -> Result<reqwest::Response> {
        let mut preauth_failure: Option<String> = None;
        if let Some(preauth_url) = download_url.map(str::trim).filter(|s| !s.is_empty()) {
            // Pre-authenticated `@microsoft.graph.downloadUrl`: never attach Bearer auth!
            match self
                .send_download_hop("Failed to fetch pre-authenticated download URL", || {
                    self.http_download().get(preauth_url)
                })
                .await
            {
                Ok(resp) if resp.status().is_success() => return Ok(resp),
                // Expired pre-authenticated URL (401/403/410, ...): fall through to `/content`.
                Ok(resp) => {
                    preauth_failure = Some(format!("it answered {}", resp.status()));
                }
                // Unreachable or stalled: `/content` may still work, so fall through as well.
                Err(err) => preauth_failure = Some(format!("{err:#}")),
            }
        }

        match self.fetch_content_via_graph(drive_id, item_id).await {
            Ok(resp) => Ok(resp),
            Err(err) => match preauth_failure {
                Some(first) => Err(err.context(format!(
                    "Download failed on both routes; the pre-authenticated download URL had failed first ({first})"
                ))),
                None => Err(err),
            },
        }
    }

    /// `GET /drives/{drive}/items/{item}/content` with Bearer auth, then the redirect it
    /// answers with (fetched without the token).
    async fn fetch_content_via_graph(
        &self,
        drive_id: &str,
        item_id: &str,
    ) -> Result<reqwest::Response> {
        let enc_drive = urlencoding::encode(drive_id.trim());
        let enc_item = urlencoding::encode(item_id.trim());
        let content_url = self.resolve_url(&format!("/drives/{enc_drive}/items/{enc_item}/content"));

        let mut token = self.auth().valid_access_token().await?;
        let mut resp = self
            .send_download_hop(
                &format!("Graph content download failed (GET {content_url})"),
                || self.http_no_redirect().get(&content_url).bearer_auth(&token),
            )
            .await?;

        if resp.status() == StatusCode::UNAUTHORIZED {
            if self.auth().refresh_for_failed_token(&token).await.is_ok() {
                token = self.auth().valid_access_token().await?;
                resp = self
                    .send_download_hop(
                        &format!("Graph content download retry failed (GET {content_url})"),
                        || self.http_no_redirect().get(&content_url).bearer_auth(&token),
                    )
                    .await?;
            }
        }

        // Handle 302/301/303/307/308 redirect from `/content` by stripping Authorization header!
        if resp.status().is_redirection() {
            let loc = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow!("Graph /content redirect missing Location header"))?
                .to_string();

            let redirect_url = if loc.starts_with("http://") || loc.starts_with("https://") {
                loc
            } else {
                let base = url::Url::parse(&content_url)?;
                base.join(&loc)?.to_string()
            };

            let redirected = self
                .send_download_hop("Failed to follow content download redirect", || {
                    self.http_download().get(&redirect_url)
                })
                .await?;
            if !redirected.status().is_success() {
                let st = redirected.status();
                let body = redirected.text().await.unwrap_or_default();
                bail!(
                    "{}GET {redirect_url}): {st} {}",
                    crate::graph::GRAPH_API_ERROR_PREFIX,
                    body.trim()
                );
            }
            return Ok(redirected);
        }

        if !resp.status().is_success() {
            let st = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!(
                "{}GET {content_url}): {st} {}",
                crate::graph::GRAPH_API_ERROR_PREFIX,
                body.trim()
            );
        }

        Ok(resp)
    }

    pub async fn download_item_bytes(
        &self,
        drive_id: &str,
        item_id: &str,
        download_url: Option<&str>,
        max_bytes: usize,
    ) -> Result<Vec<u8>> {
        let resp = self
            .fetch_content_response(drive_id, item_id, download_url)
            .await?;
        if let Some(len) = resp.content_length() {
            if len > max_bytes as u64 {
                bail!("File size ({len} bytes) exceeds preview/in-memory limit ({max_bytes} bytes)");
            }
        }
        let mut out = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk_res) = stream.next().await {
            let chunk = chunk_res.context("Failed to read download stream chunk")?;
            if out.len().saturating_add(chunk.len()) > max_bytes {
                bail!("File body exceeded preview/in-memory limit ({max_bytes} bytes)");
            }
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }

    pub async fn download_item_to_file(
        &self,
        drive_id: &str,
        item_id: &str,
        suggested_name: Option<&str>,
        suggested_mime: Option<&str>,
        download_url: Option<&str>,
        dest_dir: &Path,
    ) -> Result<DownloadedFile> {
        tokio::fs::create_dir_all(dest_dir)
            .await
            .with_context(|| format!("Failed to create download directory: {}", dest_dir.display()))?;

        let resp = self
            .fetch_content_response(drive_id, item_id, download_url)
            .await?;

        let header_mime = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.split(';').next().unwrap_or(s).trim().to_string())
            .filter(|s| !s.is_empty());

        let safe_name = sanitize_filename(
            suggested_name
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(item_id),
        );
        let dest_path = deduplicate_dest_path(dest_dir, &safe_name);
        let final_name = dest_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&safe_name)
            .to_string();

        let file = tokio::fs::File::create(&dest_path)
            .await
            .with_context(|| format!("Failed to create file: {}", dest_path.display()))?;
        let mut writer = tokio::io::BufWriter::with_capacity(64 * 1024, file);
        let mut stream = resp.bytes_stream();
        let mut total_written: u64 = 0;
        let mut head_sample: Vec<u8> = Vec::new();

        while let Some(chunk_res) = stream.next().await {
            let chunk = match chunk_res {
                Ok(c) => c,
                Err(err) => {
                    let _ = tokio::fs::remove_file(&dest_path).await;
                    return Err(err).context("Failed to stream download chunk");
                }
            };
            if head_sample.len() < 4096 {
                let take = (4096 - head_sample.len()).min(chunk.len());
                head_sample.extend_from_slice(&chunk[..take]);
            }
            if let Err(err) = writer.write_all(&chunk).await {
                let _ = tokio::fs::remove_file(&dest_path).await;
                return Err(err).context("Failed to write downloaded chunk");
            }
            total_written += chunk.len() as u64;
        }
        writer.flush().await.context("Failed to flush downloaded file")?;

        let mime_type = suggested_mime
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or(header_mime)
            .unwrap_or_else(|| guess_mime_type(&dest_path).to_string());
        let is_image = if head_sample.is_empty() {
            crate::img_preview::is_image_mime_or_name(&mime_type, Some(&final_name))
        } else {
            matches!(
                bitefoundry_html::sniff_content_kind(
                    Some(&mime_type),
                    Some(&final_name),
                    Some(&head_sample),
                ),
                bitefoundry_html::ContentKind::RasterImage | bitefoundry_html::ContentKind::Svg
            )
        };

        Ok(DownloadedFile {
            ok: true,
            path: dest_path,
            file_name: final_name,
            mime_type,
            size_bytes: total_written,
            is_image,
            drive_id: Some(drive_id.to_string()),
            item_id: Some(item_id.to_string()),
            e_tag: None,
            c_tag: None,
        })
    }

    // -----------------------------------------------------------------------
    // Uploads (Simple < 4 MiB + Resumable Chunked Session >= 4 MiB)
    // -----------------------------------------------------------------------

    pub async fn upload_file_simple(
        &self,
        drive_id: &str,
        parent_item_id: Option<&str>,
        file_name: &str,
        bytes: Vec<u8>,
        content_type: Option<&str>,
    ) -> Result<DriveItem> {
        self.upload_file_simple_with_options(
            drive_id,
            parent_item_id,
            file_name,
            bytes,
            content_type,
            &UploadOptions::default(),
        )
        .await
    }

    pub async fn upload_file_simple_with_options(
        &self,
        drive_id: &str,
        parent_item_id: Option<&str>,
        file_name: &str,
        bytes: Vec<u8>,
        content_type: Option<&str>,
        options: &UploadOptions,
    ) -> Result<DriveItem> {
        // Conditional replacement: verify exact drive/item + baseline tag up front, then prefer
        // the documented `POST /drives/{driveId}/items/{itemId}/createUploadSession` with
        // `If-Match` for non-empty payloads, and exact item-ID `PUT /content` for zero-length files.
        if options.is_conditional() {
            let target = self
                .preflight_conditional_target(drive_id, parent_item_id, file_name, options)
                .await?;
            if bytes.is_empty() {
                return self
                    .upload_item_content_put(
                        drive_id,
                        &target.id,
                        file_name,
                        bytes,
                        content_type,
                        options,
                    )
                    .await;
            }
            let mut resolved_opts = options.clone();
            resolved_opts.target_item_id = Some(target.id);
            return self
                .upload_bytes_chunked_with_options_verified(
                    drive_id,
                    parent_item_id,
                    file_name,
                    &bytes,
                    DEFAULT_UPLOAD_CHUNK_BYTES,
                    &resolved_opts,
                )
                .await;
        }

        // Explicit `fail` or `rename` conflict policy on non-conditional uploads:
        // prefer documented `createUploadSession` conflictBehavior for non-empty payloads.
        // A zero-length payload cannot use an upload session, and the simple `PUT .../content`
        // has no documented conflict parameter (it replaces), so the policy is applied up
        // front instead: `fail` rejects an existing name, `rename` uploads under a free name.
        if matches!(
            options.conflict_behavior,
            Some(ConflictBehavior::Fail | ConflictBehavior::Rename)
        ) {
            if options.conflict_behavior == Some(ConflictBehavior::Fail) {
                self.preflight_fail_if_exists(drive_id, parent_item_id, file_name)
                    .await?;
            }
            if !bytes.is_empty() {
                return self
                    .upload_bytes_chunked_with_options_verified(
                        drive_id,
                        parent_item_id,
                        file_name,
                        &bytes,
                        DEFAULT_UPLOAD_CHUNK_BYTES,
                        options,
                    )
                    .await;
            }
            let targets_item_id = options
                .target_item_id
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty());
            if options.conflict_behavior == Some(ConflictBehavior::Rename) && !targets_item_id {
                return self
                    .upload_empty_file_with_rename(
                        drive_id,
                        parent_item_id,
                        file_name,
                        content_type,
                        options,
                    )
                    .await;
            }
        }

        // Direct item-ID replacement without precondition (`< 4 MiB`):
        // match `>= 4 MiB` target selection by using `/drives/{drive}/items/{id}/content`.
        if let Some(item_id) = options
            .target_item_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return self
                .upload_item_content_put(
                    drive_id,
                    item_id,
                    file_name,
                    bytes,
                    content_type,
                    options,
                )
                .await;
        }

        // Ordinary new upload or explicit replace (`< 4 MiB`)
        self.upload_file_simple_put(
            drive_id,
            parent_item_id,
            file_name,
            bytes,
            content_type,
            options,
        )
        .await
    }

    /// Zero-length upload with conflict behavior `rename`: never replaces an existing file.
    ///
    /// The name is used as given when it is free. Otherwise the first free `name N.ext`
    /// (the form Graph itself uses when it renames) is looked up and the empty file is
    /// created there, so the existing file is never the target of the `PUT`.
    async fn upload_empty_file_with_rename(
        &self,
        drive_id: &str,
        parent_item_id: Option<&str>,
        file_name: &str,
        content_type: Option<&str>,
        options: &UploadOptions,
    ) -> Result<DriveItem> {
        let existing = match self
            .get_item_by_parent_and_name(drive_id, parent_item_id, file_name)
            .await
        {
            Ok(existing) => existing,
            Err(err) if is_not_found_error(&err) => {
                return self
                    .upload_file_simple_put(
                        drive_id,
                        parent_item_id,
                        file_name,
                        Vec::new(),
                        content_type,
                        options,
                    )
                    .await;
            }
            Err(err) => return Err(err),
        };

        for n in 1..=MAX_RENAME_CANDIDATES {
            let candidate = renamed_file_name(file_name, n);
            match self
                .get_item_by_parent_and_name(drive_id, parent_item_id, &candidate)
                .await
            {
                Ok(_taken) => continue,
                Err(err) if is_not_found_error(&err) => {
                    let created = self
                        .upload_file_simple_put(
                            drive_id,
                            parent_item_id,
                            &candidate,
                            Vec::new(),
                            content_type,
                            options,
                        )
                        .await?;
                    if created.id == existing.id {
                        // Must not be reported as a successful rename.
                        bail!(
                            "Upload of empty '{file_name}' with conflict behavior 'rename' did not create a new file: the server returned the existing item (id: {}), which may have been replaced. Check the file's version history.",
                            existing.id
                        );
                    }
                    return Ok(created);
                }
                Err(err) => return Err(err),
            }
        }
        bail!(
            "Upload conflict for '{file_name}': no free name found after {MAX_RENAME_CANDIDATES} attempts with conflict behavior 'rename'. Upload under a different name."
        )
    }

    async fn upload_item_content_put(
        &self,
        drive_id: &str,
        item_id: &str,
        file_name: &str,
        bytes: Vec<u8>,
        content_type: Option<&str>,
        options: &UploadOptions,
    ) -> Result<DriveItem> {
        let enc_drive = urlencoding::encode(drive_id.trim());
        let enc_item = urlencoding::encode(item_id.trim());
        let endpoint = format!("/drives/{enc_drive}/items/{enc_item}/content");
        let ct = content_type.unwrap_or_else(|| guess_mime_type(Path::new(file_name)));
        let mut headers = Vec::new();
        if let Some(etag) = options
            .if_match
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            headers.push(("If-Match", etag.to_string()));
        }
        let raw: GraphDriveItem = self
            .put_bytes_with_headers(&endpoint, bytes, Some(ct), &headers)
            .await
            .map_err(|err| annotate_upload_conflict(err, file_name, options))?;
        Ok(raw.into_model(drive_id))
    }

    async fn upload_file_simple_put(
        &self,
        drive_id: &str,
        parent_item_id: Option<&str>,
        file_name: &str,
        bytes: Vec<u8>,
        content_type: Option<&str>,
        options: &UploadOptions,
    ) -> Result<DriveItem> {
        let enc_drive = urlencoding::encode(drive_id.trim());
        let enc_name = urlencoding::encode(file_name.trim());
        let endpoint = match parent_item_id.map(str::trim).filter(|s| !s.is_empty() && *s != "root") {
            Some(pid) => {
                let enc_parent = urlencoding::encode(pid);
                format!("/drives/{enc_drive}/items/{enc_parent}:/{enc_name}:/content")
            }
            None => format!("/drives/{enc_drive}/root:/{enc_name}:/content"),
        };
        let ct = content_type.unwrap_or_else(|| guess_mime_type(Path::new(file_name)));
        let raw: GraphDriveItem = self
            .put_bytes_with_headers(&endpoint, bytes, Some(ct), &[])
            .await
            .map_err(|err| annotate_upload_conflict(err, file_name, options))?;
        Ok(raw.into_model(drive_id))
    }

    pub async fn upload_file_by_path_simple(
        &self,
        drive_id: &str,
        remote_path: &str,
        bytes: Vec<u8>,
        content_type: Option<&str>,
    ) -> Result<DriveItem> {
        let enc_drive = urlencoding::encode(drive_id.trim());
        let encoded_path = encode_graph_path(remote_path);
        if encoded_path.is_empty() {
            bail!("Remote upload path cannot be empty.");
        }
        let endpoint = format!("/drives/{enc_drive}/root:/{encoded_path}:/content");
        let ct = content_type.unwrap_or_else(|| guess_mime_type(Path::new(remote_path)));
        let raw: GraphDriveItem = self.put_bytes(&endpoint, bytes, Some(ct)).await?;
        Ok(raw.into_model(drive_id))
    }

    pub async fn create_upload_session(
        &self,
        drive_id: &str,
        parent_item_id: Option<&str>,
        file_name: &str,
        conflict_behavior: &str,
    ) -> Result<UploadSession> {
        self.create_upload_session_with_headers(
            drive_id,
            parent_item_id,
            None,
            file_name,
            conflict_behavior,
            &[],
        )
        .await
    }

    async fn create_upload_session_with_headers(
        &self,
        drive_id: &str,
        parent_item_id: Option<&str>,
        target_item_id: Option<&str>,
        file_name: &str,
        conflict_behavior: &str,
        extra_headers: &[(&str, String)],
    ) -> Result<UploadSession> {
        let enc_drive = urlencoding::encode(drive_id.trim());
        let (endpoint, body) = if let Some(item_id) = target_item_id
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let enc_item = urlencoding::encode(item_id);
            (
                format!("/drives/{enc_drive}/items/{enc_item}/createUploadSession"),
                json!({
                    "item": {
                        "@microsoft.graph.conflictBehavior": conflict_behavior,
                    }
                }),
            )
        } else {
            let enc_name = urlencoding::encode(file_name.trim());
            let ep = match parent_item_id
                .map(str::trim)
                .filter(|s| !s.is_empty() && *s != "root")
            {
                Some(pid) => {
                    let enc_parent = urlencoding::encode(pid);
                    format!("/drives/{enc_drive}/items/{enc_parent}:/{enc_name}:/createUploadSession")
                }
                None => format!("/drives/{enc_drive}/root:/{enc_name}:/createUploadSession"),
            };
            (
                ep,
                json!({
                    "item": {
                        "@microsoft.graph.conflictBehavior": conflict_behavior,
                        "name": file_name.trim(),
                    }
                }),
            )
        };
        let session: GraphUploadSession = self
            .post_json_with_headers(&endpoint, &body, extra_headers)
            .await?;
        Ok(UploadSession {
            upload_url: session.upload_url,
            expiration_date_time: session.expiration_date_time,
        })
    }

    pub async fn upload_bytes_chunked(
        &self,
        drive_id: &str,
        parent_item_id: Option<&str>,
        file_name: &str,
        bytes: &[u8],
        chunk_size: usize,
    ) -> Result<DriveItem> {
        self.upload_bytes_chunked_with_options(
            drive_id,
            parent_item_id,
            file_name,
            bytes,
            chunk_size,
            &UploadOptions::default(),
        )
        .await
    }

    pub async fn upload_bytes_chunked_with_options(
        &self,
        drive_id: &str,
        parent_item_id: Option<&str>,
        file_name: &str,
        bytes: &[u8],
        chunk_size: usize,
        options: &UploadOptions,
    ) -> Result<DriveItem> {
        let mut effective_opts = options.clone();
        if effective_opts.is_conditional() {
            let target = self
                .preflight_conditional_target(drive_id, parent_item_id, file_name, &effective_opts)
                .await?;
            if bytes.is_empty() {
                return self
                    .upload_item_content_put(
                        drive_id,
                        &target.id,
                        file_name,
                        Vec::new(),
                        None,
                        &effective_opts,
                    )
                    .await;
            }
            effective_opts.target_item_id = Some(target.id);
        } else if effective_opts.conflict_behavior == Some(ConflictBehavior::Fail) {
            self.preflight_fail_if_exists(drive_id, parent_item_id, file_name)
                .await?;
        }
        if bytes.is_empty() {
            if let Some(item_id) = effective_opts
                .target_item_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                return self
                    .upload_item_content_put(
                        drive_id,
                        item_id,
                        file_name,
                        Vec::new(),
                        None,
                        &effective_opts,
                    )
                    .await;
            }
            return self
                .upload_file_simple_put(
                    drive_id,
                    parent_item_id,
                    file_name,
                    Vec::new(),
                    None,
                    &effective_opts,
                )
                .await;
        }
        self.upload_bytes_chunked_with_options_verified(
            drive_id,
            parent_item_id,
            file_name,
            bytes,
            chunk_size,
            &effective_opts,
        )
        .await
    }

    async fn upload_bytes_chunked_with_options_verified(
        &self,
        drive_id: &str,
        parent_item_id: Option<&str>,
        file_name: &str,
        bytes: &[u8],
        chunk_size: usize,
        options: &UploadOptions,
    ) -> Result<DriveItem> {
        let aligned_chunk = align_chunk_size(chunk_size);
        let behavior = options
            .conflict_behavior
            .unwrap_or(ConflictBehavior::Replace)
            .as_str();
        let mut session_headers: Vec<(&str, String)> = Vec::new();
        if let Some(etag) = options.if_match.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            session_headers.push(("If-Match", etag.to_string()));
        }
        let session = self
            .create_upload_session_with_headers(
                drive_id,
                parent_item_id,
                options.target_item_id.as_deref(),
                file_name,
                behavior,
                &session_headers,
            )
            .await
            .map_err(|err| annotate_upload_conflict(err, file_name, options))?;
        let total = bytes.len() as u64;
        let mut offset = 0usize;
        while offset < bytes.len() {
            let end = (offset + aligned_chunk).min(bytes.len());
            let slice = &bytes[offset..end];
            let (status, val) = self
                .put_upload_chunk(
                    &session.upload_url,
                    slice,
                    offset as u64,
                    (end - 1) as u64,
                    total,
                )
                .await
                .map_err(|err| annotate_upload_conflict(err, file_name, options))?;
            if status == StatusCode::OK || status == StatusCode::CREATED {
                let item: GraphDriveItem = serde_json::from_value(val)
                    .context("Failed to deserialize completed chunked upload DriveItem")?;
                return Ok(item.into_model(drive_id));
            }
            offset = end;
        }
        bail!("Resumable upload session completed all chunks without returning a DriveItem");
    }

    pub async fn upload_bytes(
        &self,
        drive_id: &str,
        parent_item_id: Option<&str>,
        file_name: &str,
        bytes: Vec<u8>,
        content_type: Option<&str>,
    ) -> Result<DriveItem> {
        self.upload_bytes_with_options(
            drive_id,
            parent_item_id,
            file_name,
            bytes,
            content_type,
            &UploadOptions::default(),
        )
        .await
    }

    pub async fn upload_bytes_with_options(
        &self,
        drive_id: &str,
        parent_item_id: Option<&str>,
        file_name: &str,
        bytes: Vec<u8>,
        content_type: Option<&str>,
        options: &UploadOptions,
    ) -> Result<DriveItem> {
        if (bytes.len() as u64) < SIMPLE_UPLOAD_MAX_BYTES {
            self.upload_file_simple_with_options(
                drive_id,
                parent_item_id,
                file_name,
                bytes,
                content_type,
                options,
            )
            .await
        } else {
            self.upload_bytes_chunked_with_options(
                drive_id,
                parent_item_id,
                file_name,
                &bytes,
                DEFAULT_UPLOAD_CHUNK_BYTES,
                options,
            )
            .await
        }
    }

    pub async fn upload_local_file(
        &self,
        drive_id: &str,
        parent_item_id: Option<&str>,
        remote_file_name: Option<&str>,
        local_path: &Path,
    ) -> Result<DriveItem> {
        self.upload_local_file_with_options(
            drive_id,
            parent_item_id,
            remote_file_name,
            local_path,
            &UploadOptions::default(),
        )
        .await
    }

    pub async fn upload_local_file_with_options(
        &self,
        drive_id: &str,
        parent_item_id: Option<&str>,
        remote_file_name: Option<&str>,
        local_path: &Path,
        options: &UploadOptions,
    ) -> Result<DriveItem> {
        let meta = tokio::fs::metadata(local_path)
            .await
            .with_context(|| format!("Local file not found: {}", local_path.display()))?;
        if !meta.is_file() {
            bail!("Path is not a regular file: {}", local_path.display());
        }
        let file_name = remote_file_name
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                local_path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .map(str::to_string)
            })
            .ok_or_else(|| anyhow!("Cannot determine file name for {}", local_path.display()))?;

        let total = meta.len();
        if total < SIMPLE_UPLOAD_MAX_BYTES {
            let bytes = tokio::fs::read(local_path)
                .await
                .with_context(|| format!("Failed to read file: {}", local_path.display()))?;
            let ct = guess_mime_type(local_path);
            return self
                .upload_file_simple_with_options(
                    drive_id,
                    parent_item_id,
                    &file_name,
                    bytes,
                    Some(ct),
                    options,
                )
                .await
                .map_err(|err| annotate_local_upload_conflict(err, local_path, options));
        }

        // Streaming chunked upload session for >= 4 MiB files
        let mut effective_opts = options.clone();
        if effective_opts.is_conditional() {
            let target = self
                .preflight_conditional_target(drive_id, parent_item_id, &file_name, &effective_opts)
                .await
                .map_err(|err| annotate_local_upload_conflict(err, local_path, options))?;
            effective_opts.target_item_id = Some(target.id);
        } else if effective_opts.conflict_behavior == Some(ConflictBehavior::Fail) {
            self.preflight_fail_if_exists(drive_id, parent_item_id, &file_name)
                .await
                .map_err(|err| annotate_local_upload_conflict(err, local_path, options))?;
        }

        let behavior = effective_opts
            .conflict_behavior
            .unwrap_or(ConflictBehavior::Replace)
            .as_str();
        let mut session_headers: Vec<(&str, String)> = Vec::new();
        if let Some(etag) = effective_opts
            .if_match
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            session_headers.push(("If-Match", etag.to_string()));
        }
        let session = self
            .create_upload_session_with_headers(
                drive_id,
                parent_item_id,
                effective_opts.target_item_id.as_deref(),
                &file_name,
                behavior,
                &session_headers,
            )
            .await
            .map_err(|err| annotate_local_upload_conflict(err, local_path, &effective_opts))?;
        let chunk_cap = DEFAULT_UPLOAD_CHUNK_BYTES;
        let mut file = tokio::fs::File::open(local_path)
            .await
            .with_context(|| format!("Failed to open file for upload: {}", local_path.display()))?;
        let mut buf = vec![0u8; chunk_cap];
        let mut offset: u64 = 0;

        while offset < total {
            let mut filled = 0usize;
            let remaining = (total - offset).min(chunk_cap as u64) as usize;
            while filled < remaining {
                let n = file.read(&mut buf[filled..remaining]).await?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            if filled == 0 {
                break;
            }
            let end_inclusive = offset + filled as u64 - 1;
            let (status, val) = self
                .put_upload_chunk(&session.upload_url, &buf[..filled], offset, end_inclusive, total)
                .await
                .map_err(|err| annotate_local_upload_conflict(err, local_path, &effective_opts))?;
            if status == StatusCode::OK || status == StatusCode::CREATED {
                let item: GraphDriveItem = serde_json::from_value(val)
                    .context("Failed to deserialize completed chunked upload DriveItem")?;
                return Ok(item.into_model(drive_id));
            }
            offset += filled as u64;
        }

        bail!("Chunked upload of '{}' finished without DriveItem response", local_path.display())
    }

    // -----------------------------------------------------------------------
    // Folder Creation, Rename/Move, Delete
    // -----------------------------------------------------------------------

    pub async fn create_folder(
        &self,
        drive_id: &str,
        parent_item_id: Option<&str>,
        name: &str,
    ) -> Result<DriveItem> {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            bail!("Folder name cannot be empty.");
        }
        let enc_drive = urlencoding::encode(drive_id.trim());
        let endpoint = match parent_item_id.map(str::trim).filter(|s| !s.is_empty() && *s != "root") {
            Some(pid) => {
                let enc_parent = urlencoding::encode(pid);
                format!("/drives/{enc_drive}/items/{enc_parent}/children")
            }
            None => format!("/drives/{enc_drive}/root/children"),
        };
        let body = json!({
            "name": trimmed,
            "folder": {},
            "@microsoft.graph.conflictBehavior": "rename"
        });
        let raw: GraphDriveItem = self.post_json(&endpoint, &body).await?;
        Ok(raw.into_model(drive_id))
    }

    pub async fn get_drive_root_id(&self, drive_id: &str) -> Result<String> {
        let enc_drive = urlencoding::encode(drive_id.trim());
        let raw: GraphDriveItem = self
            .get_json(&format!("/drives/{enc_drive}/root"))
            .await?;
        let id = raw.id.trim();
        if id.is_empty() {
            bail!("Graph /drives/{drive_id}/root response returned an empty root folder id.");
        }
        Ok(id.to_string())
    }

    pub async fn rename_or_move_item(
        &self,
        drive_id: &str,
        item_id: &str,
        new_name: Option<&str>,
        new_parent_id: Option<&str>,
    ) -> Result<DriveItem> {
        let mut patch = serde_json::Map::new();
        if let Some(name) = new_name.map(str::trim).filter(|s| !s.is_empty()) {
            patch.insert("name".to_string(), json!(name));
        }
        if let Some(pid) = new_parent_id.map(str::trim).filter(|s| !s.is_empty()) {
            let resolved_pid = if pid == "/" || pid.eq_ignore_ascii_case("root") {
                self.get_drive_root_id(drive_id).await?
            } else {
                pid.to_string()
            };
            patch.insert("parentReference".to_string(), json!({ "id": resolved_pid }));
        }
        if patch.is_empty() {
            bail!("Must specify at least a new name or a destination parent folder ID.");
        }
        let enc_drive = urlencoding::encode(drive_id.trim());
        let enc_item = urlencoding::encode(item_id.trim());
        let endpoint = format!("/drives/{enc_drive}/items/{enc_item}");
        let body = serde_json::Value::Object(patch);
        let updated: Option<GraphDriveItem> = self.patch_json_optional(&endpoint, &body).await?;
        match updated {
            Some(raw) => Ok(raw.into_model(drive_id)),
            None => self.get_item(drive_id, item_id).await,
        }
    }

    pub async fn delete_item(&self, drive_id: &str, item_id: &str) -> Result<()> {
        let enc_drive = urlencoding::encode(drive_id.trim());
        let enc_item = urlencoding::encode(item_id.trim());
        self.delete_req(&format!("/drives/{enc_drive}/items/{enc_item}"))
            .await
    }

    // -----------------------------------------------------------------------
    // Search (Per-Drive & Cross-Site)
    // -----------------------------------------------------------------------

    pub async fn search_drive(
        &self,
        drive_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<DriveItem>> {
        let trimmed = query.trim();
        if trimmed.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let enc_drive = urlencoding::encode(drive_id.trim());
        let escaped_q = trimmed.replace('\'', "''");
        let enc_q = urlencoding::encode(&escaped_q);
        let mut out = Vec::new();
        let mut next: Option<String> = Some(format!(
            "/drives/{enc_drive}/root/search(q='{enc_q}')?$top={}",
            limit.clamp(1, 200)
        ));
        while let Some(url) = next {
            let page: GraphListPage<GraphDriveItem> = self.get_json(&url).await?;
            for item in page.value {
                out.push(item.into_model(drive_id));
                if out.len() >= limit {
                    return Ok(out);
                }
            }
            next = page.next_link;
        }
        Ok(out)
    }

    /// Cross-site Microsoft Search API (`POST /search/query` for `driveItem`).
    pub async fn search_across_sites(&self, query: &str, limit: usize) -> Result<Vec<DriveItem>> {
        let trimmed = query.trim();
        if trimmed.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let body = json!({
            "requests": [{
                "entityTypes": ["driveItem"],
                "query": { "queryString": trimmed },
                "from": 0,
                "size": limit.clamp(1, 100)
            }]
        });
        let resp: GraphSearchResponse = self.post_json("/search/query", &body).await?;
        let mut out = Vec::new();
        for set in resp.value {
            for container in set.hits_containers {
                for hit in container.hits {
                    if let Some(res) = hit.resource {
                        out.push(res.into_model("search"));
                        if out.len() >= limit {
                            return Ok(out);
                        }
                    }
                }
            }
        }
        Ok(out)
    }
}

/// How many `name N.ext` candidates a zero-length `rename` upload probes.
const MAX_RENAME_CANDIDATES: usize = 100;

/// `notes.txt` -> `notes 1.txt`, `README` -> `README 1`, `.env` -> `.env 1`: the suffix
/// goes before the last extension, as in the names Graph generates when it renames.
fn renamed_file_name(file_name: &str, n: usize) -> String {
    let name = file_name.trim();
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => format!("{stem} {n}.{ext}"),
        _ => format!("{name} {n}"),
    }
}

fn is_not_found_error(err: &anyhow::Error) -> bool {
    let lower = format!("{err:#}").to_ascii_lowercase();
    lower.contains("404") || lower.contains("itemnotfound") || lower.contains("not found")
}

fn is_list_item_expand_unsupported(err: &anyhow::Error) -> bool {
    let lower = format!("{err:#}").to_ascii_lowercase();
    lower.contains("400 bad request")
        || lower.contains("501 not implemented")
        || lower.contains("notsupported")
        || lower.contains("not supported")
        || lower.contains("invalidrequest")
        || lower.contains("badrequest")
}

fn align_chunk_size(requested: usize) -> usize {
    if requested < UPLOAD_CHUNK_GRANULARITY {
        return requested.max(1);
    }
    (requested / UPLOAD_CHUNK_GRANULARITY) * UPLOAD_CHUNK_GRANULARITY
}

fn annotate_upload_conflict(
    err: anyhow::Error,
    file_name: &str,
    options: &UploadOptions,
) -> anyhow::Error {
    if !is_precondition_failed_error(&err) {
        return err;
    }
    if let Some(etag) = options.if_match.as_deref() {
        anyhow!(
            "Upload conflict (412 Precondition Failed) for '{file_name}': remote file changed since expected version ({etag}). Re-read/merge the latest remote file or pass --conflict replace to overwrite. Cause: {err:#}"
        )
    } else if options.conflict_behavior == Some(ConflictBehavior::Fail) {
        anyhow!(
            "Upload conflict (412 Precondition Failed) for '{file_name}': target file already exists and conflict behavior is 'fail'. Pass --conflict replace or --conflict rename. Cause: {err:#}"
        )
    } else {
        err
    }
}

fn annotate_local_upload_conflict(
    err: anyhow::Error,
    local_path: &Path,
    options: &UploadOptions,
) -> anyhow::Error {
    if !is_precondition_failed_error(&err) {
        return err;
    }
    let already_annotated = format!("{err:#}").contains("Upload conflict (412 Precondition Failed)");
    if already_annotated {
        anyhow!("{} Local file preserved at '{}'.", err, local_path.display())
    } else if let Some(etag) = options.if_match.as_deref() {
        anyhow!(
            "Upload conflict (412 Precondition Failed): remote file changed since expected version ({etag}). Local file preserved at '{}'; re-download to merge or pass --conflict replace to overwrite. Cause: {err:#}",
            local_path.display()
        )
    } else if options.conflict_behavior == Some(ConflictBehavior::Fail) {
        anyhow!(
            "Upload conflict (412 Precondition Failed): target file already exists and conflict behavior is 'fail'. Local file preserved at '{}'. Cause: {err:#}",
            local_path.display()
        )
    } else {
        err
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_graph_parent_path_and_encode() {
        assert_eq!(normalize_graph_parent_path("/drive/root:"), "");
        assert_eq!(normalize_graph_parent_path("/drives/d1/root:/Projects/Q4%20Plan"), "/Projects/Q4 Plan");
        assert_eq!(normalize_graph_parent_path("/drive/root:/Docs/"), "/Docs");
        assert_eq!(encode_graph_path("/Projects/Q4 Plan/deck.pptx"), "Projects/Q4%20Plan/deck.pptx");
        assert_eq!(encode_graph_path("/"), "");
    }

    #[test]
    fn renamed_file_name_puts_the_suffix_before_the_extension() {
        assert_eq!(renamed_file_name("notes.txt", 1), "notes 1.txt");
        assert_eq!(renamed_file_name("notes.txt", 12), "notes 12.txt");
        assert_eq!(renamed_file_name("archive.tar.gz", 2), "archive.tar 2.gz");
        assert_eq!(renamed_file_name("README", 1), "README 1");
        assert_eq!(renamed_file_name(".env", 1), ".env 1");
        assert_eq!(renamed_file_name("trailing.", 1), "trailing. 1");
        assert_eq!(renamed_file_name("  보고서 v1.docx ", 3), "보고서 v1 3.docx");
    }

    #[tokio::test]
    async fn get_item_and_by_path_expand_list_item_fields_and_fallback_on_400() {
        use std::sync::{Arc, Mutex};
        use axum::{
            extract::{Query, State},
            http::StatusCode as AxumStatus,
            response::IntoResponse,
            routing::get,
            Json, Router,
        };
        use tokio::net::TcpListener;
        use crate::{
            auth::{AuthManager, StoredTokens},
            config::AppConfig,
        };

        let seen_queries: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_clone = seen_queries.clone();

        let app = Router::new()
            .route(
                "/v1.0/drives/sp-drive/items/doc-1",
                get(
                    |State(seen): State<Arc<Mutex<Vec<String>>>>,
                     Query(q): Query<std::collections::HashMap<String, String>>| async move {
                        seen.lock()
                            .unwrap()
                            .push(q.get("$expand").cloned().unwrap_or_default());
                        Json(json!({
                            "id": "doc-1",
                            "name": "Q4-Plan.docx",
                            "description": "Quarterly plan",
                            "size": 4096,
                            "file": {
                                "mimeType": "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                                "hashes": {
                                    "sha256Hash": "AABBCCDD",
                                    "quickXorHash": "Q123=="
                                }
                            },
                            "listItem": {
                                "id": "7",
                                "fields": {
                                    "@odata.etag": "\"7,1\"",
                                    "Owner@odata.type": "#String",
                                    "Status": "Approved",
                                    "Owner": "Alice",
                                    "Priority": 2,
                                    "Reviewed": true,
                                    "EmptyStr": "   ",
                                    "NullField": null,
                                    "EmptyArr": [],
                                    "EmptyObj": {}
                                }
                            }
                        }))
                    },
                ),
            )
            .route(
                "/v1.0/drives/personal-drive/items/od-1",
                get(
                    |State(seen): State<Arc<Mutex<Vec<String>>>>,
                     Query(q): Query<std::collections::HashMap<String, String>>| async move {
                        let expand = q.get("$expand").cloned().unwrap_or_default();
                        seen.lock().unwrap().push(expand.clone());
                        if !expand.is_empty() {
                            return (
                                AxumStatus::BAD_REQUEST,
                                Json(json!({
                                    "error": {
                                        "code": "invalidRequest",
                                        "message": "expand listItem is not supported on personal drives"
                                    }
                                })),
                            )
                                .into_response();
                        }
                        (
                            AxumStatus::OK,
                            Json(json!({
                                "id": "od-1",
                                "name": "notes.txt",
                                "description": "Personal notes",
                                "size": 128,
                                "file": {
                                    "mimeType": "text/plain",
                                    "hashes": { "sha256Hash": "112233" }
                                }
                            })),
                        )
                            .into_response()
                    },
                ),
            )
            .route(
                "/v1.0/drives/personal-drive/root:/Docs/notes.txt:",
                get(
                    |State(seen): State<Arc<Mutex<Vec<String>>>>,
                     Query(q): Query<std::collections::HashMap<String, String>>| async move {
                        let expand = q.get("$expand").cloned().unwrap_or_default();
                        seen.lock().unwrap().push(expand.clone());
                        if !expand.is_empty() {
                            return (
                                AxumStatus::BAD_REQUEST,
                                Json(json!({
                                    "error": { "code": "notSupported", "message": "not supported" }
                                })),
                            )
                                .into_response();
                        }
                        (
                            AxumStatus::OK,
                            Json(json!({
                                "id": "od-2",
                                "name": "notes.txt",
                                "size": 64
                            })),
                        )
                            .into_response()
                    },
                ),
            )
            .with_state(seen_clone);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let cfg = AppConfig {
            graph_base_url: format!("{base}/v1.0"),
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

        // 1. SharePoint Document Library item returns custom columns, description, and hashes;
        //    internal @odata.* keys and empty/null values are filtered out.
        let sp_item = graph.get_item("sp-drive", "doc-1").await.unwrap();
        assert_eq!(sp_item.description.as_deref(), Some("Quarterly plan"));
        assert_eq!(sp_item.sha256_hash.as_deref(), Some("AABBCCDD"));
        assert_eq!(sp_item.quick_xor_hash.as_deref(), Some("Q123=="));
        assert_eq!(
            sp_item.list_item_fields.keys().cloned().collect::<Vec<_>>(),
            vec!["Owner", "Priority", "Reviewed", "Status"]
        );
        assert_eq!(sp_item.list_item_fields["Status"], json!("Approved"));
        assert_eq!(sp_item.list_item_fields["Owner"], json!("Alice"));
        assert_eq!(sp_item.list_item_fields["Priority"], json!(2));
        assert_eq!(sp_item.list_item_fields["Reviewed"], json!(true));

        // 2. Personal drive returning 400 on $expand transparently falls back to unexpanded GET.
        let od_item = graph.get_item("personal-drive", "od-1").await.unwrap();
        assert_eq!(od_item.id, "od-1");
        assert_eq!(od_item.description.as_deref(), Some("Personal notes"));
        assert_eq!(od_item.sha256_hash.as_deref(), Some("112233"));
        assert!(od_item.list_item_fields.is_empty());

        // 3. get_item_by_path also transparently falls back on 400.
        let od_by_path = graph
            .get_item_by_path("personal-drive", "/Docs/notes.txt")
            .await
            .unwrap();
        assert_eq!(od_by_path.id, "od-2");

        let recorded = seen_queries.lock().unwrap().clone();
        assert_eq!(
            recorded,
            vec![
                "listItem($expand=fields)",
                "listItem($expand=fields)",
                "",
                "listItem($expand=fields)",
                "",
            ]
        );

        server.abort();
    }
}
