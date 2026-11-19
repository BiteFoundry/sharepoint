//! Single-line quick input modal (`n` new folder, `r` rename item).

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::ui::{
    action::Action,
    app::UiState,
    hitmap::{HitMap, HitTarget},
    overlays::{centered_rect, OverlayOutcome},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuickInputKind {
    NewFolder,
    RenameItem,
}

#[derive(Debug, Clone)]
pub struct QuickInputOverlay {
    pub kind: QuickInputKind,
    pub value: String,
    pub target_drive_id: Option<String>,
    pub target_item_id: Option<String>,
    pub error: Option<String>,
}

impl QuickInputOverlay {
    pub fn new_folder() -> Self {
        Self {
            kind: QuickInputKind::NewFolder,
            value: String::new(),
            target_drive_id: None,
            target_item_id: None,
            error: None,
        }
    }

    pub fn rename_item(drive_id: String, item_id: String, current_name: String) -> Self {
        Self {
            kind: QuickInputKind::RenameItem,
            value: current_name,
            target_drive_id: Some(drive_id),
            target_item_id: Some(item_id),
            error: None,
        }
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::Input(ch) => {
                self.value.push(*ch);
                self.error = None;
                OverlayOutcome::Handled
            }
            Action::Paste(text) => {
                self.value.push_str(&text.replace(['\r', '\n'], " "));
                self.error = None;
                OverlayOutcome::Handled
            }
            Action::Backspace => {
                self.value.pop();
                self.error = None;
                OverlayOutcome::Handled
            }
            Action::Activate | Action::OverlaySubmit => {
                let trimmed = self.value.trim().to_string();
                if trimmed.is_empty() {
                    self.error = Some("Enter a name".to_string());
                    return OverlayOutcome::Handled;
                }
                match self.kind {
                    QuickInputKind::NewFolder => {
                        OverlayOutcome::CloseAndDispatch(Action::CreateFolder { name: trimmed })
                    }
                    QuickInputKind::RenameItem => OverlayOutcome::CloseAndDispatch(
                        Action::RenameItem {
                            drive_id: self.target_drive_id.clone().unwrap_or_default(),
                            item_id: self.target_item_id.clone().unwrap_or_default(),
                            new_name: trimmed,
                        },
                    ),
                }
            }
            Action::CloseOverlay => OverlayOutcome::Close,
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
        let theme = &ui.theme;
        let rect = centered_rect(
            area,
            area.width.saturating_sub(8).min(64),
            area.height.saturating_sub(4).min(7),
        );
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let title = match self.kind {
            QuickInputKind::NewFolder => " New Folder · Enter Create · Esc Cancel ",
            QuickInputKind::RenameItem => " Rename Item · Enter Rename · Esc Cancel ",
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(title, theme.title(true))))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let prompt = match self.kind {
            QuickInputKind::NewFolder => " Folder name:",
            QuickInputKind::RenameItem => " New name:",
        };
        let mut lines = vec![
            Line::from(Span::styled(prompt, theme.muted_style())),
            Line::from(vec![
                Span::styled(" > ", theme.accent_style()),
                Span::raw(self.value.clone()),
                Span::styled("▏", theme.accent_style()),
            ]),
        ];
        if let Some(err) = &self.error {
            lines.push(Line::from(Span::styled(
                format!(" {err}"),
                Style::default().fg(theme.error),
            )));
        }
        frame.render_widget(Paragraph::new(lines), inner);
    }
}
