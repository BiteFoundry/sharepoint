//! TUI entry shell: terminal raw mode, 150ms CSI/DA1/Kitty active graphics probing,
//! pixel graphics overlay flush, mouse capture, event loop, and shared IPC daemon attachment.

use std::borrow::Cow;
use std::io::{self, Stdout, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
#[cfg(not(windows))]
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::event::{
    DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture,
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

/// VT DECSET sequences for X10 + button-motion + SGR mouse reporting.
/// `crossterm 0.28` skips ANSI mouse codes on Windows, so writing these explicitly
/// ensures ConPTY / Windows Terminal / xterm.js report mouse wheel events cleanly.
pub(crate) const VT_ENABLE_MOUSE: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";
pub(crate) const VT_DISABLE_MOUSE: &str = "\x1b[?1006l\x1b[?1002l\x1b[?1000l";

pub(crate) fn write_vt_mouse_enable(w: &mut impl io::Write) -> io::Result<()> {
    w.write_all(VT_ENABLE_MOUSE.as_bytes())?;
    w.flush()
}

pub(crate) fn write_vt_mouse_disable(w: &mut impl io::Write) -> io::Result<()> {
    w.write_all(VT_DISABLE_MOUSE.as_bytes())?;
    w.flush()
}

#[cfg(windows)]
mod win_console {
    use std::ptr::null_mut;

    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const OPEN_EXISTING: u32 = 3;
    const INVALID_HANDLE_VALUE: isize = -1;
    const STD_INPUT_HANDLE: u32 = 0xFFFF_FFF6;

    const ENABLE_WINDOW_INPUT: u32 = 0x0008;
    const ENABLE_MOUSE_INPUT: u32 = 0x0010;
    pub const ENABLE_EXTENDED_FLAGS: u32 = 0x0080;

    /// Raw-mode + ConPTY mouse capture console input mode (`0x0098`):
    /// enables `ENABLE_WINDOW_INPUT | ENABLE_MOUSE_INPUT | ENABLE_EXTENDED_FLAGS`,
    /// explicitly clearing `ENABLE_QUICK_EDIT_MODE`, `ENABLE_VIRTUAL_TERMINAL_INPUT`,
    /// `ENABLE_LINE_INPUT`, `ENABLE_ECHO_INPUT`, and `ENABLE_PROCESSED_INPUT`.
    pub const TUI_MOUSE_CAPTURE_MODE: u32 =
        ENABLE_WINDOW_INPUT | ENABLE_MOUSE_INPUT | ENABLE_EXTENDED_FLAGS;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateFileW(
            lp_file_name: *const u16,
            dw_desired_access: u32,
            dw_share_mode: u32,
            lp_security_attributes: *mut std::ffi::c_void,
            dw_creation_disposition: u32,
            dw_flags_and_attributes: u32,
            h_template_file: isize,
        ) -> isize;
        fn CloseHandle(h_object: isize) -> i32;
        fn GetStdHandle(n_std_handle: u32) -> isize;
        fn GetConsoleMode(h_console_handle: isize, lp_mode: *mut u32) -> i32;
        fn SetConsoleMode(h_console_handle: isize, dw_mode: u32) -> i32;
    }

    fn with_conin_handle<T>(f: impl FnOnce(isize) -> Option<T>) -> Option<T> {
        const CONIN_WIDE: [u16; 7] = [
            b'C' as u16,
            b'O' as u16,
            b'N' as u16,
            b'I' as u16,
            b'N' as u16,
            b'$' as u16,
            0,
        ];
        let conin = unsafe {
            CreateFileW(
                CONIN_WIDE.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null_mut(),
                OPEN_EXISTING,
                0,
                0,
            )
        };
        if conin != 0 && conin != INVALID_HANDLE_VALUE {
            let res = f(conin);
            unsafe {
                CloseHandle(conin);
            }
            return res;
        }
        let std_in = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        if std_in != 0 && std_in != INVALID_HANDLE_VALUE {
            f(std_in)
        } else {
            None
        }
    }

    pub fn snapshot_console_mode() -> Option<u32> {
        with_conin_handle(|h| {
            let mut mode = 0u32;
            if unsafe { GetConsoleMode(h, &mut mode) } != 0 {
                Some(mode)
            } else {
                None
            }
        })
    }

    pub fn enforce_windows_mouse_capture() {
        let _ = with_conin_handle(|h| {
            unsafe {
                SetConsoleMode(h, TUI_MOUSE_CAPTURE_MODE);
            }
            Some(())
        });
    }

    pub fn restore_console_mode(orig_mode: Option<u32>) {
        if let Some(mode) = orig_mode {
            let _ = with_conin_handle(|h| {
                unsafe {
                    SetConsoleMode(h, mode | ENABLE_EXTENDED_FLAGS);
                }
                Some(())
            });
        }
    }
}

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

        // Avoid full `terminal.clear()` flicker when only the `#crop:...` viewport changed
        // on the exact same rect and base image for Sixel without a terminal resize.
        let sixel_crop_only = !term_resized
            && self.active.len() == 1
            && desired.len() == 1
            && self.active[0].protocol == TerminalImageProtocol::Sixel
            && desired[0].protocol == TerminalImageProtocol::Sixel
            && self.active[0].rect == desired[0].rect
            && parse_pixel_key(&self.active[0].cache_key).0
                == parse_pixel_key(&desired[0].cache_key).0;

        if sixel_crop_only {
            return PixelReconcilePlan {
                erase_and_redraw: false,
                delete_kitty: false,
                erase_rects: Vec::new(),
                emit_desired: true,
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
        let header_rows = crate::ui::drive_view::inspector_header_rows(inspector_area);
        let body_y = inspector_area.y.saturating_add(header_rows);
        let body_h = inspector_area.height.saturating_sub(header_rows);
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
    #[cfg(windows)]
    orig_conin_mode: Option<u32>,
}

impl TerminalGuard {
    fn enter(
        image_pref: ImageProtocolPreference,
    ) -> Result<(Self, Terminal<CrosstermBackend<Stdout>>, TerminalGraphicsCaps)> {
        #[cfg(windows)]
        let orig_conin_mode = win_console::snapshot_console_mode();

        enable_raw_mode()?;
        let mut stdout = io::stdout();
        // Probe graphics & cell pixel size while raw mode is active and BEFORE
        // EnterAlternateScreen / EnableMouseCapture / EnableBracketedPaste so mouse
        // movement or console-mode swaps never race with CSI/DA1/Kitty replies.
        let probed_caps = img_preview::probe_terminal_graphics_caps(image_pref);

        #[cfg(not(windows))]
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste,
            EnableFocusChange
        )?;
        #[cfg(windows)]
        {
            execute!(stdout, EnterAlternateScreen, EnableMouseCapture, EnableFocusChange)?;
            write_vt_mouse_enable(&mut stdout)?;
        }
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

        #[cfg(windows)]
        {
            win_console::enforce_windows_mouse_capture();
            let _ = write_vt_mouse_enable(&mut stdout);
        }

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
                #[cfg(windows)]
                orig_conin_mode,
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
        #[cfg(not(windows))]
        let _ = execute!(
            stdout,
            DisableFocusChange,
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        #[cfg(windows)]
        {
            let _ = write_vt_mouse_disable(&mut stdout);
            let _ = execute!(stdout, DisableFocusChange, DisableMouseCapture, LeaveAlternateScreen);
        }
        let _ = disable_raw_mode();
        #[cfg(windows)]
        win_console::restore_console_mode(self.orig_conin_mode);
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

fn install_panic_hook(keyboard_enhanced: bool, orig_conin_mode: Option<u32>) {
    #[cfg(not(windows))]
    let _ = orig_conin_mode;
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
        #[cfg(not(windows))]
        let _ = execute!(
            stdout,
            DisableFocusChange,
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        #[cfg(windows)]
        {
            let _ = write_vt_mouse_disable(&mut stdout);
            let _ = execute!(stdout, DisableFocusChange, DisableMouseCapture, LeaveAlternateScreen);
        }
        let _ = disable_raw_mode();
        #[cfg(windows)]
        win_console::restore_console_mode(orig_conin_mode);
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
    #[cfg(windows)]
    let orig_conin_mode = guard.orig_conin_mode;
    #[cfg(not(windows))]
    let orig_conin_mode = None;
    install_panic_hook(guard.keyboard_enhanced, orig_conin_mode);

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
                        #[cfg(windows)]
                        {
                            drop(events);
                            img_preview::refresh_cell_pixel_size(&mut ui.graphics_caps);
                            win_console::enforce_windows_mouse_capture();
                            let _ = write_vt_mouse_enable(&mut io::stdout());
                            events = EventStream::new();
                        }
                        #[cfg(not(windows))]
                        {
                            img_preview::refresh_cell_pixel_size(&mut ui.graphics_caps);
                        }
                        app.rendered_images.retain(|k, _| !k.starts_with("inline:"));
                        dirty = true;
                        if matches!(ui.overlays.last(), Some(Overlay::ImagePreview(_))) {
                            Some(Action::LoadPreviewImage)
                        } else {
                            app.ensure_inspector_loaded(&ui);
                            None
                        }
                    }
                    CrosstermEvent::FocusGained => {
                        app.sync_ctrl.request(SyncCommand::FocusGained);
                        None
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
    let body_h = crate::ui::drive_view::inspector_body_height(ui.inspector_scroll.area);
    let max_off = ui.inspector_scroll.total_lines.saturating_sub(body_h);
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
                let bg_rgb = img_preview::color_to_rgb(
                    ui.theme.bg_canvas,
                    img_preview::color_to_rgb(ui.theme.bg_elevated, (18, 20, 26)),
                );
                app.request_image_decode(
                    &item,
                    cache_key,
                    img_preview::FitBox::fixed(fit_w, fit_h),
                    bg_rgb,
                    ui.graphics_caps,
                );
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

    #[cfg(windows)]
    #[test]
    fn test_windows_mouse_capture_mode_bits() {
        // 0x0098 = ENABLE_WINDOW_INPUT (0x0008) | ENABLE_MOUSE_INPUT (0x0010) | ENABLE_EXTENDED_FLAGS (0x0080)
        assert_eq!(win_console::TUI_MOUSE_CAPTURE_MODE, 0x0098);
        assert_eq!(win_console::TUI_MOUSE_CAPTURE_MODE & 0x0040, 0); // QuickEdit cleared
        assert_eq!(win_console::TUI_MOUSE_CAPTURE_MODE & 0x0200, 0); // VTInput cleared
    }

    #[test]
    fn sixel_crop_only_skips_erase_and_redraw() {
        let mut tracker = PixelLayerTracker::default();
        let rect = Rect::new(10, 3, 80, 24);
        let top = sample_graphic(
            "modal:d1:i1:rev1:100x30:100%:10x20:sixel#crop:0,0,80,24",
            rect,
            TerminalImageProtocol::Sixel,
            12_000,
        );
        let scrolled = sample_graphic(
            "modal:d1:i1:rev1:100x30:100%:10x20:sixel#crop:0,4,80,24",
            rect,
            TerminalImageProtocol::Sixel,
            12_500,
        );
        tracker.commit(vec![top], (120, 36));

        // Same base image, same rect, Sixel, no terminal resize -> flicker-free re-emit without erase/clear
        let plan = tracker.plan_transition(std::slice::from_ref(&scrolled), (120, 36), false);
        assert!(!plan.erase_and_redraw);
        assert!(!plan.delete_kitty);
        assert!(plan.erase_rects.is_empty());
        assert!(plan.emit_desired);

        // Terminal resize still forces full erase + redraw
        let resized = tracker.plan_transition(std::slice::from_ref(&scrolled), (124, 36), false);
        assert!(resized.erase_and_redraw);
        assert!(resized.emit_desired);

        // Different rect forces erase + redraw
        let moved = sample_graphic(
            "modal:d1:i1:rev1:100x30:100%:10x20:sixel#crop:0,4,80,24",
            Rect::new(12, 3, 80, 24),
            TerminalImageProtocol::Sixel,
            12_500,
        );
        let moved_plan = tracker.plan_transition(&[moved], (120, 36), false);
        assert!(moved_plan.erase_and_redraw);

        // Different base image forces erase + redraw
        let other_img = sample_graphic(
            "modal:d1:i2:rev1:100x30:100%:10x20:sixel#crop:0,0,80,24",
            rect,
            TerminalImageProtocol::Sixel,
            12_500,
        );
        let other_plan = tracker.plan_transition(&[other_img], (120, 36), false);
        assert!(other_plan.erase_and_redraw);
    }

    #[test]
    fn test_vt_mouse_enable_and_disable_ansi_sequences() {
        let mut enable_buf = Vec::new();
        write_vt_mouse_enable(&mut enable_buf).unwrap();
        assert_eq!(
            String::from_utf8(enable_buf).unwrap(),
            "\x1b[?1000h\x1b[?1002h\x1b[?1006h"
        );

        let mut disable_buf = Vec::new();
        write_vt_mouse_disable(&mut disable_buf).unwrap();
        assert_eq!(
            String::from_utf8(disable_buf).unwrap(),
            "\x1b[?1006l\x1b[?1002l\x1b[?1000l"
        );
    }
}
