//! Settings overlay (`Ctrl+,`).
//! Reads & persists `crate::config::AppConfig` (`config.toml`).

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::{
    config,
    ui::{
        action::Action,
        app::{AppState, UiState},
        hitmap::{HitMap, HitTarget},
        overlays::{centered_rect, OverlayKind, OverlayOutcome},
    },
};

const ITEMS: usize = 11;

#[derive(Debug, Clone, Default)]
pub struct SettingsState {
    pub selected: usize,
}

impl SettingsState {
    pub fn select(&mut self, index: usize) {
        if index < ITEMS {
            self.selected = index;
        }
    }

    pub fn rows(app: &AppState, ui: &UiState) -> [(&'static str, String); ITEMS] {
        [
            ("Notifications", toggle(ui.notify)),
            ("Left sidebar", toggle(ui.show_nav_rail)),
            ("Inline image previews", toggle(ui.inline_images)),
            ("Rendered MD/HTML preview", toggle(ui.render_markup)),
            ("Image protocol", ui.graphics_caps.protocol.label().to_string()),
            ("List density", ui.density.label().to_string()),
            ("Color theme", ui.theme_preset.label().to_string()),
            (
                "Download folder",
                app.download_dir.to_string_lossy().to_string(),
            ),
            ("Keyboard shortcuts", "View help".to_string()),
            ("Graph URL", app.config.graph_base_url.clone()),
            ("Config file", config::config_file_path().to_string_lossy().to_string()),
        ]
    }

    pub fn handle(&mut self, action: &Action, app: &AppState) -> OverlayOutcome {
        match action {
            Action::MoveUp => {
                self.selected = (self.selected + ITEMS - 1) % ITEMS;
                OverlayOutcome::Handled
            }
            Action::MoveDown => {
                self.selected = (self.selected + 1) % ITEMS;
                OverlayOutcome::Handled
            }
            Action::Activate => match self.selected {
                0 => OverlayOutcome::Dispatch(Action::ToggleNotify),
                1 => OverlayOutcome::Dispatch(Action::ToggleNavRail),
                2 => OverlayOutcome::Dispatch(Action::ToggleInlineImages),
                3 => OverlayOutcome::Dispatch(Action::TogglePreviewRenderMode),
                4 => OverlayOutcome::Dispatch(Action::CycleImageProtocol),
                5 => OverlayOutcome::Dispatch(Action::ToggleDensity),
                6 => OverlayOutcome::Dispatch(Action::CycleTheme),
                7 => OverlayOutcome::Dispatch(Action::ShowDownloadDir),
                8 => OverlayOutcome::CloseAndDispatch(Action::OpenOverlay(OverlayKind::Help)),
                9 => OverlayOutcome::Dispatch(Action::ShowToast(format!(
                    "Graph URL: {} (edit in config.toml)",
                    app.config.graph_base_url
                ))),
                _ => OverlayOutcome::Dispatch(Action::ShowToast(
                    config::config_file_path().display().to_string(),
                )),
            },
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
        let rect = centered_rect(area, 76, (ITEMS as u16 + 4).min(area.height));
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                " Settings · Enter Toggle/Open · Esc Close ",
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        crate::ui::overlays::draw_overlay_close_pill(frame, rect, ui, hitmap);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let rows = SettingsState::rows(app, ui);
        let visible = inner.height.saturating_sub(1) as usize;
        if visible > 0 {
            let active_sel = self.selected.min(rows.len() - 1);
            let skip = active_sel.saturating_sub(visible.saturating_sub(1));
            for (vis_row, (index, (label, value))) in rows
                .iter()
                .enumerate()
                .skip(skip)
                .take(visible)
                .enumerate()
            {
                let y = inner.y + vis_row as u16;
                let selected = index == active_sel;
                let style = if selected {
                    theme.selection()
                } else {
                    Style::default()
                };
                let used = unicode_width::UnicodeWidthStr::width(*label).max(25) + 3;
                let value = crate::ui::truncate_width(
                    value,
                    (inner.width as usize).saturating_sub(used + 2),
                );
                let line = Line::from(vec![
                    Span::styled(format!(" {:<25}", label), style),
                    Span::styled(value, theme.muted_style()),
                ]);
                frame.render_widget(Paragraph::new(line), Rect::new(inner.x, y, inner.width, 1));
                hitmap.add_line(inner.x, y, inner.width, HitTarget::OverlayItem { index });
            }
        }

        let hint_y = inner.y + inner.height.saturating_sub(1);
        frame.render_widget(
            Paragraph::new(Span::styled(
                " Changes are saved automatically to config.toml",
                theme.muted_style(),
            )),
            Rect::new(inner.x, hint_y, inner.width, 1),
        );
    }
}

fn toggle(on: bool) -> String {
    if on {
        "On".to_string()
    } else {
        "Off".to_string()
    }
}
