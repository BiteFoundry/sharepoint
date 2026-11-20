//! Application state (`AppState`), UI-only state (`UiState`), and async UI task messages (`UiMsg`).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::layout::Rect;
use tokio::sync::mpsc;

use crate::auth::AuthManager;
use crate::cli::DEFAULT_TEXT_PREVIEW_MAX_BYTES;
use crate::config::{AppConfig, DensityMode, ImageProtocolPreference, Persona, ThemePreset};
use crate::graph::GraphClient;
use crate::img_preview::{self, RenderedTerminalImage, TerminalGraphicsCaps, TerminalImageProtocol};
use crate::models::{
    CoreEvent, DownloadedFile, DriveInfo, DriveItem, DriveItemVersion, PinnedTarget,
    ShareLinkResult, SharePointSite, UserProfile, POLL_RETRY_SUMMARY,
};
use crate::storage::Storage;
use crate::sync::{FocusTarget, SyncController};

use super::hitmap::HitMap;
use super::keymap::Keymap;
use super::markup::CachedMarkupPreview;
use super::notification;
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
    FolderLoaded {
        drive_id: String,
        parent_id: Option<String>,
        items: Vec<DriveItem>,
    },
    RecentLoaded(Vec<DriveItem>),
    SharedLoaded(Vec<DriveItem>),
    SiteDrivesLoaded {
        site_id: String,
        drives: Vec<DriveInfo>,
    },
    TextPreviewLoaded {
        item_id: String,
        content: String,
    },
    VersionsLoaded {
        item_id: String,
        versions: Vec<DriveItemVersion>,
    },
    PreviewFailed {
        item_id: String,
        error: String,
    },
    ImageBytesFetched {
        item_id: String,
        bytes: Arc<[u8]>,
    },
    ImageDecoded {
        cache_key: String,
        rendered: RenderedTerminalImage,
    },
    ItemDownloaded(DownloadedFile),
    ItemUploaded(DriveItem),
    FolderCreated(DriveItem),
    ItemRenamed(DriveItem),
    ItemDeleted {
        drive_id: String,
        item_id: String,
        name: String,
    },
    ShareLinkCreated {
        item_id: String,
        link: ShareLinkResult,
    },
    SearchResults {
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
    /// Stack of `(item_id, display_name)` inside the currently selected drive.
    pub folder_stack: Vec<(String, String)>,
    pub current_items: Vec<DriveItem>,
    pub sort_mode: SortMode,
    pub loading_folder: bool,
    pub sync_busy: bool,
    pub sync_summary: String,
    pub text_previews: HashMap<String, String>,
    pub item_versions: HashMap<String, Vec<DriveItemVersion>>,
    pub created_share_links: HashMap<String, String>,
    pub preview_errors: HashMap<String, String>,
    pub raw_image_bytes: HashMap<String, Arc<[u8]>>,
    pub rendered_images: HashMap<String, RenderedTerminalImage>,
    pub inflight_previews: HashSet<String>,
    pub search_results: Vec<DriveItem>,
    pub active_selected_id: Option<String>,
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
            folder_stack: Vec::new(),
            current_items: Vec::new(),
            sort_mode: SortMode::NameAsc,
            loading_folder: false,
            sync_busy: false,
            sync_summary: "Ready".to_string(),
            text_previews: HashMap::new(),
            item_versions: HashMap::new(),
            created_share_links: HashMap::new(),
            preview_errors: HashMap::new(),
            raw_image_bytes: HashMap::new(),
            rendered_images: HashMap::new(),
            inflight_previews: HashSet::new(),
            search_results: Vec::new(),
            active_selected_id: None,
        }
    }

    pub fn active_drive_id(&self) -> Option<String> {
        match &self.active_source {
            ActiveViewSource::OneDriveRoot => self.my_drive.as_ref().map(|d| d.id.clone()),
            ActiveViewSource::Drive { drive_id, .. } => Some(drive_id.clone()),
            ActiveViewSource::Recent | ActiveViewSource::SharedWithMe => {
                self.my_drive.as_ref().map(|d| d.id.clone())
            }
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
        };
        if self.folder_stack.is_empty() {
            root
        } else {
            let trail: Vec<&str> = self.folder_stack.iter().map(|(_, n)| n.as_str()).collect();
            format!("{root} > {}", trail.join(" > "))
        }
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

    pub fn active_item(&self) -> Option<DriveItem> {
        let items = self.sorted_visible_items();
        if let Some(ref id) = self.active_selected_id {
            if let Some(found) = items.iter().find(|i| i.id == *id) {
                return Some(found.clone());
            }
        }
        items.into_iter().next()
    }

    pub fn inline_image_cache_key(&self, item: &DriveItem) -> String {
        format!("inline:{}:{}", item.effective_drive_id(), item.effective_item_id())
    }

    pub fn modal_image_cache_key(
        &self,
        item: &DriveItem,
        cols: u16,
        rows: u16,
        proto: TerminalImageProtocol,
    ) -> String {
        format!(
            "modal:{}:{}:{}x{}:{}",
            item.effective_drive_id(),
            item.effective_item_id(),
            cols,
            rows,
            proto.as_str()
        )
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

    pub fn load_active_folder(&mut self) {
        let tx = self.ui_tx.clone();
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        self.loading_folder = true;

        match &self.active_source {
            ActiveViewSource::Recent => {
                tokio::spawn(async move {
                    match graph.list_recent_items(50).await {
                        Ok(items) => {
                            let _ = storage.replace_recent_items(&items);
                            let _ = tx.send(UiMsg::RecentLoaded(items));
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
                tokio::spawn(async move {
                    match graph.list_shared_with_me(50).await {
                        Ok(items) => {
                            let _ = storage.replace_shared_items(&items);
                            let _ = tx.send(UiMsg::SharedLoaded(items));
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
            active_site_id: None,
        });

        // Show cached children immediately, then refresh from Graph
        if let Ok(cached) =
            self.storage
                .list_folder_children(&drive_id, parent_id.as_deref(), None, 200)
        {
            if !cached.is_empty() {
                self.current_items = cached;
            }
        }

        tokio::spawn(async move {
            match graph
                .list_children(&drive_id, parent_id.as_deref(), Some(200))
                .await
            {
                Ok(items) => {
                    let _ = storage.replace_folder_children(&drive_id, parent_id.as_deref(), &items);
                    let _ = tx.send(UiMsg::FolderLoaded {
                        drive_id,
                        parent_id,
                        items,
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskError {
                        label: "List folder".into(),
                        error: format!("{err:#}"),
                    });
                }
            }
        });
    }

    pub fn ensure_inspector_loaded(&mut self, ui: &UiState) {
        let Some(item) = self.active_item() else {
            return;
        };
        if item.is_folder {
            return;
        }
        let item_id = item.id.clone();

        // Fetch version history lazily
        if !self.item_versions.contains_key(&item_id) {
            let vkey = format!("ver:{item_id}");
            if self.inflight_previews.insert(vkey) {
                let graph = self.graph.clone();
                let tx = self.ui_tx.clone();
                let did = item.effective_drive_id().to_string();
                let iid = item.effective_item_id().to_string();
                let id_for_msg = item_id.clone();
                tokio::spawn(async move {
                    if let Ok(versions) = graph.list_item_versions(&did, &iid).await {
                        let _ = tx.send(UiMsg::VersionsLoaded {
                            item_id: id_for_msg,
                            versions,
                        });
                    }
                });
            }
        }

        if item.is_text_previewable() && !self.text_previews.contains_key(&item_id) {
            let tkey = format!("txt:{item_id}");
            if self.inflight_previews.insert(tkey) {
                let graph = self.graph.clone();
                let tx = self.ui_tx.clone();
                let did = item.effective_drive_id().to_string();
                let iid = item.effective_item_id().to_string();
                let dl = item.download_url.clone();
                let id_for_msg = item_id.clone();
                tokio::spawn(async move {
                    match graph
                        .download_item_bytes(&did, &iid, dl.as_deref(), DEFAULT_TEXT_PREVIEW_MAX_BYTES)
                        .await
                    {
                        Ok(bytes) => {
                            let content = String::from_utf8_lossy(&bytes).into_owned();
                            let _ = tx.send(UiMsg::TextPreviewLoaded {
                                item_id: id_for_msg,
                                content,
                            });
                        }
                        Err(err) => {
                            let _ = tx.send(UiMsg::PreviewFailed {
                                item_id: id_for_msg,
                                error: format!("{err:#}"),
                            });
                        }
                    }
                });
            }
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
        if self.rendered_images.contains_key(&cache_key) {
            return;
        }
        if !self.inflight_previews.insert(cache_key.clone()) {
            return;
        }
        let tx = self.ui_tx.clone();
        let item_id = item.id.clone();

        if let Some(raw) = self.raw_image_bytes.get(&item_id).cloned() {
            tokio::task::spawn_blocking(move || {
                match img_preview::decode_and_render_for_protocol(&raw, cols, rows, (18, 20, 26), caps) {
                    Ok(rendered) => {
                        let _ = tx.send(UiMsg::ImageDecoded {
                            cache_key,
                            rendered,
                        });
                    }
                    Err(err) => {
                        let _ = tx.send(UiMsg::PreviewFailed {
                            item_id,
                            error: format!("{err:#}"),
                        });
                    }
                }
            });
            return;
        }

        let graph = self.graph.clone();
        let did = item.effective_drive_id().to_string();
        let iid = item.effective_item_id().to_string();
        let dl = item.download_url.clone();
        tokio::spawn(async move {
            match graph
                .download_item_bytes(&did, &iid, dl.as_deref(), 16 * 1024 * 1024)
                .await
            {
                Ok(bytes) => {
                    let arc: Arc<[u8]> = Arc::from(bytes);
                    let _ = tx.send(UiMsg::ImageBytesFetched {
                        item_id: item_id.clone(),
                        bytes: Arc::clone(&arc),
                    });
                    let decode_tx = tx.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        match img_preview::decode_and_render_for_protocol(&arc, cols, rows, (18, 20, 26), caps) {
                            Ok(rendered) => {
                                let _ = decode_tx.send(UiMsg::ImageDecoded {
                                    cache_key,
                                    rendered,
                                });
                            }
                            Err(err) => {
                                let _ = decode_tx.send(UiMsg::PreviewFailed {
                                    item_id,
                                    error: format!("{err:#}"),
                                });
                            }
                        }
                    })
                    .await;
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::PreviewFailed {
                        item_id,
                        error: format!("{err:#}"),
                    });
                }
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
                    self.load_active_folder();
                }
            }
            CoreEvent::SitesUpdated(sites) => {
                self.sites = sites;
                // If SharePoint persona and still on empty default view, switch to first library when available
                if self.persona == Persona::SharePoint
                    && matches!(self.active_source, ActiveViewSource::OneDriveRoot)
                {
                    if let Some(first_site) = self.sites.first() {
                        if let Some(drives) = self.site_drives.get(&first_site.id) {
                            if let Some(d) = drives.first() {
                                self.active_source = ActiveViewSource::Drive {
                                    drive_id: d.id.clone(),
                                    label: d.display_label(),
                                };
                                self.folder_stack.clear();
                                self.load_active_folder();
                            }
                        }
                    }
                }
            }
            CoreEvent::DrivesUpdated { site_id, drives } => {
                let should_boot_sp = self.persona == Persona::SharePoint
                    && matches!(self.active_source, ActiveViewSource::OneDriveRoot)
                    && self.current_items.is_empty()
                    && !drives.is_empty();
                self.site_drives.insert(site_id, drives.clone());
                if should_boot_sp {
                    let d = &drives[0];
                    self.active_source = ActiveViewSource::Drive {
                        drive_id: d.id.clone(),
                        label: d.display_label(),
                    };
                    self.folder_stack.clear();
                    self.load_active_folder();
                }
            }
            CoreEvent::FolderItemsUpdated {
                drive_id,
                parent_id,
                items,
            } => {
                if self.active_drive_id().as_deref() == Some(drive_id.as_str())
                    && self.active_parent_id() == parent_id
                {
                    self.current_items = items;
                    self.loading_folder = false;
                    self.ensure_inspector_loaded(ui);
                }
            }
            CoreEvent::RecentItemsUpdated(items) => {
                if matches!(self.active_source, ActiveViewSource::Recent) {
                    self.current_items = items;
                    self.loading_folder = false;
                }
            }
            CoreEvent::SharedItemsUpdated(items) => {
                if matches!(self.active_source, ActiveViewSource::SharedWithMe) {
                    self.current_items = items;
                    self.loading_folder = false;
                }
            }
            CoreEvent::SyncStateChanged { busy, summary } => {
                self.sync_busy = busy;
                if summary != POLL_RETRY_SUMMARY {
                    self.sync_summary = summary;
                }
            }
            CoreEvent::SyncError(err) => {
                self.sync_busy = false;
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
                if self.persona == Persona::SharePoint {
                    if let Some(site) = self.sites.first() {
                        if let Some(ds) = self.site_drives.get(&site.id) {
                            if let Some(d) = ds.first() {
                                self.active_source = ActiveViewSource::Drive {
                                    drive_id: d.id.clone(),
                                    label: d.display_label(),
                                };
                            }
                        }
                    }
                }
                self.load_active_folder();
            }
            UiMsg::FolderLoaded {
                drive_id,
                parent_id,
                items,
            } => {
                if self.active_drive_id().as_deref() == Some(drive_id.as_str())
                    && self.active_parent_id() == parent_id
                {
                    self.current_items = items;
                    self.loading_folder = false;
                    ui.list_scroll.selected_idx = Some(0);
                    self.active_selected_id = self.sorted_visible_items().first().map(|i| i.id.clone());
                    self.ensure_inspector_loaded(ui);
                }
            }
            UiMsg::RecentLoaded(items) => {
                if matches!(self.active_source, ActiveViewSource::Recent) {
                    self.current_items = items;
                    self.loading_folder = false;
                    ui.list_scroll.selected_idx = Some(0);
                    self.active_selected_id = self.sorted_visible_items().first().map(|i| i.id.clone());
                    self.ensure_inspector_loaded(ui);
                }
            }
            UiMsg::SharedLoaded(items) => {
                if matches!(self.active_source, ActiveViewSource::SharedWithMe) {
                    self.current_items = items;
                    self.loading_folder = false;
                    ui.list_scroll.selected_idx = Some(0);
                    self.active_selected_id = self.sorted_visible_items().first().map(|i| i.id.clone());
                    self.ensure_inspector_loaded(ui);
                }
            }
            UiMsg::SiteDrivesLoaded { site_id, drives } => {
                self.site_drives.insert(site_id, drives);
            }
            UiMsg::TextPreviewLoaded { item_id, content } => {
                self.inflight_previews.remove(&format!("txt:{item_id}"));
                self.preview_errors.remove(&item_id);
                self.text_previews.insert(item_id, content);
            }
            UiMsg::VersionsLoaded { item_id, versions } => {
                self.inflight_previews.remove(&format!("ver:{item_id}"));
                self.item_versions.insert(item_id, versions);
            }
            UiMsg::PreviewFailed { item_id, error } => {
                self.preview_errors.insert(item_id, error);
            }
            UiMsg::ImageBytesFetched { item_id, bytes } => {
                if self.raw_image_bytes.len() >= 8 {
                    self.raw_image_bytes.clear();
                }
                self.raw_image_bytes.insert(item_id, bytes);
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
                self.load_active_folder();
            }
            UiMsg::FolderCreated(folder) => {
                ui.toast(format!("Created folder '{}'", folder.name), ToastKind::Success);
                self.load_active_folder();
            }
            UiMsg::ItemRenamed(item) => {
                ui.toast(format!("Renamed to '{}'", item.name), ToastKind::Success);
                self.load_active_folder();
            }
            UiMsg::ItemDeleted { name, .. } => {
                ui.toast(format!("Deleted '{name}'"), ToastKind::Success);
                self.load_active_folder();
            }
            UiMsg::ShareLinkCreated { item_id, link } => {
                let copied = copy_text_to_clipboard(&link.web_url).is_ok();
                self.created_share_links.insert(item_id, link.web_url.clone());
                let suffix = if copied { " (copied to clipboard)" } else { "" };
                ui.toast(format!("Share link: {}{suffix}", link.web_url), ToastKind::Success);
            }
            UiMsg::SearchResults { items } => {
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
                self.loading_folder = false;
                ui.toast(format!("{label} failed: {error}"), ToastKind::Error);
            }
            UiMsg::Toast { message, kind } => {
                ui.toast(message, kind);
            }
        }
    }

    pub fn save_ui_prefs(&mut self, ui: &UiState) {
        self.config.notify = ui.notify;
        self.config.show_nav_rail = ui.show_nav_rail;
        self.config.inline_images = ui.inline_images;
        self.config.render_markup = ui.render_markup;
        self.config.density = ui.density;
        self.config.theme = ui.theme_preset;
        self.config.image_protocol = match ui.graphics_caps.protocol {
            TerminalImageProtocol::Kitty => ImageProtocolPreference::Kitty,
            TerminalImageProtocol::ITerm2 => ImageProtocolPreference::Iterm2,
            TerminalImageProtocol::Sixel => ImageProtocolPreference::Sixel,
            TerminalImageProtocol::Halfblock => ImageProtocolPreference::Halfblock,
        };
        let _ = self.config.save();
    }
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
