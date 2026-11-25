//! Full-screen interactive HTML viewer overlay (`v` on `.html`/`.htm`/`.xhtml` or auto-routed from HTML responses).
//!
//! Backed by `bitefoundry_html::HtmlViewSession`:
//! - `0 MB` idle JS memory on static HTML
//! - On-demand `8 MiB` sandboxed QuickJS-NG when `<script>` / `on*` handlers exist, dropped on close
//! - `Tab` / `Shift+Tab` focus cycling, `Enter` / `Space` activation, mouse click via `click_at(line, col)`
//! - Inline text input typing, `m` Rendered ↔ Raw toggle, `o` lazy localhost Axum serving, `j`/`k`/wheel scroll + `Scrollbar`

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use bitefoundry_html::{HtmlEffect, HtmlTheme, HtmlViewSession};
use ratatui::{
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState,
    },
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::img_preview::{self, FitBox, RenderedTerminalImage, TerminalImageProtocol};
use crate::ui::{
    action::Action,
    app::{AppState, PendingPixelGraphic, UiState},
    hitmap::{ButtonId, HitMap, HitTarget},
    overlays::{centered_rect, OverlayOutcome},
    theme::Theme,
    truncate_width,
};

pub struct HtmlPreviewOverlay {
    pub drive_id: Option<String>,
    pub item_id: Option<String>,
    pub revision: Option<String>,
    pub title: String,
    pub session: HtmlViewSession,
    pub viewport: Rect,
    pub max_scroll_y: u16,
    /// Per-overlay inline graphic cache keyed by `(node_id, width, protocol)` (dropped on close).
    graphic_cache: HashMap<(usize, u16, TerminalImageProtocol), (Arc<[u8]>, RenderedTerminalImage)>,
    /// Tracks `(bg_rgb, cell_px)` so theme or DPI changes invalidate cached graphics.
    graphic_cache_env: Option<((u8, u8, u8), (u16, u16))>,
}

impl fmt::Debug for HtmlPreviewOverlay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HtmlPreviewOverlay")
            .field("drive_id", &self.drive_id)
            .field("item_id", &self.item_id)
            .field("revision", &self.revision)
            .field("title", &self.title)
            .field("raw_mode", &self.session.raw_mode)
            .field("scroll_y", &self.session.scroll_y)
            .field("js_active", &self.session.js_active)
            .field("focused_control", &self.session.focused_control)
            .field("editing_input", &self.session.editing_input)
            .finish()
    }
}

impl HtmlPreviewOverlay {
    pub fn new(title: impl Into<String>, raw_html: String, item_id: Option<String>) -> Self {
        let title_str = title.into();
        let session = HtmlViewSession::new(Some(title_str.clone()), raw_html);
        Self {
            drive_id: None,
            item_id,
            revision: None,
            title: title_str,
            session,
            viewport: Rect::default(),
            max_scroll_y: 0,
            graphic_cache: HashMap::new(),
            graphic_cache_env: None,
        }
    }

    pub fn with_source(mut self, drive_id: impl Into<String>, revision: impl Into<String>) -> Self {
        self.drive_id = Some(drive_id.into());
        self.revision = Some(revision.into());
        self
    }

    pub fn replace_html(
        &mut self,
        title: impl Into<String>,
        raw_html: String,
        revision: Option<String>,
    ) {
        let raw_mode = self.session.raw_mode;
        let title_str = title.into();
        let mut session = HtmlViewSession::new(Some(title_str.clone()), raw_html);
        session.raw_mode = raw_mode;
        self.title = title_str;
        self.session = session;
        self.graphic_cache.clear();
        self.graphic_cache_env = None;
        if revision.is_some() {
            self.revision = revision;
        }
    }

    pub fn is_editing_input(&self) -> bool {
        self.session.editing_input.is_some()
    }

    pub fn modal_rect(area: Rect) -> Rect {
        centered_rect(
            area,
            area.width.saturating_sub(4).max(24).min(area.width),
            area.height.saturating_sub(2).max(10).min(area.height),
        )
    }

    pub fn html_theme(theme: &Theme) -> HtmlTheme {
        if theme.no_color {
            return HtmlTheme::no_color();
        }
        let is_light = matches!(theme.bg_elevated, Color::Rgb(r, g, b) if r > 180 && g > 180 && b > 180);
        let focus_fg = if theme.ansi16_only {
            Color::Black
        } else if is_light {
            Color::Rgb(255, 255, 255)
        } else {
            Color::Rgb(15, 20, 28)
        };
        HtmlTheme {
            text: theme.fg_primary,
            muted: theme.fg_muted,
            heading: theme.accent,
            accent: theme.accent_soft,
            link: theme.accent,
            code_fg: theme.code_fg,
            code_bg: theme.bg_code,
            quote_border: theme.fg_subtle,
            border: theme.border,
            control_fg: theme.fg_primary,
            control_bg: theme.bg_reaction_me,
            focus_fg,
            focus_bg: theme.accent,
            raw_tag: theme.accent,
            raw_attr: theme.code_fg,
        }
    }

    fn apply_effects(&mut self, effects: Vec<HtmlEffect>) -> OverlayOutcome {
        let mut toast_msg: Option<String> = None;
        for effect in effects {
            match effect {
                HtmlEffect::OpenUrl(url) => {
                    return OverlayOutcome::Dispatch(Action::OpenUrl(url));
                }
                HtmlEffect::FormSubmitted { action, fields } => {
                    let pairs: Vec<String> = fields
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect();
                    let target = if action.trim().is_empty() {
                        "(current document)"
                    } else {
                        action.as_str()
                    };
                    let msg = format!("Form submitted -> {target} ({})", pairs.join(", "));
                    self.session.last_status = Some(msg.clone());
                    toast_msg = Some(msg);
                }
                HtmlEffect::Toast(msg) => {
                    self.session.last_status = Some(msg.clone());
                    toast_msg = Some(msg);
                }
                HtmlEffect::DomChanged | HtmlEffect::None => {}
            }
        }
        if let Some(msg) = toast_msg {
            OverlayOutcome::Dispatch(Action::ShowToast(msg))
        } else {
            OverlayOutcome::Handled
        }
    }

    fn ensure_focused_visible(&mut self, ui: &UiState) {
        let Some(focus_idx) = self.session.focused_control else {
            return;
        };
        let width = if self.viewport.width >= 12 {
            self.viewport.width
        } else {
            ui.term_width.saturating_sub(8).max(20)
        };
        let html_theme = Self::html_theme(&ui.theme);
        let rendered = self.session.render(width, &html_theme, ui.show_link_urls);
        let Some(ctrl) = rendered.controls.get(focus_idx) else {
            return;
        };
        let line = ctrl.line.min(u16::MAX as usize) as u16;
        let vh = self.viewport.height.max(1);
        if line < self.session.scroll_y {
            self.session.scroll_y = line;
        } else if line >= self.session.scroll_y.saturating_add(vh) {
            self.session.scroll_y = line.saturating_sub(vh.saturating_sub(1));
        }
    }

    pub fn handle_click_at(&mut self, column: u16, row: u16, ui: &UiState) -> OverlayOutcome {
        if !self.viewport.contains(Position::new(column, row)) {
            return OverlayOutcome::Handled;
        }
        let html_theme = Self::html_theme(&ui.theme);
        let _ = self
            .session
            .render(self.viewport.width.max(12), &html_theme, ui.show_link_urls);
        let line_index = (row - self.viewport.y) as usize + self.session.scroll_y as usize;
        let col = column - self.viewport.x;
        let effects = self.session.click_at(line_index, col);
        self.apply_effects(effects)
    }

    pub fn handle(&mut self, action: &Action, ui: &UiState) -> OverlayOutcome {
        if self.session.editing_input.is_some() {
            return match action {
                Action::Quit
                | Action::QuitConfirm
                | Action::ServeHtmlInBrowser { .. }
                | Action::OpenUrl(_)
                | Action::ShowToast(_) => OverlayOutcome::NotHandled,
                Action::CloseOverlay | Action::Activate | Action::OverlaySubmit => {
                    let eff = self.session.finish_editing();
                    self.apply_effects(eff)
                }
                Action::Backspace => {
                    self.session.backspace();
                    OverlayOutcome::Handled
                }
                Action::Input(ch) => {
                    self.session.input_char(*ch);
                    OverlayOutcome::Handled
                }
                Action::Paste(text) => {
                    for ch in text.replace(['\r', '\n'], " ").chars() {
                        if !ch.is_control() {
                            self.session.input_char(ch);
                        }
                    }
                    OverlayOutcome::Handled
                }
                Action::Tab => {
                    let eff = self.session.finish_editing();
                    self.session.focus_next_control();
                    self.ensure_focused_visible(ui);
                    self.apply_effects(eff)
                }
                Action::PrevFocus => {
                    let eff = self.session.finish_editing();
                    self.session.focus_prev_control();
                    self.ensure_focused_visible(ui);
                    self.apply_effects(eff)
                }
                _ => OverlayOutcome::Handled,
            };
        }

        let clamp_scroll = |val: u16, vp_h: u16, max_y: u16| -> u16 {
            if vp_h > 0 {
                val.min(max_y)
            } else {
                val
            }
        };

        match action {
            Action::CloseOverlay | Action::Input('q') => OverlayOutcome::Close,
            Action::Quit
            | Action::QuitConfirm
            | Action::ServeHtmlInBrowser { .. }
            | Action::OpenUrl(_)
            | Action::ShowToast(_) => OverlayOutcome::NotHandled,
            Action::Tab => {
                self.session.focus_next_control();
                self.ensure_focused_visible(ui);
                OverlayOutcome::Handled
            }
            Action::PrevFocus => {
                self.session.focus_prev_control();
                self.ensure_focused_visible(ui);
                OverlayOutcome::Handled
            }
            Action::Activate | Action::OverlaySubmit | Action::Input(' ') => {
                let width = if self.viewport.width >= 12 {
                    self.viewport.width
                } else {
                    ui.term_width.saturating_sub(8).max(20)
                };
                let html_theme = Self::html_theme(&ui.theme);
                let _ = self.session.render(width, &html_theme, ui.show_link_urls);
                let effects = self.session.activate_focused();
                self.apply_effects(effects)
            }
            Action::Input('m') | Action::TogglePreviewRenderMode => {
                self.session.toggle_raw_mode();
                self.session.scroll_y = 0;
                OverlayOutcome::Handled
            }
            Action::Input('o')
            | Action::Input('O')
            | Action::OpenActiveWebUrl
            | Action::ServeActiveHtml => OverlayOutcome::Dispatch(Action::ServeHtmlInBrowser {
                title: self.title.clone(),
                html: self.session.raw_html().to_string(),
            }),
            Action::MoveUp | Action::Input('k') => {
                self.session.scroll_y = self.session.scroll_y.saturating_sub(2);
                OverlayOutcome::Handled
            }
            Action::MoveDown | Action::Input('j') => {
                let next = self.session.scroll_y.saturating_add(2);
                self.session.scroll_y = clamp_scroll(next, self.viewport.height, self.max_scroll_y);
                OverlayOutcome::Handled
            }
            Action::ScrollPageUp => {
                let step = self.viewport.height.saturating_sub(2).max(6);
                self.session.scroll_y = self.session.scroll_y.saturating_sub(step);
                OverlayOutcome::Handled
            }
            Action::ScrollPageDown => {
                let step = self.viewport.height.saturating_sub(2).max(6);
                let next = self.session.scroll_y.saturating_add(step);
                self.session.scroll_y = clamp_scroll(next, self.viewport.height, self.max_scroll_y);
                OverlayOutcome::Handled
            }
            Action::CursorHome | Action::ScrollTop | Action::Input('g') => {
                self.session.scroll_y = 0;
                OverlayOutcome::Handled
            }
            Action::CursorEnd | Action::ScrollBottom | Action::Input('G') => {
                self.session.scroll_y = if self.viewport.height > 0 {
                    self.max_scroll_y
                } else {
                    u16::MAX
                };
                OverlayOutcome::Handled
            }
            Action::MouseScroll { delta, .. } => {
                if *delta < 0 {
                    self.session.scroll_y =
                        self.session.scroll_y.saturating_sub((-*delta) as u16);
                } else {
                    let next = self.session.scroll_y.saturating_add(*delta as u16);
                    self.session.scroll_y =
                        clamp_scroll(next, self.viewport.height, self.max_scroll_y);
                }
                OverlayOutcome::Handled
            }
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        _app: &AppState,
        ui: &mut UiState,
        hitmap: &mut HitMap,
    ) {
        let theme = ui.theme.clone();
        let rect = Self::modal_rect(area);
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let mode_pill = if self.session.raw_mode {
            "[m: Raw]"
        } else {
            "[m: Rendered]"
        };
        let serve_pill = "[🌐 Serve (o)]";
        let close_pill = "[✕ Esc]";

        let mode_w = mode_pill.width() as u16;
        let serve_w = serve_pill.width() as u16;
        let close_w = close_pill.width() as u16;

        let show_close = rect.width > close_w + 6;
        let show_serve = show_close && rect.width > close_w + serve_w + 14;
        let show_mode = show_serve && rect.width > close_w + serve_w + mode_w + 22;

        let right_pills_w = if show_mode {
            mode_w + serve_w + close_w + 4
        } else if show_serve {
            serve_w + close_w + 3
        } else if show_close {
            close_w + 2
        } else {
            0
        };

        let js_badge = if self.session.js_active {
            "⚡ JS"
        } else {
            "Static"
        };
        let title_budget = rect.width.saturating_sub(2 + right_pills_w) as usize;
        let title = format!(" HTML: {} · {js_badge} ", self.title);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                truncate_width(&title, title_budget),
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);

        let pill_style = Style::default().fg(theme.fg).bg(theme.bg_elevated);
        let accent_pill = theme.reaction_style(true);
        let mut cursor_right = rect.right().saturating_sub(1);
        if show_close {
            let x = cursor_right.saturating_sub(close_w);
            frame.render_widget(
                Paragraph::new(Span::styled(close_pill, pill_style)),
                Rect::new(x, rect.y, close_w, 1),
            );
            hitmap.add_line(x, rect.y, close_w, HitTarget::Button(ButtonId::ConfirmCancel));
            cursor_right = x.saturating_sub(1);
        }
        if show_serve {
            let x = cursor_right.saturating_sub(serve_w);
            frame.render_widget(
                Paragraph::new(Span::styled(serve_pill, accent_pill)),
                Rect::new(x, rect.y, serve_w, 1),
            );
            hitmap.add_line(x, rect.y, serve_w, HitTarget::Button(ButtonId::ServeHtml));
            cursor_right = x.saturating_sub(1);
        }
        if show_mode {
            let x = cursor_right.saturating_sub(mode_w);
            frame.render_widget(
                Paragraph::new(Span::styled(mode_pill, pill_style)),
                Rect::new(x, rect.y, mode_w, 1),
            );
            hitmap.add_line(
                x,
                rect.y,
                mode_w,
                HitTarget::Button(ButtonId::PreviewRenderToggle),
            );
        }

        if inner.width == 0 || inner.height == 0 {
            self.viewport = Rect::default();
            return;
        }

        let has_footer = inner.height >= 3;
        let content_h = if has_footer {
            inner.height.saturating_sub(1)
        } else {
            inner.height
        };

        let html_theme = Self::html_theme(&theme);
        let initial_len = self
            .session
            .render(inner.width.max(12), &html_theme, ui.show_link_urls)
            .lines
            .len();
        let show_scrollbar = initial_len > content_h as usize && inner.width > 2;
        let avail_w = if show_scrollbar {
            inner.width.saturating_sub(1)
        } else {
            inner.width
        };

        let rendered = self
            .session
            .render(avail_w.max(12), &html_theme, ui.show_link_urls);
        let total_lines = rendered.lines.len();
        let control_count = rendered.controls.len();
        let mut lines = rendered.lines.clone();
        let graphics = rendered.graphics.clone();

        self.viewport = Rect::new(inner.x, inner.y, avail_w, content_h);
        self.max_scroll_y = total_lines
            .saturating_sub(content_h as usize)
            .min(u16::MAX as usize) as u16;
        self.session.scroll_y = self.session.scroll_y.min(self.max_scroll_y);

        let mut pixel_placements: Vec<(String, Rect, TerminalImageProtocol, String)> = Vec::new();
        if !self.session.raw_mode && !graphics.is_empty() && avail_w > 0 && content_h > 0 {
            let bg_rgb = img_preview::color_to_rgb(theme.bg_elevated, (18, 20, 26));
            let caps = ui.graphics_caps;
            if self.graphic_cache_env != Some((bg_rgb, caps.cell_px)) {
                self.graphic_cache.clear();
                self.graphic_cache_env = Some((bg_rgb, caps.cell_px));
            }

            let view_top = self.session.scroll_y as usize;
            let view_bottom = view_top.saturating_add(content_h as usize);
            let max_safe_y = area.bottom().saturating_sub(1);

            for g in &graphics {
                if g.line_count == 0 {
                    continue;
                }
                let g_start = g.line_start;
                let g_end = g.line_start.saturating_add(g.line_count);
                if g_end <= view_top || g_start >= view_bottom {
                    continue;
                }

                let cache_key = (g.node_id.0, avail_w, caps.protocol);
                let needs_decode = self
                    .graphic_cache
                    .get(&cache_key)
                    .is_none_or(|(src, _)| !Arc::ptr_eq(src, &g.image_bytes));
                if needs_decode {
                    let fit = FitBox::fixed(avail_w, (g.line_count as u16).max(1));
                    if let Ok(img) = img_preview::decode_and_render_for_protocol(
                        &g.image_bytes,
                        fit,
                        bg_rgb,
                        caps,
                    ) {
                        self.graphic_cache
                            .insert(cache_key, (Arc::clone(&g.image_bytes), img));
                    }
                }
                let Some((_, rendered_img)) = self.graphic_cache.get(&cache_key) else {
                    continue;
                };

                if !rendered_img.protocol.is_pixel() {
                    // Halfblock: replace reserved lines in-place so exact line indices and
                    // click coordinates for controls below never shift.
                    for (i, hb_line) in rendered_img.lines.iter().take(g.line_count).enumerate() {
                        let target_idx = g_start + i;
                        if target_idx < lines.len() {
                            lines[target_idx] = hb_line.clone();
                        }
                    }
                } else {
                    let img_rows = (rendered_img.rows as usize).min(g.line_count);
                    if img_rows == 0 || rendered_img.cols == 0 {
                        continue;
                    }
                    let img_end = g_start.saturating_add(img_rows);
                    let vis_start = g_start.max(view_top);
                    let vis_end = img_end.min(view_bottom);
                    if vis_end <= vis_start {
                        continue;
                    }
                    let crop_y = (vis_start - g_start) as u16;
                    let vis_rows = (vis_end - vis_start) as u16;
                    let vis_cols = rendered_img.cols.min(avail_w);
                    let screen_y = self
                        .viewport
                        .y
                        .saturating_add((vis_start - view_top) as u16);
                    if screen_y >= max_safe_y {
                        continue;
                    }
                    let safe_vis_rows = vis_rows.min(max_safe_y.saturating_sub(screen_y));
                    if safe_vis_rows == 0 || vis_cols == 0 {
                        continue;
                    }

                    if let Some((mut payload, draw_cols, draw_rows)) =
                        rendered_img.viewport_pixel_payload(0, crop_y, vis_cols, safe_vis_rows)
                    {
                        if rendered_img.protocol == TerminalImageProtocol::Kitty {
                            let kitty_id = img_preview::KITTY_PREVIEW_IMAGE_ID
                                .saturating_add(1)
                                .saturating_add((g.node_id.0 as u32) % 10_000);
                            let old_tag =
                                format!(",i={},", img_preview::KITTY_PREVIEW_IMAGE_ID);
                            let new_tag = format!(",i={kitty_id},");
                            if let Some(pos) = payload.find(&old_tag) {
                                payload.replace_range(pos..pos + old_tag.len(), &new_tag);
                            }
                        }
                        let draw_rect = Rect::new(self.viewport.x, screen_y, draw_cols, draw_rows);
                        pixel_placements.push((
                            format!(
                                "html:{}:{}:{}:{}:{}",
                                g.node_id.0,
                                crop_y,
                                draw_cols,
                                draw_rows,
                                rendered_img.protocol.as_str()
                            ),
                            draw_rect,
                            rendered_img.protocol,
                            payload,
                        ));
                    }
                }
            }
        }

        frame.render_widget(
            Paragraph::new(lines).scroll((self.session.scroll_y, 0)),
            self.viewport,
        );

        for (cache_key, draw_rect, protocol, payload) in pixel_placements {
            let buf = frame.buffer_mut();
            let x_end = draw_rect.right().min(buf.area.right());
            let y_end = draw_rect.bottom().min(buf.area.bottom());
            for y in draw_rect.y..y_end {
                for x in draw_rect.x..x_end {
                    if let Some(cell) = buf.cell_mut((x, y)) {
                        cell.set_skip(true);
                    }
                }
            }
            ui.pending_pixel_graphics.push(PendingPixelGraphic {
                cache_key,
                rect: draw_rect,
                protocol,
                payload: Some(payload),
            });
        }

        if show_scrollbar {
            let sb_area = Rect::new(inner.right().saturating_sub(1), inner.y, 1, content_h);
            let mut sb_state =
                ScrollbarState::new(self.max_scroll_y.saturating_add(1) as usize)
                    .position(self.session.scroll_y as usize);
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .thumb_style(Style::default().fg(theme.accent))
                .track_style(theme.muted_style());
            frame.render_stateful_widget(scrollbar, sb_area, &mut sb_state);
        }

        if has_footer {
            let footer_y = inner.y + content_h;
            let status_prefix = if let Some(ref st) = self.session.last_status {
                format!(" {st}  |  ")
            } else if self.session.editing_input.is_some() {
                " Editing input (Enter/Esc to finish)  |  ".to_string()
            } else if let Some(idx) = self.session.focused_control {
                format!(" Control {}/{}  |  ", idx + 1, control_count.max(1))
            } else {
                format!(" {} controls  |  ", control_count)
            };
            let hints = if self.session.raw_mode {
                "Raw HTML · m Rendered · o Serve · j/k/Wheel Scroll · Esc Close"
            } else {
                "Tab Focus · Enter/Space/Click Activate · m Raw · o Serve · j/k Scroll · Esc Close"
            };
            let footer_text = format!("{status_prefix}{hints}");
            let footer_style = if theme.no_color {
                Style::default().add_modifier(Modifier::DIM)
            } else {
                Style::default().bg(theme.bg_header).fg(theme.fg_secondary)
            };
            frame.render_widget(
                Paragraph::new(Span::styled(
                    truncate_width(&footer_text, inner.width as usize),
                    footer_style,
                ))
                .style(footer_style),
                Rect::new(inner.x, footer_y, inner.width, 1),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};
    use tokio::sync::{mpsc, watch};

    use crate::{
        auth::AuthManager,
        config::{AppConfig, Persona},
        graph::GraphClient,
        storage::Storage,
        sync::{FocusTarget, SyncCommand, SyncController},
        ui::app::UiMsg,
    };

    fn test_app_and_ui() -> (AppState, UiState) {
        let cfg = AppConfig::default();
        let auth = AuthManager::empty_for_test(cfg.clone());
        let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
        let (nudge_tx, _nudge_rx) = mpsc::unbounded_channel::<SyncCommand>();
        let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx);
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel::<UiMsg>();
        let app = AppState::new(cfg.clone(), Persona::SharePoint, auth, graph, storage, sync_ctrl, ui_tx);
        let ui = UiState::from_config(&cfg);
        (app, ui)
    }

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn interactive_button_onclick_click_at_raw_toggle_and_serve_action() {
        let html = r#"
            <html><body>
              <h1>Dashboard</h1>
              <p id="status">Initial</p>
              <button onclick="document.getElementById('status').textContent = 'Clicked!'">Run Action</button>
              <details><summary>More Info</summary><p>Hidden Detail</p></details>
              <input type="text" name="note" value="hi" />
            </body></html>
        "#;
        let (app, mut ui) = test_app_and_ui();
        let mut overlay = HtmlPreviewOverlay::new("dashboard.html", html.to_string(), Some("h1".into()));
        assert!(overlay.session.js_active, "inline onclick must activate QuickJS");

        let mut terminal = Terminal::new(TestBackend::new(100, 26)).unwrap();
        let mut hitmap = HitMap::new();
        terminal
            .draw(|frame| {
                overlay.draw(frame, frame.area(), &app, &mut ui, &mut hitmap);
            })
            .unwrap();
        let screen0 = buffer_text(&terminal);
        assert!(screen0.contains("Initial"), "{screen0}");
        assert!(!screen0.contains("Clicked!"), "{screen0}");
        assert!(hitmap.region_of(HitTarget::Button(ButtonId::ServeHtml)).is_some());
        assert!(hitmap.region_of(HitTarget::Button(ButtonId::PreviewRenderToggle)).is_some());
        assert!(hitmap.region_of(HitTarget::Button(ButtonId::ConfirmCancel)).is_some());

        // Find exact coordinates of the `<button>` and click it via `handle_click_at`
        let html_theme = HtmlPreviewOverlay::html_theme(&ui.theme);
        let rendered0 = overlay
            .session
            .render(overlay.viewport.width, &html_theme, ui.show_link_urls);
        let btn = rendered0
            .controls
            .iter()
            .find(|c| c.label == "Run Action")
            .expect("Run Action button must be present")
            .clone();

        let click_col = overlay.viewport.x + btn.col_start + 1;
        let click_row = overlay.viewport.y + btn.line as u16;
        overlay.handle_click_at(click_col, click_row, &ui);

        terminal
            .draw(|frame| {
                hitmap.clear();
                overlay.draw(frame, frame.area(), &app, &mut ui, &mut hitmap);
            })
            .unwrap();
        let screen1 = buffer_text(&terminal);
        assert!(screen1.contains("Clicked!"), "onclick handler must update DOM:\n{screen1}");

        // Tab to `<details>` and activate with Space -> reveals hidden detail
        assert_eq!(overlay.handle(&Action::Tab, &ui), OverlayOutcome::Handled);
        overlay.handle(&Action::Input(' '), &ui);
        terminal
            .draw(|frame| {
                hitmap.clear();
                overlay.draw(frame, frame.area(), &app, &mut ui, &mut hitmap);
            })
            .unwrap();
        let screen2 = buffer_text(&terminal);
        assert!(screen2.contains("Hidden Detail"), "details must expand:\n{screen2}");

        // Tab to text input, activate, type '!', finish editing
        overlay.handle(&Action::Tab, &ui);
        overlay.handle(&Action::Activate, &ui);
        assert!(overlay.is_editing_input());
        overlay.handle(&Action::Input('!'), &ui);
        overlay.handle(&Action::Activate, &ui);
        assert!(!overlay.is_editing_input());

        // `m` toggles Raw mode and shows raw HTML tags
        overlay.handle(&Action::Input('m'), &ui);
        assert!(overlay.session.raw_mode);
        terminal
            .draw(|frame| {
                hitmap.clear();
                overlay.draw(frame, frame.area(), &app, &mut ui, &mut hitmap);
            })
            .unwrap();
        let raw_screen = buffer_text(&terminal);
        assert!(raw_screen.contains("<button"), "raw mode must show HTML tags:\n{raw_screen}");
        overlay.handle(&Action::Input('m'), &ui);
        assert!(!overlay.session.raw_mode);

        // `o` dispatches native localhost serving
        let serve_out = overlay.handle(&Action::Input('o'), &ui);
        assert!(
            matches!(
                serve_out,
                OverlayOutcome::Dispatch(Action::ServeHtmlInBrowser { ref title, .. })
                if title == "dashboard.html"
            ),
            "expected ServeHtmlInBrowser dispatch, got {serve_out:?}"
        );
    }

    #[test]
    fn static_html_keeps_zero_mb_js_idle_and_scrolls_with_scrollbar() {
        let mut body = String::from("<html><body><h1>Long Doc</h1>");
        for i in 0..60 {
            body.push_str(&format!("<p>Paragraph line number {i} with content.</p>"));
        }
        body.push_str("</body></html>");

        let (app, mut ui) = test_app_and_ui();
        let mut overlay = HtmlPreviewOverlay::new("long.html", body, None);
        assert!(!overlay.session.js_active, "static HTML must keep QuickJS at 0 MB idle");

        let mut terminal = Terminal::new(TestBackend::new(80, 18)).unwrap();
        let mut hitmap = HitMap::new();
        terminal
            .draw(|frame| {
                overlay.draw(frame, frame.area(), &app, &mut ui, &mut hitmap);
            })
            .unwrap();
        assert!(overlay.max_scroll_y > 0);
        assert_eq!(overlay.session.scroll_y, 0);

        overlay.handle(&Action::MoveDown, &ui);
        assert_eq!(overlay.session.scroll_y, 2);
        overlay.handle(
            &Action::MouseScroll {
                delta: 3,
                column: 20,
                row: 8,
            },
            &ui,
        );
        assert_eq!(overlay.session.scroll_y, 5);
        overlay.handle(&Action::ScrollBottom, &ui);
        assert_eq!(overlay.session.scroll_y, overlay.max_scroll_y);
        overlay.handle(&Action::Input('q'), &ui);
    }

    #[tokio::test]
    async fn native_html_server_serves_document_over_localhost_and_shuts_down() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpStream;

        let (app, _ui) = test_app_and_ui();
        let url = app
            .html_server
            .serve_html("SharePoint Page", "<h1>Hello SharePoint</h1>")
            .await
            .expect("serve_html");
        assert!(url.starts_with("http://127.0.0.1:"));

        let rest = url.strip_prefix("http://").unwrap();
        let (host_port, path) = rest.split_once('/').unwrap();
        let mut stream = TcpStream::connect(host_port).await.unwrap();
        let req = format!("GET /{path} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).await.unwrap();
        let mut resp = String::new();
        stream.read_to_string(&mut resp).await.unwrap();
        assert!(resp.starts_with("HTTP/1.1 200 OK"), "{resp}");
        assert!(resp.contains("<h1>Hello SharePoint</h1>"), "{resp}");

        app.html_server.shutdown();
    }

    #[test]
    fn inline_svg_renders_halfblock_in_reserved_slot_and_preserves_button_click_coords() {
        let html = r##"
            <html><body>
              <h1>Inline Graphics</h1>
              <p id="msg">Before click</p>
              <svg width="100" height="50"><rect width="100" height="50" fill="#e11d48"/></svg>
              <button onclick="document.getElementById('msg').textContent = 'After click'">Confirm Graphic</button>
            </body></html>
        "##;
        let (app, mut ui) = test_app_and_ui();
        ui.graphics_caps.protocol = TerminalImageProtocol::Halfblock;
        let mut overlay = HtmlPreviewOverlay::new("graphic.html", html.to_string(), None);

        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut hitmap = HitMap::new();
        terminal
            .draw(|frame| {
                overlay.draw(frame, frame.area(), &app, &mut ui, &mut hitmap);
            })
            .unwrap();

        let html_theme = HtmlPreviewOverlay::html_theme(&ui.theme);
        let rendered = overlay
            .session
            .render(overlay.viewport.width, &html_theme, ui.show_link_urls);
        assert_eq!(rendered.graphics.len(), 1, "expected 1 inline SVG graphic");
        let g = &rendered.graphics[0];
        assert!(g.line_count >= 3, "reserved slot must have >= 3 rows");

        // Verify Halfblock `▀` glyphs are actually rendered inside the graphic's reserved rows
        let buf = terminal.backend().buffer();
        let mut found_halfblock = false;
        for row_off in 0..g.line_count {
            let y = overlay.viewport.y + (g.line_start + row_off) as u16;
            for x in overlay.viewport.x..overlay.viewport.right() {
                if buf[(x, y)].symbol() == "▀" {
                    found_halfblock = true;
                    break;
                }
            }
        }
        assert!(
            found_halfblock,
            "expected Halfblock `▀` cells inside reserved SVG slot lines {}..{}:\n{}",
            g.line_start,
            g.line_start + g.line_count,
            buffer_text(&terminal)
        );
        assert_eq!(overlay.graphic_cache.len(), 1);
        assert!(ui.pending_pixel_graphics.is_empty());

        // Verify clicking the `<button>` below the SVG hits exact coordinates and updates DOM
        let btn = rendered
            .controls
            .iter()
            .find(|c| c.label == "Confirm Graphic")
            .expect("Confirm Graphic button must exist")
            .clone();
        assert!(
            btn.line >= g.line_start + g.line_count,
            "button line {} must be below SVG slot {}..{}",
            btn.line,
            g.line_start,
            g.line_start + g.line_count
        );

        let click_col = overlay.viewport.x + btn.col_start + 1;
        let click_row = overlay.viewport.y + btn.line as u16;
        overlay.handle_click_at(click_col, click_row, &ui);

        terminal
            .draw(|frame| {
                hitmap.clear();
                overlay.draw(frame, frame.area(), &app, &mut ui, &mut hitmap);
            })
            .unwrap();
        let screen_after = buffer_text(&terminal);
        assert!(
            screen_after.contains("After click"),
            "clicking button below inline SVG must trigger onclick:\n{screen_after}"
        );
    }

    #[test]
    fn inline_svg_pixel_protocol_pushes_pending_graphic_and_marks_cells_skip() {
        let html = r##"
            <html><body>
              <p id="msg">Ready</p>
              <svg width="100" height="50"><rect width="100" height="50" fill="#0ea5e9"/></svg>
              <button onclick="document.getElementById('msg').textContent = 'Clicked Pixel'">Go</button>
            </body></html>
        "##;
        let (app, mut ui) = test_app_and_ui();
        ui.graphics_caps = img_preview::TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::Kitty,
            cell_px: (10, 20),
            cell_px_exact: true,
        };
        let mut overlay = HtmlPreviewOverlay::new("pixel.html", html.to_string(), None);

        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut hitmap = HitMap::new();
        let mut all_cells_skipped = false;
        terminal
            .draw(|frame| {
                overlay.draw(frame, frame.area(), &app, &mut ui, &mut hitmap);
                if let Some(pg) = ui.pending_pixel_graphics.first() {
                    let buf = frame.buffer_mut();
                    all_cells_skipped = (pg.rect.y..pg.rect.bottom()).all(|y| {
                        (pg.rect.x..pg.rect.right()).all(|x| buf[(x, y)].skip)
                    });
                }
            })
            .unwrap();

        assert_eq!(ui.pending_pixel_graphics.len(), 1);
        let pg = &ui.pending_pixel_graphics[0];
        assert!(pg.cache_key.starts_with("html:"), "{}", pg.cache_key);
        assert_eq!(pg.protocol, TerminalImageProtocol::Kitty);
        assert!(
            pg.payload
                .as_deref()
                .is_some_and(|p| p.starts_with("\x1b_G")),
            "expected Kitty payload"
        );
        assert!(
            all_cells_skipped,
            "expected all cells inside pixel rect {:?} to have skip=true in frame buffer",
            pg.rect
        );

        // Clicking button below the pixel graphic still hits exact coordinates
        let html_theme = HtmlPreviewOverlay::html_theme(&ui.theme);
        let rendered = overlay
            .session
            .render(overlay.viewport.width, &html_theme, ui.show_link_urls);
        let btn = rendered
            .controls
            .iter()
            .find(|c| c.label == "Go")
            .expect("Go button")
            .clone();
        overlay.handle_click_at(
            overlay.viewport.x + btn.col_start + 1,
            overlay.viewport.y + btn.line as u16,
            &ui,
        );
        terminal
            .draw(|frame| {
                hitmap.clear();
                ui.pending_pixel_graphics.clear();
                overlay.draw(frame, frame.area(), &app, &mut ui, &mut hitmap);
            })
            .unwrap();
        assert!(buffer_text(&terminal).contains("Clicked Pixel"));
    }
}
