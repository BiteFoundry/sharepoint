//! Central `Action` enum and single `dispatch(action, app, ui) -> bool` reducer.

use std::path::PathBuf;

use crate::cli::DEFAULT_TEXT_PREVIEW_MAX_BYTES;
use crate::models::{MarkupKind, PinnedTarget};
use crate::sync::{FocusTarget, SyncCommand, SIDEBAR_DRIVE_PREFETCH_SITES};

use super::app::{
    copy_text_to_clipboard, ActiveViewSource, AppState, Focus, LoadMoreOutcome, NarrowPane, UiMsg,
    UiState,
};
use super::drive_view::inspector_body_height;
use super::hitmap::{ButtonId, HitTarget};
use super::nav_pane::{build_nav_rows, NavRow};
use super::overlays::context_menu::{ContextMenu, ContextTarget};
use super::overlays::html_preview::HtmlPreviewOverlay;
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
    OpenHtmlPreview,
    ServeActiveHtml,
    ServeHtmlInBrowser {
        title: String,
        html: String,
    },
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
    /// Load one more page of a folder whose listing was cut short.
    LoadMoreItems,

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
        if !ui.overlays.is_empty() {
            if matches!(ui.overlays.last(), Some(Overlay::ContextMenu(_))) {
                ui.pop_overlay();
            } else {
                return false;
            }
        }
        let target = match ui.hitmap.hit(row, column) {
            Some(HitTarget::ListItem(idx)) => {
                ui.focus = Focus::List;
                select_list_index(idx, app, ui);
                ContextTarget::DriveItem(idx)
            }
            Some(HitTarget::Pane(Focus::Inspector))
            | Some(HitTarget::PreviewImageChip)
            | Some(HitTarget::WebLinkUrl) => {
                if app.active_item().is_some() {
                    ui.focus = Focus::Inspector;
                    let idx = ui.list_scroll.selected_idx.unwrap_or(0);
                    ContextTarget::DriveItem(idx)
                } else {
                    ContextTarget::Global
                }
            }
            _ => {
                let pos = ratatui::layout::Position::new(column, row);
                if ui.inspector_scroll.area.contains(pos) && app.active_item().is_some() {
                    ui.focus = Focus::Inspector;
                    let idx = ui.list_scroll.selected_idx.unwrap_or(0);
                    ContextTarget::DriveItem(idx)
                } else {
                    ContextTarget::Global
                }
            }
        };
        let menu = ContextMenu::build(target, (column, row), app, ui);
        ui.push_overlay(Overlay::context_menu(menu));
        return false;
    }

    if matches!(action, Action::OpenContextMenuHere) && ui.overlays.is_empty() {
        let target = if (ui.focus == Focus::List || ui.focus == Focus::Inspector)
            && app.active_item().is_some()
        {
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
                // Route overlay-emitted actions directly to workspace dispatch so they are
                // never swallowed by the still-open overlay that emitted them.
                return dispatch_workspace(next, app, ui);
            }
            OverlayOutcome::NotHandled => {}
        }
    }

    dispatch_workspace(action, app, ui)
}

fn dispatch_workspace(action: Action, app: &mut AppState, ui: &mut UiState) -> bool {
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
            app.rendered_images.clear();
            app.save_ui_prefs(ui);
            ui.toast(format!("Theme: {}", ui.theme_preset.label()), ToastKind::Info);
            if matches!(ui.overlays.last(), Some(Overlay::ImagePreview(_))) {
                return dispatch_workspace(Action::LoadPreviewImage, app, ui);
            } else {
                app.ensure_inspector_loaded(ui);
            }
        }
        Action::CycleSortMode => {
            app.sort_mode = app.sort_mode.next();
            app.reconcile_selection_after_items_update(ui);
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
                let vh = inspector_body_height(ui.inspector_scroll.area);
                ui.inspector_scroll.scroll_down(2, vh);
            }
            Focus::Overlay => {}
        },
        Action::ScrollPageUp => {
            let vh = inspector_body_height(ui.inspector_scroll.area).max(4);
            ui.inspector_scroll.scroll_up(vh / 2);
        }
        Action::ScrollPageDown => {
            let vh = inspector_body_height(ui.inspector_scroll.area).max(4);
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
                let vh = inspector_body_height(ui.inspector_scroll.area);
                ui.inspector_scroll.bottom(vh);
            }
            _ => {}
        },
        Action::MouseScroll { delta, column, row } => {
            let pos = ratatui::layout::Position::new(column, row);
            if ui.inspector_scroll.area.contains(pos) {
                let vh = inspector_body_height(ui.inspector_scroll.area);
                if delta < 0 {
                    ui.inspector_scroll.scroll_up((-delta) as usize);
                } else {
                    ui.inspector_scroll.scroll_down(delta as usize, vh);
                }
            } else if ui.list_scroll.area.contains(pos) {
                let len = app.current_items.len();
                if len > 0 {
                    let cur = ui.list_scroll.selected_idx.unwrap_or(0).min(len - 1);
                    let next = if delta < 0 {
                        cur.saturating_sub(1)
                    } else {
                        (cur + 1).min(len - 1)
                    };
                    select_list_index(next, app, ui);
                }
            } else if ui.nav_area.contains(pos) {
                let rows = build_nav_rows(app, ui);
                if !rows.is_empty() {
                    let cur = ui.nav_selected.min(rows.len() - 1);
                    ui.nav_selected = if delta < 0 {
                        cur.saturating_sub(1)
                    } else {
                        (cur + 1).min(rows.len() - 1)
                    };
                }
            } else if delta < 0 {
                return dispatch_workspace(Action::MoveUp, app, ui);
            } else {
                return dispatch_workspace(Action::MoveDown, app, ui);
            }
        }
        Action::Activate => match ui.focus {
            Focus::Nav => activate_nav_row(ui.nav_selected, app, ui),
            Focus::List => {
                if let Some(item) = app.active_item() {
                    if item.is_folder {
                        open_folder_item(&item, app, ui);
                    } else {
                        ui.focus = Focus::Inspector;
                        ui.narrow_pane = NarrowPane::Inspector;
                        app.ensure_inspector_loaded(ui);
                    }
                }
            }
            Focus::Inspector => {
                if let Some(item) = app.active_item() {
                    if item.is_folder {
                        ui.focus = Focus::List;
                        ui.narrow_pane = NarrowPane::Files;
                        open_folder_item(&item, app, ui);
                    } else if item.is_image() {
                        return dispatch_workspace(Action::OpenImagePreview, app, ui);
                    } else if item.markup_kind() == Some(MarkupKind::Html) {
                        return dispatch_workspace(Action::OpenHtmlPreview, app, ui);
                    }
                }
            }
            Focus::Overlay => {}
        },
        Action::NavigateUp => {
            if app.folder_stack.pop().is_some() {
                app.navigate_to_active_folder(ui);
            } else if ui.focus == Focus::Inspector {
                ui.focus = Focus::List;
                ui.narrow_pane = NarrowPane::Files;
            }
        }
        Action::OpenOneDriveRoot => {
            app.initial_persona_library_selected = true;
            app.active_source = ActiveViewSource::OneDriveRoot;
            app.folder_stack.clear();
            ui.focus = Focus::List;
            app.navigate_to_active_folder(ui);
        }
        Action::OpenRecent => {
            app.initial_persona_library_selected = true;
            app.active_source = ActiveViewSource::Recent;
            app.folder_stack.clear();
            ui.focus = Focus::List;
            app.navigate_to_active_folder(ui);
        }
        Action::OpenSharedWithMe => {
            app.initial_persona_library_selected = true;
            app.active_source = ActiveViewSource::SharedWithMe;
            app.folder_stack.clear();
            ui.focus = Focus::List;
            app.navigate_to_active_folder(ui);
        }
        Action::SelectDrive { drive_id, label } => {
            app.initial_persona_library_selected = true;
            app.active_source = ActiveViewSource::Drive { drive_id, label };
            app.folder_stack.clear();
            ui.focus = Focus::List;
            app.navigate_to_active_folder(ui);
        }
        Action::TogglePinCurrent => {
            let target: Option<(String, String, Option<String>, String)> =
                if let Some(item) = app.active_item().filter(|it| it.is_folder) {
                    let eff_drive = item.effective_drive_id().to_string();
                    let eff_id = item.effective_item_id().to_string();
                    Some((
                        format!("folder:{eff_drive}:{eff_id}"),
                        eff_drive,
                        Some(eff_id),
                        item.name.clone(),
                    ))
                } else if let Some(drive_id) = app.active_drive_id() {
                    let parent_id = app.active_parent_id();
                    let scope = parent_id.as_deref().unwrap_or("root");
                    Some((
                        format!("folder:{drive_id}:{scope}"),
                        drive_id,
                        parent_id,
                        app.breadcrumb_label(),
                    ))
                } else {
                    None
                };
            if let Some((key, pin_drive_id, item_id, label)) = target {
                if app.pins.iter().any(|p| p.key == key) {
                    let _ = app.storage.remove_pin(&key);
                    app.pins.retain(|p| p.key != key);
                    ui.toast(format!("Unpinned '{label}'"), ToastKind::Info);
                } else {
                    let pin = PinnedTarget {
                        key,
                        kind: "folder".into(),
                        drive_id: Some(pin_drive_id),
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
                let opts = crate::cli::resolve_upload_options_for_local_file(
                    &storage,
                    &drive_id,
                    parent_id.as_deref(),
                    None,
                    None,
                    &local_path,
                    None,
                    None,
                );
                match graph
                    .upload_local_file_with_options(
                        &drive_id,
                        parent_id.as_deref(),
                        None,
                        &local_path,
                        &opts,
                    )
                    .await
                {
                    Ok(item) => {
                        let _ = storage.upsert_drive_items(std::slice::from_ref(&item));
                        crate::cli::refresh_download_baseline_after_upload(
                            &storage,
                            &local_path,
                            &item,
                            &opts,
                        );
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
            let raw_pair = app
                .current_items
                .iter()
                .find(|it| {
                    (it.effective_drive_id() == drive_id && it.effective_item_id() == item_id)
                        || (it.drive_id == drive_id && it.id == item_id)
                })
                .map(|it| (it.drive_id.clone(), it.id.clone()));
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
                        if let Some((raw_did, raw_iid)) = raw_pair {
                            if raw_did != updated.drive_id || raw_iid != updated.id {
                                if let Ok(Some(mut shortcut)) =
                                    storage.get_drive_item(&raw_did, &raw_iid)
                                {
                                    shortcut.name = updated.name.clone();
                                    let _ = storage
                                        .upsert_drive_items(std::slice::from_ref(&shortcut));
                                }
                            }
                        }
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
            let raw_pair = app
                .current_items
                .iter()
                .find(|it| {
                    (it.effective_drive_id() == drive_id && it.effective_item_id() == item_id)
                        || (it.drive_id == drive_id && it.id == item_id)
                })
                .map(|it| (it.drive_id.clone(), it.id.clone()));
            let graph = app.graph.clone();
            let storage = app.storage.clone();
            let tx = app.ui_tx.clone();
            tokio::spawn(async move {
                match graph.delete_item(&drive_id, &item_id).await {
                    Ok(()) => {
                        let _ = storage.delete_drive_item(&drive_id, &item_id);
                        let (raw_drive_id, raw_item_id) = match raw_pair {
                            Some((raw_did, raw_iid))
                                if raw_did != drive_id || raw_iid != item_id =>
                            {
                                let _ = storage.delete_drive_item(&raw_did, &raw_iid);
                                (Some(raw_did), Some(raw_iid))
                            }
                            _ => (None, None),
                        };
                        let _ = tx.send(UiMsg::ItemDeleted {
                            drive_id,
                            item_id,
                            raw_drive_id,
                            raw_item_id,
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
            if let Some(item) = app.active_item() {
                if item.markup_kind() == Some(MarkupKind::Html)
                    && app.text_preview_for(&item).is_some()
                {
                    return dispatch_workspace(Action::ServeActiveHtml, app, ui);
                }
                if let Some(url) = item.web_url {
                    return dispatch_workspace(Action::OpenUrl(url), app, ui);
                }
            }
            ui.toast("No web URL available for selected item", ToastKind::Warning);
        }

        // Links & image preview
        Action::OpenLinkExtractor => {
            if let Some(item) = app.active_item() {
                let share = app.created_share_links.get(&item.id).map(String::as_str);
                let text = app.text_preview_for(&item);
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
            if let Some(active) = app.active_item() {
                if active.markup_kind() == Some(MarkupKind::Html) {
                    return dispatch_workspace(Action::OpenHtmlPreview, app, ui);
                }
            }
            let items = app.sorted_visible_items();
            let active_id = app.active_item().map(|i| i.id);
            if let Some(overlay) = ImagePreviewOverlay::from_folder_items(&items, active_id.as_deref()) {
                ui.push_overlay(Overlay::ImagePreview(overlay));
                return dispatch_workspace(Action::LoadPreviewImage, app, ui);
            } else {
                ui.toast("No images in current folder", ToastKind::Info);
            }
        }
        Action::OpenHtmlPreview => {
            let Some(item) = app.active_item() else {
                ui.toast("No HTML file selected", ToastKind::Info);
                return false;
            };
            if item.markup_kind() != Some(MarkupKind::Html) {
                ui.toast("Selected item is not an HTML file", ToastKind::Info);
                return false;
            }
            if let Some(html) = app.text_preview_for(&item).map(str::to_string) {
                ui.push_overlay(Overlay::HtmlPreview(
                    HtmlPreviewOverlay::new(item.name.clone(), html, Some(item.id.clone()))
                        .with_source(item.effective_drive_id(), item.content_revision()),
                ));
            } else {
                ui.toast(format!("Loading interactive HTML: {}...", item.name), ToastKind::Info);
                app.spawn_shared_text_fetch(&item, true);
            }
        }
        Action::ServeActiveHtml => {
            let Some(item) = app.active_item() else {
                ui.toast("No HTML file selected", ToastKind::Info);
                return false;
            };
            if item.markup_kind() != Some(MarkupKind::Html) {
                ui.toast("Selected item is not an HTML file", ToastKind::Info);
                return false;
            }
            if let Some(html) = app.text_preview_for(&item).map(str::to_string) {
                return dispatch_workspace(
                    Action::ServeHtmlInBrowser {
                        title: item.name,
                        html,
                    },
                    app,
                    ui,
                );
            }
            let graph = app.graph.clone();
            let server = app.html_server.clone();
            let tx = app.ui_tx.clone();
            let did = item.effective_drive_id().to_string();
            let iid = item.effective_item_id().to_string();
            let dl = item.download_url.clone();
            let title = item.name.clone();
            ui.toast(format!("Preparing local server for {}...", item.name), ToastKind::Info);
            tokio::spawn(async move {
                match graph
                    .download_item_bytes(&did, &iid, dl.as_deref(), DEFAULT_TEXT_PREVIEW_MAX_BYTES)
                    .await
                {
                    Ok(bytes) => {
                        let html = String::from_utf8_lossy(&bytes).into_owned();
                        match server.serve_html(&title, &html).await {
                            Ok(url) => {
                                let _ = tx.send(UiMsg::HtmlServed { url });
                            }
                            Err(err) => {
                                let _ = tx.send(UiMsg::TaskError {
                                    label: "Serve HTML".into(),
                                    error: format!("{err:#}"),
                                });
                            }
                        }
                    }
                    Err(err) => {
                        let _ = tx.send(UiMsg::TaskError {
                            label: "Serve HTML".into(),
                            error: format!("{err:#}"),
                        });
                    }
                }
            });
        }
        Action::ServeHtmlInBrowser { title, html } => {
            let server = app.html_server.clone();
            let tx = app.ui_tx.clone();
            tokio::spawn(async move {
                match server.serve_html(&title, &html).await {
                    Ok(url) => {
                        let _ = tx.send(UiMsg::HtmlServed { url });
                    }
                    Err(err) => {
                        let _ = tx.send(UiMsg::TaskError {
                            label: "Serve HTML".into(),
                            error: format!("{err:#}"),
                        });
                    }
                }
            });
        }
        Action::LoadPreviewImage => {
            if let Some(Overlay::ImagePreview(ov)) = ui.overlays.last() {
                if let Some(cur) = ov.current().cloned() {
                    let fit = ov.fit_box(ui.term_width, ui.term_height);
                    let key = app.modal_image_cache_key(
                        &cur,
                        fit.view_cols,
                        fit.view_rows,
                        fit.zoom_pct,
                        ui.graphics_caps,
                    );
                    let bg_rgb = crate::img_preview::color_to_rgb(ui.theme.bg_elevated, (18, 20, 26));
                    app.request_image_decode(&cur, key, fit, bg_rgb, ui.graphics_caps);
                }
            }
        }
        Action::CycleImageProtocol => {
            ui.graphics_caps.protocol = ui.graphics_caps.protocol.next_manual();
            app.rendered_images.clear();
            app.save_image_protocol_choice(ui);
            ui.toast(
                format!("Image protocol: {}", ui.graphics_caps.protocol.label()),
                ToastKind::Info,
            );
            if matches!(ui.overlays.last(), Some(Overlay::ImagePreview(_))) {
                return dispatch_workspace(Action::LoadPreviewImage, app, ui);
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
            app.next_search_seq();
            app.search_results.clear();
            ui.push_overlay(Overlay::search());
        }
        Action::OpenSearchWithQuery(q) => {
            app.next_search_seq();
            app.search_results.clear();
            ui.push_overlay(Overlay::Search(SearchOverlay::with_query(q.clone())));
            if !q.trim().is_empty() {
                return dispatch_workspace(Action::SearchLocalFiles(q), app, ui);
            }
        }
        Action::SearchLocalFiles(q) => {
            let seq = app.next_search_seq();
            let storage = app.storage.clone();
            let tx = app.ui_tx.clone();
            tokio::task::spawn_blocking(move || {
                let items = storage
                    .search_items(&q, None, 40)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|h| h.item)
                    .collect();
                let _ = tx.send(UiMsg::SearchResults { seq, items });
            });
        }
        Action::SearchRemoteFiles(q) => {
            let seq = app.next_search_seq();
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
                        let _ = tx.send(UiMsg::SearchResults { seq, items });
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
                app.initial_persona_library_selected = true;
                let target_drive = item.effective_drive_id().to_string();
                app.active_source = ActiveViewSource::SearchResult {
                    drive_id: target_drive,
                    label: "Search Result".to_string(),
                };
                app.folder_stack.clear();
                app.reset_folder_listing_state();
                app.sync_ctrl.set_focus(FocusTarget::default());
                app.current_items = vec![item.clone()];
                app.active_selected_id = Some(item.id.clone());
                ui.list_scroll.selected_idx = Some(0);
                ui.inspector_scroll.top();
                ui.focus = Focus::Inspector;
                app.sync_inspector_preview_for(Some(&item), ui);
                app.ensure_inspector_loaded(ui);
            }
        }
        Action::RefreshAll => {
            app.invalidate_active_item_caches(ui);
            let active_sid = app.active_site_id();
            for site in app.sites.iter().skip(SIDEBAR_DRIVE_PREFETCH_SITES) {
                if app.site_drives.contains_key(&site.id)
                    && !ui.collapsed_sites.contains(&site.id)
                    && active_sid.as_deref() != Some(site.id.as_str())
                {
                    app.sync_ctrl.request(SyncCommand::SyncSiteDrives {
                        site_id: site.id.clone(),
                        site_name: Some(site.effective_name().to_string()),
                    });
                }
            }
            app.sync_ctrl.request(SyncCommand::RefreshAll);
            match &app.active_source {
                ActiveViewSource::Recent | ActiveViewSource::SharedWithMe => {
                    // `SyncCommand::RefreshAll` already refreshes Recent and SharedWithMe once.
                    app.bump_virtual_list_seq();
                    app.loading_folder = true;
                }
                ActiveViewSource::SearchResult { .. } => {
                    app.ensure_inspector_loaded(ui);
                }
                ActiveViewSource::OneDriveRoot | ActiveViewSource::Drive { .. } => {
                    app.load_active_folder(ui);
                }
            }
            ui.toast("Refreshing SharePoint & OneDrive...", ToastKind::Info);
        }

        Action::LoadMoreItems => match app.load_more_items(ui) {
            LoadMoreOutcome::Requested(limit) => {
                ui.toast(format!("Loading up to {limit} items..."), ToastKind::Info);
            }
            LoadMoreOutcome::AtMaximum(max) => {
                ui.toast(
                    format!(
                        "The list holds at most {max} items. Use `{} ls --limit` for the rest.",
                        app.persona.bin_name()
                    ),
                    ToastKind::Info,
                );
            }
            LoadMoreOutcome::NothingMore => {
                ui.toast("Every item in this folder is already loaded", ToastKind::Info);
            }
        },

        // Overlays & misc
        Action::OpenOverlay(kind) => match kind {
            OverlayKind::CommandPalette => ui.push_overlay(Overlay::command_palette()),
            OverlayKind::Settings => ui.push_overlay(Overlay::settings()),
            OverlayKind::Search => return dispatch_workspace(Action::OpenSearch, app, ui),
            OverlayKind::FilePicker => ui.push_overlay(Overlay::file_picker()),
            OverlayKind::Help => ui.push_overlay(Overlay::help()),
            OverlayKind::Links => return dispatch_workspace(Action::OpenLinkExtractor, app, ui),
            OverlayKind::ImagePreview => return dispatch_workspace(Action::OpenImagePreview, app, ui),
            OverlayKind::HtmlPreview => return dispatch_workspace(Action::OpenHtmlPreview, app, ui),
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

fn open_folder_item(item: &crate::models::DriveItem, app: &mut AppState, ui: &mut UiState) {
    app.initial_persona_library_selected = true;
    let target_drive = item.effective_drive_id().to_string();
    let leave_search = matches!(app.active_source, ActiveViewSource::SearchResult { .. });
    if leave_search || app.active_drive_id().as_deref() != Some(target_drive.as_str()) {
        app.active_source = ActiveViewSource::Drive {
            drive_id: target_drive,
            label: item.name.clone(),
        };
        app.folder_stack.clear();
    }
    app.folder_stack
        .push((item.effective_item_id().to_string(), item.name.clone()));
    app.navigate_to_active_folder(ui);
}

fn select_list_index(idx: usize, app: &mut AppState, ui: &mut UiState) {
    let items = app.sorted_visible_items();
    if items.is_empty() {
        ui.list_scroll.selected_idx = None;
        app.active_selected_id = None;
        app.clear_inspector_preview(ui);
        return;
    }
    let clamped = idx.min(items.len() - 1);
    let selected = &items[clamped];
    let changed = app.active_selected_id.as_deref() != Some(selected.id.as_str());
    ui.list_scroll.selected_idx = Some(clamped);
    app.active_selected_id = Some(selected.id.clone());
    if changed {
        ui.inspector_scroll.top();
    }
    app.sync_inspector_preview_for(Some(selected), ui);
    app.ensure_inspector_loaded(ui);
}

/// Applies one activation of a SharePoint site row and returns whether its libraries must
/// be fetched.
///
/// A site whose libraries are not loaded renders as expanded-but-empty, so its first
/// activation must load them and leave the row expanded (toggling here would collapse an
/// empty row and need a second activation). Only a loaded site toggles collapse.
pub(crate) fn site_row_activation(
    collapsed_sites: &mut std::collections::HashSet<String>,
    site_id: &str,
    drives_loaded: bool,
) -> bool {
    if !drives_loaded {
        collapsed_sites.remove(site_id);
        return true;
    }
    if !collapsed_sites.remove(site_id) {
        collapsed_sites.insert(site_id.to_string());
    }
    false
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
            let drives_loaded = app.site_drives.contains_key(&site_id);
            if site_row_activation(&mut ui.collapsed_sites, &site_id, drives_loaded) {
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
                app.initial_persona_library_selected = true;
                app.active_source = ActiveViewSource::Drive {
                    drive_id: did,
                    label: label.clone(),
                };
                app.folder_stack.clear();
                if let Some(iid) = item_id {
                    app.folder_stack.push((iid, label));
                }
                ui.focus = Focus::List;
                app.navigate_to_active_folder(ui);
            }
        }
    }
}

fn spawn_download_item(app: &AppState, item: &crate::models::DriveItem) {
    let graph = app.graph.clone();
    let storage = app.storage.clone();
    let tx = app.ui_tx.clone();
    let dest_dir = app.download_dir.clone();
    let item_clone = item.clone();
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
            Ok(mut dl) => {
                crate::cli::record_download_baseline(&storage, &item_clone, &mut dl);
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
            Some(HitTarget::Button(ButtonId::ConfirmCancel | ButtonId::OverlayClose))
            | Some(HitTarget::OverlayBackdrop)
            | None => {
                return dispatch(Action::CloseOverlay, app, ui);
            }
            Some(HitTarget::Button(ButtonId::PreviewRenderToggle)) => {
                return dispatch(Action::TogglePreviewRenderMode, app, ui);
            }
            Some(HitTarget::Button(ButtonId::ServeHtml)) => {
                return dispatch(Action::ServeActiveHtml, app, ui);
            }
            Some(HitTarget::OverlayBody) => {
                if matches!(ui.overlays.last(), Some(Overlay::HtmlPreview(_))) {
                    let outcome = {
                        let mut top = ui.overlays.pop().unwrap();
                        let res = match &mut top {
                            Overlay::HtmlPreview(ov) => ov.handle_click_at(column, row, ui),
                            _ => OverlayOutcome::Handled,
                        };
                        ui.overlays.push(top);
                        res
                    };
                    if let OverlayOutcome::Dispatch(next) = outcome {
                        return dispatch_workspace(next, app, ui);
                    }
                }
                return false;
            }
            _ => return false,
        }
    }

    match hit {
        Some(HitTarget::Pane(focus)) => {
            ui.focus = focus;
        }
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
            ButtonId::Search => return dispatch(Action::OpenSearch, app, ui),
            ButtonId::CommandPalette => {
                return dispatch(Action::OpenOverlay(OverlayKind::CommandPalette), app, ui)
            }
            ButtonId::DensityToggle => return dispatch(Action::ToggleDensity, app, ui),
            ButtonId::SortToggle => return dispatch(Action::CycleSortMode, app, ui),
            ButtonId::Help => return dispatch(Action::OpenOverlay(OverlayKind::Help), app, ui),
            ButtonId::Settings => {
                return dispatch(Action::OpenOverlay(OverlayKind::Settings), app, ui)
            }
            ButtonId::Refresh => return dispatch(Action::RefreshAll, app, ui),
            ButtonId::UploadFile => return dispatch(Action::OpenFilePicker, app, ui),
            ButtonId::NewFolder => return dispatch(Action::OpenNewFolder, app, ui),
            ButtonId::DownloadItem => return dispatch(Action::DownloadSelected, app, ui),
            ButtonId::ShareItem => return dispatch(Action::CreateAndCopyShareLink, app, ui),
            ButtonId::RenameItem => return dispatch(Action::OpenRenameItem, app, ui),
            ButtonId::DeleteItem => return dispatch(Action::ConfirmDeleteSelected, app, ui),
            ButtonId::ImagePreview => return dispatch(Action::OpenImagePreview, app, ui),
            ButtonId::HtmlPreview => return dispatch(Action::OpenHtmlPreview, app, ui),
            ButtonId::ServeHtml => return dispatch(Action::ServeActiveHtml, app, ui),
            ButtonId::PreviewRenderToggle => {
                return dispatch(Action::TogglePreviewRenderMode, app, ui)
            }
            ButtonId::LinksModal => return dispatch(Action::OpenLinkExtractor, app, ui),
            ButtonId::OpenInBrowser => return dispatch(Action::OpenActiveWebUrl, app, ui),
            ButtonId::NarrowSidebar => {
                return dispatch(Action::SelectNarrowPane(NarrowPane::Sidebar), app, ui)
            }
            ButtonId::NarrowFiles => {
                return dispatch(Action::SelectNarrowPane(NarrowPane::Files), app, ui)
            }
            ButtonId::NarrowInspector => {
                return dispatch(Action::SelectNarrowPane(NarrowPane::Inspector), app, ui)
            }
            ButtonId::ConfirmOk | ButtonId::ConfirmCancel | ButtonId::OverlayClose => {}
        },
        _ => {}
    }

    false
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::site_row_activation;

    #[test]
    fn site_row_first_activation_loads_libraries_and_stays_expanded() {
        // Libraries not loaded yet: one activation requests them and never collapses.
        let mut collapsed: HashSet<String> = HashSet::new();
        assert!(site_row_activation(&mut collapsed, "site-a", false));
        assert!(!collapsed.contains("site-a"), "first activation must not collapse the row");

        // Still not loaded (request in flight or failed): asks again, still expanded.
        assert!(site_row_activation(&mut collapsed, "site-a", false));
        assert!(!collapsed.contains("site-a"));

        // A site collapsed earlier without libraries expands on activation.
        collapsed.insert("site-b".to_string());
        assert!(site_row_activation(&mut collapsed, "site-b", false));
        assert!(!collapsed.contains("site-b"));
    }

    #[test]
    fn site_row_with_loaded_libraries_toggles_collapse() {
        let mut collapsed: HashSet<String> = HashSet::new();
        assert!(!site_row_activation(&mut collapsed, "site-a", true));
        assert!(collapsed.contains("site-a"));
        assert!(!site_row_activation(&mut collapsed, "site-a", true));
        assert!(!collapsed.contains("site-a"));
        // Other sites are untouched.
        collapsed.insert("site-b".to_string());
        assert!(!site_row_activation(&mut collapsed, "site-a", true));
        assert!(collapsed.contains("site-a") && collapsed.contains("site-b"));
    }

    fn build_action_test_env() -> (
        super::AppState,
        super::UiState,
        tokio::sync::mpsc::UnboundedReceiver<super::UiMsg>,
    ) {
        use crate::{
            auth::AuthManager,
            config::{AppConfig, Persona},
            graph::GraphClient,
            storage::Storage,
            sync::{FocusTarget, SyncCommand, SyncController},
        };
        let cfg = AppConfig::default();
        let auth = AuthManager::empty_for_test(cfg.clone());
        let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (focus_tx, _focus_rx) = tokio::sync::watch::channel(FocusTarget::default());
        let (nudge_tx, _nudge_rx) = tokio::sync::mpsc::unbounded_channel::<SyncCommand>();
        let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx);
        let (ui_tx, ui_rx) = tokio::sync::mpsc::unbounded_channel::<super::UiMsg>();
        let ui = super::UiState::from_config(&cfg);
        let app = super::AppState::new(cfg, Persona::SharePoint, auth, graph, storage, sync_ctrl, ui_tx);
        (app, ui, ui_rx)
    }

    #[tokio::test]
    async fn overlay_outcome_dispatch_executes_for_search_settings_links_and_image_preview() {
        let _env_lock = crate::config::TEST_ENV_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let dir_str = dir.path().to_string_lossy().into_owned();
        let _env = crate::config::EnvVarGuard::set(&[(
            "SHAREPOINT_CONFIG_DIR",
            Some(dir_str.as_str()),
        )]);

        let (mut app, mut ui, mut rx) = build_action_test_env();
        let report = crate::models::DriveItem {
            id: "rep-1".into(),
            drive_id: "d1".into(),
            name: "quarterly-report.md".into(),
            size: 128,
            web_url: Some("https://contoso.sharepoint.com/quarterly-report.md".into()),
            ..Default::default()
        };
        let img = crate::models::DriveItem {
            id: "img-1".into(),
            drive_id: "d1".into(),
            name: "chart.png".into(),
            size: 256,
            mime_type: Some("image/png".into()),
            ..Default::default()
        };
        app.storage.upsert_drive_items(&[report.clone(), img.clone()]).unwrap();
        app.current_items = vec![report.clone(), img.clone()];
        app.active_selected_id = Some("rep-1".into());

        // 1. SearchOverlay: typing dispatches SearchLocalFiles without being swallowed!
        super::dispatch(super::Action::OpenSearch, &mut app, &mut ui);
        super::dispatch(super::Action::Input('q'), &mut app, &mut ui);
        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("SearchLocalFiles must not be swallowed by SearchOverlay")
            .unwrap();
        app.apply_ui_msg(msg, &mut ui);
        assert_eq!(app.search_results.len(), 1);
        assert_eq!(app.search_results[0].id, "rep-1");
        super::dispatch(super::Action::CloseOverlay, &mut app, &mut ui);

        // 2. OpenSearchWithQuery dispatches SearchLocalFiles immediately without being swallowed!
        super::dispatch(super::Action::OpenSearchWithQuery("quarterly".into()), &mut app, &mut ui);
        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("OpenSearchWithQuery must execute SearchLocalFiles")
            .unwrap();
        app.apply_ui_msg(msg, &mut ui);
        assert_eq!(app.search_results.len(), 1);
        super::dispatch(super::Action::CloseOverlay, &mut app, &mut ui);

        // 3. SettingsState rows dispatch mutations while Settings remains open!
        super::dispatch(super::Action::OpenOverlay(super::OverlayKind::Settings), &mut app, &mut ui);
        let notify0 = ui.notify;
        super::dispatch(super::Action::Activate, &mut app, &mut ui); // Row 0: Notify
        assert_ne!(ui.notify, notify0);
        assert!(matches!(ui.overlays.last(), Some(super::Overlay::Settings(_))));

        let rail0 = ui.show_nav_rail;
        super::dispatch(super::Action::MoveDown, &mut app, &mut ui);
        super::dispatch(super::Action::Activate, &mut app, &mut ui); // Row 1: NavRail
        assert_ne!(ui.show_nav_rail, rail0);

        let proto0 = ui.graphics_caps.protocol;
        for _ in 0..3 {
            super::dispatch(super::Action::MoveDown, &mut app, &mut ui);
        }
        super::dispatch(super::Action::Activate, &mut app, &mut ui); // Row 4: ImageProtocol
        assert_ne!(ui.graphics_caps.protocol, proto0);
        super::dispatch(super::Action::CloseOverlay, &mut app, &mut ui);

        // 4. LinkExtractorOverlay `y` dispatches CopyUrlToClipboard (toast emitted) while staying open!
        super::dispatch(super::Action::OpenLinkExtractor, &mut app, &mut ui);
        ui.toasts = crate::ui::toast::ToastQueue::default();
        super::dispatch(super::Action::Input('y'), &mut app, &mut ui);
        assert!(matches!(ui.overlays.last(), Some(super::Overlay::Links(_))));
        assert!(
            ui.toasts.latest().is_some(),
            "y in LinkExtractorOverlay must dispatch CopyUrlToClipboard"
        );
        super::dispatch(super::Action::CloseOverlay, &mut app, &mut ui);

        // 5. ImagePreviewOverlay `p` cycles image protocol while staying open!
        super::dispatch(super::Action::OpenImagePreview, &mut app, &mut ui);
        assert!(matches!(ui.overlays.last(), Some(super::Overlay::ImagePreview(_))));
        let p_before = ui.graphics_caps.protocol;
        super::dispatch(super::Action::Input('p'), &mut app, &mut ui);
        assert_ne!(ui.graphics_caps.protocol, p_before);
        assert!(matches!(ui.overlays.last(), Some(super::Overlay::ImagePreview(_))));
    }

    #[tokio::test]
    async fn backdrop_click_closes_without_click_through_and_zoxide_returns_to_browse() {
        use ratatui::{backend::TestBackend, Terminal};

        let (mut app, mut ui, _rx) = build_action_test_env();
        let html_item = crate::models::DriveItem {
            id: "h1".into(),
            drive_id: "d1".into(),
            name: "page.html".into(),
            size: 100,
            ..Default::default()
        };
        app.current_items = vec![html_item.clone()];
        app.active_selected_id = Some("h1".into());
        app.set_text_preview("d1", "h1", "<html><body>Hi</body></html>".into());

        // Draw workspace to locate underlying PreviewRenderToggle button coords
        let mut terminal = Terminal::new(TestBackend::new(140, 32)).unwrap();
        terminal
            .draw(|f| {
                crate::ui::draw(f, &mut app, &mut ui);
            })
            .unwrap();
        let toggle_rect = ui
            .hitmap
            .region_of(super::HitTarget::Button(super::ButtonId::PreviewRenderToggle))
            .expect("PreviewRenderToggle button in Inspector");

        // Open Settings modal and redraw -> clicking underlying button coords MUST NOT toggle render_markup, and MUST close modal!
        let render_before = ui.render_markup;
        super::dispatch(super::Action::OpenOverlay(super::OverlayKind::Settings), &mut app, &mut ui);
        terminal
            .draw(|f| {
                crate::ui::draw(f, &mut app, &mut ui);
            })
            .unwrap();
        assert_eq!(
            ui.hitmap.hit(toggle_rect.y, toggle_rect.x + 1),
            Some(super::HitTarget::OverlayBackdrop)
        );
        super::dispatch(
            super::Action::Click {
                column: toggle_rect.x + 1,
                row: toggle_rect.y,
            },
            &mut app,
            &mut ui,
        );
        assert!(ui.overlays.is_empty(), "backdrop click must close Settings modal");
        assert_eq!(ui.render_markup, render_before, "backdrop click must not click through to Inspector");

        // FilePicker in ZoxideJump mode: backdrop click routes through CloseOverlay -> returns to Browse, second click closes!
        let root = tempfile::tempdir().unwrap();
        let mut picker = crate::ui::overlays::file_picker::FilePickerOverlay::new_in_dir(root.path().to_path_buf());
        picker.enter_zoxide_with_dirs(vec![root.path().to_path_buf()]);
        ui.push_overlay(super::Overlay::FilePicker(picker));
        terminal
            .draw(|f| {
                crate::ui::draw(f, &mut app, &mut ui);
            })
            .unwrap();
        assert!(ui.hitmap.region_of(super::HitTarget::Button(super::ButtonId::OverlayClose)).is_some());

        // Click top-left corner (outside centered modal) -> returns to Browse
        super::dispatch(super::Action::Click { column: 0, row: 0 }, &mut app, &mut ui);
        match ui.overlays.last() {
            Some(super::Overlay::FilePicker(p)) => {
                assert_eq!(p.mode, crate::ui::overlays::file_picker::FilePickerMode::Browse);
            }
            other => panic!("expected FilePicker in Browse mode, got {other:?}"),
        }
        // Second backdrop click closes FilePicker
        super::dispatch(super::Action::Click { column: 0, row: 0 }, &mut app, &mut ui);
        assert!(ui.overlays.is_empty());
    }

    #[tokio::test]
    async fn cycle_sort_mode_keeps_selected_idx_and_active_id_synchronized() {
        let (mut app, mut ui, _rx) = build_action_test_env();
        app.current_items = vec![
            crate::models::DriveItem {
                id: "small-new".into(),
                drive_id: "d1".into(),
                name: "alpha.txt".into(),
                size: 10,
                last_modified_at: Some("2026-11-25T10:00:00Z".into()),
                ..Default::default()
            },
            crate::models::DriveItem {
                id: "big-old".into(),
                drive_id: "d1".into(),
                name: "zeta.md".into(),
                size: 9000,
                last_modified_at: Some("2025-01-01T00:00:00Z".into()),
                ..Default::default()
            },
        ];
        app.active_selected_id = Some("big-old".into());
        ui.list_scroll.selected_idx = Some(1);

        for _ in 0..4 {
            super::dispatch(super::Action::CycleSortMode, &mut app, &mut ui);
            let sorted = app.sorted_visible_items();
            let idx = ui.list_scroll.selected_idx.expect("selected_idx");
            assert_eq!(
                Some(sorted[idx].id.as_str()),
                app.active_selected_id.as_deref(),
                "selected_idx must match active_selected_id for sort mode {:?}",
                app.sort_mode
            );
        }
    }

    #[tokio::test]
    async fn right_click_sets_list_focus_prevents_stacking_and_targets_inspector() {
        use ratatui::{backend::TestBackend, Terminal};

        let (mut app, mut ui, _rx) = build_action_test_env();
        app.active_source = super::ActiveViewSource::Drive {
            drive_id: "d1".into(),
            label: "Docs".into(),
        };
        let fld = crate::models::DriveItem {
            id: "fld-1".into(),
            drive_id: "d1".into(),
            name: "Projects".into(),
            is_folder: true,
            ..Default::default()
        };
        app.current_items = vec![fld];
        app.active_selected_id = Some("fld-1".into());
        ui.focus = super::Focus::Nav;

        let mut terminal = Terminal::new(TestBackend::new(140, 30)).unwrap();
        terminal
            .draw(|f| {
                crate::ui::draw(f, &mut app, &mut ui);
            })
            .unwrap();

        let item_rect = ui.hitmap.region_of(super::HitTarget::ListItem(0)).unwrap();
        super::dispatch(
            super::Action::OpenContextMenuAt {
                column: item_rect.x + 2,
                row: item_rect.y,
            },
            &mut app,
            &mut ui,
        );
        assert_eq!(ui.focus, super::Focus::List, "right-clicking ListItem must focus List");
        assert_eq!(ui.overlays.len(), 1);

        // Activating "Open Folder" opens the folder!
        super::dispatch(super::Action::Activate, &mut app, &mut ui);
        assert!(ui.overlays.is_empty());
        assert_eq!(app.folder_stack.len(), 1);
        assert_eq!(app.folder_stack[0].0, "fld-1");

        // ContextMenu must NOT stack on top of Settings!
        super::dispatch(super::Action::OpenOverlay(super::OverlayKind::Settings), &mut app, &mut ui);
        super::dispatch(super::Action::OpenContextMenuAt { column: 5, row: 5 }, &mut app, &mut ui);
        assert_eq!(ui.overlays.len(), 1);
        assert!(matches!(ui.overlays.last(), Some(super::Overlay::Settings(_))));
        super::dispatch(super::Action::CloseOverlay, &mut app, &mut ui);

        // Right-clicking Inspector opens DriveItem menu (not Global), and second right-click replaces (not stacks).
        app.folder_stack.clear();
        app.current_items = vec![crate::models::DriveItem {
            id: "f1".into(),
            drive_id: "d1".into(),
            name: "spec.txt".into(),
            ..Default::default()
        }];
        app.active_selected_id = Some("f1".into());
        terminal
            .draw(|f| {
                crate::ui::draw(f, &mut app, &mut ui);
            })
            .unwrap();
        let insp = ui.inspector_scroll.area;
        super::dispatch(
            super::Action::OpenContextMenuAt {
                column: insp.x + 2,
                row: insp.y + 4,
            },
            &mut app,
            &mut ui,
        );
        match ui.overlays.last() {
            Some(super::Overlay::ContextMenu(m)) => {
                assert!(matches!(m.context, super::ContextTarget::DriveItem(_)));
            }
            other => panic!("expected DriveItem context menu, got {other:?}"),
        }
        super::dispatch(super::Action::OpenContextMenuAt { column: 2, row: 2 }, &mut app, &mut ui);
        assert_eq!(ui.overlays.len(), 1, "re-opening context menu must replace, not stack");
    }

    #[tokio::test]
    async fn narrow_mode_clears_hidden_pane_rects_for_mouse_scroll() {
        use ratatui::{backend::TestBackend, layout::Rect, Terminal};

        let (mut app, mut ui, _rx) = build_action_test_env();
        app.current_items = vec![
            crate::models::DriveItem { id: "a".into(), drive_id: "d1".into(), name: "a.txt".into(), ..Default::default() },
            crate::models::DriveItem { id: "b".into(), drive_id: "d1".into(), name: "b.txt".into(), ..Default::default() },
        ];
        app.active_selected_id = Some("a".into());
        ui.list_scroll.selected_idx = Some(0);

        // Render Wide first so inspector_scroll.area is populated
        let mut wide = Terminal::new(TestBackend::new(140, 30)).unwrap();
        wide.draw(|f| {
            crate::ui::draw(f, &mut app, &mut ui);
        })
        .unwrap();
        assert_ne!(ui.inspector_scroll.area, Rect::default());

        // Switch to Narrow (width 60) on Files pane -> inspector_scroll.area MUST be reset to default
        ui.narrow_pane = super::NarrowPane::Files;
        let mut narrow = Terminal::new(TestBackend::new(60, 24)).unwrap();
        narrow
            .draw(|f| {
                crate::ui::draw(f, &mut app, &mut ui);
            })
            .unwrap();
        assert_eq!(ui.inspector_scroll.area, Rect::default());
        assert_ne!(ui.list_scroll.area, Rect::default());

        // Wheel scroll inside list pane moves selection (not hijacked by stale inspector rect)
        let lr = ui.list_scroll.area;
        super::dispatch(
            super::Action::MouseScroll {
                delta: 3,
                column: lr.x + 2,
                row: lr.y + 3,
            },
            &mut app,
            &mut ui,
        );
        assert_eq!(ui.list_scroll.selected_idx, Some(1));
        assert_eq!(app.active_selected_id.as_deref(), Some("b"));
    }

    #[tokio::test]
    async fn toggle_pin_current_handles_subfolders_and_remote_effective_drive_id() {
        let (mut app, mut ui, _rx) = build_action_test_env();
        app.active_source = super::ActiveViewSource::Drive {
            drive_id: "d1".into(),
            label: "Docs".into(),
        };
        app.folder_stack = vec![("sub-1".into(), "Sub1".into())];
        app.current_items = vec![crate::models::DriveItem {
            id: "file-1".into(),
            drive_id: "d1".into(),
            name: "doc.txt".into(),
            ..Default::default()
        }];
        app.active_selected_id = Some("file-1".into());

        super::dispatch(super::Action::TogglePinCurrent, &mut app, &mut ui);
        assert_eq!(app.pins.len(), 1);
        assert_eq!(app.pins[0].key, "folder:d1:sub-1");
        assert_eq!(app.pins[0].item_id.as_deref(), Some("sub-1"));

        // Different subfolder produces a distinct pin key instead of colliding on :root
        app.folder_stack = vec![("sub-2".into(), "Sub2".into())];
        super::dispatch(super::Action::TogglePinCurrent, &mut app, &mut ui);
        assert_eq!(app.pins.len(), 2);
        assert_eq!(app.pins[1].key, "folder:d1:sub-2");

        // Remote shortcut folder in SharedWithMe pins with effective_drive_id and effective_item_id
        app.active_source = super::ActiveViewSource::SharedWithMe;
        app.folder_stack.clear();
        app.current_items = vec![crate::models::DriveItem {
            id: "local-shortcut".into(),
            drive_id: "local-drv".into(),
            remote_drive_id: Some("remote-drv".into()),
            remote_item_id: Some("remote-fld".into()),
            name: "SharedFolder".into(),
            is_folder: true,
            ..Default::default()
        }];
        app.active_selected_id = Some("local-shortcut".into());
        super::dispatch(super::Action::TogglePinCurrent, &mut app, &mut ui);
        assert_eq!(app.pins.len(), 3);
        assert_eq!(app.pins[2].key, "folder:remote-drv:remote-fld");
        assert_eq!(app.pins[2].drive_id.as_deref(), Some("remote-drv"));
        assert_eq!(app.pins[2].item_id.as_deref(), Some("remote-fld"));
    }

    #[tokio::test]
    async fn jump_to_search_hit_resets_truncation_and_blocks_drive_root_clobber() {
        let (mut app, mut ui, _rx) = build_action_test_env();
        app.active_source = super::ActiveViewSource::Drive {
            drive_id: "d1".into(),
            label: "Docs".into(),
        };
        app.current_items_truncated = true;
        app.folder_item_limit = 400;

        let hit = crate::models::DriveItem {
            id: "hit-1".into(),
            drive_id: "d1".into(),
            name: "found.md".into(),
            ..Default::default()
        };
        app.search_results = vec![hit.clone()];

        super::dispatch(super::Action::JumpToSearchHit(0), &mut app, &mut ui);
        assert!(!app.current_items_truncated);
        assert_eq!(app.folder_item_limit, crate::sync::FOLDER_PAGE_SIZE);
        assert!(matches!(app.active_source, super::ActiveViewSource::SearchResult { .. }));
        assert_eq!(app.current_items.len(), 1);
        assert_eq!(app.current_items[0].id, "hit-1");

        // Background drive-root delta must NOT overwrite the single search hit!
        app.apply_core_event(
            crate::models::CoreEvent::FolderItemsUpdated {
                drive_id: "d1".into(),
                parent_id: None,
                items: vec![crate::models::DriveItem {
                    id: "root-file".into(),
                    drive_id: "d1".into(),
                    name: "root.txt".into(),
                    ..Default::default()
                }],
                truncated: false,
            },
            &mut ui,
        );
        assert_eq!(app.current_items.len(), 1);
        assert_eq!(app.current_items[0].id, "hit-1");
    }

    #[tokio::test]
    async fn inspector_focus_click_wrapped_lines_scroll_and_overlay_windowing() {
        use ratatui::{backend::TestBackend, Terminal};

        let (mut app, mut ui, _rx) = build_action_test_env();
        let minified = crate::models::DriveItem {
            id: "min-1".into(),
            drive_id: "d1".into(),
            name: "data.json".into(),
            size: 1000,
            mime_type: Some("application/json".into()),
            ..Default::default()
        };
        app.current_items = vec![minified.clone()];
        app.active_selected_id = Some("min-1".into());
        app.set_text_preview("d1", "min-1", format!("{{\"data\":\"{}\"}}", "x".repeat(900)));
        ui.focus = super::Focus::Nav;

        let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
        terminal
            .draw(|f| {
                crate::ui::draw(f, &mut app, &mut ui);
            })
            .unwrap();

        // Clicking empty Inspector body focuses Inspector!
        let insp = ui.inspector_scroll.area;
        super::dispatch(
            super::Action::Click {
                column: insp.x + 2,
                row: insp.y + 4,
            },
            &mut app,
            &mut ui,
        );
        assert_eq!(ui.focus, super::Focus::Inspector);

        // Wrapped visual lines exceed viewport height, enabling full scroll to bottom.
        let body_h = super::inspector_body_height(insp);
        assert!(
            ui.inspector_scroll.total_lines > body_h,
            "minified 1-line JSON must count wrapped visual lines (got {})",
            ui.inspector_scroll.total_lines
        );
        super::dispatch(super::Action::ScrollBottom, &mut app, &mut ui);
        assert_eq!(
            ui.inspector_scroll.offset,
            ui.inspector_scroll.total_lines.saturating_sub(body_h)
        );

        // Halfblock inline image registers PreviewImageChip and adjusts its Y by inspector_scroll.offset!
        let img = crate::models::DriveItem {
            id: "hb-1".into(),
            drive_id: "d1".into(),
            name: "hb.png".into(),
            mime_type: Some("image/png".into()),
            ..Default::default()
        };
        app.current_items = vec![img.clone()];
        app.active_selected_id = Some("hb-1".into());
        ui.inspector_scroll.offset = 0;
        let key = app.inline_image_cache_key(&img);
        app.rendered_images.insert(
            key,
            crate::img_preview::RenderedTerminalImage {
                width_px: 100,
                height_px: 100,
                cols: 20,
                rows: 10,
                format_label: "PNG".into(),
                protocol: crate::img_preview::TerminalImageProtocol::Halfblock,
                lines: (0..18)
                    .map(|i| ratatui::text::Line::raw(format!("img-row-{i}")))
                    .collect(),
                pixel_payload: None,
                scroll_backing: None,
            },
        );
        terminal
            .draw(|f| {
                crate::ui::draw(f, &mut app, &mut ui);
            })
            .unwrap();
        let chip_y0 = ui
            .hitmap
            .region_of(super::HitTarget::PreviewImageChip)
            .expect("Halfblock inline image must register PreviewImageChip")
            .y;
        ui.inspector_scroll.offset = 2;
        terminal
            .draw(|f| {
                crate::ui::draw(f, &mut app, &mut ui);
            })
            .unwrap();
        let chip_y2 = ui
            .hitmap
            .region_of(super::HitTarget::PreviewImageChip)
            .expect("PreviewImageChip after scroll")
            .y;
        assert_eq!(chip_y2, chip_y0.saturating_sub(2));

        // Scroll windowing for SettingsState, ContextMenu, and HelpOverlay on short terminal (height=8)
        let mut short_term = Terminal::new(TestBackend::new(70, 8)).unwrap();
        ui.push_overlay(super::Overlay::Settings(
            crate::ui::overlays::settings::SettingsState { selected: 10 },
        ));
        short_term
            .draw(|f| {
                crate::ui::draw(f, &mut app, &mut ui);
            })
            .unwrap();
        assert!(
            ui.hitmap
                .region_of(super::HitTarget::OverlayItem { index: 10 })
                .is_some(),
            "Settings row 10 must be windowed into view on short terminal"
        );
        ui.pop_overlay();

        let mut menu = super::ContextMenu::build(super::ContextTarget::DriveItem(0), (0, 0), &app, &ui);
        let last_idx = menu.items.len() - 1;
        menu.select(last_idx);
        ui.push_overlay(super::Overlay::context_menu(menu));
        short_term
            .draw(|f| {
                crate::ui::draw(f, &mut app, &mut ui);
            })
            .unwrap();
        assert!(
            ui.hitmap
                .region_of(super::HitTarget::OverlayItem { index: last_idx })
                .is_some(),
            "ContextMenu last item must be windowed into view on short terminal"
        );
        ui.pop_overlay();

        ui.push_overlay(super::Overlay::help());
        short_term
            .draw(|f| {
                crate::ui::draw(f, &mut app, &mut ui);
            })
            .unwrap();
        super::dispatch(super::Action::MoveDown, &mut app, &mut ui);
        match ui.overlays.last() {
            Some(super::Overlay::Help(h)) => {
                assert!(h.max_scroll_y > 0);
                assert_eq!(h.scroll_y, 1);
            }
            other => panic!("expected Help overlay, got {other:?}"),
        }
    }
}
