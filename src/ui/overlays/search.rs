//! File Search Overlay (`/`): hybrid local SQLite FTS5 + live Microsoft Graph search.

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::{
    models::format_human_size,
    ui::{
        action::Action,
        app::{AppState, UiState},
        hitmap::{HitMap, HitTarget},
        overlays::{centered_rect, OverlayOutcome},
    },
};

#[derive(Debug, Clone, Default)]
pub struct SearchOverlay {
    pub query: String,
    pub selected: usize,
    pub remote_requested: bool,
}

impl SearchOverlay {
    pub fn with_query(query: String) -> Self {
        Self {
            query,
            selected: 0,
            remote_requested: false,
        }
    }

    pub fn select(&mut self, index: usize) {
        self.selected = index;
    }

    pub fn handle(&mut self, action: &Action, app: &AppState) -> OverlayOutcome {
        match action {
            Action::Input(ch) => {
                self.query.push(*ch);
                self.selected = 0;
                self.remote_requested = false;
                OverlayOutcome::Dispatch(Action::SearchLocalFiles(self.query.clone()))
            }
            Action::Paste(text) => {
                self.query.push_str(&text.replace(['\r', '\n'], " "));
                self.selected = 0;
                self.remote_requested = false;
                OverlayOutcome::Dispatch(Action::SearchLocalFiles(self.query.clone()))
            }
            Action::Backspace => {
                self.query.pop();
                self.selected = 0;
                self.remote_requested = false;
                OverlayOutcome::Dispatch(Action::SearchLocalFiles(self.query.clone()))
            }
            Action::MoveUp => {
                let len = app.search_results.len();
                if len > 0 {
                    let cur = self.selected.min(len - 1);
                    self.selected = (cur + len - 1) % len;
                }
                OverlayOutcome::Handled
            }
            Action::MoveDown => {
                let len = app.search_results.len();
                if len > 0 {
                    let cur = self.selected.min(len - 1);
                    self.selected = (cur + 1) % len;
                }
                OverlayOutcome::Handled
            }
            Action::Tab => {
                if !self.query.trim().is_empty() {
                    self.remote_requested = true;
                    OverlayOutcome::Dispatch(Action::SearchRemoteFiles(self.query.clone()))
                } else {
                    OverlayOutcome::Handled
                }
            }
            Action::Activate | Action::OverlaySubmit => {
                if !app.search_results.is_empty() {
                    let idx = self.selected.min(app.search_results.len() - 1);
                    OverlayOutcome::CloseAndDispatch(Action::JumpToSearchHit(idx))
                } else if !self.query.trim().is_empty() {
                    self.remote_requested = true;
                    OverlayOutcome::Dispatch(Action::SearchRemoteFiles(self.query.clone()))
                } else {
                    OverlayOutcome::Handled
                }
            }
            Action::CloseOverlay => OverlayOutcome::Close,
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(
        &self,
        frame: &mut Frame,
        area: Rect,
        app: &AppState,
        ui: &UiState,
        hitmap: &mut HitMap,
    ) {
        let theme = &ui.theme;
        let rect = centered_rect(
            area,
            area.width.saturating_sub(6).min(88),
            area.height.saturating_sub(4).min(22),
        );
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                " Search Files · Tab Remote Graph Search · Enter Jump · Esc Close ",
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        crate::ui::overlays::draw_overlay_close_pill(frame, rect, ui, hitmap);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let query_line = Line::from(vec![
            Span::styled(" / ", theme.accent_style()),
            Span::raw(self.query.clone()),
            Span::styled("▏", theme.muted_style()),
        ]);
        frame.render_widget(
            Paragraph::new(query_line),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );

        let list_y = inner.y.saturating_add(2);
        let visible = inner.height.saturating_sub(2) as usize;
        if visible == 0 {
            return;
        }

        if app.search_results.is_empty() {
            let msg = if self.query.trim().is_empty() {
                " Type filename, path, or author (FTS5 instant local + Tab for live Graph search)"
            } else {
                " No cached matches yet — press Tab or Enter for live Microsoft Graph search"
            };
            frame.render_widget(
                Paragraph::new(Span::styled(msg, theme.muted_style())),
                Rect::new(inner.x, list_y, inner.width, 1),
            );
            return;
        }

        let active_sel = self.selected.min(app.search_results.len() - 1);
        let skip = active_sel.saturating_sub(visible.saturating_sub(1));
        for (row, (idx, item)) in app
            .search_results
            .iter()
            .enumerate()
            .skip(skip)
            .take(visible)
            .enumerate()
        {
            let y = list_y + row as u16;
            if y >= inner.bottom() {
                break;
            }
            let style = if idx == active_sel {
                theme.selection()
            } else {
                Style::default()
            };
            let line = format!(
                " [{:<4}] {}  ({} · {})",
                item.type_badge(),
                item.name,
                format_human_size(item.size),
                item.display_path()
            );
            frame.render_widget(
                Paragraph::new(Span::styled(
                    crate::ui::truncate_width(&line, inner.width as usize),
                    style,
                )),
                Rect::new(inner.x, y, inner.width, 1),
            );
            hitmap.add_line(inner.x, y, inner.width, HitTarget::OverlayItem { index: idx });
        }
    }
}
