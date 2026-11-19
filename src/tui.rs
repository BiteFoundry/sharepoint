//! TUI entry shell: terminal raw mode, 80ms CSI/DA1/Kitty active graphics probing,
//! pixel graphics overlay flush, mouse capture, event loop, and shared IPC daemon attachment.

use std::io::{self, Stdout, Write};
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
use ratatui::Terminal;
use tokio::sync::mpsc;

use crate::auth::AuthManager;
use crate::config::{AppConfig, Persona};
use crate::graph::GraphClient;
use crate::img_preview::{self, TerminalImageProtocol};
use crate::models::CoreEvent;
use crate::storage::Storage;
use crate::sync::{self, SyncCommand};
use crate::ui::{
    self, action,
    action::Action,
    app::{AppState, UiMsg, UiState},
    event,
};

struct TerminalGuard {
    keyboard_enhanced: bool,
    kitty_used: bool,
}

impl TerminalGuard {
    fn enter() -> Result<(Self, Terminal<CrosstermBackend<Stdout>>)> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
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
            },
            terminal,
        ))
    }

    fn restore(&self) {
        let mut stdout = io::stdout();
        if self.kitty_used {
            let _ = stdout.write_all(img_preview::kitty_delete_preview_sequence().as_bytes());
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
    let (mut guard, mut terminal) = TerminalGuard::enter()?;
    install_panic_hook(guard.keyboard_enhanced);

    // 80ms CSI/DA1/Kitty active graphics probe while raw mode is active, before EventStream!
    let probed_caps = img_preview::probe_terminal_graphics_caps(config.image_protocol);

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
                    CrosstermEvent::Resize(_, _) => {
                        dirty = true;
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

    if guard.kitty_used {
        let _ = stdout.write_all(img_preview::kitty_delete_preview_sequence().as_bytes());
    }

    let mut cursor_pos = None;
    terminal.draw(|frame| {
        cursor_pos = ui::draw(frame, app, ui);
    })?;

    if let Some((cx, cy)) = cursor_pos {
        terminal.show_cursor()?;
        terminal.set_cursor_position((cx, cy))?;
    } else {
        terminal.hide_cursor()?;
    }

    // Flush any Kitty / iTerm2 / Sixel pixel graphic overlays positioned cleanly over cells
    for pending in &ui.pending_pixel_graphics {
        if let Some(rendered) = app.rendered_images.get(&pending.cache_key) {
            if let Some(ref payload) = rendered.pixel_payload {
                let move_seq = format!("\x1b[{};{}H", pending.rect.y + 1, pending.rect.x + 1);
                let _ = stdout.write_all(move_seq.as_bytes());
                let _ = stdout.write_all(payload.as_bytes());
                if pending.protocol == TerminalImageProtocol::Kitty {
                    guard.kitty_used = true;
                }
            }
        }
    }

    let _ = execute!(stdout, EndSynchronizedUpdate);
    let _ = stdout.flush();
    Ok(())
}
