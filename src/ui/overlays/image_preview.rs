//! Full-screen HD inline terminal image preview modal (`v` / `Alt+v`).
//! Supports vertical/horizontal scrolling for tall/zoomed images (`j`/`k`/`↑`/`↓`/wheel + Scrollbar),
//! prev/next cycling (`[`/`]`, `←`/`→`), zoom (`+`/`-`/`0`), protocol cycle (`p`),
//! download (`d`), and close (`Esc`/`q`/`v` or `[✕ Esc]`).

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState,
    },
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::{
    img_preview::TerminalImageProtocol,
    models::DriveItem,
    ui::{
        action::Action,
        app::{AppState, PendingPixelGraphic, UiState},
        hitmap::{ButtonId, HitMap, HitTarget},
        overlays::{centered_rect, OverlayOutcome},
    },
};

#[derive(Debug, Clone)]
pub struct ImagePreviewOverlay {
    pub items: Vec<DriveItem>,
    pub index: usize,
    /// Zoom percentage (`25..=300`, default `100`).
    pub zoom_pct: u16,
    pub scroll_y: u16,
    pub scroll_x: u16,
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
            scroll_y: 0,
            scroll_x: 0,
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
        let inner_w = rect.width.saturating_sub(2).max(12);
        let inner_h = rect.height.saturating_sub(3).max(6);
        let base_cols = rect.width.saturating_sub(4).max(12);
        let max_cols = inner_w.saturating_mul(2).max(base_cols).min(240);
        let scaled_cols = ((base_cols as u32 * self.zoom_pct as u32) / 100)
            .clamp(8, max_cols as u32) as u16;
        // Pass `scrollable_max_rows >= scaled_cols` so `img_preview` fits tall images to width
        // and lets them scroll vertically immediately at 100% zoom.
        let scrollable_max_rows = ((inner_h as u32 * 4 * self.zoom_pct as u32) / 100)
            .max(scaled_cols as u32)
            .clamp(6, 240) as u16;
        (scaled_cols, scrollable_max_rows)
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::CloseOverlay
            | Action::OpenImagePreview
            | Action::Input('q')
            | Action::Input('v') => OverlayOutcome::Close,
            Action::Quit | Action::QuitConfirm | Action::LoadPreviewImage => {
                OverlayOutcome::NotHandled
            }
            Action::MoveUp | Action::Input('k') => {
                self.scroll_y = self.scroll_y.saturating_sub(2);
                OverlayOutcome::Handled
            }
            Action::MoveDown | Action::Input('j') => {
                self.scroll_y = self.scroll_y.saturating_add(2);
                OverlayOutcome::Handled
            }
            Action::ScrollPageUp => {
                self.scroll_y = self.scroll_y.saturating_sub(10);
                OverlayOutcome::Handled
            }
            Action::ScrollPageDown => {
                self.scroll_y = self.scroll_y.saturating_add(10);
                OverlayOutcome::Handled
            }
            Action::CursorHome | Action::ScrollTop | Action::Input('g') => {
                self.scroll_y = 0;
                self.scroll_x = 0;
                OverlayOutcome::Handled
            }
            Action::CursorEnd | Action::ScrollBottom | Action::Input('G') => {
                self.scroll_y = u16::MAX;
                OverlayOutcome::Handled
            }
            Action::Input('h') => {
                self.scroll_x = self.scroll_x.saturating_sub(4);
                OverlayOutcome::Handled
            }
            Action::Input('l') => {
                self.scroll_x = self.scroll_x.saturating_add(4);
                OverlayOutcome::Handled
            }
            Action::MouseScroll { delta, .. } => {
                if *delta < 0 {
                    self.scroll_y = self.scroll_y.saturating_sub((-*delta) as u16);
                } else {
                    self.scroll_y = self.scroll_y.saturating_add(*delta as u16);
                }
                OverlayOutcome::Handled
            }
            Action::CursorRight | Action::Input(']') => {
                if !self.items.is_empty() {
                    self.index = (self.index + 1) % self.items.len();
                    self.scroll_y = 0;
                    self.scroll_x = 0;
                    self.error = None;
                }
                OverlayOutcome::Dispatch(Action::LoadPreviewImage)
            }
            Action::CursorLeft | Action::Input('[') => {
                if !self.items.is_empty() {
                    self.index = (self.index + self.items.len() - 1) % self.items.len();
                    self.scroll_y = 0;
                    self.scroll_x = 0;
                    self.error = None;
                }
                OverlayOutcome::Dispatch(Action::LoadPreviewImage)
            }
            Action::Input('+') | Action::Input('=') => {
                self.zoom_pct = (self.zoom_pct + 25).min(300);
                OverlayOutcome::Dispatch(Action::LoadPreviewImage)
            }
            Action::Input('-') => {
                self.zoom_pct = self.zoom_pct.saturating_sub(25).max(25);
                self.scroll_y = 0;
                self.scroll_x = 0;
                OverlayOutcome::Dispatch(Action::LoadPreviewImage)
            }
            Action::Input('0') => {
                self.zoom_pct = 100;
                self.scroll_y = 0;
                self.scroll_x = 0;
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
        &mut self,
        frame: &mut Frame,
        area: Rect,
        app: &AppState,
        ui: &mut UiState,
        hitmap: &mut HitMap,
    ) {
        let theme = ui.theme.clone();
        let rect = Self::modal_rect(area);
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let proto = ui.graphics_caps.protocol;
        let close_label = "[✕ Esc]";
        let close_w = close_label.width() as u16;
        let show_close_pill = rect.width > close_w + 4;
        let title_budget = rect
            .width
            .saturating_sub(2 + if show_close_pill { close_w + 2 } else { 0 })
            as usize;

        let name = self
            .current()
            .map(|i| i.name.as_str())
            .unwrap_or("Image");
        let title = format!(
            " {name} ({}/{}) · {}% · {} · ↑↓/Wheel Scroll · [/] Prev/Next · +/- Zoom · p Proto · d DL ",
            self.index + 1,
            self.items.len().max(1),
            self.zoom_pct,
            proto.label()
        );
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                crate::ui::truncate_width(&title, title_budget),
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);

        if show_close_pill {
            let close_x = rect.right().saturating_sub(close_w + 1);
            let close_rect = Rect::new(close_x, rect.y, close_w, 1);
            frame.render_widget(
                Paragraph::new(Span::styled(
                    close_label,
                    Style::default().fg(theme.fg).bg(theme.bg_elevated),
                )),
                close_rect,
            );
            hitmap.add_line(
                close_x,
                rect.y,
                close_w,
                HitTarget::Button(ButtonId::ConfirmCancel),
            );
        }

        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let Some(cur) = self.current() else {
            let msg = self
                .error
                .as_deref()
                .unwrap_or("No images remaining in this folder");
            frame.render_widget(
                Paragraph::new(Span::styled(format!(" {msg}"), theme.muted_style())),
                inner,
            );
            return;
        };
        let (cols, rows) = self.target_dimensions(area.width, area.height);
        let key = app.modal_image_cache_key(cur, cols, rows, proto);
        let max_safe_bottom = area.bottom().saturating_sub(1);
        let safe_inner_h = inner.height.min(max_safe_bottom.saturating_sub(inner.y));

        if let Some(rendered) = app.rendered_images.get(&key) {
            let is_pixel =
                rendered.pixel_payload.is_some() && proto != TerminalImageProtocol::Halfblock;
            let total_rows = if is_pixel {
                rendered.rows
            } else {
                rendered.lines.len() as u16
            };
            let total_cols = rendered.cols;

            let avail_h = if is_pixel
                && !ui.graphics_caps.cell_px_exact
                && proto == TerminalImageProtocol::Sixel
            {
                safe_inner_h.saturating_sub(1).max(1)
            } else {
                safe_inner_h
            };
            let show_scrollbar = total_rows > avail_h && inner.width > 2;
            let avail_w = if show_scrollbar {
                inner.width.saturating_sub(1)
            } else {
                inner.width
            };

            let max_scroll_y = total_rows.saturating_sub(avail_h);
            let max_scroll_x = total_cols.saturating_sub(avail_w);
            self.scroll_y = self.scroll_y.min(max_scroll_y);
            self.scroll_x = self.scroll_x.min(max_scroll_x);
            let sy = self.scroll_y;
            let sx = self.scroll_x;

            if is_pixel {
                let fits_without_crop = total_cols > 0
                    && total_rows > 0
                    && total_cols <= avail_w
                    && total_rows <= avail_h
                    && sx == 0
                    && sy == 0;
                if fits_without_crop {
                    let draw_w = total_cols;
                    let draw_h = total_rows;
                    let draw_rect = Rect::new(
                        inner.x + avail_w.saturating_sub(draw_w) / 2,
                        inner.y + avail_h.saturating_sub(draw_h) / 2,
                        draw_w,
                        draw_h,
                    );
                    debug_assert!(draw_rect.bottom() <= max_safe_bottom);
                    ui.pending_pixel_graphics.push(PendingPixelGraphic {
                        cache_key: key,
                        rect: draw_rect,
                        protocol: rendered.protocol,
                    });
                } else if rendered.has_scroll_backing() && total_cols > 0 && total_rows > 0 {
                    let draw_w = total_cols.saturating_sub(sx).min(avail_w);
                    let draw_h = total_rows.saturating_sub(sy).min(avail_h);
                    let px_x = inner.x + avail_w.saturating_sub(draw_w) / 2;
                    let px_y = inner.y + avail_h.saturating_sub(draw_h) / 2;
                    let draw_rect = Rect::new(px_x, px_y, draw_w, draw_h);
                    debug_assert!(draw_rect.bottom() <= max_safe_bottom);
                    ui.pending_pixel_graphics.push(PendingPixelGraphic {
                        cache_key: format!("{key}#crop:{sx},{sy},{avail_w},{avail_h}"),
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
                let view_w = total_cols.saturating_sub(sx).min(avail_w);
                let view_h = total_rows.saturating_sub(sy).min(avail_h);
                let pad_x = avail_w.saturating_sub(view_w) / 2;
                let pad_y = avail_h.saturating_sub(view_h) / 2;
                let draw_rect = Rect::new(inner.x + pad_x, inner.y + pad_y, view_w, view_h);
                frame.render_widget(
                    Paragraph::new(rendered.lines.clone()).scroll((sy, sx)),
                    draw_rect,
                );
            }

            if show_scrollbar {
                let sb_area = Rect::new(inner.right().saturating_sub(1), inner.y, 1, inner.height);
                let mut sb_state = ScrollbarState::new(max_scroll_y.saturating_add(1) as usize)
                    .position(sy as usize);
                let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .thumb_style(Style::default().fg(theme.accent))
                    .track_style(theme.muted_style());
                frame.render_stateful_widget(scrollbar, sb_area, &mut sb_state);
            }
        } else if let Some(err) = self
            .error
            .as_deref()
            .or_else(|| app.image_preview_error_for(cur))
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
    use std::io::Cursor;

    use image::{DynamicImage, ImageBuffer, ImageFormat, Rgba, RgbaImage};
    use ratatui::{backend::TestBackend, Terminal};
    use tokio::sync::{mpsc, watch};

    use super::*;
    use crate::{
        auth::AuthManager,
        config::{AppConfig, Persona},
        graph::GraphClient,
        img_preview::{
            decode_and_render_for_protocol, RenderedTerminalImage, TerminalGraphicsCaps,
            TerminalImageProtocol,
        },
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

    fn sample_image_item(id: &str, name: &str) -> DriveItem {
        DriveItem {
            id: id.to_string(),
            drive_id: "d1".to_string(),
            name: name.to_string(),
            mime_type: Some("image/png".to_string()),
            ..Default::default()
        }
    }

    fn make_tall_png(w: u32, h: u32) -> Vec<u8> {
        let img: RgbaImage = ImageBuffer::from_fn(w, h, |x, y| {
            Rgba([
                ((x * 255) / w.max(1)) as u8,
                ((y * 255) / h.max(1)) as u8,
                160,
                255,
            ])
        });
        let mut buf = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(img)
            .write_to(&mut buf, ImageFormat::Png)
            .unwrap();
        buf.into_inner()
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

        let mut overlay =
            ImagePreviewOverlay::from_folder_items(&[item], Some("img-err-1")).unwrap();
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
        let mut overlay =
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
                scroll_backing: None,
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

        // Oversized image without scroll_backing -> suppressed instead of overflowing modal border
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
                scroll_backing: None,
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
    fn test_modal_tall_image_scroll_clamps_and_emits_crop_for_halfblock_and_sixel() {
        let png = make_tall_png(400, 2000);
        let area = Rect::new(0, 0, 100, 30);

        for proto in [TerminalImageProtocol::Halfblock, TerminalImageProtocol::Sixel] {
            let (mut app, mut ui) = test_app_and_ui();
            ui.graphics_caps = TerminalGraphicsCaps {
                protocol: proto,
                cell_px: (8, 16),
                cell_px_exact: true,
            };
            let item = sample_image_item("tall-1", "tall.png");
            let mut overlay =
                ImagePreviewOverlay::from_folder_items(&[item.clone()], Some("tall-1")).unwrap();
            let (cols, rows) = overlay.target_dimensions(area.width, area.height);
            let rendered =
                decode_and_render_for_protocol(&png, cols, rows, (25, 25, 25), ui.graphics_caps)
                    .unwrap();
            let total_rows = if proto == TerminalImageProtocol::Halfblock {
                rendered.lines.len() as u16
            } else {
                rendered.rows
            };
            let rect = ImagePreviewOverlay::modal_rect(area);
            let inner_h = rect.height.saturating_sub(2);
            let expected_max_scroll = total_rows.saturating_sub(inner_h);
            assert!(expected_max_scroll > 0);

            let base_key = app.modal_image_cache_key(&item, cols, rows, proto);
            app.rendered_images.insert(base_key.clone(), rendered);

            let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
            let mut hitmap = HitMap::new();

            terminal
                .draw(|frame| {
                    ui.pending_pixel_graphics.clear();
                    hitmap.clear();
                    overlay.draw(frame, area, &app, &mut ui, &mut hitmap);
                })
                .unwrap();

            assert!(hitmap
                .region_of(HitTarget::Button(ButtonId::ConfirmCancel))
                .is_some());

            if proto == TerminalImageProtocol::Sixel {
                let pg0 = ui
                    .pending_pixel_graphics
                    .first()
                    .expect("expected cropped pixel graphic");
                assert!(pg0.cache_key.starts_with(&format!("{base_key}#crop:0,0,")));
                let rendered_ref = app.rendered_images.get(&base_key).unwrap();
                let (payload, dw, dh) = rendered_ref
                    .viewport_pixel_payload(0, 0, pg0.rect.width, pg0.rect.height)
                    .expect("viewport_pixel_payload must succeed");
                assert!(payload.starts_with("\x1bP0;1;q\"1;1;"));
                assert!(dw <= pg0.rect.width && dh <= pg0.rect.height);
            }

            overlay.handle(&Action::ScrollBottom);
            terminal
                .draw(|frame| {
                    ui.pending_pixel_graphics.clear();
                    hitmap.clear();
                    overlay.draw(frame, area, &app, &mut ui, &mut hitmap);
                })
                .unwrap();
            assert_eq!(overlay.scroll_y, expected_max_scroll);

            overlay.handle(&Action::MouseScroll {
                delta: -3,
                column: 50,
                row: 15,
            });
            assert_eq!(overlay.scroll_y, expected_max_scroll.saturating_sub(3));
        }
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
        overlay.handle(&Action::Input('0'));
        assert_eq!(overlay.zoom_pct, 100);
        assert_eq!(overlay.target_dimensions(120, 40), (base_c, base_r));
    }
}
