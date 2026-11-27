//! Core TUI state structs (`AppState`, `UiState`), view enums, and `UiMsg`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use bitefoundry_html::NativeHtmlServer;
use ratatui::layout::Rect;
use tokio::sync::mpsc;

use crate::auth::AuthManager;
use crate::config::{AppConfig, DensityMode, Persona, ThemePreset};
use crate::graph::GraphClient;
use crate::img_preview::{
    self, FitBox, RenderedTerminalImage, TerminalGraphicsCaps, TerminalImageProtocol,
};
use crate::models::{
    DownloadedFile, DriveInfo, DriveItem, DriveItemVersion, PinnedTarget, ShareLinkResult,
    SharePointSite, UserProfile, VirtualListKind, VirtualListStatus,
};
use crate::storage::Storage;
use crate::sync::{SyncController, FOLDER_PAGE_SIZE};

use super::super::hitmap::HitMap;
use super::super::keymap::Keymap;
use super::super::markup::CachedMarkupPreview;
use super::super::overlays::Overlay;
use super::super::theme::Theme;
use super::super::toast::{TaskBoard, ToastKind, ToastQueue};

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
    /// Optional pre-encoded escape sequence payload (used by `HtmlPreviewOverlay` inline graphics
    /// whose cache lives on the overlay and is dropped on close).
    pub payload: Option<String>,
}

pub enum LocalUploadPreview {
    Text(String),
    Image(Arc<[u8]>),
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
    ItemUploaded {
        item: DriveItem,
        parent_id: Option<String>,
        local_preview: Option<LocalUploadPreview>,
    },
    FolderCreated {
        folder: DriveItem,
        parent_id: Option<String>,
    },
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
        self.toasts.push(super::super::toast::Toast::new(message, kind));
    }
}

impl Default for UiState {
    fn default() -> Self {
        Self::new()
    }
}

pub(super) const MAX_RAW_IMAGE_ENTRIES: usize = 8;

pub(super) struct ImageDecodeRequest {
    pub(super) cache_key: String,
    pub(super) fit: FitBox,
    pub(super) bg_rgb: (u8, u8, u8),
    pub(super) caps: TerminalGraphicsCaps,
}

pub(super) struct PendingImageDownload {
    pub(super) item: DriveItem,
    pub(super) decodes: Vec<ImageDecodeRequest>,
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
    /// View the user came from when they opened a folder of a drive they may not be allowed
    /// to list from its root: a Shared with Me / Recent entry, a shortcut, or a pinned remote
    /// folder. Popping the last `folder_stack` entry returns here instead of asking Graph for
    /// that drive's `/root/children` (403 for a folder shared out of someone else's drive).
    pub origin_source: Option<ActiveViewSource>,
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
    pub(super) listed_folder: Option<(String, Option<String>)>,
    pub sort_mode: SortMode,
    pub loading_folder: bool,
    pub sync_busy: bool,
    pub sync_summary: String,
    pub text_previews: Option<(String, String)>,
    pub text_preview_revision: Option<String>,
    /// Monotonic generation counter bumped whenever `text_previews` changes (O(1) cache key).
    pub text_preview_gen: u64,
    pub item_versions: HashMap<String, Vec<DriveItemVersion>>,
    pub created_share_links: HashMap<String, String>,
    pub preview_errors: HashMap<String, String>,
    pub(super) raw_image_bytes: VecDeque<(String, Arc<[u8]>)>,
    pub rendered_images: HashMap<String, RenderedTerminalImage>,
    pub inflight_previews: HashSet<String>,
    /// Maps `txt:{drive_id}:{item_id}` -> revision currently being downloaded so inspector & overlays share one fetch.
    pub inflight_text_revisions: HashMap<String, String>,
    /// Tracks `txt:{drive_id}:{item_id}` keys whose in-flight download should also update/open an HTML preview overlay.
    pub pending_html_overlay_fetches: HashSet<String>,
    /// Deduplicates raw image byte downloads across inline and modal views for the same item revision.
    pub(super) inflight_raw_images: HashMap<String, PendingImageDownload>,
    pub search_results: Vec<DriveItem>,
    /// Monotonic generation guard for local/remote search responses.
    pub search_seq: u64,
    /// Monotonic generation guard for Recent/SharedWithMe virtual list loads.
    pub virtual_list_seq: u64,
    pub active_selected_id: Option<String>,
    /// True once the user has toggled the rendered-preview setting in this session.
    pub(super) render_markup_user_set: bool,
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
            origin_source: None,
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
            text_preview_gen: 0,
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

}
