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
        if let Some(rendered) = app.rendered_images.get(&key) {
            if rendered.protocol.is_pixel() && rendered.pixel_payload.is_some() {
                let draw_w = rendered.cols.min(inner.width);
                let draw_h = rendered.rows.min(inner.height);
                let draw_rect = Rect::new(
                    inner.x + inner.width.saturating_sub(draw_w) / 2,
                    inner.y + inner.height.saturating_sub(draw_h) / 2,
                    draw_w,
                    draw_h,
                );
                ui.pending_pixel_graphics.push(PendingPixelGraphic {
                    cache_key: key,
                    rect: draw_rect,
                    protocol: rendered.protocol,
                });
            } else {
                frame.render_widget(Paragraph::new(rendered.lines.clone()), inner);
            }
        } else if let Some(err) = &self.error {
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
