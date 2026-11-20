//! TUI entry shell: terminal raw mode, 80ms CSI/DA1/Kitty active graphics probing,
//! pixel graphics overlay flush, mouse capture, event loop, and shared IPC daemon attachment.

use std::borrow::Cow;
use std::io::{self, Stdout, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as CrosstermEvent, EventStream, KeyEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, BeginSynchronizedUpdate,
    EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::event::{KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;
use tokio::sync::mpsc;

use crate::auth::AuthManager;
use crate::config::{AppConfig, ImageProtocolPreference, Persona};
use crate::graph::GraphClient;
use crate::img_preview::{
    self, ActivePixelGraphic, TerminalGraphicsCaps, TerminalImageProtocol,
};
use crate::models::CoreEvent;
use crate::storage::Storage;
use crate::sync::{self, SyncCommand};
use crate::ui::{
    self, action,
    action::Action,
    app::{AppState, UiMsg, UiState},
    event,
    overlays::Overlay,
};

static KITTY_USED_GLOBAL: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, PartialEq, Eq)]
struct PixelReconcilePlan {
    erase_and_redraw: bool,
    delete_kitty: bool,
    erase_rects: Vec<Rect>,
    emit_desired: bool,
}

#[derive(Debug, Default)]
struct PixelLayerTracker {
    active: Vec<ActivePixelGraphic>,
    last_term_size: (u16, u16),
}

impl PixelLayerTracker {
    fn plan_transition(
        &self,
        desired: &[ActivePixelGraphic],
        term_size: (u16, u16),
        kitty_used: bool,
    ) -> PixelReconcilePlan {
        let term_resized = self.last_term_size != term_size;
        if self.active == desired && !term_resized {
            return PixelReconcilePlan {
                erase_and_redraw: false,
                delete_kitty: false,
                erase_rects: Vec::new(),
                emit_desired: false,
            };
        }

        let had_active = !self.active.is_empty();
        let delete_kitty = had_active
            && (kitty_used
                || self
                    .active
                    .iter()
                    .any(|g| g.protocol == TerminalImageProtocol::Kitty));
        let erase_rects = self
            .active
            .iter()
            .map(|prev| {
                Rect::new(
                    prev.rect.x,
                    prev.rect.y,
                    prev.rect.width.max(prev.cols),
                    prev.rect.height.max(prev.rows),
                )
            })
            .collect();

        PixelReconcilePlan {
            erase_and_redraw: had_active,
            delete_kitty,
            erase_rects,
            emit_desired: !desired.is_empty(),
        }
    }

    fn commit(&mut self, desired: Vec<ActivePixelGraphic>, term_size: (u16, u16)) {
        self.active = desired;
        self.last_term_size = term_size;
    }
}

fn parse_pixel_key(key: &str) -> (&str, Option<(u16, u16, u16, u16)>) {
    if let Some((base, crop_str)) = key.rsplit_once("#crop:") {
        let mut parts = crop_str.split(',');
        if let (Some(sx), Some(sy), Some(vw), Some(vh), None) = (
            parts.next().and_then(|s| s.parse::<u16>().ok()),
            parts.next().and_then(|s| s.parse::<u16>().ok()),
            parts.next().and_then(|s| s.parse::<u16>().ok()),
            parts.next().and_then(|s| s.parse::<u16>().ok()),
            parts.next(),
        ) {
            return (base, Some((sx, sy, vw, vh)));
        }
    }
    (key, None)
}

/// Overlay Z-order guard: blocks inline pixel graphics whenever any modal overlay is active,
/// and blocks modal pixel graphics whenever another modal (e.g., `ConfirmDialog`) sits on top of `ImagePreview`.
fn pixel_graphics_allowed_for_overlays(overlays: &[Overlay], cache_key: &str) -> bool {
    if overlays.is_empty() {
        true
    } else if matches!(overlays.last(), Some(Overlay::ImagePreview(_))) {
        !cache_key.starts_with("inline:")
    } else {
        false
    }
}

/// Evaluates whether a pending pixel graphic fits cleanly inside its visible region and above the bottom status row.
/// Returns `Ok(placement_rect)` when safe to emit, `Err(Some((fit_w, fit_h)))` when an oversized inline image
/// should be re-decoded at exact dimensions, or `Err(None)` when the graphic must be suppressed.
fn evaluate_pixel_placement(
    cache_key: &str,
    pending_rect: Rect,
    rendered_cols: u16,
    rendered_rows: u16,
    inspector_area: Rect,
    inspector_scroll_y: u16,
    term_width: u16,
    term_height: u16,
) -> std::result::Result<Rect, Option<(u16, u16)>> {
    let mut rect = pending_rect;
    let is_inline = cache_key.starts_with("inline:");

    let (avail_w, avail_h) = if is_inline {
        let body_y = inspector_area.y.saturating_add(1);
        let body_h = inspector_area.height.saturating_sub(1);
        let body_bottom = body_y.saturating_add(body_h);
        if inspector_scroll_y > 0 {
            if rect.y < body_y.saturating_add(inspector_scroll_y) {
                return Err(None);
            }
            rect.y = rect.y.saturating_sub(inspector_scroll_y);
        }
        if rect.y >= body_bottom {
            return Err(None);
        }
        let insp_w = inspector_area.width.saturating_sub(2);
        let insp_h = body_bottom.saturating_sub(rect.y);
        (insp_w, insp_h)
    } else {
        (rect.width, rect.height)
    };

    // Never allow any pixel graphic to touch the bottom-most terminal row (`term_height - 1`),
    // preventing Sixel upward terminal scrolling that desynchronizes Ratatui's grid.
    let safe_max_h = term_height.saturating_sub(1).saturating_sub(rect.y);
    let safe_max_w = term_width.saturating_sub(rect.x);
    let fit_w = avail_w.min(safe_max_w);
    let fit_h = avail_h.min(safe_max_h);

    if fit_w < 4 || fit_h < 2 {
        return Err(None);
    }
    if rendered_cols == 0 || rendered_rows == 0 {
        return Err(None);
    }
    if rendered_cols > fit_w || rendered_rows > fit_h {
        if is_inline {
            return Err(Some((fit_w, fit_h)));
        }
        return Err(None);
    }

    Ok(Rect::new(rect.x, rect.y, rendered_cols, rendered_rows))
}

struct TerminalGuard {
    keyboard_enhanced: bool,
    kitty_used: bool,
    pixel_tracker: PixelLayerTracker,
    last_inspector_area: Rect,
}

impl TerminalGuard {
    fn enter(
        image_pref: ImageProtocolPreference,
    ) -> Result<(Self, Terminal<CrosstermBackend<Stdout>>, TerminalGraphicsCaps)> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        // Probe graphics & cell pixel size while raw mode is active and BEFORE
        // EnterAlternateScreen / EnableMouseCapture / EnableBracketedPaste so mouse
        // movement or console-mode swaps never race with CSI/DA1/Kitty replies.
        let probed_caps = img_preview::probe_terminal_graphics_caps(image_pref);

        execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )?;
        let keyboard_enhanced = if supports_keyboard_enhancement().unwrap_or(false) {
            execute!(
                stdout,
                PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                        | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                )
            )
            .is_ok()
        } else {
            false
        };

        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        terminal.hide_cursor()?;
        terminal.clear()?;

        Ok((
            Self {
                keyboard_enhanced,
                kitty_used: false,
                pixel_tracker: PixelLayerTracker::default(),
                last_inspector_area: Rect::default(),
            },
            terminal,
            probed_caps,
        ))
    }

    fn restore(&self) {
        let mut stdout = io::stdout();
        if self.kitty_used || KITTY_USED_GLOBAL.load(Ordering::Relaxed) {
            let _ = stdout.write_all(img_preview::kitty_delete_preview_sequence().as_bytes());
            let _ = stdout.write_all(img_preview::kitty_delete_all_sequence().as_bytes());
            let _ = stdout.flush();
        }
        if self.keyboard_enhanced {
            let _ = execute!(stdout, PopKeyboardEnhancementFlags);
        }
        let _ = execute!(
            stdout,
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = disable_raw_mode();
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

fn install_panic_hook(keyboard_enhanced: bool) {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let mut stdout = io::stdout();
        if KITTY_USED_GLOBAL.load(Ordering::Relaxed) {
            let _ = stdout.write_all(img_preview::kitty_delete_preview_sequence().as_bytes());
            let _ = stdout.write_all(img_preview::kitty_delete_all_sequence().as_bytes());
            let _ = stdout.flush();
        }
        if keyboard_enhanced {
            let _ = execute!(stdout, PopKeyboardEnhancementFlags);
        }
        let _ = execute!(
            stdout,
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = disable_raw_mode();
        prev(info);
    }));
}

pub async fn run_tui(
    config: AppConfig,
    auth: AuthManager,
    graph: GraphClient,
    storage: Storage,
    persona: Persona,
) -> Result<()> {
    let (mut guard, mut terminal, probed_caps) = TerminalGuard::enter(config.image_protocol)?;
    install_panic_hook(guard.keyboard_enhanced);

    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiMsg>();
    let user_id = graph.my_user_id().map(str::to_string);

    // Try attaching to shared IPC daemon when auto_daemon is enabled or daemon already running
    let (sync_ctrl, mut core_rx) = if config.auto_daemon {
        if let Some(conn) = crate::daemon::try_connect_tui(&config, user_id.as_deref()).await {
            (conn.sync_ctrl, conn.events_rx)
        } else {
            let (tx, rx) = mpsc::unbounded_channel::<CoreEvent>();
            let ctrl = sync::spawn_sync_engine(config.clone(), graph.clone(), storage.clone(), tx);
            (ctrl, rx)
        }
    } else {
        let (tx, rx) = mpsc::unbounded_channel::<CoreEvent>();
        let ctrl = sync::spawn_sync_engine(config.clone(), graph.clone(), storage.clone(), tx);
        (ctrl, rx)
    };

    let mut ui = UiState::from_config(&config);
    ui.graphics_caps = probed_caps;
    let mut app = AppState::new(
        config,
        persona,
        auth,
        graph,
        storage,
        sync_ctrl.clone(),
        ui_tx,
    );
    app.spawn_bootstrap_from_cache();
    sync_ctrl.request(SyncCommand::Bootstrap);

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(120));
    let mut dirty = true;
    let mut last_activity_sent: Option<Instant> = None;

    loop {
        if dirty {
            draw_frame(&mut terminal, &mut app, &mut ui, &mut guard)?;
            dirty = false;
        }

        tokio::select! {
            maybe_ev = events.next() => {
                let Some(Ok(ev)) = maybe_ev else { break; };
                if last_activity_sent.is_none_or(|t| t.elapsed() >= Duration::from_secs(5)) {
                    app.sync_ctrl.request(SyncCommand::NotifyUserActivity);
                    last_activity_sent = Some(Instant::now());
                }
                let maybe_action = match ev {
                    CrosstermEvent::Key(key) if key.kind == KeyEventKind::Press => {
                        event::key_to_action(key, ui.effective_focus(), &ui.keymap, ui.accepts_text_input())
                    }
                    CrosstermEvent::Mouse(mouse) => event::mouse_to_action(mouse, &ui.hitmap),
                    CrosstermEvent::Paste(text) => Some(Action::Paste(text)),
                    CrosstermEvent::Resize(w, h) => {
                        ui.term_width = w;
                        ui.term_height = h;
                        app.rendered_images.retain(|k, _| !k.starts_with("inline:"));
                        dirty = true;
                        if matches!(ui.overlays.last(), Some(Overlay::ImagePreview(_))) {
                            Some(Action::LoadPreviewImage)
                        } else {
                            app.ensure_inspector_loaded(&ui);
                            None
                        }
                    }
                    _ => None,
                };
                if let Some(act) = maybe_action {
                    dirty = true;
                    if action::dispatch(act, &mut app, &mut ui) {
                        break;
                    }
                }
            }
            Some(core_ev) = core_rx.recv() => {
                app.apply_core_event(core_ev, &mut ui);
                dirty = true;
            }
            Some(ui_msg) = ui_rx.recv() => {
                app.apply_ui_msg(ui_msg, &mut ui);
                dirty = true;
            }
            _ = tick.tick() => {
                let had_toast = ui.toasts.latest().is_some();
                ui.toasts.prune_expired();
                let toast_changed = had_toast != ui.toasts.latest().is_some();
                if app.sync_busy || !ui.tasks.is_empty() || toast_changed {
                    ui.spinner_tick = ui.spinner_tick.wrapping_add(1);
                    dirty = true;
                }
            }
        }
    }

    app.html_server.shutdown();
    app.sync_ctrl.request(SyncCommand::Shutdown);
    Ok(())
}

fn draw_frame(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut AppState,
    ui: &mut UiState,
    guard: &mut TerminalGuard,
) -> Result<()> {
    let mut stdout = io::stdout();
    let _ = execute!(stdout, BeginSynchronizedUpdate);

    let mut cursor_pos = None;
    terminal.draw(|frame| {
        cursor_pos = ui::draw(frame, app, ui);
    })?;

    // Detect startup / pane-resize geometry changes in the inspector so inline images re-decode cleanly.
    if ui.inspector_scroll.area != guard.last_inspector_area {
        guard.last_inspector_area = ui.inspector_scroll.area;
        if ui.inspector_scroll.area.width > 0 && ui.inspector_scroll.area.height > 0 {
            app.rendered_images.retain(|k, _| !k.starts_with("inline:"));
            app.ensure_inspector_loaded(ui);
        }
    }

    // Resolve desired pixel graphic placements with overlay Z-order, scroll, crop, and overflow guards.
    let mut desired: Vec<ActivePixelGraphic> = Vec::new();
    let mut desired_payloads: Vec<Cow<'_, str>> = Vec::new();
    let pending_list = std::mem::take(&mut ui.pending_pixel_graphics);
    let body_h = ui.inspector_scroll.area.height.saturating_sub(1);
    let max_off = ui.inspector_scroll.total_lines.saturating_sub(body_h as usize);
    let inspector_scroll_y = ui.inspector_scroll.offset.min(max_off) as u16;
    let mut redecode_requests: Vec<(String, u16, u16)> = Vec::new();

    for pending in pending_list {
        if !pixel_graphics_allowed_for_overlays(&ui.overlays, &pending.cache_key) {
            continue;
        }
        let (base_key, crop) = parse_pixel_key(&pending.cache_key);
        let Some(rendered) = app.rendered_images.get(base_key) else {
            continue;
        };
        let resolved: Option<(Cow<'_, str>, u16, u16)> = if let Some((sx, sy, vw, vh)) = crop {
            rendered
                .viewport_pixel_payload(sx, sy, vw, vh)
                .map(|(p, dw, dh)| (Cow::Owned(p), dw, dh))
        } else {
            rendered
                .pixel_payload
                .as_deref()
                .map(|p| (Cow::Borrowed(p), rendered.cols, rendered.rows))
        };
        let Some((payload, draw_cols, draw_rows)) = resolved else {
            continue;
        };

        match evaluate_pixel_placement(
            &pending.cache_key,
            pending.rect,
            draw_cols,
            draw_rows,
            ui.inspector_scroll.area,
            inspector_scroll_y,
            ui.term_width,
            ui.term_height,
        ) {
            Ok(placement_rect) => {
                desired.push(ActivePixelGraphic {
                    cache_key: pending.cache_key,
                    rect: placement_rect,
                    protocol: rendered.protocol,
                    cols: draw_cols,
                    rows: draw_rows,
                    payload_len: payload.len(),
                });
                desired_payloads.push(payload);
            }
            Err(Some((fit_w, fit_h))) => {
                redecode_requests.push((base_key.to_string(), fit_w, fit_h));
            }
            Err(None) => {}
        }
    }

    let emitted_payloads: Vec<String> = desired_payloads
        .into_iter()
        .map(Cow::into_owned)
        .collect();

    for (cache_key, fit_w, fit_h) in redecode_requests {
        app.rendered_images.remove(&cache_key);
        if let Some(item) = app.active_item() {
            if app.inline_image_cache_key(&item) == cache_key {
                app.request_image_decode(&item, cache_key, fit_w, fit_h, ui.graphics_caps);
            }
        }
    }

    let term_size = (ui.term_width, ui.term_height);
    let plan = guard
        .pixel_tracker
        .plan_transition(&desired, term_size, guard.kitty_used);

    if plan.erase_and_redraw {
        if plan.delete_kitty {
            let _ = stdout.write_all(img_preview::kitty_delete_preview_sequence().as_bytes());
            let _ = stdout.write_all(img_preview::kitty_delete_all_sequence().as_bytes());
        }
        for rect in &plan.erase_rects {
            let erase = img_preview::erase_pixel_rect_sequence(
                rect.x,
                rect.y,
                rect.width,
                rect.height,
                ui.term_width,
                ui.term_height,
            );
            if !erase.is_empty() {
                let _ = stdout.write_all(erase.as_bytes());
            }
        }
        let _ = stdout.flush();
        terminal.clear()?;
        terminal.draw(|frame| {
            cursor_pos = ui::draw(frame, app, ui);
        })?;
        ui.pending_pixel_graphics.clear();
    }

    if let Some((cx, cy)) = cursor_pos {
        terminal.show_cursor()?;
        terminal.set_cursor_position((cx, cy))?;
    } else {
        terminal.hide_cursor()?;
    }

    if plan.emit_desired {
        for (graphic, payload) in desired.iter().zip(emitted_payloads.iter()) {
            let move_seq = format!("\x1b[{};{}H", graphic.rect.y + 1, graphic.rect.x + 1);
            let _ = stdout.write_all(move_seq.as_bytes());
            let _ = stdout.write_all(payload.as_bytes());
            if graphic.protocol == TerminalImageProtocol::Kitty {
                guard.kitty_used = true;
                KITTY_USED_GLOBAL.store(true, Ordering::Relaxed);
            }
        }
    } else if plan.delete_kitty {
        guard.kitty_used = false;
    }

    guard.pixel_tracker.commit(desired, term_size);

    let _ = execute!(stdout, EndSynchronizedUpdate);
    let _ = stdout.flush();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::DriveItem;
    use crate::ui::overlays::image_preview::ImagePreviewOverlay;

    fn sample_graphic(
        key: &str,
        rect: Rect,
        protocol: TerminalImageProtocol,
        payload_len: usize,
    ) -> ActivePixelGraphic {
        ActivePixelGraphic {
            cache_key: key.to_string(),
            rect,
            protocol,
            cols: rect.width,
            rows: rect.height,
            payload_len,
        }
    }

    #[test]
    fn test_pixel_tracker_image_to_non_image_triggers_erase_and_redraw() {
        let mut tracker = PixelLayerTracker::default();
        let img_a = sample_graphic(
            "inline:d1:i1",
            Rect::new(60, 10, 30, 12),
            TerminalImageProtocol::Sixel,
            4096,
        );
        tracker.commit(vec![img_a.clone()], (120, 36));

        let plan = tracker.plan_transition(&[], (120, 36), false);
        assert!(plan.erase_and_redraw);
        assert!(!plan.delete_kitty);
        assert_eq!(plan.erase_rects, vec![img_a.rect]);
        assert!(!plan.emit_desired);
    }

    #[test]
    fn test_pixel_tracker_image_a_to_image_b_erases_and_emits_b() {
        let mut tracker = PixelLayerTracker::default();
        let img_a = sample_graphic(
            "inline:d1:i1",
            Rect::new(60, 10, 30, 12),
            TerminalImageProtocol::Kitty,
            4096,
        );
        let img_b = sample_graphic(
            "inline:d1:i2",
            Rect::new(60, 10, 28, 10),
            TerminalImageProtocol::Kitty,
            3072,
        );
        tracker.commit(vec![img_a.clone()], (120, 36));

        let plan = tracker.plan_transition(&[img_b], (120, 36), true);
        assert!(plan.erase_and_redraw);
        assert!(plan.delete_kitty);
        assert_eq!(plan.erase_rects, vec![img_a.rect]);
        assert!(plan.emit_desired);
    }

    #[test]
    fn test_pixel_tracker_unchanged_spinner_tick_skips_erase_and_reemit() {
        let mut tracker = PixelLayerTracker::default();
        let img_a = sample_graphic(
            "inline:d1:i1",
            Rect::new(60, 10, 30, 12),
            TerminalImageProtocol::Sixel,
            4096,
        );
        tracker.commit(vec![img_a.clone()], (120, 36));

        let plan = tracker.plan_transition(&[img_a.clone()], (120, 36), false);
        assert!(!plan.erase_and_redraw);
        assert!(!plan.delete_kitty);
        assert!(plan.erase_rects.is_empty());
        assert!(!plan.emit_desired);

        // Terminal resize with the same rect still forces erase + redraw + re-emit
        let resize_plan = tracker.plan_transition(&[img_a], (130, 36), false);
        assert!(resize_plan.erase_and_redraw);
        assert!(resize_plan.emit_desired);
    }

    #[test]
    fn test_overlay_z_order_guard_suppresses_pixels_under_non_image_modals() {
        let item = DriveItem {
            id: "img1".into(),
            drive_id: "d1".into(),
            name: "photo.png".into(),
            mime_type: Some("image/png".into()),
            ..Default::default()
        };
        let modal = ImagePreviewOverlay::from_folder_items(&[item], Some("img1")).unwrap();

        // No overlays: inline allowed
        assert!(pixel_graphics_allowed_for_overlays(&[], "inline:d1:img1"));

        // ImagePreview alone: modal allowed, inline blocked
        let only_img = vec![Overlay::ImagePreview(modal.clone())];
        assert!(pixel_graphics_allowed_for_overlays(
            &only_img,
            "modal:d1:img1:80x24:sixel"
        ));
        assert!(!pixel_graphics_allowed_for_overlays(
            &only_img,
            "inline:d1:img1"
        ));

        // CommandPalette over inline: blocked -> desired becomes [] and triggers erase
        let cmd_palette = vec![Overlay::command_palette()];
        assert!(!pixel_graphics_allowed_for_overlays(
            &cmd_palette,
            "inline:d1:img1"
        ));

        // ConfirmDialog stacked on top of ImagePreviewOverlay: BOTH modal and inline blocked
        let stacked = vec![
            Overlay::ImagePreview(modal),
            Overlay::confirm("Quit SharePoint TUI?", None),
        ];
        assert!(!pixel_graphics_allowed_for_overlays(
            &stacked,
            "modal:d1:img1:80x24:sixel"
        ));
        assert!(!pixel_graphics_allowed_for_overlays(
            &stacked,
            "inline:d1:img1"
        ));

        // Verify tracker transitions from active image -> [] with erase_and_redraw
        let mut tracker = PixelLayerTracker::default();
        let active = sample_graphic(
            "modal:d1:img1:80x24:sixel",
            Rect::new(4, 3, 80, 24),
            TerminalImageProtocol::Sixel,
            8192,
        );
        tracker.commit(vec![active.clone()], (120, 36));
        let plan = tracker.plan_transition(&[], (120, 36), false);
        assert!(plan.erase_and_redraw);
        assert_eq!(plan.erase_rects, vec![active.rect]);
        assert!(!plan.emit_desired);
    }

    #[test]
    fn test_oversized_inline_image_suppressed_and_requests_exact_redecode() {
        let inspector_area = Rect::new(60, 0, 50, 25);
        // VersionsLoaded pushed img_y to row 18, leaving only 7 rows (y=18..25),
        // while the cached Sixel image is 12 rows tall.
        let pending_rect = Rect::new(61, 18, 40, 7);
        let res = evaluate_pixel_placement(
            "inline:d1:i1",
            pending_rect,
            40,
            12,
            inspector_area,
            0,
            120,
            26,
        );
        // Bottom-row guard reserves row 25 (`term_height - 1`), so safe_max_h = 25 - 18 = 7
        assert_eq!(res, Err(Some((48, 7))));

        // Once re-decoded to fit (40x7), placement succeeds cleanly above bottom status row
        let ok = evaluate_pixel_placement(
            "inline:d1:i1",
            pending_rect,
            40,
            7,
            inspector_area,
            0,
            120,
            26,
        );
        assert_eq!(ok, Ok(Rect::new(61, 18, 40, 7)));

        // Scrolled past the top of inspector body -> suppressed without re-decode
        let scrolled_out = evaluate_pixel_placement(
            "inline:d1:i1",
            Rect::new(61, 10, 40, 7),
            40,
            7,
            inspector_area,
            12,
            120,
            26,
        );
        assert_eq!(scrolled_out, Err(None));

        // Tiny inspector space (< 2 rows) -> suppressed completely
        let too_small = evaluate_pixel_placement(
            "inline:d1:i1",
            Rect::new(61, 24, 40, 1),
            40,
            7,
            inspector_area,
            0,
            120,
            26,
        );
        assert_eq!(too_small, Err(None));
    }
}
