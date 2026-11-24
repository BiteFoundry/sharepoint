//! Shared domain models and wire-independent contracts across Graph, Storage, Sync, TUI, CLI, HTTP, and MCP.

use std::{collections::BTreeMap, path::PathBuf};

use serde::{Deserialize, Serialize};

/// Sentinel summary emitted via `CoreEvent::SyncStateChanged { busy: false, summary }`
/// while background polling is silently retrying transient errors.
pub const POLL_RETRY_SUMMARY: &str = "__poll_retrying__";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserProfile {
    pub id: String,
    pub display_name: String,
    pub email: Option<String>,
    pub job_title: Option<String>,
    pub department: Option<String>,
}

// ---------------------------------------------------------------------------
// SharePoint Sites & OneDrive Drives Domain
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharePointSite {
    pub id: String,
    pub name: String,
    pub display_name: String,
    pub description: Option<String>,
    pub web_url: Option<String>,
    pub is_followed: bool,
    pub created_at: Option<String>,
    pub last_modified_at: Option<String>,
}

impl SharePointSite {
    pub fn effective_name(&self) -> &str {
        let disp = self.display_name.trim();
        if !disp.is_empty() {
            disp
        } else if !self.name.trim().is_empty() {
            self.name.trim()
        } else {
            &self.id
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveInfo {
    pub id: String,
    pub name: String,
    /// `personal`, `business`, or `documentLibrary`.
    pub drive_type: String,
    pub site_id: Option<String>,
    pub site_name: Option<String>,
    pub web_url: Option<String>,
    pub owner_name: Option<String>,
    pub quota_total: Option<u64>,
    pub quota_used: Option<u64>,
    pub quota_remaining: Option<u64>,
    pub quota_state: Option<String>,
}

impl DriveInfo {
    pub fn is_personal_onedrive(&self) -> bool {
        self.site_id.is_none()
            && matches!(
                self.drive_type.to_ascii_lowercase().as_str(),
                "personal" | "business" | ""
            )
    }

    pub fn display_label(&self) -> String {
        let name = self.name.trim();
        if let Some(site) = self
            .site_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if name.is_empty() {
                site.to_string()
            } else {
                format!("{site} / {name}")
            }
        } else if !name.is_empty() {
            name.to_string()
        } else if self.is_personal_onedrive() {
            "OneDrive".to_string()
        } else {
            self.id.clone()
        }
    }
}

/// Markup document format eligible for rich terminal rendering in the TUI inspector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MarkupKind {
    Markdown,
    Html,
}

impl MarkupKind {
    pub fn detect(item: &DriveItem) -> Option<Self> {
        item.markup_kind()
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Markdown => "Markdown",
            Self::Html => "HTML",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveItem {
    pub id: String,
    pub drive_id: String,
    pub parent_id: Option<String>,
    /// Clean logical folder path relative to drive root (e.g. `""` or `"/Projects/Q4"`).
    pub parent_path: String,
    pub name: String,
    pub size: u64,
    pub is_folder: bool,
    pub child_count: Option<u32>,
    pub mime_type: Option<String>,
    pub web_url: Option<String>,
    /// Optional pre-authenticated `@microsoft.graph.downloadUrl`.
    #[serde(skip_serializing)]
    pub download_url: Option<String>,
    pub e_tag: Option<String>,
    pub c_tag: Option<String>,
    pub created_at: Option<String>,
    pub last_modified_at: Option<String>,
    pub created_by: Option<String>,
    pub last_modified_by: Option<String>,
    pub shared_scope: Option<String>,
    /// Populated for `sharedWithMe` / remote shortcut items pointing into another drive.
    pub remote_drive_id: Option<String>,
    pub remote_item_id: Option<String>,
    /// Optional item description from Microsoft Graph `driveItem.description`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// SHA-256 content hash from `file.hashes.sha256Hash` when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256_hash: Option<String>,
    /// QuickXorHash content hash from `file.hashes.quickXorHash` when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quick_xor_hash: Option<String>,
    /// Populated custom SharePoint Document Library metadata columns from `listItem.fields`
    /// (excluding internal `@odata.*` metadata keys and null/empty values).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub list_item_fields: BTreeMap<String, serde_json::Value>,
}

impl DriveItem {
    pub fn effective_drive_id(&self) -> &str {
        self.remote_drive_id
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(&self.drive_id)
    }

    pub fn effective_item_id(&self) -> &str {
        self.remote_item_id
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(&self.id)
    }

    /// Revision stamp tracking content changes via `cTag` when present,
    /// falling back to `eTag`/modification time/size when `cTag` is omitted (e.g. ODfB delta).
    pub fn content_revision(&self) -> String {
        if let Some(ctag) = self
            .c_tag
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            format!("ctag:{ctag}|{}", self.size)
        } else {
            format!(
                "meta:{}|{}|{}",
                self.e_tag.as_deref().unwrap_or(""),
                self.last_modified_at.as_deref().unwrap_or(""),
                self.size
            )
        }
    }

    pub fn extension(&self) -> Option<String> {
        if self.is_folder {
            return None;
        }
        std::path::Path::new(&self.name)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
    }

    pub fn display_path(&self) -> String {
        let p = self.parent_path.trim().trim_end_matches('/');
        if p.is_empty() || p == "/" {
            format!("/{}", self.name)
        } else if p.starts_with('/') {
            format!("{p}/{}", self.name)
        } else {
            format!("/{p}/{}", self.name)
        }
    }

    pub fn is_image(&self) -> bool {
        if self.is_folder {
            return false;
        }
        crate::img_preview::is_image_mime_or_name(
            self.mime_type.as_deref().unwrap_or(""),
            Some(&self.name),
        )
    }

    pub fn markup_kind(&self) -> Option<MarkupKind> {
        if self.is_folder {
            return None;
        }
        if let Some(ext) = self.extension() {
            match ext.as_str() {
                "md" | "markdown" | "mdown" | "mkd" | "mdx" => return Some(MarkupKind::Markdown),
                "html" | "htm" | "xhtml" => return Some(MarkupKind::Html),
                _ => {}
            }
        }
        if let Some(mime) = self.mime_type.as_deref() {
            let essence = mime
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            match essence.as_str() {
                "text/markdown" | "text/x-markdown" => return Some(MarkupKind::Markdown),
                "text/html" | "application/xhtml+xml" => return Some(MarkupKind::Html),
                _ => {}
            }
        }
        None
    }

    pub fn is_text_previewable(&self) -> bool {
        if self.is_folder || self.is_image() {
            return false;
        }
        if self.markup_kind().is_some() {
            return true;
        }
        if let Some(mime) = self.mime_type.as_deref() {
            let lower = mime.to_ascii_lowercase();
            if lower.starts_with("text/")
                || lower.contains("json")
                || lower.contains("yaml")
                || lower.contains("xml")
                || lower.contains("toml")
                || lower.contains("javascript")
                || lower.contains("typescript")
                || lower.contains("x-sh")
                || lower.contains("sql")
                || lower.contains("csv")
            {
                return true;
            }
        }
        matches!(
            self.extension().as_deref(),
            Some(
                "txt"
                    | "md"
                    | "markdown"
                    | "mdown"
                    | "mkd"
                    | "mdx"
                    | "json"
                    | "jsonl"
                    | "yaml"
                    | "yml"
                    | "toml"
                    | "ini"
                    | "cfg"
                    | "conf"
                    | "csv"
                    | "tsv"
                    | "xml"
                    | "html"
                    | "htm"
                    | "xhtml"
                    | "css"
                    | "scss"
                    | "rs"
                    | "py"
                    | "js"
                    | "mjs"
                    | "cjs"
                    | "ts"
                    | "tsx"
                    | "jsx"
                    | "go"
                    | "java"
                    | "kt"
                    | "c"
                    | "h"
                    | "cpp"
                    | "hpp"
                    | "cs"
                    | "rb"
                    | "php"
                    | "sh"
                    | "bash"
                    | "zsh"
                    | "ps1"
                    | "bat"
                    | "cmd"
                    | "sql"
                    | "log"
                    | "env"
                    | "gitignore"
                    | "dockerfile"
                    | "makefile"
            )
        )
    }

    pub fn type_badge(&self) -> &'static str {
        if self.is_folder {
            return "DIR";
        }
        if self.is_image() {
            return "IMG";
        }
        match self.extension().as_deref() {
            Some("pdf") => "PDF",
            Some("md" | "markdown" | "mdown" | "mkd" | "mdx") => "MD",
            Some("doc" | "docx" | "rtf" | "odt") => "DOC",
            Some("xls" | "xlsx" | "ods") => "XLS",
            Some("ppt" | "pptx" | "odp") => "PPT",
            Some("csv" | "tsv") => "CSV",
            Some("json" | "jsonl" | "yaml" | "yml" | "toml" | "xml") => "DAT",
            Some("zip" | "tar" | "gz" | "7z" | "rar") => "ZIP",
            Some("mp4" | "mov" | "mkv" | "webm" | "avi") => "VID",
            Some("mp3" | "wav" | "flac" | "ogg" | "m4a") => "AUD",
            Some(
                "rs" | "py" | "js" | "ts" | "tsx" | "jsx" | "go" | "java" | "c" | "cpp" | "cs"
                | "sh" | "ps1" | "sql" | "html" | "htm" | "xhtml" | "css",
            ) => "CODE",
            Some("txt" | "log") => "TXT",
            _ => "FILE",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListColumn {
    pub id: String,
    pub name: String,
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub read_only: Option<bool>,
    pub hidden: Option<bool>,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column_type: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveItemVersion {
    pub id: String,
    pub last_modified_at: Option<String>,
    pub last_modified_by: Option<String>,
    pub size: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareLinkResult {
    pub id: String,
    pub web_url: String,
    pub link_type: String,
    pub scope: String,
    pub expiration_date_time: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadSession {
    pub upload_url: String,
    pub expiration_date_time: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveSearchHit {
    pub item: DriveItem,
    pub snippet: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadedFile {
    pub ok: bool,
    pub path: PathBuf,
    pub file_name: String,
    pub mime_type: String,
    pub size_bytes: u64,
    pub is_image: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drive_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub e_tag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub c_tag: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadBaseline {
    pub local_path: String,
    pub drive_id: String,
    pub item_id: String,
    pub parent_id: Option<String>,
    pub parent_path: String,
    pub remote_name: String,
    pub e_tag: Option<String>,
    pub c_tag: Option<String>,
    pub last_modified_at: Option<String>,
    pub downloaded_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedTarget {
    pub key: String,
    /// `"site"` or `"folder"`.
    pub kind: String,
    pub drive_id: Option<String>,
    pub item_id: Option<String>,
    pub site_id: Option<String>,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeNode {
    pub item: DriveItem,
    pub children: Vec<TreeNode>,
    pub has_more: Option<bool>,
}

// ---------------------------------------------------------------------------
// Sync & Notification Events
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncCursor {
    pub scope_key: String,
    pub delta_link: Option<String>,
    pub history_next_link: Option<String>,
    pub last_synced_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VirtualListKind {
    Recent,
    SharedWithMe,
}

impl VirtualListKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Recent => "Recent",
            Self::SharedWithMe => "Shared with Me",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum VirtualListAvailability {
    Available,
    Unavailable { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtualListStatus {
    pub availability: VirtualListAvailability,
    pub last_checked_at: String,
}

impl VirtualListStatus {
    pub fn is_available(&self) -> bool {
        matches!(self.availability, VirtualListAvailability::Available)
    }
}

/// Asynchronous events pushed from `SyncEngine` to TUI or live subscribers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum CoreEvent {
    ProfileLoaded(UserProfile),
    MyDriveLoaded(DriveInfo),
    SitesUpdated(Vec<SharePointSite>),
    DrivesUpdated {
        site_id: String,
        drives: Vec<DriveInfo>,
    },
    FolderItemsUpdated {
        drive_id: String,
        parent_id: Option<String>,
        items: Vec<DriveItem>,
        /// True when the folder holds more rows than `items` carries.
        /// Defaults to false so frames from a daemon without this field still parse.
        #[serde(default)]
        truncated: bool,
    },
    RecentItemsUpdated(Vec<DriveItem>),
    SharedItemsUpdated(Vec<DriveItem>),
    VirtualListStatusUpdated {
        kind: VirtualListKind,
        status: VirtualListStatus,
    },
    SyncStateChanged {
        busy: bool,
        summary: String,
    },
    SyncError(String),
}

// ---------------------------------------------------------------------------
// Self-Updater Model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateCheckResult {
    pub current_version: String,
    pub latest_version: String,
    pub update_available: bool,
    pub release_url: String,
    pub asset_name: Option<String>,
    pub asset_size: Option<u64>,
    pub updated: bool,
    pub binary_path: PathBuf,
}

pub fn format_human_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.1} GiB", b / GIB)
    } else if b >= MIB {
        format!("{:.1} MiB", b / MIB)
    } else if b >= KIB {
        format!("{:.1} KiB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_event_folder_items_updated_deserializes_without_truncated() {
        // A daemon built before the field existed sends the event without it.
        let old_frame = serde_json::json!({
            "event": "folder_items_updated",
            "data": { "drive_id": "d1", "parent_id": null, "items": [] }
        });
        let event: CoreEvent = serde_json::from_value(old_frame).expect("old frame parses");
        match event {
            CoreEvent::FolderItemsUpdated { drive_id, truncated, .. } => {
                assert_eq!(drive_id, "d1");
                assert!(!truncated);
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let new_frame = serde_json::to_value(CoreEvent::FolderItemsUpdated {
            drive_id: "d1".into(),
            parent_id: None,
            items: Vec::new(),
            truncated: true,
        })
        .unwrap();
        assert_eq!(new_frame["data"]["truncated"], serde_json::json!(true));
    }

    #[test]
    fn test_drive_item_helpers_and_badges() {
        let img = DriveItem {
            id: "i1".into(),
            drive_id: "d1".into(),
            parent_path: "/Designs".into(),
            name: "mockup.png".into(),
            size: 2048,
            mime_type: Some("image/png".into()),
            ..Default::default()
        };
        assert!(img.is_image());
        assert!(!img.is_text_previewable());
        assert_eq!(img.type_badge(), "IMG");
        assert_eq!(img.display_path(), "/Designs/mockup.png");

        let md = DriveItem {
            id: "i2".into(),
            drive_id: "d1".into(),
            parent_path: "".into(),
            name: "README.md".into(),
            size: 512,
            ..Default::default()
        };
        assert!(!md.is_image());
        assert!(md.is_text_previewable());
        assert_eq!(md.type_badge(), "MD");
        assert_eq!(md.display_path(), "/README.md");

        let folder = DriveItem {
            id: "f1".into(),
            drive_id: "d1".into(),
            name: "Reports".into(),
            is_folder: true,
            ..Default::default()
        };
        assert_eq!(folder.type_badge(), "DIR");
        assert!(!folder.is_image());
        assert!(!folder.is_text_previewable());
        assert_eq!(format_human_size(1536), "1.5 KiB");
    }

    #[test]
    fn test_core_event_tagged_serialization() {
        let evt = CoreEvent::SyncStateChanged {
            busy: false,
            summary: POLL_RETRY_SUMMARY.to_string(),
        };
        let json = serde_json::to_value(&evt).unwrap();
        assert_eq!(json["event"], "sync_state_changed");
        assert_eq!(json["data"]["busy"], false);
        assert_eq!(json["data"]["summary"], POLL_RETRY_SUMMARY);
    }
}
