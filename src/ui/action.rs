//! Central `Action` enum and single `dispatch(action, app, ui) -> bool` reducer.

use std::path::PathBuf;

use crate::models::PinnedTarget;
use crate::sync::SyncCommand;

use super::app::{
    copy_text_to_clipboard, ActiveViewSource, AppState, Focus, NarrowPane, UiMsg, UiState,
};
use super::hitmap::{ButtonId, HitTarget};
use super::nav_pane::{build_nav_rows, NavRow};
use super::overlays::context_menu::{ContextMenu, ContextTarget};
use super::overlays::image_preview::ImagePreviewOverlay;
use super::overlays::links::LinkExtractorOverlay;
use super::overlays::quick_input::QuickInputOverlay;
use super::overlays::search::SearchOverlay;
use super::overlays::{Overlay, OverlayKind, OverlayOutcome};
use super::theme::Theme;
use super::toast::ToastKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Quit,
    QuitConfirm,
    Noop,

    // Focus & layout
    Focus(Focus),
    NextFocus,
    PrevFocus,
    ToggleNavRail,
    SelectNarrowPane(NarrowPane),
    ToggleNotify,
    ToggleDensity,
    CycleTheme,
    CycleSortMode,
    ShowDownloadDir,

    // Navigation & movement
    MoveUp,
    MoveDown,
    ScrollPageUp,
    ScrollPageDown,
    ScrollTop,
    ScrollBottom,
    MouseScroll {
        delta: i32,
        column: u16,
        row: u16,
    },
    Activate,
    NavigateUp,
    OpenOneDriveRoot,
    OpenRecent,
    OpenSharedWithMe,
    SelectDrive {
        drive_id: String,
        label: String,
    },
    TogglePinCurrent,

    // File operations
    DownloadSelected,
    OpenFilePicker,
    UploadLocalFile(PathBuf),
    UploadClipboardImage,
    OpenNewFolder,
    CreateFolder {
        name: String,
    },
    OpenRenameItem,
    RenameItem {
        drive_id: String,
        item_id: String,
        new_name: String,
    },
    ConfirmDeleteSelected,
    ExecuteDelete {
        drive_id: String,
        item_id: String,
        name: String,
    },
    CreateAndCopyShareLink,
    OpenActiveWebUrl,

    // Links & image preview
    OpenLinkExtractor,
    ToggleShowLinkUrls,
    TogglePreviewRenderMode,
    OpenUrl(String),
    CopyUrlToClipboard(String),
    ToggleInlineImages,
    OpenImagePreview,
    LoadPreviewImage,
    CycleImageProtocol,
    DownloadPreviewImage,

    // Search & sync
    OpenSearch,
    OpenSearchWithQuery(String),
    SearchLocalFiles(String),
    SearchRemoteFiles(String),
    JumpToSearchHit(usize),
    RefreshAll,

    // Overlay text editing & mouse
    Input(char),
    Paste(String),
    Backspace,
    DeleteForward,
    Tab,
    CursorLeft,
    CursorRight,
    CursorUp,
    CursorDown,
    CursorHome,
    CursorEnd,
    OpenOverlay(OverlayKind),
    CloseOverlay,
    OverlaySubmit,
    OpenContextMenuHere,
    OpenContextMenuAt {
        column: u16,
        row: u16,
    },
    Click {
        column: u16,
        row: u16,
    },
    ShowToast(String),
    CheckForUpdates,
}

/// Dispatches an `Action`, mutating `app` and `ui`.
/// Returns `true` when the application should exit.
pub fn dispatch(action: Action, app: &mut AppState, ui: &mut UiState) -> bool {
    if matches!(action, Action::Quit) {
        return true;
    }

    if let Action::Click { column, row } = action {
        return handle_click(column, row, app, ui);
    }

    if let Action::OpenContextMenuAt { column, row } = action {
        let target = match ui.hitmap.hit(row, column) {
            Some(HitTarget::ListItem(idx)) => {
                select_list_index(idx, app, ui);
                ContextTarget::DriveItem(idx)
            }
            _ => ContextTarget::Global,
        };
        let menu = ContextMenu::build(target, (column, row), app, ui);
        ui.push_overlay(Overlay::context_menu(menu));
        return false;
    }

    if matches!(action, Action::OpenContextMenuHere) && ui.overlays.is_empty() {
        let target = if ui.focus == Focus::List || ui.focus == Focus::Inspector {
            let idx = ui.list_scroll.selected_idx.unwrap_or(0);
            ContextTarget::DriveItem(idx)
        } else {
            ContextTarget::Global
        };
        let anchor = (ui.term_width / 3, ui.term_height / 3);
        let menu = ContextMenu::build(target, anchor, app, ui);
        ui.push_overlay(Overlay::context_menu(menu));
        return false;
    }

    // Route through top overlay first when any overlay is open
    if !ui.overlays.is_empty() {
        let outcome = {
            let mut top = ui.overlays.pop().unwrap();
            let res = top.handle(&action, app, ui);
            match &res {
                OverlayOutcome::Close | OverlayOutcome::CloseAndDispatch(_) => {}
                _ => ui.overlays.push(top),
            }
            res
        };
        match outcome {
            OverlayOutcome::Handled | OverlayOutcome::Close => return false,
            OverlayOutcome::Dispatch(next) | OverlayOutcome::CloseAndDispatch(next) => {
                return dispatch(next, app, ui);
            }
            OverlayOutcome::NotHandled => {}
        }
    }

    match action {
        Action::Quit => return true,
        Action::QuitConfirm => {
            ui.push_overlay(Overlay::confirm(
                "Quit SharePoint / OneDrive TUI?",
                Some(Action::Quit),
            ));
        }
        Action::Noop => {}

        // Focus & layout
        Action::Focus(f) => ui.focus = f,
        Action::NextFocus => {
            ui.focus = match ui.focus {
                Focus::Nav => Focus::List,
                Focus::List => Focus::Inspector,
                Focus::Inspector => {
                    if ui.show_nav_rail {
                        Focus::Nav
                    } else {
                        Focus::List
                    }
                }
                Focus::Overlay => Focus::List,
            };
        }
        Action::PrevFocus => {
            ui.focus = match ui.focus {
                Focus::Nav => Focus::Inspector,
                Focus::List => {
                    if ui.show_nav_rail {
                        Focus::Nav
                    } else {
                        Focus::Inspector
                    }
                }
                Focus::Inspector => Focus::List,
                Focus::Overlay => Focus::List,
            };
        }
        Action::ToggleNavRail => {
            ui.show_nav_rail = !ui.show_nav_rail;
            app.save_ui_prefs(ui);
        }
        Action::SelectNarrowPane(p) => {
            ui.narrow_pane = p;
            ui.focus = match p {
                NarrowPane::Sidebar => Focus::Nav,
                NarrowPane::Files => Focus::List,
                NarrowPane::Inspector => Focus::Inspector,
            };
        }
        Action::ToggleNotify => {
            ui.notify = !ui.notify;
            app.save_ui_prefs(ui);
            ui.toast(
                format!("Notifications: {}", if ui.notify { "On" } else { "Off" }),
                ToastKind::Info,
            );
        }
        Action::ToggleDensity => {
            ui.density = ui.density.toggle();
            app.save_ui_prefs(ui);
            ui.toast(format!("List density: {}", ui.density.label()), ToastKind::Info);
        }
        Action::CycleTheme => {
            ui.theme_preset = ui.theme_preset.next();
            ui.theme = Theme::detect(ui.theme_preset);
            app.save_ui_prefs(ui);
            ui.toast(format!("Theme: {}", ui.theme_preset.label()), ToastKind::Info);
        }
        Action::CycleSortMode => {
            app.sort_mode = app.sort_mode.next();
            ui.toast(format!("Sorted by {}", app.sort_mode.label()), ToastKind::Info);
        }
        Action::ShowDownloadDir => {
            ui.toast(
                format!("Downloads: {}", app.download_dir.display()),
                ToastKind::Info,
            );
        }

        // Movement & activation
        Action::MoveUp => match ui.focus {
            Focus::Nav => {
                let rows = build_nav_rows(app, ui);
                if !rows.is_empty() {
                    let cur = ui.nav_selected.min(rows.len() - 1);
                    ui.nav_selected = cur.saturating_sub(1);
                }
            }
            Focus::List => {
                let len = app.current_items.len();
                if len > 0 {
                    let cur = ui.list_scroll.selected_idx.unwrap_or(0).min(len - 1);
                    select_list_index(cur.saturating_sub(1), app, ui);
                }
            }
            Focus::Inspector => {
                ui.inspector_scroll.scroll_up(2);
            }
            Focus::Overlay => {}
        },
        Action::MoveDown => match ui.focus {
            Focus::Nav => {
                let rows = build_nav_rows(app, ui);
                if !rows.is_empty() {
                    ui.nav_selected = (ui.nav_selected + 1).min(rows.len() - 1);
                }
            }
            Focus::List => {
                let len = app.current_items.len();
                if len > 0 {
                    let cur = ui.list_scroll.selected_idx.unwrap_or(0).min(len - 1);
                    select_list_index((cur + 1).min(len - 1), app, ui);
                }
            }
            Focus::Inspector => {
                let vh = ui.inspector_scroll.area.height.saturating_sub(2) as usize;
                ui.inspector_scroll.scroll_down(2, vh);
            }
            Focus::Overlay => {}
        },
        Action::ScrollPageUp => {
            let vh = ui.inspector_scroll.area.height.saturating_sub(2).max(4) as usize;
            ui.inspector_scroll.scroll_up(vh / 2);
        }
        Action::ScrollPageDown => {
            let vh = ui.inspector_scroll.area.height.saturating_sub(2).max(4) as usize;
            ui.inspector_scroll.scroll_down(vh / 2, vh);
        }
        Action::ScrollTop => match ui.focus {
            Focus::List => select_list_index(0, app, ui),
            Focus::Inspector => ui.inspector_scroll.top(),
            _ => {}
        },
        Action::ScrollBottom => match ui.focus {
            Focus::List => {
                if !app.current_items.is_empty() {
                    select_list_index(app.current_items.len() - 1, app, ui);
                }
            }
            Focus::Inspector => {
                let vh = ui.inspector_scroll.area.height.saturating_sub(2) as usize;
                ui.inspector_scroll.bottom(vh);
            }
            _ => {}
        },
        Action::MouseScroll { delta, column, row } => {
            let pos = ratatui::layout::Position::new(column, row);
            if ui.inspector_scroll.area.contains(pos) {
                let vh = ui.inspector_scroll.area.height.saturating_sub(2) as usize;
                if delta < 0 {
                    ui.inspector_scroll.scroll_up((-delta) as usize);
                } else {
                    ui.inspector_scroll.scroll_down(delta as usize, vh);
                }
            } else if delta < 0 {
                return dispatch(Action::MoveUp, app, ui);
            } else {
                return dispatch(Action::MoveDown, app, ui);
            }
        }
        Action::Activate => match ui.focus {
            Focus::Nav => activate_nav_row(ui.nav_selected, app, ui),
            Focus::List => {
                if let Some(item) = app.active_item() {
                    if item.is_folder {
                        let target_drive = item.effective_drive_id().to_string();
                        if app.active_drive_id().as_deref() != Some(target_drive.as_str()) {
                            app.active_source = ActiveViewSource::Drive {
                                drive_id: target_drive,
                                label: item.name.clone(),
                            };
                            app.folder_stack.clear();
                        }
                        app.folder_stack
                            .push((item.effective_item_id().to_string(), item.name.clone()));
                        ui.inspector_scroll.top();
                        app.load_active_folder();
                    } else {
                        ui.focus = Focus::Inspector;
                        ui.narrow_pane = NarrowPane::Inspector;
                        app.ensure_inspector_loaded(ui);
                    }
                }
            }
            Focus::Inspector => {
                if let Some(item) = app.active_item() {
                    if item.is_image() {
                        return dispatch(Action::OpenImagePreview, app, ui);
                    }
                }
            }
            Focus::Overlay => {}
        },
        Action::NavigateUp => {
            if app.folder_stack.pop().is_some() {
                ui.inspector_scroll.top();
                app.load_active_folder();
            } else if ui.focus == Focus::Inspector {
                ui.focus = Focus::List;
                ui.narrow_pane = NarrowPane::Files;
            }
        }
        Action::OpenOneDriveRoot => {
            app.active_source = ActiveViewSource::OneDriveRoot;
            app.folder_stack.clear();
            ui.focus = Focus::List;
            app.load_active_folder();
        }
        Action::OpenRecent => {
            app.active_source = ActiveViewSource::Recent;
            app.folder_stack.clear();
            ui.focus = Focus::List;
            app.load_active_folder();
        }
        Action::OpenSharedWithMe => {
            app.active_source = ActiveViewSource::SharedWithMe;
            app.folder_stack.clear();
            ui.focus = Focus::List;
            app.load_active_folder();
        }
        Action::SelectDrive { drive_id, label } => {
            app.active_source = ActiveViewSource::Drive { drive_id, label };
            app.folder_stack.clear();
            ui.focus = Focus::List;
            app.load_active_folder();
        }
        Action::TogglePinCurrent => {
            if let Some(drive_id) = app.active_drive_id() {
                let (key, item_id, label) = if let Some(item) = app.active_item() {
                    if item.is_folder {
                        (
                            format!("folder:{}:{}", item.effective_drive_id(), item.effective_item_id()),
                            Some(item.effective_item_id().to_string()),
                            item.name.clone(),
                        )
                    } else {
                        (
                            format!("folder:{drive_id}:root"),
                            app.active_parent_id(),
                            app.breadcrumb_label(),
                        )
                    }
                } else {
                    (
                        format!("folder:{drive_id}:root"),
                        app.active_parent_id(),
                        app.breadcrumb_label(),
                    )
                };
                if app.pins.iter().any(|p| p.key == key) {
                    let _ = app.storage.remove_pin(&key);
                    app.pins.retain(|p| p.key != key);
                    ui.toast(format!("Unpinned '{label}'"), ToastKind::Info);
                } else {
                    let pin = PinnedTarget {
                        key,
                        kind: "folder".into(),
                        drive_id: Some(drive_id),
                        item_id,
                        site_id: None,
                        label: label.clone(),
                    };
                    let _ = app.storage.upsert_pin(&pin);
                    app.pins.push(pin);
                    ui.toast(format!("Pinned '{label}'"), ToastKind::Success);
                }
            }
        }

        // File operations
        Action::DownloadSelected => {
            if let Some(item) = app.active_item() {
                if item.is_folder {
                    ui.toast("Cannot download a folder; open it and select a file", ToastKind::Warning);
                } else {
                    spawn_download_item(app, &item);
                    ui.toast(format!("Downloading {}...", item.name), ToastKind::Info);
                }
            }
        }
        Action::OpenFilePicker => {
            ui.push_overlay(Overlay::file_picker());
        }
        Action::UploadLocalFile(local_path) => {
            let Some(drive_id) = app.active_drive_id() else {
                ui.toast("Select a drive or folder before uploading", ToastKind::Warning);
                return false;
            };
            let parent_id = app.active_parent_id();
            let graph = app.graph.clone();
            let storage = app.storage.clone();
            let tx = app.ui_tx.clone();
            let fname = local_path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("file")
                .to_string();
            ui.toast(format!("Uploading {fname}..."), ToastKind::Info);
            tokio::spawn(async move {
                match graph
                    .upload_local_file(&drive_id, parent_id.as_deref(), None, &local_path)
                    .await
                {
                    Ok(item) => {
                        let _ = storage.upsert_drive_items(std::slice::from_ref(&item));
                        let _ = tx.send(UiMsg::ItemUploaded(item));
                    }
                    Err(err) => {
                        let _ = tx.send(UiMsg::TaskError {
                            label: "Upload".into(),
                            error: format!("{err:#}"),
                        });
                    }
                }
            });
        }
        Action::UploadClipboardImage => {
            ui.toast("Reading clipboard image...", ToastKind::Info);
            app.spawn_upload_clipboard_png();
        }
        Action::OpenNewFolder => {
            ui.push_overlay(Overlay::quick_input(QuickInputOverlay::new_folder()));
        }
        Action::CreateFolder { name } => {
            let Some(drive_id) = app.active_drive_id() else {
                ui.toast("No active drive selected", ToastKind::Warning);
                return false;
            };
            let parent_id = app.active_parent_id();
            let graph = app.graph.clone();
            let storage = app.storage.clone();
            let tx = app.ui_tx.clone();
            tokio::spawn(async move {
                match graph.create_folder(&drive_id, parent_id.as_deref(), &name).await {
                    Ok(folder) => {
                        let _ = storage.upsert_drive_items(std::slice::from_ref(&folder));
                        let _ = tx.send(UiMsg::FolderCreated(folder));
                    }
                    Err(err) => {
                        let _ = tx.send(UiMsg::TaskError {
                            label: "Create folder".into(),
                            error: format!("{err:#}"),
                        });
                    }
                }
            });
        }
        Action::OpenRenameItem => {
            if let Some(item) = app.active_item() {
                ui.push_overlay(Overlay::quick_input(QuickInputOverlay::rename_item(
                    item.effective_drive_id().to_string(),
                    item.effective_item_id().to_string(),
                    item.name,
                )));
            }
        }
        Action::RenameItem {
            drive_id,
            item_id,
            new_name,
        } => {
            let graph = app.graph.clone();
            let storage = app.storage.clone();
            let tx = app.ui_tx.clone();
            tokio::spawn(async move {
                match graph
                    .rename_or_move_item(&drive_id, &item_id, Some(&new_name), None)
                    .await
                {
                    Ok(updated) => {
                        let _ = storage.upsert_drive_items(std::slice::from_ref(&updated));
                        let _ = tx.send(UiMsg::ItemRenamed(updated));
                    }
                    Err(err) => {
                        let _ = tx.send(UiMsg::TaskError {
                            label: "Rename".into(),
                            error: format!("{err:#}"),
                        });
                    }
                }
            });
        }
        Action::ConfirmDeleteSelected => {
            if let Some(item) = app.active_item() {
                let msg = format!("Delete '{}' permanently?", item.name);
                ui.push_overlay(Overlay::confirm(
                    msg,
                    Some(Action::ExecuteDelete {
                        drive_id: item.effective_drive_id().to_string(),
                        item_id: item.effective_item_id().to_string(),
                        name: item.name,
                    }),
                ));
            }
        }
        Action::ExecuteDelete {
            drive_id,
            item_id,
            name,
        } => {
            let graph = app.graph.clone();
            let storage = app.storage.clone();
            let tx = app.ui_tx.clone();
            tokio::spawn(async move {
                match graph.delete_item(&drive_id, &item_id).await {
                    Ok(()) => {
                        let _ = storage.delete_drive_item(&drive_id, &item_id);
                        let _ = tx.send(UiMsg::ItemDeleted {
                            drive_id,
                            item_id,
                            name,
                        });
                    }
                    Err(err) => {
                        let _ = tx.send(UiMsg::TaskError {
                            label: "Delete".into(),
                            error: format!("{err:#}"),
                        });
                    }
                }
            });
        }
        Action::CreateAndCopyShareLink => {
            if let Some(item) = app.active_item() {
                let graph = app.graph.clone();
                let tx = app.ui_tx.clone();
                let did = item.effective_drive_id().to_string();
                let iid = item.effective_item_id().to_string();
                let item_id = item.id.clone();
                ui.toast(format!("Creating share link for {}...", item.name), ToastKind::Info);
                tokio::spawn(async move {
                    match graph
                        .create_share_link(&did, &iid, "view", "organization", None)
                        .await
                    {
                        Ok(link) => {
                            let _ = tx.send(UiMsg::ShareLinkCreated { item_id, link });
                        }
                        Err(err) => {
                            let _ = tx.send(UiMsg::TaskError {
                                label: "Share link".into(),
                                error: format!("{err:#}"),
                            });
                        }
                    }
                });
            }
        }
        Action::OpenActiveWebUrl => {
            if let Some(url) = app.active_item().and_then(|i| i.web_url) {
                return dispatch(Action::OpenUrl(url), app, ui);
            } else {
                ui.toast("No web URL available for selected item", ToastKind::Warning);
            }
        }

        // Links & image preview
        Action::OpenLinkExtractor => {
            if let Some(item) = app.active_item() {
                let share = app.created_share_links.get(&item.id).map(String::as_str);
                let text = app.text_previews.get(&item.id).map(String::as_str);
                let overlay = LinkExtractorOverlay::from_drive_item(&item, share, text);
                ui.push_overlay(Overlay::links(overlay));
            }
        }
        Action::ToggleShowLinkUrls => {
            ui.show_link_urls = !ui.show_link_urls;
            ui.toast(
                format!(
                    "Inline link URLs: {}",
                    if ui.show_link_urls { "Shown" } else { "Hidden" }
                ),
                ToastKind::Info,
            );
        }
        Action::TogglePreviewRenderMode => {
            ui.render_markup = !ui.render_markup;
            app.save_ui_prefs(ui);
            ui.toast(
                format!(
                    "Preview mode: {}",
                    if ui.render_markup { "Rendered" } else { "Raw" }
                ),
                ToastKind::Info,
            );
        }
        Action::OpenUrl(url) => match open::that(&url) {
            Ok(()) => ui.toast(format!("Opened {url}"), ToastKind::Info),
            Err(err) => ui.toast(format!("Failed to open browser: {err}"), ToastKind::Error),
        },
        Action::CopyUrlToClipboard(url) => match copy_text_to_clipboard(&url) {
            Ok(()) => ui.toast(format!("Copied URL: {url}"), ToastKind::Success),
            Err(err) => ui.toast(format!("Clipboard error: {err}"), ToastKind::Error),
        },
        Action::ToggleInlineImages => {
            ui.inline_images = !ui.inline_images;
            app.save_ui_prefs(ui);
            app.ensure_inspector_loaded(ui);
            ui.toast(
                format!(
                    "Inline image previews: {}",
                    if ui.inline_images { "On" } else { "Off" }
                ),
                ToastKind::Info,
            );
        }
        Action::OpenImagePreview => {
            let items = app.sorted_visible_items();
            let active_id = app.active_item().map(|i| i.id);
            if let Some(overlay) = ImagePreviewOverlay::from_folder_items(&items, active_id.as_deref()) {
                ui.push_overlay(Overlay::ImagePreview(overlay));
                return dispatch(Action::LoadPreviewImage, app, ui);
            } else {
                ui.toast("No images in current folder", ToastKind::Info);
            }
        }
        Action::LoadPreviewImage => {
            if let Some(Overlay::ImagePreview(ov)) = ui.overlays.last() {
                if let Some(cur) = ov.current().cloned() {
                    let (cols, rows) = ov.target_dimensions(ui.term_width, ui.term_height);
                    let key = app.modal_image_cache_key(&cur, cols, rows, ui.graphics_caps.protocol);
                    app.request_image_decode(&cur, key, cols, rows, ui.graphics_caps);
                }
            }
        }
        Action::CycleImageProtocol => {
            ui.graphics_caps.protocol = ui.graphics_caps.protocol.next_manual();
            app.rendered_images.clear();
            app.save_ui_prefs(ui);
            ui.toast(
                format!("Image protocol: {}", ui.graphics_caps.protocol.label()),
                ToastKind::Info,
            );
            if matches!(ui.overlays.last(), Some(Overlay::ImagePreview(_))) {
                return dispatch(Action::LoadPreviewImage, app, ui);
            } else {
                app.ensure_inspector_loaded(ui);
            }
        }
        Action::DownloadPreviewImage => {
            if let Some(Overlay::ImagePreview(ov)) = ui.overlays.last() {
                if let Some(cur) = ov.current().cloned() {
                    spawn_download_item(app, &cur);
                    ui.toast(format!("Downloading {}...", cur.name), ToastKind::Info);
                }
            }
        }

        // Search & sync
        Action::OpenSearch => {
            app.search_results.clear();
            ui.push_overlay(Overlay::search());
        }
        Action::OpenSearchWithQuery(q) => {
            app.search_results.clear();
            ui.push_overlay(Overlay::Search(SearchOverlay::with_query(q.clone())));
            if !q.trim().is_empty() {
                return dispatch(Action::SearchLocalFiles(q), app, ui);
            }
        }
        Action::SearchLocalFiles(q) => {
            let storage = app.storage.clone();
            let tx = app.ui_tx.clone();
            tokio::task::spawn_blocking(move || {
                let items = storage
                    .search_items(&q, None, 40)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|h| h.item)
                    .collect();
                let _ = tx.send(UiMsg::SearchResults { items });
            });
        }
        Action::SearchRemoteFiles(q) => {
            let graph = app.graph.clone();
            let storage = app.storage.clone();
            let tx = app.ui_tx.clone();
            let drive_id = app.active_drive_id();
            ui.toast(format!("Searching Graph for '{q}'..."), ToastKind::Info);
            tokio::spawn(async move {
                match crate::cli::search_items_combined(
                    &graph,
                    &storage,
                    &q,
                    drive_id.as_deref(),
                    true,
                    40,
                )
                .await
                {
                    Ok(items) => {
                        let _ = tx.send(UiMsg::SearchResults { items });
                    }
                    Err(err) => {
                        let _ = tx.send(UiMsg::TaskError {
                            label: "Search".into(),
                            error: format!("{err:#}"),
                        });
                    }
                }
            });
        }
        Action::JumpToSearchHit(idx) => {
            if let Some(item) = app.search_results.get(idx).cloned() {
                let target_drive = item.effective_drive_id().to_string();
                app.active_source = ActiveViewSource::Drive {
                    drive_id: target_drive,
                    label: "Search Result".to_string(),
                };
                app.folder_stack.clear();
                app.current_items = vec![item.clone()];
                app.active_selected_id = Some(item.id);
                ui.list_scroll.selected_idx = Some(0);
                ui.focus = Focus::Inspector;
                app.ensure_inspector_loaded(ui);
            }
        }
        Action::RefreshAll => {
            app.sync_ctrl.request(SyncCommand::RefreshAll);
            app.load_active_folder();
            ui.toast("Refreshing SharePoint & OneDrive...", ToastKind::Info);
        }

        // Overlays & misc
        Action::OpenOverlay(kind) => match kind {
            OverlayKind::CommandPalette => ui.push_overlay(Overlay::command_palette()),
            OverlayKind::Settings => ui.push_overlay(Overlay::settings()),
            OverlayKind::Search => return dispatch(Action::OpenSearch, app, ui),
            OverlayKind::FilePicker => ui.push_overlay(Overlay::file_picker()),
            OverlayKind::Help => ui.push_overlay(Overlay::help()),
            OverlayKind::Links => return dispatch(Action::OpenLinkExtractor, app, ui),
            OverlayKind::ImagePreview => return dispatch(Action::OpenImagePreview, app, ui),
            _ => {}
        },
        Action::CloseOverlay => {
            if ui.pop_overlay().is_none() && ui.focus == Focus::Inspector {
                ui.focus = Focus::List;
            }
        }
        Action::ShowToast(msg) => ui.toast(msg, ToastKind::Info),
        Action::CheckForUpdates => {
            let repo = app.config.github_repo.clone();
            let tx = app.ui_tx.clone();
            tokio::spawn(async move {
                match crate::updater::check_or_apply_update(&repo, true, false).await {
                    Ok(res) => {
                        let _ = tx.send(UiMsg::UpdateChecked(res));
                    }
                    Err(err) => {
                        let _ = tx.send(UiMsg::TaskError {
                            label: "Update check".into(),
                            error: format!("{err:#}"),
                        });
                    }
                }
            });
        }

        // Handled by overlays
        Action::Input(_)
        | Action::Paste(_)
        | Action::Backspace
        | Action::DeleteForward
        | Action::Tab
        | Action::CursorLeft
        | Action::CursorRight
        | Action::CursorUp
        | Action::CursorDown
        | Action::CursorHome
        | Action::CursorEnd
        | Action::OverlaySubmit
        | Action::OpenContextMenuHere
        | Action::OpenContextMenuAt { .. }
        | Action::Click { .. } => {}
    }

    false
}

fn select_list_index(idx: usize, app: &mut AppState, ui: &mut UiState) {
    let items = app.sorted_visible_items();
    if items.is_empty() {
        ui.list_scroll.selected_idx = None;
        app.active_selected_id = None;
        return;
    }
    let clamped = idx.min(items.len() - 1);
    ui.list_scroll.selected_idx = Some(clamped);
    app.active_selected_id = Some(items[clamped].id.clone());
    ui.inspector_scroll.top();
    app.ensure_inspector_loaded(ui);
}

fn activate_nav_row(row_idx: usize, app: &mut AppState, ui: &mut UiState) {
    let rows = build_nav_rows(app, ui);
    let Some(row) = rows.get(row_idx).cloned() else {
        return;
    };
    match row {
        NavRow::SectionHeader { section_id, .. } => {
            if !ui.collapsed_sections.remove(section_id) {
                ui.collapsed_sections.insert(section_id);
            }
        }
        NavRow::OneDriveMyFiles => {
            let _ = dispatch(Action::OpenOneDriveRoot, app, ui);
        }
        NavRow::OneDriveRecent => {
            let _ = dispatch(Action::OpenRecent, app, ui);
        }
        NavRow::OneDriveShared => {
            let _ = dispatch(Action::OpenSharedWithMe, app, ui);
        }
        NavRow::SharePointSite { site_id, label, .. } => {
            if !ui.collapsed_sites.remove(&site_id) {
                ui.collapsed_sites.insert(site_id.clone());
            } else if !app.site_drives.contains_key(&site_id) {
                let graph = app.graph.clone();
                let storage = app.storage.clone();
                let tx = app.ui_tx.clone();
                tokio::spawn(async move {
                    if let Ok(drives) = graph.list_site_drives(&site_id, Some(&label)).await {
                        let _ = storage.upsert_drives(&drives);
                        let _ = tx.send(UiMsg::SiteDrivesLoaded { site_id, drives });
                    }
                });
            }
        }
        NavRow::SharePointLibrary {
            drive_id, label, ..
        } => {
            let _ = dispatch(Action::SelectDrive { drive_id, label }, app, ui);
        }
        NavRow::PinnedItem {
            drive_id,
            item_id,
            label,
            ..
        } => {
            if let Some(did) = drive_id {
                app.active_source = ActiveViewSource::Drive {
                    drive_id: did,
                    label: label.clone(),
                };
                app.folder_stack.clear();
                if let Some(iid) = item_id {
                    app.folder_stack.push((iid, label));
                }
                ui.focus = Focus::List;
                app.load_active_folder();
            }
        }
    }
}

fn spawn_download_item(app: &AppState, item: &crate::models::DriveItem) {
    let graph = app.graph.clone();
    let tx = app.ui_tx.clone();
    let dest_dir = app.download_dir.clone();
    let did = item.effective_drive_id().to_string();
    let iid = item.effective_item_id().to_string();
    let name = item.name.clone();
    let mime = item.mime_type.clone();
    let dl_url = item.download_url.clone();
    tokio::spawn(async move {
        match graph
            .download_item_to_file(&did, &iid, Some(&name), mime.as_deref(), dl_url.as_deref(), &dest_dir)
            .await
        {
            Ok(dl) => {
                let _ = tx.send(UiMsg::ItemDownloaded(dl));
            }
            Err(err) => {
                let _ = tx.send(UiMsg::TaskError {
                    label: "Download".into(),
                    error: format!("{err:#}"),
                });
            }
        }
    });
}

fn handle_click(column: u16, row: u16, app: &mut AppState, ui: &mut UiState) -> bool {
    let hit = ui.hitmap.hit(row, column);

    if !ui.overlays.is_empty() {
        match hit {
            Some(HitTarget::OverlayItem { index }) => {
                if let Some(top) = ui.overlays.last_mut() {
                    top.select(index);
                }
                return dispatch(Action::Activate, app, ui);
            }
            Some(HitTarget::Button(ButtonId::ConfirmOk)) => {
                return dispatch(Action::Activate, app, ui);
            }
            Some(HitTarget::Button(ButtonId::ConfirmCancel)) | None => {
                ui.pop_overlay();
                return false;
            }
            Some(HitTarget::OverlayBody) => return false,
            _ => return false,
        }
    }

    match hit {
        Some(HitTarget::NavRow(idx)) => {
            ui.focus = Focus::Nav;
            ui.nav_selected = idx;
            activate_nav_row(idx, app, ui);
        }
        Some(HitTarget::ListItem(idx)) => {
            let already_sel = ui.list_scroll.selected_idx == Some(idx);
            ui.focus = Focus::List;
            select_list_index(idx, app, ui);
            if already_sel {
                return dispatch(Action::Activate, app, ui);
            }
        }
        Some(HitTarget::BreadcrumbUp) => {
            return dispatch(Action::NavigateUp, app, ui);
        }
        Some(HitTarget::PreviewImageChip) => {
            return dispatch(Action::OpenImagePreview, app, ui);
        }
        Some(HitTarget::WebLinkUrl) => {
            return dispatch(Action::OpenActiveWebUrl, app, ui);
        }
        Some(HitTarget::Button(btn)) => match btn {
            ButtonId::SortToggle => return dispatch(Action::CycleSortMode, app, ui),
            ButtonId::PreviewRenderToggle => {
                return dispatch(Action::TogglePreviewRenderMode, app, ui)
            }
            ButtonId::Help => return dispatch(Action::OpenOverlay(OverlayKind::Help), app, ui),
            ButtonId::Settings => {
                return dispatch(Action::OpenOverlay(OverlayKind::Settings), app, ui)
            }
            ButtonId::NarrowSidebar => {
                return dispatch(Action::SelectNarrowPane(NarrowPane::Sidebar), app, ui)
            }
            ButtonId::NarrowFiles => {
                return dispatch(Action::SelectNarrowPane(NarrowPane::Files), app, ui)
            }
            ButtonId::NarrowInspector => {
                return dispatch(Action::SelectNarrowPane(NarrowPane::Inspector), app, ui)
            }
            _ => {}
        },
        _ => {}
    }

    false
}
