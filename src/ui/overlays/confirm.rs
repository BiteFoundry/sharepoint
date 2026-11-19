//! Confirmation dialog for destructive actions (e.g., message deletion).

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
    Frame,
};

use crate::ui::{
    action::Action,
    app::UiState,
    hitmap::{ButtonId, HitMap, HitTarget},
    overlays::{centered_rect, OverlayOutcome},
};

#[derive(Debug, Clone)]
pub struct ConfirmDialog {
    pub message: String,
    pub on_confirm: Option<Action>,
}

impl ConfirmDialog {
    pub fn new(message: impl Into<String>, on_confirm: Option<Action>) -> Self {
        Self {
            message: message.into(),
            on_confirm,
        }
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::Activate | Action::Input('y') => {
                let action = self.on_confirm.clone().unwrap_or(Action::Noop);
                OverlayOutcome::CloseAndDispatch(action)
            }
            Action::CloseOverlay | Action::Input('n') => OverlayOutcome::Close,
            Action::Quit => OverlayOutcome::NotHandled,
            Action::QuitConfirm => {
                if matches!(self.on_confirm, Some(Action::Quit)) {
                    OverlayOutcome::Handled
                } else {
                    OverlayOutcome::NotHandled
                }
            }
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
        let theme = &ui.theme;
        let rect = centered_rect(area, 56, 9);
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(" Confirm ", theme.title(true))))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let msg_area = Rect::new(
            inner.x,
            inner.y.saturating_add(1),
            inner.width,
            inner.height.saturating_sub(3).max(1),
        );
        frame.render_widget(
            Paragraph::new(Span::styled(self.message.clone(), theme.muted_style()))
                .wrap(Wrap { trim: true }),
            msg_area,
        );

        let btn_y = inner.y + inner.height.saturating_sub(1);
        let ok_label = " [Enter] Confirm ";
        let cancel_label = " [Esc] Cancel ";
        let ok_w = ok_label.len() as u16;
        let cancel_w = cancel_label.len() as u16;

        let ok_x = inner.x.saturating_add(1);
        if ok_x < inner.x + inner.width {
            let w = ok_w.min(inner.x + inner.width - ok_x);
            frame.render_widget(
                Paragraph::new(Span::styled(ok_label, theme.selection())),
                Rect::new(ok_x, btn_y, w, 1),
            );
            hitmap.add_line(ok_x, btn_y, w, HitTarget::Button(ButtonId::ConfirmOk));
        }

        let cancel_x = ok_x.saturating_add(ok_w + 3);
        if cancel_x < inner.x + inner.width {
            let w = cancel_w.min(inner.x + inner.width - cancel_x);
            frame.render_widget(
                Paragraph::new(Span::styled(cancel_label, theme.muted_style())),
                Rect::new(cancel_x, btn_y, w, 1),
            );
            hitmap.add_line(cancel_x, btn_y, w, HitTarget::Button(ButtonId::ConfirmCancel));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn confirm_dispatches_and_cancel_closes() {
        let mut dialog = ConfirmDialog::new("Delete this item?", Some(Action::Quit));
        assert!(matches!(
            dialog.handle(&Action::Activate),
            OverlayOutcome::CloseAndDispatch(Action::Quit)
        ));
        assert!(matches!(
            dialog.handle(&Action::CloseOverlay),
            OverlayOutcome::Close
        ));
    }

    #[test]
    fn renders_in_test_backend() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let ui = UiState::new();
        let dialog = ConfirmDialog::new("Delete?", None);
        let mut hitmap = HitMap::new();
        terminal
            .draw(|frame| dialog.draw(frame, frame.area(), &ui, &mut hitmap))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("[Enter] Confirm"));
        assert!(text.contains("[Esc] Cancel"));
        assert!(hitmap.region_of(HitTarget::Button(ButtonId::ConfirmOk)).is_some());
        assert!(hitmap.region_of(HitTarget::Button(ButtonId::ConfirmCancel)).is_some());
    }
}
