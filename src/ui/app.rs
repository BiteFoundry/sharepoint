//! Application state (`AppState`), UI-only state (`UiState`), and async UI task messages (`UiMsg`).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bitefoundry_html::{is_safe_raster_image, sniff_content_kind, ContentKind, NativeHtmlServer};
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
        drive_id: String,
        item_id: String,
        content: String,
    },
    VersionsLoaded {
        item_id: String,
        versions: Vec<DriveItemVersion>,
    },
    PreviewFailed {
        drive_id: String,
        item_id: String,
        inflight_key: Option<String>,
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
    HtmlContentSniffed {
        drive_id: String,
        item_id: String,
        title: String,
        html: String,
        cache_key: String,
    },
    HtmlPreviewReady {
        drive_id: String,
        item_id: String,
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
    pub text_previews: Option<(String, String)>,
    pub item_versions: HashMap<String, Vec<DriveItemVersion>>,
    pub created_share_links: HashMap<String, String>,
    pub preview_errors: HashMap<String, String>,
    pub raw_image_bytes: HashMap<String, Arc<[u8]>>,
    pub rendered_images: HashMap<String, RenderedTerminalImage>,
    pub inflight_previews: HashSet<String>,
    pub search_results: Vec<DriveItem>,
    pub active_selected_id: Option<String>,
    /// Lazy localhost Axum HTML server (`0 MB` idle until `o` is pressed).
    pub html_server: NativeHtmlServer,
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
            text_previews: None,
            item_versions: HashMap::new(),
            created_share_links: HashMap::new(),
            preview_errors: HashMap::new(),
            raw_image_bytes: HashMap::new(),
            rendered_images: HashMap::new(),
            inflight_previews: HashSet::new(),
            search_results: Vec::new(),
            active_selected_id: None,
            html_server: NativeHtmlServer::new(),
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

    pub fn text_preview_key(drive_id: &str, item_id: &str) -> String {
        format!("{drive_id}:{item_id}")
    }

    pub fn text_preview_inflight_key(drive_id: &str, item_id: &str) -> String {
        format!("txt:{drive_id}:{item_id}")
    }

    pub fn text_preview_for(&self, item: &DriveItem) -> Option<&str> {
        let (key, content) = self.text_previews.as_ref()?;
        text_preview_key_matches(key, item.effective_drive_id(), &item.id)
            .then_some(content.as_str())
    }

    pub fn text_preview_error_for(&self, item: &DriveItem) -> Option<&str> {
        let key = Self::text_preview_inflight_key(item.effective_drive_id(), &item.id);
        self.preview_errors.get(&key).map(String::as_str)
    }

    pub fn set_text_preview(&mut self, drive_id: &str, item_id: &str, content: String) {
        self.text_previews = Some((Self::text_preview_key(drive_id, item_id), content));
    }

    pub fn clear_inspector_preview(&mut self, ui: &mut UiState) {
        self.text_previews = None;
        ui.markup_cache = None;
        self.preview_errors.retain(|k, _| !k.starts_with("txt:"));
    }

    pub fn sync_inspector_preview_for(&mut self, selected: Option<&DriveItem>, ui: &mut UiState) {
        let active_pair = selected
            .filter(|item| !item.is_folder && item.is_text_previewable())
            .map(|item| {
                (
                    Self::text_preview_key(item.effective_drive_id(), &item.id),
                    Self::text_preview_inflight_key(item.effective_drive_id(), &item.id),
                )
            });
        match active_pair {
            Some((ref expected_key, ref expected_err_key)) => {
                if self
                    .text_previews
                    .as_ref()
                    .is_some_and(|(stored, _)| stored != expected_key)
                {
                    self.text_previews = None;
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

    fn reconcile_selection_after_items_update(&mut self, ui: &mut UiState) {
        let items = self.sorted_visible_items();
        let selected = if items.is_empty() {
            ui.list_scroll.selected_idx = None;
            self.active_selected_id = None;
            None
        } else if let Some(ref selected_id) = self.active_selected_id {
            if let Some(pos) = items.iter().position(|item| item.id == *selected_id) {
                ui.list_scroll.selected_idx = Some(pos);
                items.get(pos)
            } else {
                ui.list_scroll.selected_idx = Some(0);
                self.active_selected_id = Some(items[0].id.clone());
                ui.inspector_scroll.top();
                items.first()
            }
        } else {
            ui.list_scroll.selected_idx = Some(0);
            self.active_selected_id = Some(items[0].id.clone());
            ui.inspector_scroll.top();
            items.first()
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

    pub fn load_active_folder(&mut self, ui: &mut UiState) {
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
                self.reconcile_selection_after_items_update(ui);
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

        if item.is_text_previewable() && self.text_preview_for(&item).is_none() {
            let did = item.effective_drive_id().to_string();
            let iid = item.effective_item_id().to_string();
            let tkey = Self::text_preview_inflight_key(&did, &item_id);
            if self.inflight_previews.insert(tkey.clone()) {
                let graph = self.graph.clone();
                let tx = self.ui_tx.clone();
                let dl = item.download_url.clone();
                let drive_for_msg = did.clone();
                let id_for_msg = item_id.clone();
                tokio::spawn(async move {
                    match graph
                        .download_item_bytes(&did, &iid, dl.as_deref(), DEFAULT_TEXT_PREVIEW_MAX_BYTES)
                        .await
                    {
                        Ok(bytes) => {
                            let content = String::from_utf8_lossy(&bytes).into_owned();
                            let _ = tx.send(UiMsg::TextPreviewLoaded {
                                drive_id: drive_for_msg,
                                item_id: id_for_msg,
                                content,
                            });
                        }
                        Err(err) => {
                            let _ = tx.send(UiMsg::PreviewFailed {
                                drive_id: drive_for_msg,
                                item_id: id_for_msg,
                                inflight_key: Some(tkey),
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
        let did = item.effective_drive_id().to_string();
        let item_id = item.id.clone();
        let item_name = item.name.clone();
        let item_mime = item.mime_type.clone();

        if let Some(raw) = self.raw_image_bytes.get(&item_id).cloned() {
            tokio::task::spawn_blocking(move || {
                match sniff_content_kind(item_mime.as_deref(), Some(&item_name), Some(&raw)) {
                    ContentKind::Html => {
                        let _ = tx.send(UiMsg::HtmlContentSniffed {
                            drive_id: did,
                            item_id,
                            title: item_name,
                            html: String::from_utf8_lossy(&raw).into_owned(),
                            cache_key,
                        });
                        return;
                    }
                    ContentKind::Svg => {
                        let _ = tx.send(UiMsg::PreviewFailed {
                            drive_id: did,
                            item_id,
                            inflight_key: Some(cache_key),
                            error: "SVG vector image (image/svg+xml) cannot be raster-decoded — press o to open in browser".into(),
                        });
                        return;
                    }
                    kind if kind != ContentKind::RasterImage => {
                        let _ = tx.send(UiMsg::PreviewFailed {
                            drive_id: did,
                            item_id,
                            inflight_key: Some(cache_key),
                            error: format!("Unsupported image response ({kind:?}); refusing raster decode"),
                        });
                        return;
                    }
                    _ => {}
                }
                match img_preview::decode_and_render_for_protocol(&raw, cols, rows, (18, 20, 26), caps) {
                    Ok(rendered) => {
                        let _ = tx.send(UiMsg::ImageDecoded {
                            cache_key,
                            rendered,
                        });
                    }
                    Err(err) => {
                        let _ = tx.send(UiMsg::PreviewFailed {
                            drive_id: did,
                            item_id,
                            inflight_key: Some(cache_key),
                            error: format!("{err:#}"),
                        });
                    }
                }
            });
            return;
        }

        let graph = self.graph.clone();
        let iid = item.effective_item_id().to_string();
        let dl = item.download_url.clone();
        tokio::spawn(async move {
            match graph
                .download_item_bytes(&did, &iid, dl.as_deref(), 16 * 1024 * 1024)
                .await
            {
                Ok(bytes) => {
                    let kind = sniff_content_kind(item_mime.as_deref(), Some(&item_name), Some(&bytes));
                    if kind == ContentKind::Html {
                        let _ = tx.send(UiMsg::HtmlContentSniffed {
                            drive_id: did,
                            item_id,
                            title: item_name,
                            html: String::from_utf8_lossy(&bytes).into_owned(),
                            cache_key,
                        });
                        return;
                    }
                    if kind == ContentKind::Svg {
                        let _ = tx.send(UiMsg::PreviewFailed {
                            drive_id: did,
                            item_id,
                            inflight_key: Some(cache_key),
                            error: "SVG vector image (image/svg+xml) cannot be raster-decoded — press o to open in browser".into(),
                        });
                        return;
                    }
                    if !is_safe_raster_image(item_mime.as_deref(), Some(&item_name), Some(&bytes)) {
                        let _ = tx.send(UiMsg::PreviewFailed {
                            drive_id: did,
                            item_id,
                            inflight_key: Some(cache_key),
                            error: format!("Unsupported image response ({kind:?}); refusing raster decode"),
                        });
                        return;
                    }
                    let arc: Arc<[u8]> = Arc::from(bytes);
                    let _ = tx.send(UiMsg::ImageBytesFetched {
                        item_id: item_id.clone(),
                        bytes: Arc::clone(&arc),
                    });
                    let decode_tx = tx.clone();
                    let decode_did = did.clone();
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
                                    drive_id: decode_did,
                                    item_id,
                                    inflight_key: Some(cache_key),
                                    error: format!("{err:#}"),
                                });
                            }
                        }
                    })
                    .await;
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::PreviewFailed {
                        drive_id: did,
                        item_id,
                        inflight_key: Some(cache_key),
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
                    self.load_active_folder(ui);
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
                                self.navigate_to_active_folder(ui);
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
                    self.navigate_to_active_folder(ui);
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
                    self.reconcile_selection_after_items_update(ui);
                    self.ensure_inspector_loaded(ui);
                }
            }
            CoreEvent::RecentItemsUpdated(items) => {
                if matches!(self.active_source, ActiveViewSource::Recent) {
                    self.current_items = items;
                    self.loading_folder = false;
                    self.reconcile_selection_after_items_update(ui);
                }
            }
            CoreEvent::SharedItemsUpdated(items) => {
                if matches!(self.active_source, ActiveViewSource::SharedWithMe) {
                    self.current_items = items;
                    self.loading_folder = false;
                    self.reconcile_selection_after_items_update(ui);
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
                self.load_active_folder(ui);
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
                    let sorted = self.sorted_visible_items();
                    let first = sorted.first();
                    ui.list_scroll.selected_idx = first.map(|_| 0);
                    self.active_selected_id = first.map(|i| i.id.clone());
                    self.sync_inspector_preview_for(first, ui);
                    self.ensure_inspector_loaded(ui);
                }
            }
            UiMsg::RecentLoaded(items) => {
                if matches!(self.active_source, ActiveViewSource::Recent) {
                    self.current_items = items;
                    self.loading_folder = false;
                    let sorted = self.sorted_visible_items();
                    let first = sorted.first();
                    ui.list_scroll.selected_idx = first.map(|_| 0);
                    self.active_selected_id = first.map(|i| i.id.clone());
                    self.sync_inspector_preview_for(first, ui);
                    self.ensure_inspector_loaded(ui);
                }
            }
            UiMsg::SharedLoaded(items) => {
                if matches!(self.active_source, ActiveViewSource::SharedWithMe) {
                    self.current_items = items;
                    self.loading_folder = false;
                    let sorted = self.sorted_visible_items();
                    let first = sorted.first();
                    ui.list_scroll.selected_idx = first.map(|_| 0);
                    self.active_selected_id = first.map(|i| i.id.clone());
                    self.sync_inspector_preview_for(first, ui);
                    self.ensure_inspector_loaded(ui);
                }
            }
            UiMsg::SiteDrivesLoaded { site_id, drives } => {
                self.site_drives.insert(site_id, drives);
            }
            UiMsg::TextPreviewLoaded {
                drive_id,
                item_id,
                content,
            } => {
                let err_key = Self::text_preview_inflight_key(&drive_id, &item_id);
                self.inflight_previews.remove(&err_key);
                if self.is_active_text_item(&drive_id, &item_id) {
                    self.preview_errors.remove(&err_key);
                    self.set_text_preview(&drive_id, &item_id, content);
                }
            }
            UiMsg::VersionsLoaded { item_id, versions } => {
                self.inflight_previews.remove(&format!("ver:{item_id}"));
                self.item_versions.insert(item_id, versions);
            }
            UiMsg::PreviewFailed {
                drive_id,
                item_id,
                inflight_key,
                error,
            } => {
                let is_text_req = inflight_key.as_deref().is_none_or(|k| k.starts_with("txt:"));
                if let Some(key) = inflight_key {
                    self.inflight_previews.remove(&key);
                }
                if is_text_req {
                    if self.is_active_text_item(&drive_id, &item_id) {
                        self.preview_errors.insert(
                            Self::text_preview_inflight_key(&drive_id, &item_id),
                            error,
                        );
                    }
                } else {
                    self.preview_errors.insert(item_id, error);
                }
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
            UiMsg::HtmlContentSniffed {
                drive_id,
                item_id,
                title,
                html,
                cache_key,
            } => {
                self.inflight_previews.remove(&cache_key);
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
                title,
                html,
            } => {
                if self.is_active_text_item(&drive_id, &item_id) {
                    self.preview_errors
                        .remove(&Self::text_preview_inflight_key(&drive_id, &item_id));
                    self.set_text_preview(&drive_id, &item_id, html.clone());
                }
                ui.push_overlay(Overlay::HtmlPreview(HtmlPreviewOverlay::new(
                    title,
                    html,
                    Some(item_id),
                )));
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
                if self.active_selected_id.as_deref() == Some(item.id.as_str()) {
                    ui.markup_cache = None;
                }
                self.load_active_folder(ui);
            }
            UiMsg::ItemDeleted { item_id, name, .. } => {
                ui.toast(format!("Deleted '{name}'"), ToastKind::Success);
                if self.active_selected_id.as_deref() == Some(item_id.as_str()) {
                    self.clear_inspector_preview(ui);
                }
                self.load_active_folder(ui);
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
        app.apply_ui_msg(
            UiMsg::FolderLoaded {
                drive_id: "drv-2".into(),
                parent_id: None,
                items: vec![doc_d2.clone()],
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
}
