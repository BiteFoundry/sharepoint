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

use super::state::MAX_RAW_IMAGE_ENTRIES;
use crate::config::ImageProtocolPreference;
use crate::img_preview;
use crate::img_preview::TerminalImageProtocol;
use crate::models::CoreEvent;
use crate::models::DriveInfo;
use crate::models::PinnedTarget;
use crate::models::SharePointSite;
use crate::models::VirtualListKind;
use crate::models::VirtualListStatus;
use crate::sync::FOLDER_PAGE_SIZE;
use crate::sync::MAX_FOLDER_ITEMS;
use crate::ui::keymap::Keymap;
use crate::ui::overlays::Overlay;
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
        let fit = overlay.fit_box(ui.term_width, ui.term_height);
        let key = app.modal_image_cache_key(
            item,
            fit.view_cols,
            fit.view_rows,
            fit.zoom_pct,
            ui.graphics_caps,
        );
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

#[tokio::test]
async fn folder_created_and_item_uploaded_render_immediately_without_loading_blank_or_redownload() {
    use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
    use std::io::Cursor;

    let content_hits = Arc::new(AtomicUsize::new(0));
    let ch = Arc::clone(&content_hits);
    let router = Router::new()
        .route(
            "/v1.0/drives/{drive_id}/items/{item_id}/content",
            get(move || {
                let hits = Arc::clone(&ch);
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR
                }
            }),
        )
        .route(
            "/v1.0/drives/{drive_id}/items/{item_id}/versions",
            get(|| async { axum::Json(serde_json::json!({ "value": [] })) }),
        );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let base_url = format!("http://{addr}/v1.0");
    let (mut app, mut ui, mut rx) = test_app_and_ui_with_base_url(Some(&base_url));
    let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
    let (nudge_tx, mut nudge_rx) = mpsc::unbounded_channel::<SyncCommand>();
    app.sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx);
    ui.inline_images = true;
    ui.graphics_caps.protocol = TerminalImageProtocol::Halfblock;
    app.active_source = ActiveViewSource::Drive {
        drive_id: "drv-1".into(),
        label: "Docs".into(),
    };

    // 1. FolderCreated inserts & selects immediately without triggering a "Loading..." blank.
    app.loading_folder = true;
    let new_folder = make_folder("drv-1", "fld-new", "NewFolder");
    app.apply_ui_msg(
        UiMsg::FolderCreated {
            folder: new_folder,
            parent_id: None,
        },
        &mut ui,
    );
    assert!(!app.loading_folder);
    assert_eq!(app.active_selected_id.as_deref(), Some("fld-new"));
    assert!(app.current_items.iter().any(|i| i.id == "fld-new"));
    let cmd1 = nudge_rx.try_recv().expect("expected background SyncFolder");
    assert!(matches!(
        cmd1,
        SyncCommand::SyncFolder { ref drive_id, parent_id: None, .. } if drive_id == "drv-1"
    ));

    // 2. ItemUploaded with local text preview renders immediately with 0 /content downloads.
    app.loading_folder = true;
    let uploaded_md = DriveItem {
        id: "doc-up".into(),
        drive_id: "drv-1".into(),
        name: "draft.md".into(),
        size: 14,
        mime_type: Some("text/markdown".into()),
        e_tag: Some("\"etag-up\"".into()),
        c_tag: Some("\"ctag-up\"".into()),
        ..Default::default()
    };
    app.apply_ui_msg(
        UiMsg::ItemUploaded {
            item: uploaded_md.clone(),
            parent_id: None,
            local_preview: Some(LocalUploadPreview::Text("# Local Draft\n".into())),
        },
        &mut ui,
    );
    assert!(!app.loading_folder);
    assert_eq!(app.active_selected_id.as_deref(), Some("doc-up"));
    assert_eq!(
        app.text_preview_for(&uploaded_md),
        Some("# Local Draft\n")
    );
    let screen = draw_inspector_frame(&app, &mut ui);
    assert!(screen.contains("Local Draft"), "{screen}");

    // 3. ItemUploaded with local image bytes decodes directly from memory with 0 /content downloads.
    let mut png_bytes = Vec::new();
    let img = RgbaImage::from_pixel(2, 2, Rgba([0, 128, 255, 255]));
    DynamicImage::ImageRgba8(img)
        .write_to(&mut Cursor::new(&mut png_bytes), ImageFormat::Png)
        .unwrap();
    let uploaded_png = DriveItem {
        id: "img-up".into(),
        drive_id: "drv-1".into(),
        name: "snap.png".into(),
        size: png_bytes.len() as u64,
        mime_type: Some("image/png".into()),
        e_tag: Some("\"etag-img\"".into()),
        c_tag: Some("\"ctag-img\"".into()),
        ..Default::default()
    };
    app.apply_ui_msg(
        UiMsg::ItemUploaded {
            item: uploaded_png.clone(),
            parent_id: None,
            local_preview: Some(LocalUploadPreview::Image(Arc::from(png_bytes))),
        },
        &mut ui,
    );
    let img_key = app.inline_image_cache_key(&uploaded_png);
    let mut saw_decoded = false;
    while !saw_decoded {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
            .await
            .expect("expected ImageDecoded from local upload bytes")
            .expect("channel open");
        if matches!(&msg, UiMsg::ImageDecoded { cache_key, .. } if *cache_key == img_key) {
            saw_decoded = true;
        }
        app.apply_ui_msg(msg, &mut ui);
    }
    assert_eq!(app.rendered_images.get(&img_key).unwrap().width_px, 2);
    assert_eq!(
        content_hits.load(Ordering::SeqCst),
        0,
        "locally uploaded text and image items must not trigger redundant /content downloads"
    );

    server.abort();
}
