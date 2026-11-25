//! Application state (`AppState`), UI-only state (`UiState`), and async UI task messages (`UiMsg`).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bitefoundry_html::{sniff_content_kind, ContentKind, NativeHtmlServer};
use ratatui::layout::Rect;
use tokio::sync::mpsc;

use crate::auth::AuthManager;
use crate::cli::DEFAULT_TEXT_PREVIEW_MAX_BYTES;
use crate::config::{AppConfig, DensityMode, ImageProtocolPreference, Persona, ThemePreset};
use crate::graph::GraphClient;
use crate::img_preview::{self, RenderedTerminalImage, TerminalGraphicsCaps, TerminalImageProtocol};
use crate::models::{
    CoreEvent, DownloadedFile, DriveInfo, DriveItem, DriveItemVersion, PinnedTarget,
    ShareLinkResult, SharePointSite, UserProfile, VirtualListKind, VirtualListStatus,
    POLL_RETRY_SUMMARY,
};
use crate::storage::Storage;
use crate::sync::{
    FocusTarget, FocusViewKind, SyncController, FOLDER_PAGE_SIZE, MAX_FOLDER_ITEMS,
};

use super::hitmap::HitMap;
use super::keymap::Keymap;
use super::markup::CachedMarkupPreview;
use super::notification;
use super::overlays::html_preview::HtmlPreviewOverlay;
use super::overlays::Overlay;
use super::theme::Theme;
use super::toast::{TaskBoard, ToastKind, ToastQueue};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Nav,
    List,
    Inspector,
    Overlay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NarrowPane {
    Sidebar,
    #[default]
    Files,
    Inspector,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortMode {
    #[default]
    NameAsc,
    ModifiedDesc,
    SizeDesc,
    TypeAsc,
}

impl SortMode {
    pub fn next(self) -> Self {
        match self {
            Self::NameAsc => Self::ModifiedDesc,
            Self::ModifiedDesc => Self::SizeDesc,
            Self::SizeDesc => Self::TypeAsc,
            Self::TypeAsc => Self::NameAsc,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::NameAsc => "Name",
            Self::ModifiedDesc => "Modified",
            Self::SizeDesc => "Size",
            Self::TypeAsc => "Type",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActiveViewSource {
    OneDriveRoot,
    Recent,
    SharedWithMe,
    Drive { drive_id: String, label: String },
    SearchResult { drive_id: String, label: String },
}

#[derive(Debug, Clone, Default)]
pub struct ScrollState {
    pub selected_idx: Option<usize>,
    pub area: Rect,
    pub total_lines: usize,
    pub offset: usize,
}

impl ScrollState {
    pub fn scroll_up(&mut self, lines: usize) {
        self.offset = self.offset.saturating_sub(lines);
    }

    pub fn scroll_down(&mut self, lines: usize, viewport_height: usize) {
        let max_off = self.total_lines.saturating_sub(viewport_height);
        self.offset = self.offset.saturating_add(lines).min(max_off);
    }

    pub fn top(&mut self) {
        self.offset = 0;
    }

    pub fn bottom(&mut self, viewport_height: usize) {
        self.offset = self.total_lines.saturating_sub(viewport_height);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPixelGraphic {
    pub cache_key: String,
    pub rect: Rect,
    pub protocol: TerminalImageProtocol,
}

pub enum UiMsg {
    BootstrapLoaded {
        my_drive: Option<DriveInfo>,
        sites: Vec<SharePointSite>,
        drives: Vec<DriveInfo>,
        pins: Vec<PinnedTarget>,
    },
    RecentLoaded(Vec<DriveItem>),
    SharedLoaded(Vec<DriveItem>),
    VirtualListLoaded {
        seq: u64,
        kind: VirtualListKind,
        items: Vec<DriveItem>,
        status: VirtualListStatus,
    },
    SiteDrivesLoaded {
        site_id: String,
        drives: Vec<DriveInfo>,
    },
    TextPreviewLoaded {
        drive_id: String,
        item_id: String,
        revision: Option<String>,
        content: String,
    },
    VersionsLoaded {
        cache_key: String,
        item_id: String,
        versions: Vec<DriveItemVersion>,
    },
    VersionsFailed {
        cache_key: String,
    },
    PreviewFailed {
        drive_id: String,
        item_id: String,
        revision: Option<String>,
        inflight_key: Option<String>,
        error: String,
    },
    ImageBytesFetched {
        cache_key: String,
        result: Result<Arc<[u8]>, String>,
    },
    ImageDecoded {
        cache_key: String,
        rendered: RenderedTerminalImage,
    },
    HtmlContentSniffed {
        drive_id: String,
        item_id: String,
        title: String,
        html: String,
        cache_keys: Vec<String>,
    },
    HtmlPreviewReady {
        drive_id: String,
        item_id: String,
        revision: Option<String>,
        title: String,
        html: String,
    },
    HtmlServed {
        url: String,
    },
    ItemDownloaded(DownloadedFile),
    ItemUploaded(DriveItem),
    FolderCreated(DriveItem),
    ItemRenamed(DriveItem),
    ItemDeleted {
        drive_id: String,
        item_id: String,
        raw_drive_id: Option<String>,
        raw_item_id: Option<String>,
        name: String,
    },
    ShareLinkCreated {
        item_id: String,
        link: ShareLinkResult,
    },
    SearchResults {
        seq: u64,
        items: Vec<DriveItem>,
    },
    UpdateChecked(crate::models::UpdateCheckResult),
    TaskError {
        label: String,
        error: String,
    },
    Toast {
        message: String,
        kind: ToastKind,
    },
}

pub struct UiState {
    pub focus: Focus,
    pub overlays: Vec<Overlay>,
    pub hitmap: HitMap,
    pub toasts: ToastQueue,
    pub tasks: TaskBoard,
    pub keymap: Keymap,
    pub theme: Theme,
    pub theme_preset: ThemePreset,
    pub density: DensityMode,
    pub show_nav_rail: bool,
    pub inline_images: bool,
    pub render_markup: bool,
    pub show_link_urls: bool,
    pub markup_cache: Option<CachedMarkupPreview>,
    pub notify: bool,
    pub nav_selected: usize,
    pub nav_area: Rect,
    pub list_scroll: ScrollState,
    pub inspector_scroll: ScrollState,
    pub narrow_pane: NarrowPane,
    pub collapsed_sections: HashSet<&'static str>,
    pub collapsed_sites: HashSet<String>,
    pub graphics_caps: TerminalGraphicsCaps,
    pub pending_pixel_graphics: Vec<PendingPixelGraphic>,
    pub term_width: u16,
    pub term_height: u16,
    pub spinner_tick: usize,
}

impl UiState {
    pub fn new() -> Self {
        Self::from_config(&AppConfig::default())
    }

    pub fn from_config(cfg: &AppConfig) -> Self {
        let caps = img_preview::detect_graphics_caps(cfg.image_protocol);
        Self {
            focus: Focus::List,
            overlays: Vec::new(),
            hitmap: HitMap::default(),
            toasts: ToastQueue::default(),
            tasks: TaskBoard::default(),
            keymap: Keymap::default_sharepoint(),
            theme: Theme::detect(cfg.theme),
            theme_preset: cfg.theme,
            density: cfg.density,
            show_nav_rail: cfg.show_nav_rail,
            inline_images: cfg.inline_images,
            render_markup: cfg.render_markup,
            show_link_urls: false,
            markup_cache: None,
            notify: cfg.notify,
            nav_selected: 0,
            nav_area: Rect::default(),
            list_scroll: ScrollState::default(),
            inspector_scroll: ScrollState::default(),
            narrow_pane: NarrowPane::Files,
            collapsed_sections: HashSet::new(),
            collapsed_sites: HashSet::new(),
            graphics_caps: caps,
            pending_pixel_graphics: Vec::new(),
            term_width: 120,
            term_height: 36,
            spinner_tick: 0,
        }
    }

    pub fn effective_focus(&self) -> Focus {
        if !self.overlays.is_empty() {
            Focus::Overlay
        } else {
            self.focus
        }
    }

    pub fn accepts_text_input(&self) -> bool {
        self.overlays.last().is_some_and(Overlay::accepts_text)
    }

    pub fn push_overlay(&mut self, overlay: Overlay) {
        self.overlays.push(overlay);
    }

    pub fn pop_overlay(&mut self) -> Option<Overlay> {
        self.overlays.pop()
    }

    pub fn toast(&mut self, message: impl Into<String>, kind: ToastKind) {
        self.toasts.push(super::toast::Toast::new(message, kind));
    }
}

impl Default for UiState {
    fn default() -> Self {
        Self::new()
    }
}

const MAX_RAW_IMAGE_ENTRIES: usize = 8;

struct ImageDecodeRequest {
    cache_key: String,
    cols: u16,
    rows: u16,
    caps: TerminalGraphicsCaps,
}

struct PendingImageDownload {
    item: DriveItem,
    decodes: Vec<ImageDecodeRequest>,
}

pub struct AppState {
    pub config: AppConfig,
    pub persona: Persona,
    pub auth: AuthManager,
    pub graph: GraphClient,
    pub storage: Storage,
    pub sync_ctrl: SyncController,
    pub ui_tx: mpsc::UnboundedSender<UiMsg>,
    pub download_dir: PathBuf,
    pub me: Option<UserProfile>,
    pub my_drive: Option<DriveInfo>,
    pub sites: Vec<SharePointSite>,
    pub site_drives: HashMap<String, Vec<DriveInfo>>,
    pub pins: Vec<PinnedTarget>,
    pub active_source: ActiveViewSource,
    /// True once initial SharePoint persona library selection has occurred or the user explicitly navigated.
    pub initial_persona_library_selected: bool,
    pub recent_status: Option<VirtualListStatus>,
    pub shared_status: Option<VirtualListStatus>,
    /// Stack of `(item_id, display_name)` inside the currently selected drive.
    pub folder_stack: Vec<(String, String)>,
    pub current_items: Vec<DriveItem>,
    /// True when the active folder holds more rows than `current_items`.
    pub current_items_truncated: bool,
    /// Rows asked for in the active folder; "load more" raises it one page at a time.
    pub folder_item_limit: usize,
    /// Folder the truncation state and the limit belong to; a different target resets both.
    listed_folder: Option<(String, Option<String>)>,
    pub sort_mode: SortMode,
    pub loading_folder: bool,
    pub sync_busy: bool,
    pub sync_summary: String,
    pub text_previews: Option<(String, String)>,
    pub text_preview_revision: Option<String>,
    pub item_versions: HashMap<String, Vec<DriveItemVersion>>,
    pub created_share_links: HashMap<String, String>,
    pub preview_errors: HashMap<String, String>,
    raw_image_bytes: VecDeque<(String, Arc<[u8]>)>,
    pub rendered_images: HashMap<String, RenderedTerminalImage>,
    pub inflight_previews: HashSet<String>,
    /// Maps `txt:{drive_id}:{item_id}` -> revision currently being downloaded so inspector & overlays share one fetch.
    pub inflight_text_revisions: HashMap<String, String>,
    /// Tracks `txt:{drive_id}:{item_id}` keys whose in-flight download should also update/open an HTML preview overlay.
    pub pending_html_overlay_fetches: HashSet<String>,
    /// Deduplicates raw image byte downloads across inline and modal views for the same item revision.
    inflight_raw_images: HashMap<String, PendingImageDownload>,
    pub search_results: Vec<DriveItem>,
    /// Monotonic generation guard for local/remote search responses.
    pub search_seq: u64,
    /// Monotonic generation guard for Recent/SharedWithMe virtual list loads.
    pub virtual_list_seq: u64,
    pub active_selected_id: Option<String>,
    /// True once the user has toggled the rendered-preview setting in this session.
    render_markup_user_set: bool,
    /// Lazy localhost Axum HTML server (`0 MB` idle until `o` is pressed).
    pub html_server: NativeHtmlServer,
}

/// What a "load more" request did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadMoreOutcome {
    /// The folder is fully loaded, or the view is not a folder.
    NothingMore,
    /// The folder already holds the most rows the TUI keeps.
    AtMaximum(usize),
    /// A listing with this row limit was requested.
    Requested(usize),
}

impl AppState {
    pub fn new(
        config: AppConfig,
        persona: Persona,
        auth: AuthManager,
        graph: GraphClient,
        storage: Storage,
        sync_ctrl: SyncController,
        ui_tx: mpsc::UnboundedSender<UiMsg>,
    ) -> Self {
        let download_dir = config.resolved_download_dir();
        let active_source = match persona {
            Persona::OneDrive => ActiveViewSource::OneDriveRoot,
            Persona::SharePoint => ActiveViewSource::OneDriveRoot,
        };
        Self {
            config,
            persona,
            auth,
            graph,
            storage,
            sync_ctrl,
            ui_tx,
            download_dir,
            me: None,
            my_drive: None,
            sites: Vec::new(),
            site_drives: HashMap::new(),
            pins: Vec::new(),
            active_source,
            initial_persona_library_selected: false,
            recent_status: None,
            shared_status: None,
            folder_stack: Vec::new(),
            current_items: Vec::new(),
            current_items_truncated: false,
            folder_item_limit: FOLDER_PAGE_SIZE,
            listed_folder: None,
            sort_mode: SortMode::NameAsc,
            loading_folder: false,
            sync_busy: false,
            sync_summary: "Ready".to_string(),
            text_previews: None,
            text_preview_revision: None,
            item_versions: HashMap::new(),
            created_share_links: HashMap::new(),
            preview_errors: HashMap::new(),
            raw_image_bytes: VecDeque::new(),
            rendered_images: HashMap::new(),
            inflight_previews: HashSet::new(),
            inflight_text_revisions: HashMap::new(),
            pending_html_overlay_fetches: HashSet::new(),
            inflight_raw_images: HashMap::new(),
            search_results: Vec::new(),
            search_seq: 0,
            virtual_list_seq: 0,
            active_selected_id: None,
            render_markup_user_set: false,
            html_server: NativeHtmlServer::new(),
        }
    }

    pub fn next_search_seq(&mut self) -> u64 {
        self.search_seq = self.search_seq.wrapping_add(1);
        self.search_seq
    }

    pub fn bump_virtual_list_seq(&mut self) -> u64 {
        self.virtual_list_seq = self.virtual_list_seq.wrapping_add(1);
        self.virtual_list_seq
    }

    pub(crate) fn reset_folder_listing_state(&mut self) {
        self.listed_folder = None;
        self.current_items_truncated = false;
        self.folder_item_limit = FOLDER_PAGE_SIZE;
        self.loading_folder = false;
        self.bump_virtual_list_seq();
    }

    pub fn active_drive_id(&self) -> Option<String> {
        match &self.active_source {
            ActiveViewSource::OneDriveRoot => self.my_drive.as_ref().map(|d| d.id.clone()),
            ActiveViewSource::Drive { drive_id, .. }
            | ActiveViewSource::SearchResult { drive_id, .. } => Some(drive_id.clone()),
            ActiveViewSource::Recent | ActiveViewSource::SharedWithMe => None,
        }
    }

    /// Returns the SharePoint `site_id` owning the currently active library, if any.
    pub fn active_site_id(&self) -> Option<String> {
        let drive_id = self.active_drive_id()?;
        for (site_id, drives) in &self.site_drives {
            if drives.iter().any(|d| d.id == drive_id) {
                return Some(site_id.clone());
            }
        }
        None
    }

    /// Returns `Some((drive_id, parent_id))` only when viewing a real folder (`OneDriveRoot` or `Drive`),
    /// never for virtual views (`Recent` / `SharedWithMe`) or synthetic `SearchResult` views.
    pub fn active_folder_target(&self) -> Option<(String, Option<String>)> {
        match &self.active_source {
            ActiveViewSource::OneDriveRoot | ActiveViewSource::Drive { .. } => {
                let drive_id = self.active_drive_id()?;
                Some((drive_id, self.active_parent_id()))
            }
            ActiveViewSource::Recent
            | ActiveViewSource::SharedWithMe
            | ActiveViewSource::SearchResult { .. } => None,
        }
    }

    pub fn virtual_list_status_for_active(&self) -> Option<(VirtualListKind, Option<&VirtualListStatus>)> {
        match &self.active_source {
            ActiveViewSource::Recent => Some((VirtualListKind::Recent, self.recent_status.as_ref())),
            ActiveViewSource::SharedWithMe => {
                Some((VirtualListKind::SharedWithMe, self.shared_status.as_ref()))
            }
            _ => None,
        }
    }

    pub fn active_parent_id(&self) -> Option<String> {
        self.folder_stack.last().map(|(id, _)| id.clone())
    }

    pub fn breadcrumb_label(&self) -> String {
        let root = match &self.active_source {
            ActiveViewSource::OneDriveRoot => "OneDrive > My Files".to_string(),
            ActiveViewSource::Recent => "OneDrive > Recent".to_string(),
            ActiveViewSource::SharedWithMe => "OneDrive > Shared with Me".to_string(),
            ActiveViewSource::Drive { label, .. } => format!("SharePoint > {label}"),
            ActiveViewSource::SearchResult { label, .. } => label.clone(),
        };
        if self.folder_stack.is_empty() {
            root
        } else {
            let trail: Vec<&str> = self.folder_stack.iter().map(|(_, n)| n.as_str()).collect();
            format!("{root} > {}", trail.join(" > "))
        }
    }

    /// Header note for a folder that holds more rows than are loaded, as `(full, short)`.
    /// The short form is for a header too narrow for the full one.
    pub fn truncation_hint(&self) -> Option<(String, String)> {
        if !self.current_items_truncated || self.active_folder_target().is_none() {
            return None;
        }
        let shown = self.current_items.len();
        if self.folder_item_limit >= MAX_FOLDER_ITEMS {
            return Some((
                format!(" · first {shown} shown · rest: {} ls --limit", self.persona.bin_name()),
                format!(" · {shown}+"),
            ));
        }
        Some((
            format!(" · first {shown} shown · L loads more"),
            format!(" · {shown}+ (L)"),
        ))
    }

    /// Limit to send to the sync engine: `None` while the folder is at the default page, so
    /// the frames match what an older daemon expects.
    fn requested_folder_limit(&self) -> Option<usize> {
        (self.folder_item_limit != FOLDER_PAGE_SIZE).then_some(self.folder_item_limit)
    }

    /// Asks for one more page of the active folder when the last listing was cut short.
    pub fn load_more_items(&mut self, ui: &mut UiState) -> LoadMoreOutcome {
        if !self.current_items_truncated || self.active_folder_target().is_none() {
            return LoadMoreOutcome::NothingMore;
        }
        if self.folder_item_limit >= MAX_FOLDER_ITEMS {
            return LoadMoreOutcome::AtMaximum(MAX_FOLDER_ITEMS);
        }
        self.folder_item_limit = self
            .folder_item_limit
            .saturating_add(FOLDER_PAGE_SIZE)
            .min(MAX_FOLDER_ITEMS);
        self.load_active_folder(ui);
        LoadMoreOutcome::Requested(self.folder_item_limit)
    }

    pub fn sorted_visible_items(&self) -> Vec<DriveItem> {
        let mut items = self.current_items.clone();
        items.sort_by(|a, b| {
            b.is_folder.cmp(&a.is_folder).then_with(|| match self.sort_mode {
                SortMode::NameAsc => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                SortMode::ModifiedDesc => b.last_modified_at.cmp(&a.last_modified_at),
                SortMode::SizeDesc => b.size.cmp(&a.size),
                SortMode::TypeAsc => a
                    .type_badge()
                    .cmp(b.type_badge())
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())),
            })
        });
        items
    }

    fn selected_item_ref(&self) -> Option<&DriveItem> {
        let id = self.active_selected_id.as_deref()?;
        self.current_items.iter().find(|item| item.id == id)
    }

    pub fn active_item(&self) -> Option<DriveItem> {
        if let Some(selected) = self.selected_item_ref() {
            return Some(selected.clone());
        }
        self.sorted_visible_items().into_iter().next()
    }

    pub fn item_revision_key(item: &DriveItem) -> String {
        format!(
            "{}:{}:{}",
            item.effective_drive_id(),
            item.effective_item_id(),
            item.content_revision()
        )
    }

    pub fn text_preview_key(drive_id: &str, item_id: &str) -> String {
        format!("{drive_id}:{item_id}")
    }

    pub fn text_preview_inflight_key(drive_id: &str, item_id: &str) -> String {
        format!("txt:{drive_id}:{item_id}")
    }

    pub fn text_preview_for(&self, item: &DriveItem) -> Option<&str> {
        let (key, content) = self.text_previews.as_ref()?;
        let rev_ok = self
            .text_preview_revision
            .as_deref()
            .is_none_or(|rev| rev == item.content_revision());
        (text_preview_key_matches(key, item.effective_drive_id(), &item.id) && rev_ok)
            .then_some(content.as_str())
    }

    pub fn text_preview_error_for(&self, item: &DriveItem) -> Option<&str> {
        let key = Self::text_preview_inflight_key(item.effective_drive_id(), &item.id);
        self.preview_errors.get(&key).map(String::as_str)
    }

    pub fn versions_for(&self, item: &DriveItem) -> Option<&[DriveItemVersion]> {
        self.item_versions
            .get(&Self::item_revision_key(item))
            .map(Vec::as_slice)
    }

    pub fn image_preview_error_for(&self, item: &DriveItem) -> Option<&str> {
        self.preview_errors
            .get(&Self::item_revision_key(item))
            .or_else(|| self.preview_errors.get(&item.id))
            .map(String::as_str)
    }

    pub fn set_text_preview(&mut self, drive_id: &str, item_id: &str, content: String) {
        let rev = self
            .selected_item_ref()
            .filter(|item| item.effective_drive_id() == drive_id && item.id == item_id)
            .map(DriveItem::content_revision);
        self.text_previews = Some((Self::text_preview_key(drive_id, item_id), content));
        self.text_preview_revision = rev;
    }

    pub fn clear_inspector_preview(&mut self, ui: &mut UiState) {
        self.text_previews = None;
        self.text_preview_revision = None;
        ui.markup_cache = None;
        self.preview_errors.retain(|k, _| !k.starts_with("txt:"));
    }

    pub fn invalidate_active_item_caches(&mut self, ui: &mut UiState) {
        let active = self.active_item();
        self.clear_inspector_preview(ui);
        if let Some(item) = active {
            let rev_key = Self::item_revision_key(&item);
            let drive_item_prefix = format!("{}:{}:", item.effective_drive_id(), item.effective_item_id());
            self.item_versions.retain(|k, _| !k.starts_with(&drive_item_prefix));
            self.raw_image_bytes
                .retain(|(k, _)| !k.starts_with(&drive_item_prefix));
            self.rendered_images.retain(|k, _| {
                !k.starts_with(&format!("inline:{drive_item_prefix}"))
                    && !k.starts_with(&format!("modal:{drive_item_prefix}"))
            });
            self.preview_errors.remove(&rev_key);
            self.preview_errors.remove(&item.id);
            let tkey = Self::text_preview_inflight_key(item.effective_drive_id(), &item.id);
            self.inflight_previews.remove(&tkey);
            self.inflight_text_revisions.remove(&tkey);
            self.pending_html_overlay_fetches.remove(&tkey);
            self.inflight_previews.remove(&format!("ver:{rev_key}"));
        }
    }

    pub fn sync_inspector_preview_for(&mut self, selected: Option<&DriveItem>, ui: &mut UiState) {
        let active_triple = selected
            .filter(|item| !item.is_folder && item.is_text_previewable())
            .map(|item| {
                (
                    Self::text_preview_key(item.effective_drive_id(), &item.id),
                    Self::text_preview_inflight_key(item.effective_drive_id(), &item.id),
                    item.content_revision(),
                )
            });
        match active_triple {
            Some((ref expected_key, ref expected_err_key, ref expected_rev)) => {
                let key_mismatch = self
                    .text_previews
                    .as_ref()
                    .is_some_and(|(stored, _)| stored != expected_key);
                let rev_mismatch = self
                    .text_preview_revision
                    .as_deref()
                    .is_some_and(|rev| rev != expected_rev.as_str());
                if key_mismatch || rev_mismatch {
                    self.text_previews = None;
                    self.text_preview_revision = None;
                    ui.markup_cache = None;
                    if rev_mismatch {
                        let inflight_same_rev = self
                            .inflight_text_revisions
                            .get(expected_err_key)
                            .is_some_and(|r| r == expected_rev);
                        if !inflight_same_rev {
                            self.inflight_previews.remove(expected_err_key);
                            self.inflight_text_revisions.remove(expected_err_key);
                        }
                        self.preview_errors.remove(expected_err_key);
                    }
                }
                if ui
                    .markup_cache
                    .as_ref()
                    .is_some_and(|cached| cached.key.item_id != *expected_key)
                {
                    ui.markup_cache = None;
                }
                self.preview_errors
                    .retain(|k, _| !k.starts_with("txt:") || k == expected_err_key);
            }
            None => {
                self.clear_inspector_preview(ui);
            }
        }
    }

    fn is_active_item(&self, drive_id: &str, item_id: &str) -> bool {
        self.selected_item_ref()
            .is_some_and(|item| item.effective_drive_id() == drive_id && item.id == item_id)
    }

    fn is_active_text_item(&self, drive_id: &str, item_id: &str) -> bool {
        self.selected_item_ref().is_some_and(|item| {
            !item.is_folder
                && item.is_text_previewable()
                && item.effective_drive_id() == drive_id
                && item.id == item_id
        })
    }

    pub(crate) fn reconcile_selection_after_items_update(&mut self, ui: &mut UiState) {
        let items = self.sorted_visible_items();
        let prev_idx = ui.list_scroll.selected_idx;
        let selected = if items.is_empty() {
            ui.list_scroll.selected_idx = None;
            self.active_selected_id = None;
            None
        } else if let Some(ref selected_id) = self.active_selected_id {
            if let Some(pos) = items.iter().position(|item| item.id == *selected_id) {
                ui.list_scroll.selected_idx = Some(pos);
                items.get(pos)
            } else {
                let fallback = prev_idx.unwrap_or(0).min(items.len() - 1);
                ui.list_scroll.selected_idx = Some(fallback);
                self.active_selected_id = Some(items[fallback].id.clone());
                ui.inspector_scroll.top();
                items.get(fallback)
            }
        } else {
            let fallback = prev_idx.unwrap_or(0).min(items.len() - 1);
            ui.list_scroll.selected_idx = Some(fallback);
            self.active_selected_id = Some(items[fallback].id.clone());
            ui.inspector_scroll.top();
            items.get(fallback)
        };
        self.sync_inspector_preview_for(selected, ui);
    }

    pub fn navigate_to_active_folder(&mut self, ui: &mut UiState) {
        self.current_items.clear();
        self.active_selected_id = None;
        ui.list_scroll.selected_idx = None;
        ui.inspector_scroll.top();
        self.clear_inspector_preview(ui);
        self.load_active_folder(ui);
    }

    pub fn inline_image_cache_key(&self, item: &DriveItem) -> String {
        format!("inline:{}", Self::item_revision_key(item))
    }

    pub fn modal_image_cache_key(
        &self,
        item: &DriveItem,
        cols: u16,
        rows: u16,
        proto: TerminalImageProtocol,
    ) -> String {
        format!(
            "modal:{}:{}x{}:{}",
            Self::item_revision_key(item),
            cols,
            rows,
            proto.as_str()
        )
    }

    /// Shared text/HTML content fetcher used by both the reading pane (inspector) and open
    /// `HtmlPreviewOverlay`, guaranteeing at most one `/content` download per item revision.
    pub fn spawn_shared_text_fetch(&mut self, item: &DriveItem, notify_html_overlay: bool) {
        let did = item.effective_drive_id().to_string();
        let iid = item.effective_item_id().to_string();
        let item_id = item.id.clone();
        let rev = item.content_revision();
        let tkey = Self::text_preview_inflight_key(&did, &item_id);

        // If already cached at this exact revision, serve synchronously with 0 HTTP calls.
        if let Some(cached) = self.text_preview_for(item).map(str::to_string) {
            if notify_html_overlay {
                let _ = self.ui_tx.send(UiMsg::HtmlPreviewReady {
                    drive_id: did,
                    item_id,
                    revision: Some(rev),
                    title: item.name.clone(),
                    html: cached,
                });
            }
            return;
        }

        // If the exact same revision is already in flight, coalesce instead of starting a second GET.
        if self
            .inflight_text_revisions
            .get(&tkey)
            .is_some_and(|in_rev| *in_rev == rev)
        {
            if notify_html_overlay {
                self.pending_html_overlay_fetches.insert(tkey);
            }
            return;
        }

        self.inflight_previews.insert(tkey.clone());
        self.inflight_text_revisions.insert(tkey.clone(), rev.clone());
        if notify_html_overlay {
            self.pending_html_overlay_fetches.insert(tkey.clone());
        }

        let graph = self.graph.clone();
        let tx = self.ui_tx.clone();
        let dl = item.download_url.clone();
        let drive_for_msg = did.clone();
        let id_for_msg = item_id.clone();
        let title = item.name.clone();
        let emit_html_ready = notify_html_overlay;

        tokio::spawn(async move {
            match graph
                .download_item_bytes(&did, &iid, dl.as_deref(), DEFAULT_TEXT_PREVIEW_MAX_BYTES)
                .await
            {
                Ok(bytes) => {
                    let content = String::from_utf8_lossy(&bytes).into_owned();
                    let _ = tx.send(UiMsg::TextPreviewLoaded {
                        drive_id: drive_for_msg.clone(),
                        item_id: id_for_msg.clone(),
                        revision: Some(rev.clone()),
                        content: content.clone(),
                    });
                    if emit_html_ready {
                        let _ = tx.send(UiMsg::HtmlPreviewReady {
                            drive_id: drive_for_msg,
                            item_id: id_for_msg,
                            revision: Some(rev),
                            title,
                            html: content,
                        });
                    }
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::PreviewFailed {
                        drive_id: drive_for_msg,
                        item_id: id_for_msg,
                        revision: Some(rev),
                        inflight_key: Some(tkey),
                        error: format!("{err:#}"),
                    });
                }
            }
        });
    }

    pub fn refresh_open_overlays_for_items(&mut self, ui: &mut UiState) {
        let mut html_refresh: Option<DriveItem> = None;
        let mut image_refresh: Option<(DriveItem, String, u16, u16)> = None;

        for overlay in &mut ui.overlays {
            match overlay {
                Overlay::HtmlPreview(ov) => {
                    if let Some(ref target_id) = ov.item_id {
                        if let Some(updated) = self.current_items.iter().find(|it| {
                            it.id == *target_id
                                && ov
                                    .drive_id
                                    .as_deref()
                                    .is_none_or(|d| d == it.effective_drive_id())
                        }) {
                            let new_rev = updated.content_revision();
                            if ov.revision.as_deref() != Some(new_rev.as_str()) {
                                ov.revision = Some(new_rev);
                                html_refresh = Some(updated.clone());
                            } else if ov.title != updated.name {
                                // Metadata-only rename: update overlay title without re-downloading body.
                                ov.title = updated.name.clone();
                            }
                        }
                    }
                }
                Overlay::ImagePreview(ov) => {
                    let prev_cur = ov.current().cloned();
                    let new_images: Vec<DriveItem> = self
                        .sorted_visible_items()
                        .into_iter()
                        .filter(|i| i.is_image())
                        .collect();
                    if new_images.is_empty() {
                        ov.items.clear();
                        ov.index = 0;
                        ov.error = Some("No images remaining in this folder".to_string());
                    } else {
                        if let Some(ref prev) = prev_cur {
                            if let Some(pos) = new_images.iter().position(|i| i.id == prev.id) {
                                let updated = &new_images[pos];
                                let changed = updated.content_revision() != prev.content_revision();
                                ov.index = pos;
                                if changed {
                                    let (cols, rows) =
                                        ov.target_dimensions(ui.term_width, ui.term_height);
                                    let key = self.modal_image_cache_key(
                                        updated,
                                        cols,
                                        rows,
                                        ui.graphics_caps.protocol,
                                    );
                                    image_refresh = Some((updated.clone(), key, cols, rows));
                                }
                            } else {
                                ov.index = ov.index.min(new_images.len() - 1);
                                let next_img = &new_images[ov.index];
                                let (cols, rows) =
                                    ov.target_dimensions(ui.term_width, ui.term_height);
                                let key = self.modal_image_cache_key(
                                    next_img,
                                    cols,
                                    rows,
                                    ui.graphics_caps.protocol,
                                );
                                image_refresh = Some((next_img.clone(), key, cols, rows));
                            }
                        }
                        ov.items = new_images;
                    }
                }
                _ => {}
            }
        }

        if let Some(item) = html_refresh {
            self.spawn_shared_text_fetch(&item, true);
        }

        if let Some((item, key, cols, rows)) = image_refresh {
            self.request_image_decode(&item, key, cols, rows, ui.graphics_caps);
        }
    }

    pub fn spawn_bootstrap_from_cache(&self) {
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::task::spawn_blocking(move || {
            let sites = storage.list_sites().unwrap_or_default();
            let drives = storage.list_drives(None).unwrap_or_default();
            let my_drive = drives.iter().find(|d| d.is_personal_onedrive()).cloned();
            let pins = storage.list_pins().unwrap_or_default();
            let _ = tx.send(UiMsg::BootstrapLoaded {
                my_drive,
                sites,
                drives,
                pins,
            });
        });
    }

    pub fn load_active_folder(&mut self, ui: &mut UiState) {
        self.load_folder(ui, false);
    }

    fn load_folder(&mut self, ui: &mut UiState, reuse_startup_root: bool) {
        let tx = self.ui_tx.clone();
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let vseq = self.bump_virtual_list_seq();
        self.loading_folder = true;

        // Truncation belongs to one folder. Moving to another folder or to a virtual view
        // starts clean; a refresh of the same folder keeps what the last listing said.
        let target = self.active_folder_target();
        if target != self.listed_folder {
            self.listed_folder = target;
            self.current_items_truncated = false;
            self.folder_item_limit = FOLDER_PAGE_SIZE;
        }

        match &self.active_source {
            ActiveViewSource::SearchResult { .. } => {
                self.loading_folder = false;
                return;
            }
            ActiveViewSource::Recent => {
                self.sync_ctrl.set_focus(FocusTarget {
                    active_drive_id: None,
                    active_parent_id: None,
                    active_site_id: None,
                    view_kind: FocusViewKind::Recent,
                    active_limit: None,
                });
                tokio::spawn(async move {
                    match graph.fetch_virtual_list(VirtualListKind::Recent, 50).await {
                        Ok(fetch) => {
                            if fetch.status.is_available() {
                                let _ = storage.replace_recent_items(&fetch.items);
                            }
                            let _ = tx.send(UiMsg::VirtualListLoaded {
                                seq: vseq,
                                kind: VirtualListKind::Recent,
                                items: fetch.items,
                                status: fetch.status,
                            });
                        }
                        Err(err) => {
                            let _ = tx.send(UiMsg::TaskError {
                                label: "Recent".into(),
                                error: format!("{err:#}"),
                            });
                        }
                    }
                });
                return;
            }
            ActiveViewSource::SharedWithMe => {
                self.sync_ctrl.set_focus(FocusTarget {
                    active_drive_id: None,
                    active_parent_id: None,
                    active_site_id: None,
                    view_kind: FocusViewKind::SharedWithMe,
                    active_limit: None,
                });
                tokio::spawn(async move {
                    match graph.fetch_virtual_list(VirtualListKind::SharedWithMe, 50).await {
                        Ok(fetch) => {
                            if fetch.status.is_available() {
                                let _ = storage.replace_shared_items(&fetch.items);
                            }
                            let _ = tx.send(UiMsg::VirtualListLoaded {
                                seq: vseq,
                                kind: VirtualListKind::SharedWithMe,
                                items: fetch.items,
                                status: fetch.status,
                            });
                        }
                        Err(err) => {
                            let _ = tx.send(UiMsg::TaskError {
                                label: "Shared".into(),
                                error: format!("{err:#}"),
                            });
                        }
                    }
                });
                return;
            }
            _ => {}
        }

        let Some(drive_id) = self.active_drive_id() else {
            self.loading_folder = false;
            return;
        };
        let parent_id = self.active_parent_id();
        self.sync_ctrl.set_focus(FocusTarget {
            active_drive_id: Some(drive_id.clone()),
            active_parent_id: parent_id.clone(),
            active_site_id: self.active_site_id(),
            view_kind: FocusViewKind::Folder,
            active_limit: self.requested_folder_limit(),
        });

        // Show cached children immediately, then refresh from Graph.
        // Also clear stale in-memory rows when the cached folder was just emptied (e.g., last item deleted).
        if let Ok((cached, cache_truncated)) = self.storage.list_folder_children_page(
            &drive_id,
            parent_id.as_deref(),
            self.folder_item_limit,
        ) {
            if !cached.is_empty() || !self.current_items.is_empty() {
                self.current_items = cached;
                // The cache can prove there is more, never that there is not: a folder whose
                // last listing stopped early has exactly one page cached.
                self.current_items_truncated |= cache_truncated;
                self.reconcile_selection_after_items_update(ui);
            }
        }

        self.sync_ctrl.request(crate::sync::SyncCommand::SyncFolder {
            drive_id,
            parent_id,
            reuse_startup_root,
            limit: self.requested_folder_limit(),
        });
    }

    pub fn ensure_inspector_loaded(&mut self, ui: &UiState) {
        let item = if let Some(selected) = self.selected_item_ref() {
            selected.clone()
        } else if let Some(fallback) = self.active_item() {
            self.active_selected_id = Some(fallback.id.clone());
            fallback
        } else {
            return;
        };
        if item.is_folder {
            return;
        }
        let item_id = item.id.clone();
        let rev_key = Self::item_revision_key(&item);

        // Fetch version history lazily (keyed by drive + item + revision)
        if !self.item_versions.contains_key(&rev_key) {
            let vkey = format!("ver:{rev_key}");
            if self.inflight_previews.insert(vkey) {
                let graph = self.graph.clone();
                let tx = self.ui_tx.clone();
                let did = item.effective_drive_id().to_string();
                let iid = item.effective_item_id().to_string();
                let id_for_msg = item_id.clone();
                let cache_key = rev_key.clone();
                tokio::spawn(async move {
                    match graph.list_item_versions(&did, &iid).await {
                        Ok(versions) => {
                            let _ = tx.send(UiMsg::VersionsLoaded {
                                cache_key,
                                item_id: id_for_msg,
                                versions,
                            });
                        }
                        Err(_) => {
                            let _ = tx.send(UiMsg::VersionsFailed { cache_key });
                        }
                    }
                });
            }
        }

        if item.is_text_previewable() && self.text_preview_for(&item).is_none() {
            self.spawn_shared_text_fetch(&item, false);
        } else if item.is_image() && ui.inline_images {
            let cache_key = self.inline_image_cache_key(&item);
            if !self.rendered_images.contains_key(&cache_key) {
                let cols = ui.inspector_scroll.area.width.saturating_sub(4).clamp(24, 72);
                let rows = ui.inspector_scroll.area.height.saturating_sub(12).clamp(8, 22);
                self.request_image_decode(&item, cache_key, cols, rows, ui.graphics_caps);
            }
        }
    }

    pub fn request_image_decode(
        &mut self,
        item: &DriveItem,
        cache_key: String,
        cols: u16,
        rows: u16,
        caps: TerminalGraphicsCaps,
    ) {
        if self.rendered_images.contains_key(&cache_key)
            || !self.inflight_previews.insert(cache_key.clone())
        {
            return;
        }

        let decode = ImageDecodeRequest {
            cache_key,
            cols,
            rows,
            caps,
        };
        let raw_cache_key = Self::item_revision_key(item);
        if let Some((_, bytes)) = self.raw_image_bytes.iter().find(|(key, _)| key == &raw_cache_key) {
            let kind = sniff_content_kind(item.mime_type.as_deref(), Some(&item.name), Some(bytes));
            self.spawn_image_decodes(item.clone(), Arc::clone(bytes), kind, vec![decode]);
            return;
        }

        if let Some(pending) = self.inflight_raw_images.get_mut(&raw_cache_key) {
            pending.decodes.push(decode);
            return;
        }
        self.inflight_raw_images.insert(
            raw_cache_key.clone(),
            PendingImageDownload {
                item: item.clone(),
                decodes: vec![decode],
            },
        );

        let tx = self.ui_tx.clone();
        let graph = self.graph.clone();
        let did = item.effective_drive_id().to_string();
        let iid = item.effective_item_id().to_string();
        let dl = item.download_url.clone();
        tokio::spawn(async move {
            let result = graph
                .download_item_bytes(&did, &iid, dl.as_deref(), 16 * 1024 * 1024)
                .await
                .map(Arc::from)
                .map_err(|err| format!("{err:#}"));
            let _ = tx.send(UiMsg::ImageBytesFetched {
                cache_key: raw_cache_key,
                result,
            });
        });
    }

    pub(crate) fn cache_raw_image(&mut self, cache_key: String, bytes: Arc<[u8]>) {
        if bytes.len() > img_preview::MAX_IMAGE_CACHE_BYTES {
            return;
        }
        self.raw_image_bytes.retain(|(key, _)| key != &cache_key);
        let mut total_bytes: usize = self.raw_image_bytes.iter().map(|(_, raw)| raw.len()).sum();
        while self.raw_image_bytes.len() >= MAX_RAW_IMAGE_ENTRIES
            || total_bytes + bytes.len() > img_preview::MAX_IMAGE_CACHE_BYTES
        {
            let (_, evicted) = self.raw_image_bytes.pop_front().expect("cache has entries to evict");
            total_bytes -= evicted.len();
        }
        self.raw_image_bytes.push_back((cache_key, bytes));
    }

    fn spawn_image_decodes(
        &self,
        item: DriveItem,
        bytes: Arc<[u8]>,
        kind: ContentKind,
        decodes: Vec<ImageDecodeRequest>,
    ) {
        let tx = self.ui_tx.clone();
        tokio::task::spawn_blocking(move || {
            let drive_id = item.effective_drive_id().to_string();
            if kind == ContentKind::Html {
                let _ = tx.send(UiMsg::HtmlContentSniffed {
                    drive_id,
                    item_id: item.id,
                    title: item.name,
                    html: String::from_utf8_lossy(&bytes).into_owned(),
                    cache_keys: decodes.into_iter().map(|decode| decode.cache_key).collect(),
                });
                return;
            }
            for ImageDecodeRequest { cache_key, cols, rows, caps } in decodes {
                let result = match kind {
                    ContentKind::RasterImage => {
                        img_preview::decode_and_render_for_protocol(&bytes, cols, rows, (18, 20, 26), caps)
                            .map_err(|err| format!("{err:#}"))
                    }
                    ContentKind::Svg => Err("SVG vector image (image/svg+xml) cannot be raster-decoded — press o to open in browser".into()),
                    _ => Err(format!("Unsupported image response ({kind:?}); refusing raster decode")),
                };
                let msg = match result {
                    Ok(rendered) => UiMsg::ImageDecoded { cache_key, rendered },
                    Err(error) => UiMsg::PreviewFailed {
                        drive_id: drive_id.clone(),
                        item_id: item.id.clone(),
                        revision: None,
                        inflight_key: Some(cache_key),
                        error,
                    },
                };
                let _ = tx.send(msg);
            }
        });
    }

    pub fn spawn_upload_clipboard_png(&self) {
        let Some(drive_id) = self.active_drive_id() else {
            let _ = self.ui_tx.send(UiMsg::Toast {
                message: "Select a OneDrive or SharePoint folder first".into(),
                kind: ToastKind::Warning,
            });
            return;
        };
        let parent_id = self.active_parent_id();
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();

        tokio::spawn(async move {
            let png_res = tokio::task::spawn_blocking(read_clipboard_png_bytes).await;
            let bytes = match png_res {
                Ok(Ok(b)) => b,
                Ok(Err(err)) => {
                    let _ = tx.send(UiMsg::Toast {
                        message: format!("Clipboard PNG paste failed: {err:#}"),
                        kind: ToastKind::Warning,
                    });
                    return;
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskError {
                        label: "Clipboard PNG".into(),
                        error: err.to_string(),
                    });
                    return;
                }
            };

            let unix_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let file_name = format!("pasted-image-{unix_ms}.png");

            match graph
                .upload_bytes(
                    &drive_id,
                    parent_id.as_deref(),
                    &file_name,
                    bytes,
                    Some("image/png"),
                )
                .await
            {
                Ok(item) => {
                    let _ = storage.upsert_drive_items(std::slice::from_ref(&item));
                    let _ = tx.send(UiMsg::ItemUploaded(item));
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskError {
                        label: "Upload PNG".into(),
                        error: format!("{err:#}"),
                    });
                }
            }
        });
    }

    pub fn apply_core_event(&mut self, ev: CoreEvent, ui: &mut UiState) {
        match ev {
            CoreEvent::ProfileLoaded(me) => {
                self.me = Some(me);
            }
            CoreEvent::MyDriveLoaded(drive) => {
                let first_load = self.my_drive.is_none();
                self.my_drive = Some(drive);
                if first_load && matches!(self.active_source, ActiveViewSource::OneDriveRoot) {
                    self.load_folder(ui, self.folder_stack.is_empty());
                }
            }
            CoreEvent::SitesUpdated(sites) => {
                self.sites = sites;
                // Allow initial SharePoint startup library selection at most once, before user navigation.
                if self.persona == Persona::SharePoint
                    && !self.initial_persona_library_selected
                    && matches!(self.active_source, ActiveViewSource::OneDriveRoot)
                    && self.folder_stack.is_empty()
                {
                    if let Some(first_site) = self.sites.first() {
                        if let Some(drives) = self.site_drives.get(&first_site.id) {
                            if let Some(d) = drives.first() {
                                self.initial_persona_library_selected = true;
                                self.active_source = ActiveViewSource::Drive {
                                    drive_id: d.id.clone(),
                                    label: d.display_label(),
                                };
                                self.folder_stack.clear();
                                self.navigate_to_active_folder(ui);
                            }
                        }
                    }
                }
            }
            CoreEvent::DrivesUpdated { site_id, drives } => {
                let should_boot_sp = self.persona == Persona::SharePoint
                    && !self.initial_persona_library_selected
                    && matches!(self.active_source, ActiveViewSource::OneDriveRoot)
                    && self.folder_stack.is_empty()
                    && !drives.is_empty();
                self.site_drives.insert(site_id, drives.clone());
                if should_boot_sp {
                    self.initial_persona_library_selected = true;
                    let d = &drives[0];
                    self.active_source = ActiveViewSource::Drive {
                        drive_id: d.id.clone(),
                        label: d.display_label(),
                    };
                    self.folder_stack.clear();
                    self.navigate_to_active_folder(ui);
                }
            }
            CoreEvent::FolderItemsUpdated {
                drive_id,
                parent_id,
                items,
                truncated,
            } => {
                if self.active_folder_target() == Some((drive_id, parent_id)) {
                    // Guard against an older smaller-limit background poll truncating a
                    // folder whose user-requested page limit was already raised via `LoadMoreItems`.
                    if truncated && items.len() < self.folder_item_limit {
                        return;
                    }
                    self.current_items = items;
                    self.current_items_truncated = truncated;
                    self.loading_folder = false;
                    self.reconcile_selection_after_items_update(ui);
                    self.refresh_open_overlays_for_items(ui);
                    self.ensure_inspector_loaded(ui);
                }
            }
            CoreEvent::RecentItemsUpdated(items) => {
                if matches!(self.active_source, ActiveViewSource::Recent) {
                    self.current_items = items;
                    self.loading_folder = false;
                    self.reconcile_selection_after_items_update(ui);
                    self.refresh_open_overlays_for_items(ui);
                    self.ensure_inspector_loaded(ui);
                }
            }
            CoreEvent::SharedItemsUpdated(items) => {
                if matches!(self.active_source, ActiveViewSource::SharedWithMe) {
                    self.current_items = items;
                    self.loading_folder = false;
                    self.reconcile_selection_after_items_update(ui);
                    self.refresh_open_overlays_for_items(ui);
                    self.ensure_inspector_loaded(ui);
                }
            }
            CoreEvent::VirtualListStatusUpdated { kind, status } => {
                let unavailable = !status.is_available();
                match kind {
                    VirtualListKind::Recent => {
                        self.recent_status = Some(status);
                        if unavailable && matches!(self.active_source, ActiveViewSource::Recent) {
                            self.current_items.clear();
                            self.active_selected_id = None;
                            ui.list_scroll.selected_idx = None;
                            self.loading_folder = false;
                            self.clear_inspector_preview(ui);
                        }
                    }
                    VirtualListKind::SharedWithMe => {
                        self.shared_status = Some(status);
                        if unavailable && matches!(self.active_source, ActiveViewSource::SharedWithMe) {
                            self.current_items.clear();
                            self.active_selected_id = None;
                            ui.list_scroll.selected_idx = None;
                            self.loading_folder = false;
                            self.clear_inspector_preview(ui);
                        }
                    }
                }
            }
            CoreEvent::SyncStateChanged { busy, summary } => {
                let was_busy = self.sync_busy;
                self.sync_busy = busy;
                if was_busy && !busy {
                    self.loading_folder = false;
                    self.ensure_inspector_loaded(ui);
                }
                if summary != POLL_RETRY_SUMMARY {
                    self.sync_summary = summary;
                }
            }
            CoreEvent::SyncError(err) => {
                self.sync_busy = false;
                self.loading_folder = false;
                self.ensure_inspector_loaded(ui);
                ui.toast(format!("Sync error: {err}"), ToastKind::Error);
            }
        }
    }

    pub fn apply_ui_msg(&mut self, msg: UiMsg, ui: &mut UiState) {
        match msg {
            UiMsg::BootstrapLoaded {
                my_drive,
                sites,
                drives,
                pins,
            } => {
                let loaded_drive = self.my_drive.is_none() && my_drive.is_some();
                if self.my_drive.is_none() {
                    self.my_drive = my_drive;
                }
                if self.sites.is_empty() {
                    self.sites = sites;
                }
                for d in drives {
                    if let Some(ref sid) = d.site_id {
                        let list = self.site_drives.entry(sid.clone()).or_default();
                        if !list.iter().any(|existing| existing.id == d.id) {
                            list.push(d);
                        }
                    }
                }
                self.pins = pins;
                if self.persona == Persona::SharePoint
                    && !self.initial_persona_library_selected
                    && matches!(self.active_source, ActiveViewSource::OneDriveRoot)
                    && self.folder_stack.is_empty()
                {
                    if let Some(site) = self.sites.first() {
                        if let Some(ds) = self.site_drives.get(&site.id) {
                            if let Some(d) = ds.first() {
                                self.initial_persona_library_selected = true;
                                self.active_source = ActiveViewSource::Drive {
                                    drive_id: d.id.clone(),
                                    label: d.display_label(),
                                };
                                self.load_active_folder(ui);
                                return;
                            }
                        }
                    }
                }
                if loaded_drive && matches!(self.active_source, ActiveViewSource::OneDriveRoot) {
                    self.load_folder(ui, self.folder_stack.is_empty());
                }
            }
            UiMsg::RecentLoaded(items) => {
                if matches!(self.active_source, ActiveViewSource::Recent) {
                    self.current_items = items;
                    self.loading_folder = false;
                    self.reconcile_selection_after_items_update(ui);
                    self.refresh_open_overlays_for_items(ui);
                    self.ensure_inspector_loaded(ui);
                }
            }
            UiMsg::SharedLoaded(items) => {
                if matches!(self.active_source, ActiveViewSource::SharedWithMe) {
                    self.current_items = items;
                    self.loading_folder = false;
                    self.reconcile_selection_after_items_update(ui);
                    self.refresh_open_overlays_for_items(ui);
                    self.ensure_inspector_loaded(ui);
                }
            }
            UiMsg::VirtualListLoaded {
                seq,
                kind,
                items,
                status,
            } => {
                if seq != self.virtual_list_seq {
                    return;
                }
                let is_avail = status.is_available();
                let active_matches = match kind {
                    VirtualListKind::Recent => {
                        self.recent_status = Some(status);
                        matches!(self.active_source, ActiveViewSource::Recent)
                    }
                    VirtualListKind::SharedWithMe => {
                        self.shared_status = Some(status);
                        matches!(self.active_source, ActiveViewSource::SharedWithMe)
                    }
                };
                if active_matches {
                    self.loading_folder = false;
                    if is_avail {
                        self.current_items = items;
                        self.reconcile_selection_after_items_update(ui);
                        self.refresh_open_overlays_for_items(ui);
                        self.ensure_inspector_loaded(ui);
                    } else {
                        self.current_items.clear();
                        self.active_selected_id = None;
                        ui.list_scroll.selected_idx = None;
                        self.clear_inspector_preview(ui);
                    }
                }
            }
            UiMsg::SiteDrivesLoaded { site_id, drives } => {
                self.site_drives.insert(site_id, drives);
            }
            UiMsg::TextPreviewLoaded {
                drive_id,
                item_id,
                revision,
                content,
            } => {
                let err_key = Self::text_preview_inflight_key(&drive_id, &item_id);
                if revision.as_ref().is_some_and(|rev| self.inflight_text_revisions.get(&err_key) != Some(rev)) {
                    return;
                }
                self.inflight_previews.remove(&err_key);
                self.inflight_text_revisions.remove(&err_key);
                let want_html_overlay = self.pending_html_overlay_fetches.remove(&err_key);
                let matched_item = self
                    .current_items
                    .iter()
                    .find(|it| it.effective_drive_id() == drive_id && it.id == item_id)
                    .cloned();
                let current_rev = matched_item.as_ref().map(DriveItem::content_revision);
                if revision.is_some() && current_rev != revision {
                    return;
                }
                if self.is_active_text_item(&drive_id, &item_id) {
                    self.preview_errors.remove(&err_key);
                    self.set_text_preview(&drive_id, &item_id, content.clone());
                }
                let mut updated_open_overlay = false;
                for overlay in &mut ui.overlays {
                    if let Overlay::HtmlPreview(ov) = overlay {
                        if ov.item_id.as_deref() == Some(item_id.as_str())
                            && ov.drive_id.as_deref().is_none_or(|d| d == drive_id)
                        {
                            let title = matched_item
                                .as_ref()
                                .map(|it| it.name.clone())
                                .unwrap_or_else(|| ov.title.clone());
                            ov.replace_html(title, content.clone(), current_rev.clone());
                            updated_open_overlay = true;
                        }
                    }
                }
                if want_html_overlay && !updated_open_overlay {
                    if let Some(it) = matched_item {
                        let mut ov = HtmlPreviewOverlay::new(it.name, content, Some(item_id));
                        ov.drive_id = Some(drive_id);
                        ov.revision = current_rev;
                        ui.push_overlay(Overlay::HtmlPreview(ov));
                    }
                }
            }
            UiMsg::VersionsLoaded {
                cache_key,
                versions,
                ..
            } => {
                self.inflight_previews.remove(&format!("ver:{cache_key}"));
                self.item_versions.insert(cache_key, versions);
            }
            UiMsg::VersionsFailed { cache_key } => {
                self.inflight_previews.remove(&format!("ver:{cache_key}"));
            }
            UiMsg::PreviewFailed {
                drive_id,
                item_id,
                revision,
                inflight_key,
                error,
            } => {
                if let Some(rev) = revision.as_ref() {
                    let tracked = inflight_key.as_ref().and_then(|key| self.inflight_text_revisions.get(key));
                    if tracked != Some(rev) {
                        return;
                    }
                }
                let is_text_req = inflight_key.as_deref().is_none_or(|k| k.starts_with("txt:"));
                if let Some(key) = inflight_key {
                    self.inflight_previews.remove(&key);
                    self.inflight_text_revisions.remove(&key);
                    self.pending_html_overlay_fetches.remove(&key);
                }
                if is_text_req {
                    if self.is_active_text_item(&drive_id, &item_id) {
                        self.preview_errors.insert(
                            Self::text_preview_inflight_key(&drive_id, &item_id),
                            error,
                        );
                    }
                } else {
                    let err_key = self
                        .current_items
                        .iter()
                        .find(|it| it.effective_drive_id() == drive_id && it.id == item_id)
                        .map(Self::item_revision_key)
                        .unwrap_or(item_id);
                    self.preview_errors.insert(err_key, error);
                }
            }
            UiMsg::ImageBytesFetched { cache_key, result } => {
                let Some(pending) = self.inflight_raw_images.remove(&cache_key) else {
                    return;
                };
                match result {
                    Ok(bytes) => {
                        let kind = sniff_content_kind(
                            pending.item.mime_type.as_deref(),
                            Some(&pending.item.name),
                            Some(&bytes),
                        );
                        if kind == ContentKind::RasterImage {
                            self.cache_raw_image(cache_key, Arc::clone(&bytes));
                        }
                        self.spawn_image_decodes(pending.item, bytes, kind, pending.decodes);
                    }
                    Err(error) => {
                        for decode in pending.decodes {
                            self.inflight_previews.remove(&decode.cache_key);
                        }
                        self.preview_errors.insert(cache_key, error);
                    }
                }
            }
            UiMsg::ImageDecoded {
                cache_key,
                rendered,
            } => {
                self.inflight_previews.remove(&cache_key);
                if self.rendered_images.len() >= 12 {
                    self.rendered_images.clear();
                }
                self.rendered_images.insert(cache_key, rendered);
            }
            UiMsg::HtmlContentSniffed {
                drive_id,
                item_id,
                title,
                html,
                cache_keys,
            } => {
                for cache_key in cache_keys {
                    self.inflight_previews.remove(&cache_key);
                }
                self.preview_errors.remove(&item_id);
                if self.is_active_item(&drive_id, &item_id) {
                    self.set_text_preview(&drive_id, &item_id, html.clone());
                }
                if matches!(ui.overlays.last(), Some(Overlay::ImagePreview(_))) {
                    ui.pop_overlay();
                }
                let already_open = matches!(
                    ui.overlays.last(),
                    Some(Overlay::HtmlPreview(ov)) if ov.item_id.as_deref() == Some(item_id.as_str())
                );
                if !already_open {
                    ui.push_overlay(Overlay::HtmlPreview(HtmlPreviewOverlay::new(
                        title,
                        html,
                        Some(item_id),
                    )));
                }
                ui.toast(
                    "HTML content detected — opened interactive HTML viewer",
                    ToastKind::Info,
                );
            }
            UiMsg::HtmlPreviewReady {
                drive_id,
                item_id,
                revision,
                title,
                html,
            } => {
                let current_rev = self
                    .current_items
                    .iter()
                    .find(|it| it.effective_drive_id() == drive_id && it.id == item_id)
                    .map(DriveItem::content_revision);
                if revision.is_some() && current_rev != revision {
                    return;
                }
                if self.is_active_text_item(&drive_id, &item_id) {
                    self.preview_errors
                        .remove(&Self::text_preview_inflight_key(&drive_id, &item_id));
                    self.set_text_preview(&drive_id, &item_id, html.clone());
                }
                if let Some(Overlay::HtmlPreview(ov)) = ui.overlays.last_mut() {
                    if ov.item_id.as_deref() == Some(item_id.as_str())
                        && ov.drive_id.as_deref().is_none_or(|d| d == drive_id)
                    {
                        ov.replace_html(title, html, current_rev);
                        return;
                    }
                }
                let mut ov = HtmlPreviewOverlay::new(title, html, Some(item_id));
                ov.drive_id = Some(drive_id);
                ov.revision = current_rev;
                ui.push_overlay(Overlay::HtmlPreview(ov));
            }
            UiMsg::HtmlServed { url } => match open::that(&url) {
                Ok(()) => ui.toast(format!("Serving HTML at {url}"), ToastKind::Success),
                Err(err) => ui.toast(
                    format!("HTML ready at {url} (browser open failed: {err})"),
                    ToastKind::Warning,
                ),
            },
            UiMsg::ItemDownloaded(dl) => {
                let msg = format!("Downloaded {} -> {}", dl.file_name, dl.path.display());
                ui.toast(msg.clone(), ToastKind::Success);
                if ui.notify {
                    notification::show("SharePoint Download Complete", &msg);
                }
            }
            UiMsg::ItemUploaded(item) => {
                let msg = format!("Uploaded {} ({})", item.name, crate::models::format_human_size(item.size));
                ui.toast(msg.clone(), ToastKind::Success);
                if ui.notify {
                    notification::show("SharePoint Upload Complete", &msg);
                }
                self.load_active_folder(ui);
            }
            UiMsg::FolderCreated(folder) => {
                ui.toast(format!("Created folder '{}'", folder.name), ToastKind::Success);
                self.load_active_folder(ui);
            }
            UiMsg::ItemRenamed(item) => {
                ui.toast(format!("Renamed to '{}'", item.name), ToastKind::Success);
                let mut touched_active = self.active_selected_id.as_deref() == Some(item.id.as_str());
                for existing in &mut self.current_items {
                    let is_same = (existing.drive_id == item.drive_id && existing.id == item.id)
                        || (existing.effective_drive_id() == item.drive_id
                            && existing.effective_item_id() == item.id);
                    if is_same {
                        existing.name = item.name.clone();
                        if self.active_selected_id.as_deref() == Some(existing.id.as_str()) {
                            touched_active = true;
                        }
                    }
                }
                for pin in &mut self.pins {
                    if pin.drive_id.as_deref() == Some(item.drive_id.as_str())
                        && pin.item_id.as_deref() == Some(item.id.as_str())
                    {
                        pin.label = item.name.clone();
                        let _ = self.storage.upsert_pin(pin);
                    }
                }
                if touched_active {
                    ui.markup_cache = None;
                }
                self.reconcile_selection_after_items_update(ui);
                self.refresh_open_overlays_for_items(ui);
                self.load_active_folder(ui);
            }
            UiMsg::ItemDeleted {
                drive_id,
                item_id,
                raw_drive_id,
                raw_item_id,
                name,
            } => {
                ui.toast(format!("Deleted '{name}'"), ToastKind::Success);
                let raw_did = raw_drive_id.as_deref();
                let raw_iid = raw_item_id.as_deref();
                let active_deleted = self.active_selected_id.as_deref() == Some(item_id.as_str())
                    || raw_iid.is_some_and(|rid| self.active_selected_id.as_deref() == Some(rid));
                if active_deleted {
                    self.clear_inspector_preview(ui);
                }
                self.current_items.retain(|it| {
                    let eff_match = it.effective_drive_id() == drive_id
                        && it.effective_item_id() == item_id;
                    let direct_match = it.drive_id == drive_id && it.id == item_id;
                    let raw_match = matches!((raw_did, raw_iid), (Some(rd), Some(ri)) if it.drive_id == rd && it.id == ri);
                    !(eff_match || direct_match || raw_match)
                });
                let eff_pin_key = format!("folder:{drive_id}:{item_id}");
                let raw_pin_key = match (raw_did, raw_iid) {
                    (Some(rd), Some(ri)) => Some(format!("folder:{rd}:{ri}")),
                    _ => None,
                };
                let mut removed_pin_keys: Vec<String> = Vec::new();
                self.pins.retain(|p| {
                    let eff_pin = p.drive_id.as_deref() == Some(drive_id.as_str())
                        && p.item_id.as_deref() == Some(item_id.as_str());
                    let raw_pin = matches!(
                        (raw_did, raw_iid),
                        (Some(rd), Some(ri)) if p.drive_id.as_deref() == Some(rd) && p.item_id.as_deref() == Some(ri)
                    );
                    let key_pin = p.key == eff_pin_key
                        || raw_pin_key.as_deref() == Some(p.key.as_str());
                    if eff_pin || raw_pin || key_pin {
                        removed_pin_keys.push(p.key.clone());
                        false
                    } else {
                        true
                    }
                });
                for key in removed_pin_keys {
                    let _ = self.storage.remove_pin(&key);
                }
                self.reconcile_selection_after_items_update(ui);
                self.refresh_open_overlays_for_items(ui);
                self.ensure_inspector_loaded(ui);
                self.load_active_folder(ui);
            }
            UiMsg::ShareLinkCreated { item_id, link } => {
                let copied = copy_text_to_clipboard(&link.web_url).is_ok();
                self.created_share_links.insert(item_id, link.web_url.clone());
                let suffix = if copied { " (copied to clipboard)" } else { "" };
                ui.toast(format!("Share link: {}{suffix}", link.web_url), ToastKind::Success);
            }
            UiMsg::SearchResults { seq, items } => {
                if seq != self.search_seq {
                    return;
                }
                self.search_results = items;
            }
            UiMsg::UpdateChecked(res) => {
                if res.update_available {
                    ui.toast(
                        format!("Update available: v{} -> v{}", res.current_version, res.latest_version),
                        ToastKind::Info,
                    );
                } else {
                    ui.toast(format!("Up to date (v{})", res.current_version), ToastKind::Success);
                }
            }
            UiMsg::TaskError { label, error } => {
                let was_loading = self.loading_folder;
                self.loading_folder = false;
                if was_loading {
                    self.ensure_inspector_loaded(ui);
                }
                ui.toast(format!("{label} failed: {error}"), ToastKind::Error);
            }
            UiMsg::Toast { message, kind } => {
                ui.toast(message, kind);
            }
        }
    }

    /// Saves the preferences the user can change in the TUI, and nothing else: not the
    /// rest of the config, and not the image protocol the terminal probe came up with.
    pub fn save_ui_prefs(&mut self, ui: &UiState) {
        // `render_markup` may have come from the environment. It becomes the user's own
        // setting, worth writing down, the first time they toggle it.
        if ui.render_markup != self.config.render_markup {
            self.render_markup_user_set = true;
        }
        self.config.notify = ui.notify;
        self.config.show_nav_rail = ui.show_nav_rail;
        self.config.inline_images = ui.inline_images;
        self.config.render_markup = ui.render_markup;
        self.config.density = ui.density;
        self.config.theme = ui.theme_preset;
        self.persist_prefs(&ui_pref_updates(ui, self.render_markup_user_set));
    }

    /// Saves the image protocol the user picked by hand. `auto` stays `auto` until then.
    pub fn save_image_protocol_choice(&mut self, ui: &UiState) {
        let choice = image_protocol_preference(ui.graphics_caps.protocol);
        self.config.image_protocol = choice;
        self.persist_prefs(&[("image_protocol", pref_value(choice))]);
    }

    fn persist_prefs(&self, updates: &[(&'static str, toml::Value)]) {
        if let Err(err) = crate::config::persist_config_keys(updates) {
            let _ = self.ui_tx.send(UiMsg::Toast {
                message: format!("Settings not saved: {err:#}"),
                kind: ToastKind::Error,
            });
        }
    }
}

fn image_protocol_preference(protocol: TerminalImageProtocol) -> ImageProtocolPreference {
    match protocol {
        TerminalImageProtocol::Kitty => ImageProtocolPreference::Kitty,
        TerminalImageProtocol::ITerm2 => ImageProtocolPreference::Iterm2,
        TerminalImageProtocol::Sixel => ImageProtocolPreference::Sixel,
        TerminalImageProtocol::Halfblock => ImageProtocolPreference::Halfblock,
    }
}

/// A preference as it is written in `config.toml`.
fn pref_value<T: serde::Serialize>(value: T) -> toml::Value {
    // Preferences are booleans and unit enums; neither can fail to become a TOML value.
    toml::Value::try_from(value).expect("a preference serializes to a TOML value")
}

/// The config keys `save_ui_prefs` writes, with their values.
///
/// `image_protocol` is never among them: what the UI holds is the probe's answer for this
/// terminal, not a choice. `render_markup` is included only once the user has toggled it.
pub fn ui_pref_updates(ui: &UiState, render_markup_user_set: bool) -> Vec<(&'static str, toml::Value)> {
    let mut updates = vec![
        ("notify", pref_value(ui.notify)),
        ("show_nav_rail", pref_value(ui.show_nav_rail)),
        ("inline_images", pref_value(ui.inline_images)),
        ("density", pref_value(ui.density)),
        ("theme", pref_value(ui.theme_preset)),
    ];
    if render_markup_user_set {
        updates.push(("render_markup", pref_value(ui.render_markup)));
    }
    updates
}

fn text_preview_key_matches(key: &str, drive_id: &str, item_id: &str) -> bool {
    key.strip_prefix(drive_id)
        .and_then(|rest| rest.strip_prefix(':'))
        .is_some_and(|rest| rest == item_id)
}

pub fn copy_text_to_clipboard(text: &str) -> anyhow::Result<()> {
    let mut cb = arboard::Clipboard::new()?;
    cb.set_text(text.to_string())?;
    Ok(())
}

fn read_clipboard_png_bytes() -> anyhow::Result<Vec<u8>> {
    let mut cb = arboard::Clipboard::new()?;
    let img = cb.get_image()?;
    let rgba = image::RgbaImage::from_raw(
        img.width as u32,
        img.height as u32,
        img.bytes.into_owned(),
    )
    .ok_or_else(|| anyhow::anyhow!("Invalid RGBA clipboard image buffer"))?;
    let mut out = Vec::new();
    let dyn_img = image::DynamicImage::ImageRgba8(rgba);
    dyn_img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use axum::{
        extract::{Path, State},
        http::header,
        response::IntoResponse,
        routing::get,
        Router,
    };
    use ratatui::{backend::TestBackend, Terminal};
    use tokio::{
        net::TcpListener,
        sync::{mpsc, watch, Notify},
    };

    use super::*;
    use crate::{
        auth::{AuthManager, StoredTokens},
        config::{AppConfig, Persona},
        graph::GraphClient,
        models::DriveItem,
        storage::Storage,
        sync::{FocusTarget, SyncCommand, SyncController},
        ui::{
            action::{dispatch, Action},
            drive_view::draw_inspector,
            hitmap::HitMap,
        },
    };

    fn test_app_and_ui_with_base_url(
        base_url: Option<&str>,
    ) -> (AppState, UiState, mpsc::UnboundedReceiver<UiMsg>) {
        let mut cfg = AppConfig::default();
        let auth = if let Some(url) = base_url {
            cfg.graph_base_url = url.to_string();
            let tokens = StoredTokens {
                access_token: "synthetic-token".into(),
                refresh_token: None,
                expires_at: chrono::Utc::now().timestamp() + 3600,
                client_id: cfg.client_id.clone(),
                tenant_id: cfg.tenant_id.clone(),
                user_id: Some("user-1".into()),
                user_display_name: Some("Test User".into()),
                user_email: Some("test@example.com".into()),
                auth_mode: "access_token".into(),
            };
            AuthManager::from_tokens(cfg.clone(), tokens)
        } else {
            AuthManager::empty_for_test(cfg.clone())
        };
        let graph = GraphClient::new(cfg.clone(), auth.clone()).expect("graph");
        let storage = Storage::open_in_memory().expect("storage");
        let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
        let (nudge_tx, _nudge_rx) = mpsc::unbounded_channel::<SyncCommand>();
        let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx);
        let (ui_tx, ui_rx) = mpsc::unbounded_channel::<UiMsg>();
        let ui = UiState::from_config(&cfg);
        let app = AppState::new(cfg, Persona::SharePoint, auth, graph, storage, sync_ctrl, ui_tx);
        (app, ui, ui_rx)
    }

    fn make_folder(drive_id: &str, id: &str, name: &str) -> DriveItem {
        DriveItem {
            id: id.into(),
            drive_id: drive_id.into(),
            name: name.into(),
            is_folder: true,
            child_count: Some(0),
            ..Default::default()
        }
    }

    fn make_text_file(drive_id: &str, id: &str, name: &str, mime: &str) -> DriveItem {
        DriveItem {
            id: id.into(),
            drive_id: drive_id.into(),
            name: name.into(),
            size: 256,
            mime_type: Some(mime.into()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn folder_truncation_follows_events_and_resets_on_navigation() {
        let (mut app, mut ui, _ui_rx) = test_app_and_ui_with_base_url(None);
        app.active_source = ActiveViewSource::Drive {
            drive_id: "d1".into(),
            label: "Docs".into(),
        };
        app.navigate_to_active_folder(&mut ui);
        assert!(!app.current_items_truncated);
        assert!(app.truncation_hint().is_none());

        let page: Vec<DriveItem> = (0..200)
            .map(|i| make_text_file("d1", &format!("it-{i}"), &format!("f-{i:03}.txt"), "text/plain"))
            .collect();
        app.apply_core_event(
            CoreEvent::FolderItemsUpdated {
                drive_id: "d1".into(),
                parent_id: None,
                items: page.clone(),
                truncated: true,
            },
            &mut ui,
        );
        assert!(app.current_items_truncated);
        let (full, short) = app.truncation_hint().expect("hint for a truncated folder");
        assert!(full.contains("first 200 shown"), "{full}");
        assert!(full.contains("L loads more"), "{full}");
        assert!(short.contains("200+ (L)"), "{short}");

        // Refreshing the same folder keeps the mark until the next listing answers.
        app.load_active_folder(&mut ui);
        assert!(app.current_items_truncated);

        // An event for another folder leaves it alone.
        app.apply_core_event(
            CoreEvent::FolderItemsUpdated {
                drive_id: "d1".into(),
                parent_id: Some("elsewhere".into()),
                items: Vec::new(),
                truncated: false,
            },
            &mut ui,
        );
        assert!(app.current_items_truncated);

        // Entering a subfolder starts clean, and so does a virtual view.
        app.folder_stack.push(("sub".into(), "Sub".into()));
        app.navigate_to_active_folder(&mut ui);
        assert!(!app.current_items_truncated);

        app.folder_stack.clear();
        app.navigate_to_active_folder(&mut ui);
        app.apply_core_event(
            CoreEvent::FolderItemsUpdated {
                drive_id: "d1".into(),
                parent_id: None,
                items: page,
                truncated: true,
            },
            &mut ui,
        );
        assert!(app.current_items_truncated);
        dispatch(Action::OpenRecent, &mut app, &mut ui);
        assert!(!app.current_items_truncated);
        assert!(app.truncation_hint().is_none());

        // A complete listing clears the mark.
        app.active_source = ActiveViewSource::Drive {
            drive_id: "d1".into(),
            label: "Docs".into(),
        };
        app.navigate_to_active_folder(&mut ui);
        app.apply_core_event(
            CoreEvent::FolderItemsUpdated {
                drive_id: "d1".into(),
                parent_id: None,
                items: Vec::new(),
                truncated: false,
            },
            &mut ui,
        );
        assert!(!app.current_items_truncated);
    }

    #[tokio::test]
    async fn load_more_raises_limit_and_resets_on_navigation() {
        let (mut app, mut ui, _ui_rx) = test_app_and_ui_with_base_url(None);
        let (focus_tx, focus_rx) = watch::channel(FocusTarget::default());
        let (nudge_tx, mut nudge_rx) = mpsc::unbounded_channel::<SyncCommand>();
        app.sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx);
        let mut folder_requests = move || {
            let mut limits = Vec::new();
            while let Ok(cmd) = nudge_rx.try_recv() {
                if let SyncCommand::SyncFolder { limit, .. } = cmd {
                    limits.push(limit);
                }
            }
            limits
        };

        app.active_source = ActiveViewSource::Drive {
            drive_id: "d1".into(),
            label: "Docs".into(),
        };
        app.navigate_to_active_folder(&mut ui);
        assert_eq!(folder_requests(), vec![None], "the first page uses the default limit");
        assert_eq!(focus_rx.borrow().active_limit, None);

        // Nothing is cut short yet: the key does nothing.
        dispatch(Action::LoadMoreItems, &mut app, &mut ui);
        assert_eq!(app.folder_item_limit, FOLDER_PAGE_SIZE);
        assert!(folder_requests().is_empty());

        let page: Vec<DriveItem> = (0..200)
            .map(|i| make_text_file("d1", &format!("it-{i}"), &format!("f-{i:03}.txt"), "text/plain"))
            .collect();
        let truncated_page = |items: Vec<DriveItem>| CoreEvent::FolderItemsUpdated {
            drive_id: "d1".into(),
            parent_id: None,
            items,
            truncated: true,
        };
        app.apply_core_event(truncated_page(page.clone()), &mut ui);

        dispatch(Action::LoadMoreItems, &mut app, &mut ui);
        assert_eq!(app.folder_item_limit, 400);
        assert_eq!(folder_requests(), vec![Some(400)]);
        assert_eq!(focus_rx.borrow().active_limit, Some(400));
        assert_eq!(focus_rx.borrow().active_drive_id.as_deref(), Some("d1"));

        // A refresh of the same folder keeps the raised limit.
        app.load_active_folder(&mut ui);
        assert_eq!(folder_requests(), vec![Some(400)]);

        // Going somewhere else starts from one page again.
        app.folder_stack.push(("sub".into(), "Sub".into()));
        app.navigate_to_active_folder(&mut ui);
        assert_eq!(app.folder_item_limit, FOLDER_PAGE_SIZE);
        assert_eq!(folder_requests(), vec![None]);
        assert_eq!(focus_rx.borrow().active_limit, None);

        // At the ceiling the key asks for nothing and the note points at the CLI.
        app.folder_stack.clear();
        app.navigate_to_active_folder(&mut ui);
        app.apply_core_event(truncated_page(page), &mut ui);
        app.folder_item_limit = MAX_FOLDER_ITEMS - 100;
        let _ = folder_requests();
        dispatch(Action::LoadMoreItems, &mut app, &mut ui);
        assert_eq!(app.folder_item_limit, MAX_FOLDER_ITEMS, "the limit stops at the ceiling");
        assert_eq!(folder_requests(), vec![Some(MAX_FOLDER_ITEMS)]);
        dispatch(Action::LoadMoreItems, &mut app, &mut ui);
        assert_eq!(app.folder_item_limit, MAX_FOLDER_ITEMS);
        assert!(folder_requests().is_empty());
        let (full, _) = app.truncation_hint().unwrap();
        assert!(full.contains("ls --limit"), "{full}");

        // The key is bound where the list is, and is plain text inside a text overlay.
        let keymap = Keymap::default_sharepoint();
        let shift_l = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('L'),
            crossterm::event::KeyModifiers::SHIFT,
        );
        for focus in [Focus::List, Focus::Inspector] {
            assert_eq!(keymap.resolve(focus, shift_l), Some(Action::LoadMoreItems));
        }
        assert_eq!(keymap.resolve(Focus::Nav, shift_l), None);
        assert_eq!(
            crate::ui::event::key_to_action(shift_l, Focus::Overlay, &keymap, true),
            Some(Action::Input('L'))
        );
    }

    #[tokio::test]
    async fn ui_pref_updates_never_include_image_protocol() {
        let keys = |updates: &[(&'static str, toml::Value)]| -> Vec<&'static str> {
            updates.iter().map(|(k, _)| *k).collect()
        };
        let ui_only = UiState::from_config(&AppConfig::default());
        assert_eq!(
            keys(&ui_pref_updates(&ui_only, false)),
            ["notify", "show_nav_rail", "inline_images", "density", "theme"]
        );
        assert_eq!(
            keys(&ui_pref_updates(&ui_only, true)),
            ["notify", "show_nav_rail", "inline_images", "density", "theme", "render_markup"]
        );

        // The same through the real actions, against a config file of our own.
        let _env_lock = crate::config::TEST_ENV_LOCK.lock().await;
        let _clean = crate::config::EnvVarGuard::clear_auth_env();
        let dir = tempfile::tempdir().unwrap();
        let dir_str = dir.path().to_string_lossy().into_owned();
        let _env = crate::config::EnvVarGuard::set(&[(
            "SHAREPOINT_CONFIG_DIR",
            Some(dir_str.as_str()),
        )]);
        let path = crate::config::config_file_path();
        assert!(path.starts_with(dir.path()));
        let read = || -> toml::Table {
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
        };

        let (mut app, mut ui, mut ui_rx) = test_app_and_ui_with_base_url(None);
        ui.graphics_caps.protocol = TerminalImageProtocol::Kitty; // what a probe would find
        assert_eq!(app.config.image_protocol, ImageProtocolPreference::Auto);

        dispatch(Action::ToggleDensity, &mut app, &mut ui);
        let saved = read();
        assert_eq!(saved.len(), 5, "only the UI preferences: {saved:?}");
        assert!(!saved.contains_key("image_protocol"), "{saved:?}");
        assert!(!saved.contains_key("render_markup"), "{saved:?}");
        assert!(!saved.contains_key("graph_base_url"), "{saved:?}");
        assert_eq!(saved["density"], toml::Value::try_from(ui.density).unwrap());
        assert_eq!(AppConfig::load_from(&path).unwrap().density, ui.density);
        assert_eq!(app.config.image_protocol, ImageProtocolPreference::Auto);

        // Toggled by the user: saved from now on, also when toggled back.
        let rendered_at_start = ui.render_markup;
        dispatch(Action::TogglePreviewRenderMode, &mut app, &mut ui);
        assert_eq!(read()["render_markup"].as_bool(), Some(!rendered_at_start));
        dispatch(Action::TogglePreviewRenderMode, &mut app, &mut ui);
        assert_eq!(read()["render_markup"].as_bool(), Some(rendered_at_start));
        assert!(!read().contains_key("image_protocol"));

        // Picking a protocol by hand is a choice, and that is saved.
        dispatch(Action::CycleImageProtocol, &mut app, &mut ui);
        assert_eq!(ui.graphics_caps.protocol, TerminalImageProtocol::ITerm2);
        assert_eq!(read()["image_protocol"].as_str(), Some("iterm2"));
        assert_eq!(app.config.image_protocol, ImageProtocolPreference::Iterm2);
        assert_eq!(
            AppConfig::load_from(&path).unwrap().image_protocol,
            ImageProtocolPreference::Iterm2
        );

        // A config file that does not parse is left alone, and the user is told.
        while ui_rx.try_recv().is_ok() {}
        std::fs::write(&path, "notify = = true\n").unwrap();
        dispatch(Action::ToggleNotify, &mut app, &mut ui);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "notify = = true\n");
        let mut told = false;
        while let Ok(msg) = ui_rx.try_recv() {
            if let UiMsg::Toast { message, .. } = msg {
                told |= message.contains("Settings not saved");
            }
        }
        assert!(told, "a failed save must reach the user");
    }

    #[test]
    fn raw_image_cache_evicts_oldest_within_both_limits() {
        let (mut app, _, _) = test_app_and_ui_with_base_url(None);
        let budget = img_preview::MAX_IMAGE_CACHE_BYTES;
        let large: Arc<[u8]> = Arc::from(vec![0; 5_942_323]);
        for i in 0..9 {
            let key = i.to_string();
            app.cache_raw_image(key.clone(), Arc::clone(&large));
            assert_eq!(app.raw_image_bytes.len(), 1);
            assert_eq!(app.raw_image_bytes[0].0, key);
        }

        app.cache_raw_image("oversized".into(), Arc::from(vec![0; budget + 1]));
        assert_eq!(app.raw_image_bytes.len(), 1);
        assert_eq!(app.raw_image_bytes[0].0, "8");

        for i in 0..MAX_RAW_IMAGE_ENTRIES {
            app.cache_raw_image(format!("small-{i}"), Arc::from([0]));
        }
        assert_eq!(app.raw_image_bytes.len(), MAX_RAW_IMAGE_ENTRIES);
        assert_eq!(app.raw_image_bytes.front().unwrap().0, "small-0");
        app.cache_raw_image("small-0".into(), Arc::from([1]));
        assert_eq!(app.raw_image_bytes.len(), MAX_RAW_IMAGE_ENTRIES);
        assert_eq!(app.raw_image_bytes.front().unwrap().0, "small-1");
        assert_eq!(app.raw_image_bytes.back().unwrap().1.as_ref(), &[1]);
    }

    #[tokio::test]
    async fn hd_overlay_decodes_current_cached_image_and_cycles() {
        let (mut app, mut ui, mut rx) = test_app_and_ui_with_base_url(None);
        ui.graphics_caps.protocol = TerminalImageProtocol::Halfblock;
        for (width, name) in [(4, "a.png"), (8, "b.png")] {
            let item = make_text_file("drive", name, name, "image/png");
            let mut png = std::io::Cursor::new(Vec::new());
            image::RgbaImage::from_pixel(width, 4, image::Rgba([80, 90, 100, 255]))
                .write_to(&mut png, image::ImageFormat::Png)
                .unwrap();
            app.cache_raw_image(AppState::item_revision_key(&item), Arc::from(png.into_inner()));
            app.current_items.push(item);
        }
        app.active_selected_id = Some("a.png".into());
        for (action, expected_name, width) in [
            (Action::OpenImagePreview, "a.png", 4),
            (Action::Input(']'), "b.png", 8),
            (Action::Input('['), "a.png", 4),
        ] {
            dispatch(action, &mut app, &mut ui);
            let Some(Overlay::ImagePreview(overlay)) = ui.overlays.last() else {
                panic!("expected HD overlay");
            };
            let item = overlay.current().unwrap();
            assert_eq!(item.name, expected_name);
            let (cols, rows) = overlay.target_dimensions(ui.term_width, ui.term_height);
            let key = app.modal_image_cache_key(item, cols, rows, ui.graphics_caps.protocol);
            if !app.rendered_images.contains_key(&key) {
                let msg = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
                    .await
                    .unwrap()
                    .unwrap();
                app.apply_ui_msg(msg, &mut ui);
            }
            assert_eq!(app.rendered_images[&key].width_px, width);
        }
        assert!(app.inflight_raw_images.is_empty());
    }

    fn draw_inspector_frame(app: &AppState, ui: &mut UiState) -> String {
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).expect("terminal");
        let mut hitmap = HitMap::new();
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw_inspector(frame, area, app, ui, &mut hitmap);
            })
            .expect("draw");
        let buf = terminal.backend().buffer();
        let mut lines = Vec::with_capacity(24);
        for y in 0..24 {
            let mut row = String::new();
            for x in 0..90 {
                row.push_str(buf[(x, y)].symbol());
            }
            lines.push(row);
        }
        lines.join("\n")
    }

    #[tokio::test]
    async fn text_preview_and_markup_cache_bound_across_96_files_and_folder_return() {
        let (mut app, mut ui, _rx) = test_app_and_ui_with_base_url(None);
        app.active_source = ActiveViewSource::Drive {
            drive_id: "drv-1".into(),
            label: "Docs".into(),
        };

        let mut items = Vec::with_capacity(97);
        items.push(make_folder("drv-1", "fld-0", "00-Archive"));
        for idx in 1..=96 {
            items.push(make_text_file(
                "drv-1",
                &format!("file-{idx:02}"),
                &format!("note-{idx:02}.md"),
                "text/markdown",
            ));
        }
        app.current_items = items;
        app.active_selected_id = Some("fld-0".into());
        ui.list_scroll.selected_idx = Some(0);
        ui.focus = Focus::List;

        for idx in 1..=96 {
            dispatch(Action::MoveDown, &mut app, &mut ui);
            let id = format!("file-{idx:02}");
            let expected_key = AppState::text_preview_key("drv-1", &id);
            assert_eq!(app.active_selected_id.as_deref(), Some(id.as_str()));
            assert!(
                app.text_previews.is_none(),
                "moving to {id} must drop prior file preview"
            );
            assert!(
                ui.markup_cache.is_none(),
                "moving to {id} must drop prior markup cache"
            );

            app.apply_ui_msg(
                UiMsg::TextPreviewLoaded {
                    drive_id: "drv-1".into(),
                    item_id: id.clone(),
                    revision: None,
                    content: format!("# Document {idx:02}\n\nBody marker {idx:02}.\n"),
                },
                &mut ui,
            );

            assert_eq!(
                app.text_previews.as_ref().map(|(k, _)| k.as_str()),
                Some(expected_key.as_str())
            );
            let screen = draw_inspector_frame(&app, &mut ui);
            assert!(screen.contains(&format!("Document {idx:02}")), "{screen}");
            assert_eq!(
                ui.markup_cache.as_ref().map(|c| c.key.item_id.as_str()),
                Some(expected_key.as_str())
            );
        }

        // Toggling Rendered <-> Raw on the active file preserves the loaded body.
        ui.render_markup = false;
        let raw_screen = draw_inspector_frame(&app, &mut ui);
        assert!(raw_screen.contains("# Document 96"), "{raw_screen}");
        assert!(app.text_previews.is_some());
        ui.render_markup = true;
        let rendered_screen = draw_inspector_frame(&app, &mut ui);
        assert!(rendered_screen.contains("Document 96"), "{rendered_screen}");

        // Returning to the folder row via Home (`Action::ScrollTop`) frees both caches.
        dispatch(Action::ScrollTop, &mut app, &mut ui);
        assert_eq!(app.active_selected_id.as_deref(), Some("fld-0"));
        assert!(app.text_previews.is_none());
        assert!(ui.markup_cache.is_none());
    }

    #[tokio::test]
    async fn folder_drive_and_sync_transitions_clear_preview_and_reject_stale_responses() {
        let (mut app, mut ui, _rx) = test_app_and_ui_with_base_url(None);
        app.active_source = ActiveViewSource::Drive {
            drive_id: "drv-1".into(),
            label: "Drive One".into(),
        };
        let sub = make_folder("drv-1", "fld-sub", "Subfolder");
        let doc = make_text_file("drv-1", "shared-id", "spec.md", "text/markdown");
        app.current_items = vec![sub.clone(), doc.clone()];
        app.active_selected_id = Some(doc.id.clone());
        ui.list_scroll.selected_idx = Some(1);
        ui.focus = Focus::List;

        app.set_text_preview("drv-1", "shared-id", "# Drive One Spec\n".into());
        let _ = draw_inspector_frame(&app, &mut ui);
        assert!(app.text_previews.is_some());
        assert!(ui.markup_cache.is_some());

        // Switching drive clears preview & markup cache immediately even before FolderLoaded arrives.
        let stale_inflight = AppState::text_preview_inflight_key("drv-1", "shared-id");
        app.inflight_previews.insert(stale_inflight.clone());
        dispatch(
            Action::SelectDrive {
                drive_id: "drv-2".into(),
                label: "Drive Two".into(),
            },
            &mut app,
            &mut ui,
        );
        assert!(app.text_previews.is_none());
        assert!(ui.markup_cache.is_none());

        // Even if Drive Two also has an item with the same `item_id`, Drive One's late
        // PreviewFailed and TextPreviewLoaded must not clear Drive Two's pending request/error.
        let doc_d2 = make_text_file("drv-2", "shared-id", "spec.md", "text/markdown");
        let d2_inflight = AppState::text_preview_inflight_key("drv-2", "shared-id");
        app.apply_core_event(
            CoreEvent::FolderItemsUpdated {
                drive_id: "drv-2".into(),
                parent_id: None,
                items: vec![doc_d2.clone()],
                truncated: false,
            },
            &mut ui,
        );
        assert!(app.inflight_previews.contains(&d2_inflight));

        // Drive One fails late -> removes only drv-1's inflight marker, preserves drv-2's pending request
        // and does not attach drv-1's error to drv-2.
        app.apply_ui_msg(
            UiMsg::PreviewFailed {
                drive_id: "drv-1".into(),
                item_id: "shared-id".into(),
                revision: None,
                inflight_key: Some(stale_inflight.clone()),
                error: "drv-1 timeout".into(),
            },
            &mut ui,
        );
        assert!(!app.inflight_previews.contains(&stale_inflight));
        assert!(app.inflight_previews.contains(&d2_inflight));
        assert_eq!(app.text_preview_error_for(&doc_d2), None);

        // Drive Two fails -> records drv-2 error; another stale drv-1 failure or success must keep it intact.
        app.apply_ui_msg(
            UiMsg::PreviewFailed {
                drive_id: "drv-2".into(),
                item_id: "shared-id".into(),
                revision: None,
                inflight_key: Some(d2_inflight.clone()),
                error: "drv-2 403".into(),
            },
            &mut ui,
        );
        assert_eq!(app.text_preview_error_for(&doc_d2), Some("drv-2 403"));

        app.inflight_previews.insert(stale_inflight.clone());
        app.apply_ui_msg(
            UiMsg::TextPreviewLoaded {
                drive_id: "drv-1".into(),
                item_id: "shared-id".into(),
                revision: None,
                content: "# Stale Drive One Body\n".into(),
            },
            &mut ui,
        );
        assert!(!app.inflight_previews.contains(&stale_inflight));
        assert!(
            app.text_previews.is_none(),
            "late response from drv-1 must not populate drv-2 item with same id"
        );
        assert_eq!(
            app.text_preview_error_for(&doc_d2),
            Some("drv-2 403"),
            "late response from drv-1 must not clear drv-2 text error"
        );

        // Populate Drive Two's preview, then enter a folder -> cleared immediately.
        app.apply_ui_msg(
            UiMsg::TextPreviewLoaded {
                drive_id: "drv-2".into(),
                item_id: "shared-id".into(),
                revision: None,
                content: "# Drive Two Spec\n".into(),
            },
            &mut ui,
        );
        let _ = draw_inspector_frame(&app, &mut ui);
        assert!(app.text_previews.is_some());
        assert!(ui.markup_cache.is_some());

        let sub_d2 = make_folder("drv-2", "fld-2", "Child");
        app.current_items = vec![sub_d2, doc_d2.clone()];
        app.active_selected_id = Some("fld-2".into());
        ui.list_scroll.selected_idx = Some(0);
        dispatch(Action::Activate, &mut app, &mut ui);
        assert!(app.text_previews.is_none());
        assert!(ui.markup_cache.is_none());

        // Background sync removing the active file also clears its preview and markup cache.
        app.folder_stack.clear();
        app.current_items = vec![doc_d2.clone()];
        app.active_selected_id = Some(doc_d2.id.clone());
        app.set_text_preview("drv-2", "shared-id", "# Active\n".into());
        let _ = draw_inspector_frame(&app, &mut ui);
        assert!(app.text_previews.is_some());
        assert!(ui.markup_cache.is_some());

        app.apply_core_event(
            CoreEvent::FolderItemsUpdated {
                drive_id: "drv-2".into(),
                parent_id: None,
                items: vec![make_folder("drv-2", "fld-only", "OnlyFolder")],
                truncated: false,
            },
            &mut ui,
        );
        assert_eq!(app.active_selected_id.as_deref(), Some("fld-only"));
        assert!(app.text_previews.is_none());
        assert!(ui.markup_cache.is_none());
    }

    #[tokio::test]
    async fn out_of_order_responses_pending_dedup_and_revisit_refetch() {
        #[derive(Clone)]
        struct DelayServer {
            a_hits: Arc<AtomicUsize>,
            b_hits: Arc<AtomicUsize>,
            release_a: Arc<Notify>,
            a_started: Arc<Notify>,
        }

        let state = DelayServer {
            a_hits: Arc::new(AtomicUsize::new(0)),
            b_hits: Arc::new(AtomicUsize::new(0)),
            release_a: Arc::new(Notify::new()),
            a_started: Arc::new(Notify::new()),
        };

        let router = Router::new()
            .route(
                "/v1.0/drives/{drive_id}/items/{item_id}/content",
                get(
                    |State(st): State<DelayServer>, Path((_d, item_id)): Path<(String, String)>| async move {
                        if item_id == "file-1" {
                            let hit = st.a_hits.fetch_add(1, Ordering::SeqCst);
                            st.a_started.notify_one();
                            if hit == 0 {
                                st.release_a.notified().await;
                            }
                            (
                                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                                format!("BODY_A_HIT_{}", hit + 1),
                            )
                                .into_response()
                        } else {
                            let hit = st.b_hits.fetch_add(1, Ordering::SeqCst);
                            (
                                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                                format!("BODY_B_HIT_{}", hit + 1),
                            )
                                .into_response()
                        }
                    },
                ),
            )
            .route(
                "/v1.0/drives/{drive_id}/items/{item_id}/versions",
                get(|| async { axum::Json(serde_json::json!({ "value": [] })) }),
            )
            .with_state(state.clone());

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let base_url = format!("http://{addr}/v1.0");
        let (mut app, mut ui, mut rx) = test_app_and_ui_with_base_url(Some(&base_url));
        let item_a = make_text_file("drv-1", "file-1", "a.txt", "text/plain");
        let item_b = make_text_file("drv-1", "file-10", "b.txt", "text/plain");
        app.current_items = vec![item_a.clone(), item_b.clone()];
        app.active_selected_id = Some(item_a.id.clone());
        ui.list_scroll.selected_idx = Some(0);
        ui.focus = Focus::List;

        // 1. Start loading A (held open on server), then move A -> B -> A while A is pending.
        app.ensure_inspector_loaded(&ui);
        state.a_started.notified().await;
        assert_eq!(state.a_hits.load(Ordering::SeqCst), 1);

        dispatch(Action::MoveDown, &mut app, &mut ui);
        assert_eq!(app.active_selected_id.as_deref(), Some("file-10"));
        // Pending A must remain tracked so moving back to A does not fire a duplicate GET.
        assert!(app
            .inflight_previews
            .contains(&AppState::text_preview_inflight_key("drv-1", "file-1")));

        dispatch(Action::MoveUp, &mut app, &mut ui);
        assert_eq!(app.active_selected_id.as_deref(), Some("file-1"));
        assert_eq!(
            state.a_hits.load(Ordering::SeqCst),
            1,
            "returning to A while A is still pending must reuse the pending request"
        );

        // Drain B's response first (now stale because A is selected).
        let mut saw_b = false;
        while !saw_b {
            let msg = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
                .await
                .expect("expected B response")
                .expect("channel open");
            if matches!(&msg, UiMsg::TextPreviewLoaded { item_id, .. } if item_id == "file-10") {
                saw_b = true;
            }
            app.apply_ui_msg(msg, &mut ui);
        }
        assert!(
            app.text_previews.is_none(),
            "late B response must be discarded while A is selected"
        );
        assert!(!app
            .inflight_previews
            .contains(&AppState::text_preview_inflight_key("drv-1", "file-10")));

        // Release A -> A populates inspector.
        state.release_a.notify_one();
        let mut saw_a = false;
        while !saw_a {
            let msg = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
                .await
                .expect("expected A response")
                .expect("channel open");
            if matches!(&msg, UiMsg::TextPreviewLoaded { item_id, .. } if item_id == "file-1") {
                saw_a = true;
            }
            app.apply_ui_msg(msg, &mut ui);
        }
        assert_eq!(app.text_preview_for(&item_a), Some("BODY_A_HIT_1"));

        // 2. Move A -> B (evicts A, fetches B) -> A (evicts B, re-fetches A).
        dispatch(Action::MoveDown, &mut app, &mut ui);
        assert!(app.text_previews.is_none());
        let mut saw_b2 = false;
        while !saw_b2 {
            let msg = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
                .await
                .expect("expected B2 response")
                .expect("channel open");
            if matches!(&msg, UiMsg::TextPreviewLoaded { item_id, .. } if item_id == "file-10") {
                saw_b2 = true;
            }
            app.apply_ui_msg(msg, &mut ui);
        }
        assert_eq!(app.text_preview_for(&item_b), Some("BODY_B_HIT_2"));

        dispatch(Action::MoveUp, &mut app, &mut ui);
        assert!(app.text_previews.is_none());
        let mut saw_a2 = false;
        while !saw_a2 {
            let msg = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
                .await
                .expect("expected A2 response")
                .expect("channel open");
            if matches!(&msg, UiMsg::TextPreviewLoaded { item_id, .. } if item_id == "file-1") {
                saw_a2 = true;
            }
            app.apply_ui_msg(msg, &mut ui);
        }
        assert_eq!(state.a_hits.load(Ordering::SeqCst), 2);
        assert_eq!(app.text_preview_for(&item_a), Some("BODY_A_HIT_2"));

        // 3. Out-of-order late A arriving while B is selected must keep B displayed.
        dispatch(Action::MoveDown, &mut app, &mut ui);
        let mut saw_b3 = false;
        while !saw_b3 {
            let msg = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
                .await
                .expect("expected B3 response")
                .expect("channel open");
            if matches!(&msg, UiMsg::TextPreviewLoaded { item_id, .. } if item_id == "file-10") {
                saw_b3 = true;
            }
            app.apply_ui_msg(msg, &mut ui);
        }
        app.apply_ui_msg(
            UiMsg::TextPreviewLoaded {
                drive_id: "drv-1".into(),
                item_id: "file-1".into(),
                revision: None,
                content: "LATE_A_MUST_NOT_REPLACE_B".into(),
            },
            &mut ui,
        );
        assert_eq!(app.text_preview_for(&item_b), Some("BODY_B_HIT_3"));

        server.abort();
    }

    #[tokio::test]
    async fn explicit_html_overlay_and_browser_serve_use_requested_file_after_selection_change() {
        let (mut app, mut ui, mut rx) = test_app_and_ui_with_base_url(None);
        let html_item = make_text_file("drv-1", "html-1", "report.html", "text/html");
        let md_item = make_text_file("drv-1", "md-1", "notes.md", "text/markdown");
        app.current_items = vec![html_item.clone(), md_item.clone()];
        app.active_selected_id = Some(md_item.id.clone());
        ui.list_scroll.selected_idx = Some(1);
        app.set_text_preview("drv-1", "md-1", "# Active Notes\n".into());
        let _ = draw_inspector_frame(&app, &mut ui);

        // Simulate an in-flight auto-preview for html-1 plus an explicitly requested HtmlPreviewReady
        // arriving after selection moved to md-1.
        let html_inflight = AppState::text_preview_inflight_key("drv-1", "html-1");
        app.inflight_previews.insert(html_inflight.clone());
        let requested_html = "<html><body><h1>Requested Report</h1></body></html>".to_string();
        app.apply_ui_msg(
            UiMsg::HtmlPreviewReady {
                drive_id: "drv-1".into(),
                item_id: "html-1".into(),
                revision: None,
                title: "report.html".into(),
                html: requested_html.clone(),
            },
            &mut ui,
        );

        // Inspector caches stay bound to current selection (md-1), and pending auto-preview marker stays intact.
        assert_eq!(app.text_preview_for(&md_item), Some("# Active Notes\n"));
        assert!(app.text_preview_for(&html_item).is_none());
        assert_eq!(
            ui.markup_cache.as_ref().map(|c| c.key.item_id.as_str()),
            Some("drv-1:md-1")
        );
        assert!(app.inflight_previews.contains(&html_inflight));

        // Explicit HTML overlay still opens with report.html's body, and pressing `o` serves report.html.
        match ui.overlays.last() {
            Some(Overlay::HtmlPreview(ov)) => {
                assert_eq!(ov.title, "report.html");
                assert_eq!(ov.session.raw_html(), requested_html);
            }
            other => panic!("expected HtmlPreview overlay, got {other:?}"),
        }

        dispatch(Action::Input('o'), &mut app, &mut ui);
        let served = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
            .await
            .expect("expected UiMsg::HtmlServed from overlay serve")
            .expect("channel open");
        let url = match served {
            UiMsg::HtmlServed { url } => url,
            _ => panic!("expected UiMsg::HtmlServed"),
        };
        assert!(url.starts_with("http://127.0.0.1:"), "{url}");
        let body = reqwest::get(&url).await.unwrap().text().await.unwrap();
        assert!(body.contains("Requested Report"), "{body}");
        app.html_server.shutdown();
    }

    #[tokio::test]
    async fn external_edit_same_id_refreshes_tui_text_html_image_and_versions() {
        use std::io::Cursor;
        use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};

        fn make_png(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
            let mut out = Vec::new();
            let img = RgbaImage::from_pixel(w, h, Rgba(rgba));
            DynamicImage::ImageRgba8(img)
                .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
                .unwrap();
            out
        }

        #[derive(Clone)]
        struct RevisionServer {
            version: Arc<AtomicUsize>,
            doc_hits: Arc<AtomicUsize>,
            html_hits: Arc<AtomicUsize>,
            img_hits: Arc<AtomicUsize>,
            ver_hits: Arc<AtomicUsize>,
        }

        let st = RevisionServer {
            version: Arc::new(AtomicUsize::new(1)),
            doc_hits: Arc::new(AtomicUsize::new(0)),
            html_hits: Arc::new(AtomicUsize::new(0)),
            img_hits: Arc::new(AtomicUsize::new(0)),
            ver_hits: Arc::new(AtomicUsize::new(0)),
        };

        let router = Router::new()
            .route(
                "/v1.0/drives/{drive_id}/items/{item_id}/content",
                get(
                    |State(s): State<RevisionServer>, Path((drive_id, item_id)): Path<(String, String)>| async move {
                        let ver = s.version.load(Ordering::SeqCst);
                        match item_id.as_str() {
                            "item-doc" => {
                                s.doc_hits.fetch_add(1, Ordering::SeqCst);
                                (
                                    [(header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
                                    format!("# Doc v{ver}\nBody revision {ver}\n"),
                                )
                                    .into_response()
                            }
                            "item-html" => {
                                s.html_hits.fetch_add(1, Ordering::SeqCst);
                                (
                                    [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                                    format!("<html><body><h1>HTML v{ver}</h1></body></html>"),
                                )
                                    .into_response()
                            }
                            "item-img" => {
                                s.img_hits.fetch_add(1, Ordering::SeqCst);
                                let png = if drive_id == "drv-2" {
                                    make_png(4, 4, [0, 0, 255, 255])
                                } else if ver == 1 {
                                    make_png(1, 1, [255, 0, 0, 255])
                                } else {
                                    make_png(3, 3, [0, 255, 0, 255])
                                };
                                ([(header::CONTENT_TYPE, "image/png")], png).into_response()
                            }
                            _ => axum::http::StatusCode::NOT_FOUND.into_response(),
                        }
                    },
                ),
            )
            .route(
                "/v1.0/drives/{drive_id}/items/{item_id}/versions",
                get(|State(s): State<RevisionServer>| async move {
                    let ver = s.ver_hits.fetch_add(1, Ordering::SeqCst);
                    let cur = s.version.load(Ordering::SeqCst);
                    let _ = ver;
                    let list = if cur == 1 {
                        serde_json::json!({ "value": [{ "id": "1.0", "size": 24 }] })
                    } else {
                        serde_json::json!({
                            "value": [
                                { "id": "2.0", "size": 48 },
                                { "id": "1.0", "size": 24 }
                            ]
                        })
                    };
                    axum::Json(list)
                }),
            )
            .with_state(st.clone());

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let base_url = format!("http://{addr}/v1.0");
        let (mut app, mut ui, mut rx) = test_app_and_ui_with_base_url(Some(&base_url));
        ui.inline_images = true;
        ui.graphics_caps.protocol = TerminalImageProtocol::Halfblock;
        app.active_source = ActiveViewSource::Drive {
            drive_id: "drv-1".into(),
            label: "Docs".into(),
        };

        let make_rev_item = |drive: &str, id: &str, name: &str, mime: &str, ver: usize| DriveItem {
            id: id.into(),
            drive_id: drive.into(),
            name: name.into(),
            size: (20 * ver) as u64,
            mime_type: Some(mime.into()),
            e_tag: Some(format!("\"etag-{id}-v{ver}\"")),
            c_tag: Some(format!("\"ctag-{id}-v{ver}\"")),
            last_modified_at: Some(format!("2026-11-20T0{ver}:00:00Z")),
            ..Default::default()
        };

        let doc_v1 = make_rev_item("drv-1", "item-doc", "doc.md", "text/markdown", 1);
        let html_v1 = make_rev_item("drv-1", "item-html", "page.html", "text/html", 1);
        let img_v1 = make_rev_item("drv-1", "item-img", "pic.png", "image/png", 1);
        app.current_items = vec![doc_v1.clone(), html_v1.clone(), img_v1.clone()];
        app.active_selected_id = Some("item-doc".into());
        ui.list_scroll.selected_idx = Some(0);

        // 1. Load Markdown v1 + versions v1
        app.ensure_inspector_loaded(&ui);
        let mut saw_doc_v1 = false;
        let mut saw_ver_v1 = false;
        while !(saw_doc_v1 && saw_ver_v1) {
            let msg = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
                .await
                .expect("expected v1 doc/versions")
                .expect("channel open");
            if matches!(&msg, UiMsg::TextPreviewLoaded { item_id, .. } if item_id == "item-doc") {
                saw_doc_v1 = true;
            }
            if matches!(&msg, UiMsg::VersionsLoaded { item_id, .. } if item_id == "item-doc") {
                saw_ver_v1 = true;
            }
            app.apply_ui_msg(msg, &mut ui);
        }
        assert_eq!(app.text_preview_for(&doc_v1), Some("# Doc v1\nBody revision 1\n"));
        let screen_v1 = draw_inspector_frame(&app, &mut ui);
        assert!(screen_v1.contains("Doc v1"), "{screen_v1}");

        // External edit on server -> v2 for all 3 items, delivered via FolderItemsUpdated
        st.version.store(2, Ordering::SeqCst);
        let doc_v2 = make_rev_item("drv-1", "item-doc", "doc.md", "text/markdown", 2);
        let html_v2 = make_rev_item("drv-1", "item-html", "page.html", "text/html", 2);
        let img_v2 = make_rev_item("drv-1", "item-img", "pic.png", "image/png", 2);

        app.apply_core_event(
            CoreEvent::FolderItemsUpdated {
                drive_id: "drv-1".into(),
                parent_id: None,
                items: vec![doc_v2.clone(), html_v2.clone(), img_v2.clone()],
                truncated: false,
            },
            &mut ui,
        );
        assert!(
            app.text_preview_for(&doc_v2).is_none(),
            "stale v1 text preview must be invalidated when revision changes to v2"
        );
        assert!(
            ui.markup_cache.is_none(),
            "stale v1 markup cache must be invalidated when revision changes to v2"
        );

        let mut saw_doc_v2 = false;
        let mut saw_ver_v2 = false;
        while !(saw_doc_v2 && saw_ver_v2) {
            let msg = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
                .await
                .expect("expected v2 doc/versions refetch after external edit")
                .expect("channel open");
            if matches!(&msg, UiMsg::TextPreviewLoaded { item_id, .. } if item_id == "item-doc") {
                saw_doc_v2 = true;
            }
            if matches!(&msg, UiMsg::VersionsLoaded { item_id, .. } if item_id == "item-doc") {
                saw_ver_v2 = true;
            }
            app.apply_ui_msg(msg, &mut ui);
        }
        assert_eq!(st.doc_hits.load(Ordering::SeqCst), 2);
        assert_eq!(app.text_preview_for(&doc_v2), Some("# Doc v2\nBody revision 2\n"));
        let screen_v2 = draw_inspector_frame(&app, &mut ui);
        assert!(screen_v2.contains("Doc v2"), "{screen_v2}");
        assert!(screen_v2.contains("v2.0"), "inspector must show refreshed v2.0 version:\n{screen_v2}");

        // 2. HTML file: load v2 (reset to v1 first), open overlay, then bump to v2 -> overlay updates
        st.version.store(1, Ordering::SeqCst);
        app.current_items = vec![doc_v1.clone(), html_v1.clone(), img_v1.clone()];
        app.active_selected_id = Some("item-html".into());
        ui.list_scroll.selected_idx = Some(1);
        app.sync_inspector_preview_for(Some(&html_v1), &mut ui);
        app.ensure_inspector_loaded(&ui);
        let mut saw_html_v1 = false;
        while !saw_html_v1 {
            let msg = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
                .await
                .expect("expected html v1")
                .expect("channel open");
            if matches!(&msg, UiMsg::TextPreviewLoaded { item_id, .. } if item_id == "item-html") {
                saw_html_v1 = true;
            }
            app.apply_ui_msg(msg, &mut ui);
        }
        dispatch(Action::OpenHtmlPreview, &mut app, &mut ui);
        match ui.overlays.last() {
            Some(Overlay::HtmlPreview(ov)) => {
                assert!(ov.session.raw_html().contains("HTML v1"));
            }
            other => panic!("expected HtmlPreview overlay, got {other:?}"),
        }

        // External edit HTML v1 -> v2 while HTML overlay is open
        st.version.store(2, Ordering::SeqCst);
        app.apply_core_event(
            CoreEvent::FolderItemsUpdated {
                drive_id: "drv-1".into(),
                parent_id: None,
                items: vec![doc_v2.clone(), html_v2.clone(), img_v2.clone()],
                truncated: false,
            },
            &mut ui,
        );
        let mut saw_html_text_v2 = false;
        let mut saw_html_overlay_v2 = false;
        while !(saw_html_text_v2 && saw_html_overlay_v2) {
            let msg = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
                .await
                .expect("expected html v2 refetch")
                .expect("channel open");
            if matches!(&msg, UiMsg::TextPreviewLoaded { item_id, .. } if item_id == "item-html") {
                saw_html_text_v2 = true;
            }
            if matches!(&msg, UiMsg::HtmlPreviewReady { item_id, .. } if item_id == "item-html") {
                saw_html_overlay_v2 = true;
            }
            app.apply_ui_msg(msg, &mut ui);
        }
        assert_eq!(
            app.text_preview_for(&html_v2),
            Some("<html><body><h1>HTML v2</h1></body></html>")
        );
        match ui.overlays.last() {
            Some(Overlay::HtmlPreview(ov)) => {
                assert!(
                    ov.session.raw_html().contains("HTML v2"),
                    "open HTML overlay must refresh to v2, got: {}",
                    ov.session.raw_html()
                );
            }
            other => panic!("expected updated HtmlPreview overlay, got {other:?}"),
        }
        ui.pop_overlay();

        // 3. Image file: load v1 (1x1 red), then external edit -> v2 (3x3 green) -> must re-fetch & re-decode!
        st.version.store(1, Ordering::SeqCst);
        app.current_items = vec![doc_v1.clone(), html_v1.clone(), img_v1.clone()];
        app.active_selected_id = Some("item-img".into());
        ui.list_scroll.selected_idx = Some(2);
        app.sync_inspector_preview_for(Some(&img_v1), &mut ui);
        app.ensure_inspector_loaded(&ui);
        let v1_img_key = app.inline_image_cache_key(&img_v1);
        let mut saw_img_v1 = false;
        while !saw_img_v1 {
            let msg = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
                .await
                .expect("expected img v1 decode")
                .expect("channel open");
            if matches!(&msg, UiMsg::ImageDecoded { cache_key, .. } if *cache_key == v1_img_key) {
                saw_img_v1 = true;
            }
            app.apply_ui_msg(msg, &mut ui);
        }
        assert_eq!(app.rendered_images.get(&v1_img_key).unwrap().width_px, 1);

        st.version.store(2, Ordering::SeqCst);
        app.apply_core_event(
            CoreEvent::FolderItemsUpdated {
                drive_id: "drv-1".into(),
                parent_id: None,
                items: vec![doc_v2.clone(), html_v2.clone(), img_v2.clone()],
                truncated: false,
            },
            &mut ui,
        );
        let v2_img_key = app.inline_image_cache_key(&img_v2);
        let mut saw_img_v2 = false;
        while !saw_img_v2 {
            let msg = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
                .await
                .expect("expected img v2 decode after external edit")
                .expect("channel open");
            if matches!(&msg, UiMsg::ImageDecoded { cache_key, .. } if *cache_key == v2_img_key) {
                saw_img_v2 = true;
            }
            app.apply_ui_msg(msg, &mut ui);
        }
        assert_eq!(app.rendered_images.get(&v2_img_key).unwrap().width_px, 3);
        assert!(st.img_hits.load(Ordering::SeqCst) >= 2);

        // 4. Cross-drive isolation for identical image item_id (`drv-2` 4x4 blue vs `drv-1` 3x3 green)
        let img_drv2 = make_rev_item("drv-2", "item-img", "pic.png", "image/png", 2);
        app.active_source = ActiveViewSource::Drive {
            drive_id: "drv-2".into(),
            label: "Drive Two".into(),
        };
        app.current_items = vec![img_drv2.clone()];
        app.active_selected_id = Some("item-img".into());
        ui.list_scroll.selected_idx = Some(0);
        app.ensure_inspector_loaded(&ui);
        let d2_img_key = app.inline_image_cache_key(&img_drv2);
        let mut saw_img_d2 = false;
        while !saw_img_d2 {
            let msg = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
                .await
                .expect("expected drv-2 img decode")
                .expect("channel open");
            if matches!(&msg, UiMsg::ImageDecoded { cache_key, .. } if *cache_key == d2_img_key) {
                saw_img_d2 = true;
            }
            app.apply_ui_msg(msg, &mut ui);
        }
        assert_eq!(
            app.rendered_images.get(&d2_img_key).unwrap().width_px,
            4,
            "drv-2 image must not reuse drv-1 raw_image_bytes for same item_id"
        );
        // Verify HTML overlay + inspector shared a single GET /content for v2 (1 for v1 + 1 for v2 = 2 total)
        assert_eq!(
            st.html_hits.load(Ordering::SeqCst),
            2,
            "HTML inspector + open HtmlPreviewOverlay must share one /content download per revision"
        );

        server.abort();
    }

    #[tokio::test]
    async fn view_preservation_across_recent_shared_myfiles_sites_update_and_retired_virtual_lists() {
        let (mut app, mut ui, _rx) = test_app_and_ui_with_base_url(None);
        app.persona = Persona::SharePoint;
        app.my_drive = Some(DriveInfo {
            id: "drv-me".into(),
            name: "OneDrive".into(),
            drive_type: "business".into(),
            ..Default::default()
        });
        let sp_site = SharePointSite {
            id: "site-eng".into(),
            name: "engineering".into(),
            display_name: "Engineering Hub".into(),
            is_followed: true,
            ..Default::default()
        };
        let sp_drive = DriveInfo {
            id: "drv-sp-eng".into(),
            name: "Documents".into(),
            drive_type: "documentLibrary".into(),
            site_id: Some("site-eng".into()),
            site_name: Some("Engineering Hub".into()),
            ..Default::default()
        };
        app.site_drives.insert("site-eng".into(), vec![sp_drive.clone()]);

        // 1. Even if MyDrive root items arrived first, initial DrivesUpdated switches SharePoint persona once to first library
        app.current_items = vec![DriveItem {
            id: "my-early".into(),
            drive_id: "drv-me".into(),
            name: "early.md".into(),
            ..Default::default()
        }];
        app.site_drives.clear();
        app.apply_core_event(CoreEvent::SitesUpdated(vec![sp_site.clone()]), &mut ui);
        assert_eq!(app.active_source, ActiveViewSource::OneDriveRoot);
        assert!(!app.initial_persona_library_selected);
        app.apply_core_event(
            CoreEvent::DrivesUpdated {
                site_id: "site-eng".into(),
                drives: vec![sp_drive.clone()],
            },
            &mut ui,
        );
        assert!(matches!(
            &app.active_source,
            ActiveViewSource::Drive { drive_id, .. } if drive_id == "drv-sp-eng"
        ));
        assert!(app.initial_persona_library_selected);

        // 2. User navigates to OneDrive > My Files: subsequent SitesUpdated MUST NOT hijack view
        dispatch(Action::OpenOneDriveRoot, &mut app, &mut ui);
        assert_eq!(app.active_source, ActiveViewSource::OneDriveRoot);
        app.current_items = vec![
            DriveItem {
                id: "my-a".into(),
                drive_id: "drv-me".into(),
                name: "a.md".into(),
                ..Default::default()
            },
            DriveItem {
                id: "my-b".into(),
                drive_id: "drv-me".into(),
                name: "b.md".into(),
                ..Default::default()
            },
        ];
        app.active_selected_id = Some("my-b".into());
        app.apply_core_event(CoreEvent::SitesUpdated(vec![sp_site.clone()]), &mut ui);
        assert_eq!(app.active_source, ActiveViewSource::OneDriveRoot);
        assert_eq!(app.active_selected_id.as_deref(), Some("my-b"));

        // Stale FolderItemsUpdated from another drive or subfolder must not replace My Files
        app.apply_core_event(
            CoreEvent::FolderItemsUpdated {
                drive_id: "drv-sp-eng".into(),
                parent_id: None,
                items: vec![DriveItem {
                    id: "sp-other".into(),
                    drive_id: "drv-sp-eng".into(),
                    name: "sp.md".into(),
                    ..Default::default()
                }],
                truncated: false,
            },
            &mut ui,
        );
        assert_eq!(app.current_items.len(), 2);
        assert_eq!(app.active_selected_id.as_deref(), Some("my-b"));

        // 3. Switch to Recent: root FolderItemsUpdated MUST NOT clobber Recent, and RecentItemsUpdated preserves selection by ID
        dispatch(Action::OpenRecent, &mut app, &mut ui);
        assert_eq!(app.active_source, ActiveViewSource::Recent);
        app.apply_core_event(
            CoreEvent::RecentItemsUpdated(vec![
                DriveItem {
                    id: "rec-1".into(),
                    drive_id: "drv-me".into(),
                    name: "alpha-rec.md".into(),
                    ..Default::default()
                },
                DriveItem {
                    id: "rec-2".into(),
                    drive_id: "drv-me".into(),
                    name: "beta-rec.md".into(),
                    ..Default::default()
                },
            ]),
            &mut ui,
        );
        app.active_selected_id = Some("rec-2".into());

        // Inject drv-me root FolderItemsUpdated while viewing Recent -> must be ignored!
        app.apply_core_event(
            CoreEvent::FolderItemsUpdated {
                drive_id: "drv-me".into(),
                parent_id: None,
                items: vec![DriveItem {
                    id: "my-a".into(),
                    drive_id: "drv-me".into(),
                    name: "a.md".into(),
                    ..Default::default()
                }],
                truncated: false,
            },
            &mut ui,
        );
        assert_eq!(app.active_source, ActiveViewSource::Recent);
        assert!(app.current_items.iter().any(|i| i.id == "rec-2"));
        assert!(!app.current_items.iter().any(|i| i.id == "my-a"));
        assert_eq!(app.active_selected_id.as_deref(), Some("rec-2"));

        // Re-deliver RecentItemsUpdated with an added item -> selection stays on rec-2
        app.apply_core_event(
            CoreEvent::RecentItemsUpdated(vec![
                DriveItem {
                    id: "rec-0".into(),
                    drive_id: "drv-me".into(),
                    name: "aaa-new.md".into(),
                    ..Default::default()
                },
                DriveItem {
                    id: "rec-1".into(),
                    drive_id: "drv-me".into(),
                    name: "alpha-rec.md".into(),
                    ..Default::default()
                },
                DriveItem {
                    id: "rec-2".into(),
                    drive_id: "drv-me".into(),
                    name: "beta-rec.md".into(),
                    ..Default::default()
                },
            ]),
            &mut ui,
        );
        assert_eq!(app.active_selected_id.as_deref(), Some("rec-2"));

        // 4. Switch to SharedWithMe and simulate retired API -> shows status + timestamp, never root items
        dispatch(Action::OpenSharedWithMe, &mut app, &mut ui);
        app.apply_core_event(
            CoreEvent::VirtualListStatusUpdated {
                kind: VirtualListKind::SharedWithMe,
                status: VirtualListStatus {
                    availability: crate::models::VirtualListAvailability::Unavailable {
                        reason: "Microsoft Graph retired this endpoint".into(),
                    },
                    last_checked_at: "2026-11-20T10:00:00Z".into(),
                },
            },
            &mut ui,
        );
        app.apply_core_event(
            CoreEvent::FolderItemsUpdated {
                drive_id: "drv-me".into(),
                parent_id: None,
                items: vec![DriveItem {
                    id: "root-intruder".into(),
                    drive_id: "drv-me".into(),
                    name: "intruder.txt".into(),
                    ..Default::default()
                }],
                truncated: false,
            },
            &mut ui,
        );
        assert!(app.current_items.is_empty());

        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let mut hm = HitMap::default();
                crate::ui::drive_view::draw_list(f, f.area(), &app, &mut ui, &mut hm);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let screen: String = (0..20)
            .map(|y| {
                (0..100)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(screen.contains("Shared with Me unavailable"), "{screen}");
        assert!(screen.contains("2026-11-20T10:00:00Z"), "{screen}");
        assert!(!screen.contains("intruder.txt"), "{screen}");
    }

    #[tokio::test]
    async fn metadata_only_rename_and_duplicate_notifications_avoid_extra_content_downloads() {
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_for_route = Arc::clone(&hits);
        let router = Router::new().route(
            "/v1.0/drives/{drive_id}/items/{item_id}/content",
            get(move || {
                let hits = Arc::clone(&hits_for_route);
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    (
                        [(header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
                        "# Same Body\n",
                    )
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let base_url = format!("http://{addr}/v1.0");
        let (mut app, mut ui, mut rx) = test_app_and_ui_with_base_url(Some(&base_url));
        app.active_source = ActiveViewSource::Drive {
            drive_id: "drv-1".into(),
            label: "Docs".into(),
        };

        let item_v1 = DriveItem {
            id: "doc-1".into(),
            drive_id: "drv-1".into(),
            name: "orig.md".into(),
            size: 12,
            mime_type: Some("text/markdown".into()),
            e_tag: Some("\"etag-1\"".into()),
            c_tag: Some("\"ctag-1\"".into()),
            last_modified_at: Some("2026-11-20T01:00:00Z".into()),
            ..Default::default()
        };
        // Send two rapid identical notifications before download finishes -> must only fetch once!
        for _ in 0..2 {
            app.apply_core_event(
                CoreEvent::FolderItemsUpdated {
                    drive_id: "drv-1".into(),
                    parent_id: None,
                    items: vec![item_v1.clone()],
                    truncated: false,
                },
                &mut ui,
            );
        }
        let mut saw_text = false;
        while !saw_text {
            let msg = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap();
            if matches!(msg, UiMsg::TextPreviewLoaded { .. }) {
                saw_text = true;
            }
            app.apply_ui_msg(msg, &mut ui);
        }
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // Metadata-only rename: name, eTag, and lastModifiedDateTime change; cTag & size stay identical!
        let item_renamed = DriveItem {
            name: "renamed.md".into(),
            e_tag: Some("\"etag-2-rename\"".into()),
            last_modified_at: Some("2026-11-20T01:05:00Z".into()),
            ..item_v1.clone()
        };
        assert_eq!(item_v1.content_revision(), item_renamed.content_revision());
        app.apply_core_event(
            CoreEvent::FolderItemsUpdated {
                drive_id: "drv-1".into(),
                parent_id: None,
                items: vec![item_renamed.clone()],
                truncated: false,
            },
            &mut ui,
        );
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "metadata-only rename with unchanged cTag must not refetch /content"
        );
        assert_eq!(app.active_selected_id.as_deref(), Some("doc-1"));
        assert_eq!(app.text_preview_for(&item_renamed), Some("# Same Body\n"));

        server.abort();
    }

    #[tokio::test]
    async fn sequence_guards_ignore_stale_search_virtual_list_and_smaller_folder_polls() {
        let (mut app, mut ui, _rx) = test_app_and_ui_with_base_url(None);

        // 1. Search sequence guard ignores older out-of-order search responses.
        let seq1 = app.next_search_seq();
        let seq2 = app.next_search_seq();
        app.apply_ui_msg(
            UiMsg::SearchResults {
                seq: seq2,
                items: vec![make_text_file("d1", "fresh", "fresh.txt", "text/plain")],
            },
            &mut ui,
        );
        app.apply_ui_msg(
            UiMsg::SearchResults {
                seq: seq1,
                items: vec![make_text_file("d1", "stale", "stale.txt", "text/plain")],
            },
            &mut ui,
        );
        assert_eq!(app.search_results.len(), 1);
        assert_eq!(app.search_results[0].id, "fresh");

        // 2. VirtualListLoaded sequence guard ignores stale Recent responses.
        app.active_source = ActiveViewSource::Recent;
        let vseq1 = app.bump_virtual_list_seq();
        let vseq2 = app.bump_virtual_list_seq();
        let avail_status = VirtualListStatus {
            availability: crate::models::VirtualListAvailability::Available,
            last_checked_at: "2026-11-25T00:00:00Z".into(),
        };
        app.apply_ui_msg(
            UiMsg::VirtualListLoaded {
                seq: vseq2,
                kind: VirtualListKind::Recent,
                items: vec![make_text_file("d1", "rec-new", "new.txt", "text/plain")],
                status: avail_status.clone(),
            },
            &mut ui,
        );
        app.apply_ui_msg(
            UiMsg::VirtualListLoaded {
                seq: vseq1,
                kind: VirtualListKind::Recent,
                items: vec![make_text_file("d1", "rec-old", "old.txt", "text/plain")],
                status: avail_status,
            },
            &mut ui,
        );
        assert_eq!(app.current_items.len(), 1);
        assert_eq!(app.current_items[0].id, "rec-new");

        // 3. Stale 200-item truncated background poll must NOT truncate a 400-item loaded folder.
        app.active_source = ActiveViewSource::Drive {
            drive_id: "d1".into(),
            label: "Docs".into(),
        };
        app.navigate_to_active_folder(&mut ui);
        let page400: Vec<DriveItem> = (0..400)
            .map(|i| make_text_file("d1", &format!("f-{i}"), &format!("f-{i:03}.txt"), "text/plain"))
            .collect();
        app.folder_item_limit = 400;
        app.apply_core_event(
            CoreEvent::FolderItemsUpdated {
                drive_id: "d1".into(),
                parent_id: None,
                items: page400,
                truncated: true,
            },
            &mut ui,
        );
        assert_eq!(app.current_items.len(), 400);

        let stale200: Vec<DriveItem> = (0..200)
            .map(|i| make_text_file("d1", &format!("f-{i}"), &format!("f-{i:03}.txt"), "text/plain"))
            .collect();
        app.apply_core_event(
            CoreEvent::FolderItemsUpdated {
                drive_id: "d1".into(),
                parent_id: None,
                items: stale200,
                truncated: true,
            },
            &mut ui,
        );
        assert_eq!(
            app.current_items.len(),
            400,
            "stale 200-item truncated poll must not clobber 400-item folder"
        );

        // Complete listing (truncated=false) still applies even if smaller.
        app.apply_core_event(
            CoreEvent::FolderItemsUpdated {
                drive_id: "d1".into(),
                parent_id: None,
                items: vec![make_text_file("d1", "only", "only.txt", "text/plain")],
                truncated: false,
            },
            &mut ui,
        );
        assert_eq!(app.current_items.len(), 1);
        assert!(!app.current_items_truncated);
    }

    #[tokio::test]
    async fn post_delete_clamps_selection_clears_last_ghost_overlay_pins_and_versions_failure() {
        let (mut app, mut ui, _rx) = test_app_and_ui_with_base_url(None);
        app.active_source = ActiveViewSource::Drive {
            drive_id: "d1".into(),
            label: "Docs".into(),
        };
        let fld = make_folder("d1", "fld-pin", "PinnedFolder");
        let a = make_text_file("d1", "a", "a.txt", "text/plain");
        let b = make_text_file("d1", "b", "b.txt", "text/plain");
        let c = make_text_file("d1", "c", "c.txt", "text/plain");
        let img = make_text_file("d1", "img1", "pic.png", "image/png");
        let all = vec![fld.clone(), a.clone(), b.clone(), c.clone(), img.clone()];
        app.storage.upsert_drive_items(&all).unwrap();
        app.current_items = all;
        app.active_selected_id = Some("b".into());
        ui.list_scroll.selected_idx = Some(2);

        // Pin `fld-pin`
        let pin = PinnedTarget {
            key: "folder:d1:fld-pin".into(),
            kind: "folder".into(),
            drive_id: Some("d1".into()),
            item_id: Some("fld-pin".into()),
            site_id: None,
            label: "PinnedFolder".into(),
        };
        app.storage.upsert_pin(&pin).unwrap();
        app.pins.push(pin);

        // Delete item at index 2 (`b`) -> selection clamps near index 2 (`c`), NOT jumping to 0!
        app.storage.delete_drive_item("d1", "b").unwrap();
        app.apply_ui_msg(
            UiMsg::ItemDeleted {
                drive_id: "d1".into(),
                item_id: "b".into(),
                raw_drive_id: None,
                raw_item_id: None,
                name: "b.txt".into(),
            },
            &mut ui,
        );
        assert_eq!(ui.list_scroll.selected_idx, Some(2));
        assert_eq!(app.active_selected_id.as_deref(), Some("c"));

        // Delete pinned folder -> removed from app.pins and SQLite!
        app.storage.delete_drive_item("d1", "fld-pin").unwrap();
        app.apply_ui_msg(
            UiMsg::ItemDeleted {
                drive_id: "d1".into(),
                item_id: "fld-pin".into(),
                raw_drive_id: None,
                raw_item_id: None,
                name: "PinnedFolder".into(),
            },
            &mut ui,
        );
        assert!(app.pins.is_empty());
        assert!(app.storage.list_pins().unwrap().is_empty());

        // Open ImagePreviewOverlay on `img1`, then delete `img1` -> overlay items cleared!
        let ov = crate::ui::overlays::image_preview::ImagePreviewOverlay::from_folder_items(
            &app.sorted_visible_items(),
            Some("img1"),
        )
        .unwrap();
        ui.push_overlay(Overlay::ImagePreview(ov));
        app.storage.delete_drive_item("d1", "img1").unwrap();
        app.apply_ui_msg(
            UiMsg::ItemDeleted {
                drive_id: "d1".into(),
                item_id: "img1".into(),
                raw_drive_id: None,
                raw_item_id: None,
                name: "pic.png".into(),
            },
            &mut ui,
        );
        match ui.overlays.last() {
            Some(Overlay::ImagePreview(ov)) => assert!(ov.items.is_empty()),
            other => panic!("expected ImagePreview overlay, got {other:?}"),
        }
        ui.pop_overlay();

        // Delete remaining items so SQLite folder is empty -> no ghost last item remains!
        app.storage.delete_drive_item("d1", "a").unwrap();
        app.storage.delete_drive_item("d1", "c").unwrap();
        app.current_items = vec![c.clone()];
        app.active_selected_id = Some("c".into());
        ui.list_scroll.selected_idx = Some(0);
        app.load_active_folder(&mut ui);
        assert!(
            app.current_items.is_empty(),
            "load_folder must not keep ghost deleted last item when SQLite cache is empty"
        );

        // Failed list_item_versions removes `ver:` key from `inflight_previews`.
        let rev_key = AppState::item_revision_key(&a);
        let vkey = format!("ver:{rev_key}");
        app.inflight_previews.insert(vkey.clone());
        app.apply_ui_msg(UiMsg::VersionsFailed { cache_key: rev_key }, &mut ui);
        assert!(!app.inflight_previews.contains(&vkey));
    }

    #[tokio::test]
    async fn refresh_all_avoids_virtual_list_re_fetch_and_recovers_inspector_on_error() {
        let (mut app, mut ui, _rx) = test_app_and_ui_with_base_url(None);
        let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
        let (nudge_tx, mut nudge_rx) = mpsc::unbounded_channel::<SyncCommand>();
        app.sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx);

        // On Recent, RefreshAll sends SyncCommand::RefreshAll without spawning duplicate load_active_folder fetch.
        app.active_source = ActiveViewSource::Recent;
        let doc = make_text_file("d1", "rec-doc", "notes.txt", "text/plain");
        app.current_items = vec![doc.clone()];
        app.active_selected_id = Some("rec-doc".into());
        app.set_text_preview("d1", "rec-doc", "cached text".into());
        let seq_before = app.virtual_list_seq;

        dispatch(Action::RefreshAll, &mut app, &mut ui);
        assert!(app.text_preview_for(&doc).is_none());
        assert_eq!(app.virtual_list_seq, seq_before + 1);
        let mut cmds = Vec::new();
        while let Ok(cmd) = nudge_rx.try_recv() {
            cmds.push(cmd);
        }
        assert!(cmds.iter().any(|c| matches!(c, SyncCommand::RefreshAll)));
        assert!(!cmds.iter().any(|c| matches!(c, SyncCommand::SyncFolder { .. })));

        // When SyncError arrives, loading_folder clears and inspector fetch is re-requested (not stuck on Loading...).
        let tkey = AppState::text_preview_inflight_key("d1", "rec-doc");
        assert!(!app.inflight_previews.contains(&tkey));
        app.apply_core_event(CoreEvent::SyncError("network down".into()), &mut ui);
        assert!(!app.loading_folder);
        assert!(
            app.inflight_previews.contains(&tkey),
            "SyncError after RefreshAll must re-trigger ensure_inspector_loaded"
        );
    }
}
