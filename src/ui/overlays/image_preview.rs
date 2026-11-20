//! Full-screen HD inline terminal image preview modal (`v` / `Alt+v`).
//! Supports prev/next cycling (`[`/`]`, `←`/`→`), zoom (`+`/`-`/`0`), protocol cycle (`p`),
//! download (`d`), and close (`Esc`/`q`/`v`).

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::{
    models::DriveItem,
    ui::{
        action::Action,
        app::{AppState, PendingPixelGraphic, UiState},
        hitmap::{HitMap, HitTarget},
        overlays::{centered_rect, OverlayOutcome},
    },
};

#[derive(Debug, Clone)]
pub struct ImagePreviewOverlay {
    pub items: Vec<DriveItem>,
    pub index: usize,
    /// Zoom percentage (`25..=200`, default `100`).
    pub zoom_pct: u16,
    pub error: Option<String>,
}

impl ImagePreviewOverlay {
    pub fn from_folder_items(all_items: &[DriveItem], active_item_id: Option<&str>) -> Option<Self> {
        let images: Vec<DriveItem> = all_items.iter().filter(|i| i.is_image()).cloned().collect();
        if images.is_empty() {
            return None;
        }
        let start = active_item_id
            .and_then(|id| images.iter().position(|i| i.id == id))
            .unwrap_or(0);
        Some(Self {
            items: images,
            index: start,
            zoom_pct: 100,
            error: None,
        })
    }

    pub fn current(&self) -> Option<&DriveItem> {
        self.items.get(self.index)
    }

    pub fn modal_rect(area: Rect) -> Rect {
        centered_rect(
            area,
            area.width.saturating_sub(4).max(20).min(area.width),
            area.height.saturating_sub(2).max(10).min(area.height),
        )
    }

    pub fn target_dimensions(&self, term_width: u16, term_height: u16) -> (u16, u16) {
        let rect = Self::modal_rect(Rect::new(0, 0, term_width.max(24), term_height.max(12)));
        let base_cols = rect.width.saturating_sub(4).max(12);
        let base_rows = rect.height.saturating_sub(4).max(6);
        let scaled_cols = ((base_cols as u32 * self.zoom_pct as u32) / 100)
            .clamp(8, base_cols as u32) as u16;
        let scaled_rows = ((base_rows as u32 * self.zoom_pct as u32) / 100)
            .clamp(4, base_rows as u32) as u16;
        (scaled_cols, scaled_rows)
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::CloseOverlay | Action::OpenImagePreview | Action::Input('q') | Action::Input('v') => {
                OverlayOutcome::Close
            }
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            Action::CursorRight | Action::Input(']') | Action::Input('l') => {
                if !self.items.is_empty() {
                    self.index = (self.index + 1) % self.items.len();
                    self.error = None;
                }
                OverlayOutcome::Dispatch(Action::LoadPreviewImage)
            }
            Action::CursorLeft | Action::Input('[') | Action::Input('h') => {
                if !self.items.is_empty() {
                    self.index = (self.index + self.items.len() - 1) % self.items.len();
                    self.error = None;
                }
                OverlayOutcome::Dispatch(Action::LoadPreviewImage)
            }
            Action::Input('+') | Action::Input('=') => {
                self.zoom_pct = (self.zoom_pct + 25).min(200);
                OverlayOutcome::Dispatch(Action::LoadPreviewImage)
            }
            Action::Input('-') => {
                self.zoom_pct = self.zoom_pct.saturating_sub(25).max(25);
                OverlayOutcome::Dispatch(Action::LoadPreviewImage)
            }
            Action::Input('0') => {
                self.zoom_pct = 100;
                OverlayOutcome::Dispatch(Action::LoadPreviewImage)
            }
            Action::Input('p') => OverlayOutcome::Dispatch(Action::CycleImageProtocol),
            Action::Input('d') | Action::DownloadSelected => {
                OverlayOutcome::Dispatch(Action::DownloadPreviewImage)
            }
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(
        &self,
        frame: &mut Frame,
        area: Rect,
        app: &AppState,
        ui: &mut UiState,
        hitmap: &mut HitMap,
    ) {
        let theme = &ui.theme;
        let rect = Self::modal_rect(area);
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let name = self
            .current()
            .map(|i| i.name.as_str())
            .unwrap_or("Image");
        let title = format!(
            " {name} ({}/{}) · {}% · {} · [/] Prev/Next · +/- Zoom · p Proto · d DL · Esc Close ",
            self.index + 1,
            self.items.len().max(1),
            self.zoom_pct,
            ui.graphics_caps.protocol.label()
        );
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                crate::ui::truncate_width(&title, rect.width.saturating_sub(2) as usize),
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let Some(cur) = self.current() else {
            return;
        };
        let (cols, rows) = self.target_dimensions(area.width, area.height);
        let key = app.modal_image_cache_key(cur, cols, rows, ui.graphics_caps.protocol);
        let max_safe_bottom = area.bottom().saturating_sub(1);
        let safe_inner_h = inner.height.min(max_safe_bottom.saturating_sub(inner.y));
        if let Some(rendered) = app.rendered_images.get(&key) {
            if rendered.protocol.is_pixel() && rendered.pixel_payload.is_some() {
                if rendered.cols > 0
                    && rendered.rows > 0
                    && rendered.cols <= inner.width
                    && rendered.rows <= safe_inner_h
                {
                    let draw_w = rendered.cols;
                    let draw_h = rendered.rows;
                    let draw_rect = Rect::new(
                        inner.x + inner.width.saturating_sub(draw_w) / 2,
                        inner.y + safe_inner_h.saturating_sub(draw_h) / 2,
                        draw_w,
                        draw_h,
                    );
                    debug_assert!(draw_rect.bottom() <= max_safe_bottom);
                    ui.pending_pixel_graphics.push(PendingPixelGraphic {
                        cache_key: key,
                        rect: draw_rect,
                        protocol: rendered.protocol,
                    });
                } else {
                    frame.render_widget(
                        Paragraph::new(Span::styled(
                            " Resizing HD image preview...",
                            theme.muted_style(),
                        )),
                        inner,
                    );
                }
            } else {
                frame.render_widget(Paragraph::new(rendered.lines.clone()), inner);
            }
        } else if let Some(err) = self
            .error
            .as_deref()
            .or_else(|| app.preview_errors.get(&cur.id).map(String::as_str))
        {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    format!(" Failed to preview image: {err}"),
                    Style::default().fg(theme.error),
                )),
                inner,
            );
        } else {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    " Decoding HD image preview...",
                    theme.muted_style(),
                )),
                inner,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        auth::AuthManager,
        config::{AppConfig, Persona},
        graph::GraphClient,
        img_preview::{RenderedTerminalImage, TerminalGraphicsCaps, TerminalImageProtocol},
        storage::Storage,
        sync::{FocusTarget, SyncCommand, SyncController},
        ui::app::UiMsg,
    };
    use ratatui::{backend::TestBackend, Terminal};
    use tokio::sync::{mpsc, watch};

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

    fn sample_image_item(id: &str, name: &str) -> DriveItem {
        DriveItem {
            id: id.to_string(),
            drive_id: "d1".to_string(),
            name: name.to_string(),
            mime_type: Some("image/png".to_string()),
            ..Default::default()
        }
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
    fn test_modal_surfaces_app_preview_error_instead_of_loading_forever() {
        let (mut app, mut ui) = test_app_and_ui();
        let item = sample_image_item("img-err-1", "broken.png");
        app.preview_errors
            .insert(item.id.clone(), "403 Forbidden".to_string());

        let overlay = ImagePreviewOverlay::from_folder_items(&[item], Some("img-err-1")).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        let mut hitmap = HitMap::default();
        terminal
            .draw(|frame| {
                overlay.draw(frame, frame.area(), &app, &mut ui, &mut hitmap);
            })
            .unwrap();

        let rendered_text = buffer_text(&terminal);
        assert!(
            rendered_text.contains("Failed to preview image: 403 Forbidden"),
            "expected error message in modal buffer, got:\n{rendered_text}"
        );
        assert!(
            !rendered_text.contains("Decoding HD image preview..."),
            "must not stay stuck on loading text when app.preview_errors has an entry"
        );
    }

    #[test]
    fn test_modal_pixel_rect_clamps_within_inner_and_above_bottom_row() {
        let (mut app, mut ui) = test_app_and_ui();
        ui.graphics_caps = TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::Sixel,
            cell_px: (10, 20),
            cell_px_exact: true,
        };
        let item = sample_image_item("img-ok-1", "chart.png");
        let overlay =
            ImagePreviewOverlay::from_folder_items(&[item.clone()], Some("img-ok-1")).unwrap();
        let area = Rect::new(0, 0, 80, 24);
        let (cols, rows) = overlay.target_dimensions(area.width, area.height);
        let key = app.modal_image_cache_key(&item, cols, rows, TerminalImageProtocol::Sixel);

        // Fitting image -> emits PendingPixelGraphic strictly above area.bottom() - 1
        app.rendered_images.insert(
            key.clone(),
            RenderedTerminalImage {
                width_px: 400,
                height_px: 200,
                cols: cols.min(40),
                rows: rows.min(12),
                format_label: "PNG".into(),
                protocol: TerminalImageProtocol::Sixel,
                lines: Vec::new(),
                pixel_payload: Some("\x1bP0;1;q\"1;1;400;200#0;2;0;0;0\x1b\\".into()),
            },
        );
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        let mut hitmap = HitMap::default();
        terminal
            .draw(|frame| {
                overlay.draw(frame, frame.area(), &app, &mut ui, &mut hitmap);
            })
            .unwrap();
        assert_eq!(ui.pending_pixel_graphics.len(), 1);
        let placed = ui.pending_pixel_graphics[0].rect;
        assert!(placed.bottom() <= area.bottom().saturating_sub(1));

        // Oversized image -> suppressed instead of overflowing modal border
        ui.pending_pixel_graphics.clear();
        app.rendered_images.insert(
            key,
            RenderedTerminalImage {
                width_px: 800,
                height_px: 600,
                cols: area.width + 10,
                rows: area.height + 5,
                format_label: "PNG".into(),
                protocol: TerminalImageProtocol::Sixel,
                lines: Vec::new(),
                pixel_payload: Some("\x1bP0;1;q\"1;1;800;600#0;2;0;0;0\x1b\\".into()),
            },
        );
        terminal
            .draw(|frame| {
                overlay.draw(frame, frame.area(), &app, &mut ui, &mut hitmap);
            })
            .unwrap();
        assert!(ui.pending_pixel_graphics.is_empty());
    }

    #[test]
    fn test_modal_cycling_and_zoom_dimension_scaling() {
        let items = vec![
            sample_image_item("a", "a.png"),
            sample_image_item("b", "b.jpg"),
            sample_image_item("c", "c.webp"),
        ];
        let mut overlay = ImagePreviewOverlay::from_folder_items(&items, Some("a")).unwrap();
        overlay.error = Some("stale".into());

        // `]` advances to next item and clears stale error
        assert_eq!(
            overlay.handle(&Action::Input(']')),
            OverlayOutcome::Dispatch(Action::LoadPreviewImage)
        );
        assert_eq!(overlay.index, 1);
        assert!(overlay.error.is_none());

        // `[` wraps cleanly backward
        overlay.handle(&Action::Input('['));
        assert_eq!(overlay.index, 0);
        overlay.handle(&Action::Input('['));
        assert_eq!(overlay.index, 2);

        // `-` / `+` / `0` zoom scaling
        let (base_c, base_r) = overlay.target_dimensions(120, 40);
        overlay.handle(&Action::Input('-'));
        assert_eq!(overlay.zoom_pct, 75);
        let (small_c, small_r) = overlay.target_dimensions(120, 40);
        assert!(small_c < base_c && small_r < base_r);

        overlay.handle(&Action::Input('+'));
        assert_eq!(overlay.zoom_pct, 100);
        assert_eq!(overlay.target_dimensions(120, 40), (base_c, base_r));

        overlay.handle(&Action::Input('+'));
        assert_eq!(overlay.zoom_pct, 125);
        // Clamps to base bounds at >=100% and resets cleanly on `0`
        overlay.handle(&Action::Input('0'));
        assert_eq!(overlay.zoom_pct, 100);
        assert_eq!(overlay.target_dimensions(120, 40), (base_c, base_r));
    }
}
