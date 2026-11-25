//! Overlay stack & HelpOverlay (`?`).

pub mod command_palette;
pub mod confirm;
pub mod context_menu;
pub mod file_picker;
pub mod html_preview;
pub mod image_preview;
pub mod links;
pub mod quick_input;
pub mod search;
pub mod settings;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Wrap,
    },
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::ui::{
    action::Action,
    app::{AppState, UiState},
    hitmap::{ButtonId, HitMap, HitTarget},
    overlays::{
        command_palette::CommandPalette, confirm::ConfirmDialog, context_menu::ContextMenu,
        file_picker::FilePickerOverlay, html_preview::HtmlPreviewOverlay,
        image_preview::ImagePreviewOverlay, links::LinkExtractorOverlay,
        quick_input::QuickInputOverlay, search::SearchOverlay, settings::SettingsState,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayKind {
    ContextMenu,
    CommandPalette,
    Settings,
    Search,
    FilePicker,
    Links,
    QuickInput,
    ImagePreview,
    HtmlPreview,
    Help,
    Confirm,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayOutcome {
    Handled,
    Close,
    Dispatch(Action),
    CloseAndDispatch(Action),
    NotHandled,
}

#[derive(Debug)]
pub enum Overlay {
    ContextMenu(ContextMenu),
    CommandPalette(CommandPalette),
    Settings(SettingsState),
    Search(SearchOverlay),
    FilePicker(FilePickerOverlay),
    Links(LinkExtractorOverlay),
    QuickInput(QuickInputOverlay),
    ImagePreview(ImagePreviewOverlay),
    HtmlPreview(HtmlPreviewOverlay),
    Help(HelpOverlay),
    Confirm(ConfirmDialog),
}

impl Overlay {
    pub fn kind(&self) -> OverlayKind {
        match self {
            Overlay::ContextMenu(_) => OverlayKind::ContextMenu,
            Overlay::CommandPalette(_) => OverlayKind::CommandPalette,
            Overlay::Settings(_) => OverlayKind::Settings,
            Overlay::Search(_) => OverlayKind::Search,
            Overlay::FilePicker(_) => OverlayKind::FilePicker,
            Overlay::Links(_) => OverlayKind::Links,
            Overlay::QuickInput(_) => OverlayKind::QuickInput,
            Overlay::ImagePreview(_) => OverlayKind::ImagePreview,
            Overlay::HtmlPreview(_) => OverlayKind::HtmlPreview,
            Overlay::Help(_) => OverlayKind::Help,
            Overlay::Confirm(_) => OverlayKind::Confirm,
        }
    }

    pub fn accepts_text(&self) -> bool {
        match self {
            Overlay::Search(_)
            | Overlay::CommandPalette(_)
            | Overlay::FilePicker(_)
            | Overlay::QuickInput(_) => true,
            Overlay::HtmlPreview(html) => html.is_editing_input(),
            _ => false,
        }
    }

    pub fn context_menu(menu: ContextMenu) -> Self {
        Overlay::ContextMenu(menu)
    }

    pub fn command_palette() -> Self {
        Overlay::CommandPalette(CommandPalette::new())
    }

    pub fn settings() -> Self {
        Overlay::Settings(SettingsState::default())
    }

    pub fn search() -> Self {
        Overlay::Search(SearchOverlay::default())
    }

    pub fn file_picker() -> Self {
        Overlay::FilePicker(FilePickerOverlay::new())
    }

    pub fn links(overlay: LinkExtractorOverlay) -> Self {
        Overlay::Links(overlay)
    }

    pub fn quick_input(quick: QuickInputOverlay) -> Self {
        Overlay::QuickInput(quick)
    }

    pub fn help() -> Self {
        Overlay::Help(HelpOverlay::default())
    }

    pub fn confirm(message: impl Into<String>, on_confirm: Option<Action>) -> Self {
        Overlay::Confirm(ConfirmDialog::new(message, on_confirm))
    }

    pub fn handle(&mut self, action: &Action, app: &AppState, ui: &UiState) -> OverlayOutcome {
        let wheel_step = match action {
            Action::MouseScroll { delta, .. } if *delta < 0 => Some(Action::MoveUp),
            Action::MouseScroll { delta, .. } if *delta > 0 => Some(Action::MoveDown),
            _ => None,
        };
        match self {
            Overlay::ContextMenu(menu) => match wheel_step.as_ref().unwrap_or(action) {
                Action::MoveUp => {
                    menu.prev();
                    OverlayOutcome::Handled
                }
                Action::MoveDown => {
                    menu.next();
                    OverlayOutcome::Handled
                }
                Action::Activate => menu.activate(),
                Action::CloseOverlay => OverlayOutcome::Close,
                Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
                _ => OverlayOutcome::Handled,
            },
            Overlay::CommandPalette(palette) => match wheel_step.as_ref().unwrap_or(action) {
                Action::Tab => palette.handle(&Action::MoveDown, app, ui),
                Action::PrevFocus => palette.handle(&Action::MoveUp, app, ui),
                other => palette.handle(other, app, ui),
            },
            Overlay::Settings(settings) => match wheel_step.as_ref().unwrap_or(action) {
                Action::Tab => settings.handle(&Action::MoveDown, app),
                Action::PrevFocus => settings.handle(&Action::MoveUp, app),
                other => settings.handle(other, app),
            },
            Overlay::Search(search) => search.handle(wheel_step.as_ref().unwrap_or(action), app),
            Overlay::FilePicker(picker) => picker.handle(wheel_step.as_ref().unwrap_or(action)),
            Overlay::Links(links) => links.handle(wheel_step.as_ref().unwrap_or(action)),
            Overlay::QuickInput(input) => input.handle(action),
            Overlay::ImagePreview(preview) => preview.handle(action),
            Overlay::HtmlPreview(html) => html.handle(action, ui),
            Overlay::Help(help) => match action {
                Action::CloseOverlay | Action::Activate | Action::Input('q') => {
                    OverlayOutcome::Close
                }
                Action::MoveUp | Action::CursorUp | Action::Input('k') => {
                    help.scroll_y = help.scroll_y.saturating_sub(1);
                    OverlayOutcome::Handled
                }
                Action::MoveDown | Action::CursorDown | Action::Input('j') => {
                    help.scroll_y = help.scroll_y.saturating_add(1).min(help.max_scroll_y);
                    OverlayOutcome::Handled
                }
                Action::ScrollPageUp => {
                    help.scroll_y = help.scroll_y.saturating_sub(6);
                    OverlayOutcome::Handled
                }
                Action::ScrollPageDown => {
                    help.scroll_y = help.scroll_y.saturating_add(6).min(help.max_scroll_y);
                    OverlayOutcome::Handled
                }
                Action::ScrollTop | Action::CursorHome | Action::Input('g') => {
                    help.scroll_y = 0;
                    OverlayOutcome::Handled
                }
                Action::ScrollBottom | Action::CursorEnd | Action::Input('G') => {
                    help.scroll_y = help.max_scroll_y;
                    OverlayOutcome::Handled
                }
                Action::MouseScroll { delta, .. } => {
                    if *delta < 0 {
                        help.scroll_y = help.scroll_y.saturating_sub((-*delta) as u16);
                    } else {
                        help.scroll_y = help
                            .scroll_y
                            .saturating_add(*delta as u16)
                            .min(help.max_scroll_y);
                    }
                    OverlayOutcome::Handled
                }
                Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
                _ => OverlayOutcome::Handled,
            },
            Overlay::Confirm(dialog) => dialog.handle(action),
        }
    }

    pub fn select(&mut self, index: usize) {
        match self {
            Overlay::ContextMenu(menu) => menu.select(index),
            Overlay::CommandPalette(palette) => palette.select(index),
            Overlay::Settings(settings) => settings.select(index),
            Overlay::Search(search) => search.select(index),
            Overlay::FilePicker(picker) => picker.select(index),
            Overlay::Links(links) => links.select(index),
            Overlay::QuickInput(_)
            | Overlay::ImagePreview(_)
            | Overlay::HtmlPreview(_)
            | Overlay::Help(_)
            | Overlay::Confirm(_) => {}
        }
    }

    pub fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        app: &AppState,
        ui: &mut UiState,
        hitmap: &mut HitMap,
    ) -> Option<(u16, u16)> {
        match self {
            Overlay::ContextMenu(menu) => {
                menu.draw(frame, area, ui, hitmap);
                None
            }
            Overlay::CommandPalette(palette) => {
                palette.draw(frame, area, app, ui, hitmap);
                None
            }
            Overlay::Settings(settings) => {
                settings.draw(frame, area, app, ui, hitmap);
                None
            }
            Overlay::Search(search) => {
                search.draw(frame, area, app, ui, hitmap);
                None
            }
            Overlay::FilePicker(picker) => {
                picker.draw(frame, area, ui, hitmap);
                None
            }
            Overlay::Links(links) => {
                links.draw(frame, area, ui, hitmap);
                None
            }
            Overlay::QuickInput(input) => {
                input.draw(frame, area, ui, hitmap);
                None
            }
            Overlay::ImagePreview(preview) => {
                preview.draw(frame, area, app, ui, hitmap);
                None
            }
            Overlay::HtmlPreview(html) => {
                html.draw(frame, area, app, ui, hitmap);
                None
            }
            Overlay::Help(help) => {
                help.draw(frame, area, ui, hitmap);
                None
            }
            Overlay::Confirm(dialog) => {
                dialog.draw(frame, area, ui, hitmap);
                None
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct HelpOverlay {
    pub scroll_y: u16,
    pub max_scroll_y: u16,
}

const HELP_SECTIONS: [(&str, &[(&str, &str)]); 4] = [
    (
        "Navigation & Tree",
        &[
            ("Tab / Shift+Tab", "Switch Sidebar <-> Files <-> Inspector"),
            ("j / k · ↑ / ↓", "Move selection / scroll inspector"),
            ("Enter", "Open folder / toggle site / inspect file"),
            ("Backspace / ←", "Navigate to parent folder"),
            ("S", "Cycle sort (Name / Modified / Size / Type)"),
            ("L", "Load 200 more items when a folder is cut short"),
            ("Ctrl+B", "Toggle left sidebar"),
            ("Ctrl+R / F5", "Refresh current folder & sites"),
        ],
    ),
    (
        "File Operations",
        &[
            ("d", "Download selected file"),
            ("Ctrl+F", "Upload file (File Picker with zoxide jump z)"),
            ("Ctrl+V", "Upload clipboard image as PNG immediately"),
            ("n", "Create new folder in current directory"),
            ("r", "Rename selected file or folder"),
            ("x / Del", "Delete selected item (with confirm)"),
            ("s / o", "Create & copy share link / open in browser"),
        ],
    ),
    (
        "Preview & Links",
        &[
            ("v / Alt+v", "HD image or interactive HTML preview overlay"),
            ("m", "Toggle Rendered / Raw preview (.md / .html)"),
            ("u", "Link Extractor (web URL, share link, file URLs)"),
            ("h", "Toggle inline href URLs in inspector"),
            ("p", "Pin / Unpin current folder or site"),
            ("/", "Search files (FTS5 local + Graph remote)"),
            (". / Right-click", "Open context menu"),
        ],
    ),
    (
        "Palette & Preferences",
        &[
            ("Ctrl+P / Ctrl+K", "Command Palette / Omni-Switcher"),
            ("Ctrl+D", "Toggle Cozy / Compact list density"),
            ("Ctrl+,", "Settings (Theme, Image Protocol, Notifications)"),
            ("?", "Toggle this help modal"),
            ("Ctrl+Q / Ctrl+C", "Quit SharePoint / OneDrive TUI"),
        ],
    ),
];

impl HelpOverlay {
    fn build_section_lines(
        sections: &[usize],
        theme: &crate::ui::theme::Theme,
    ) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for (idx, &sec_idx) in sections.iter().enumerate() {
            if idx > 0 {
                lines.push(Line::raw(""));
            }
            let (title, entries) = HELP_SECTIONS[sec_idx];
            lines.push(Line::from(Span::styled(
                format!(" {title}"),
                theme.title(true),
            )));
            for (key, desc) in entries {
                lines.push(Line::from(vec![
                    Span::styled(format!("  {key:<20}"), theme.accent_style()),
                    Span::raw((*desc).to_string()),
                ]));
            }
        }
        lines
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
        let theme = &ui.theme;
        let rect = centered_rect(
            area,
            area.width.saturating_sub(4).min(100),
            area.height.saturating_sub(3).min(24),
        );
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                " Microsoft SharePoint & OneDrive TUI Help · ↑↓/Wheel Scroll · Esc Close ",
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        draw_overlay_close_pill(frame, rect, ui, hitmap);

        if inner.width == 0 || inner.height == 0 {
            self.max_scroll_y = 0;
            self.scroll_y = 0;
            return;
        }

        if inner.width >= 76 {
            let left_lines = Self::build_section_lines(&[0, 2], theme);
            let right_lines = Self::build_section_lines(&[1, 3], theme);
            let initial_cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(inner);
            let raw_max = crate::ui::wrapped_line_count(&left_lines, initial_cols[0].width)
                .max(crate::ui::wrapped_line_count(&right_lines, initial_cols[1].width));
            let show_sb = raw_max > inner.height as usize && inner.width > 4;
            let content_area = if show_sb {
                Rect::new(inner.x, inner.y, inner.width.saturating_sub(1), inner.height)
            } else {
                inner
            };
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(content_area);
            let total_rows = crate::ui::wrapped_line_count(&left_lines, columns[0].width)
                .max(crate::ui::wrapped_line_count(&right_lines, columns[1].width));
            self.max_scroll_y = total_rows
                .saturating_sub(inner.height as usize)
                .min(u16::MAX as usize) as u16;
            self.scroll_y = self.scroll_y.min(self.max_scroll_y);

            frame.render_widget(
                Paragraph::new(left_lines)
                    .wrap(Wrap { trim: false })
                    .scroll((self.scroll_y, 0)),
                columns[0],
            );
            frame.render_widget(
                Paragraph::new(right_lines)
                    .wrap(Wrap { trim: false })
                    .scroll((self.scroll_y, 0)),
                columns[1],
            );

            if show_sb {
                let sb_area = Rect::new(inner.right().saturating_sub(1), inner.y, 1, inner.height);
                let mut sb_state =
                    ScrollbarState::new(self.max_scroll_y.saturating_add(1) as usize)
                        .position(self.scroll_y as usize);
                let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .thumb_style(Style::default().fg(theme.accent))
                    .track_style(theme.muted_style());
                frame.render_stateful_widget(scrollbar, sb_area, &mut sb_state);
            }
        } else {
            let all_lines = Self::build_section_lines(&[0, 1, 2, 3], theme);
            let raw_rows = crate::ui::wrapped_line_count(&all_lines, inner.width);
            let show_sb = raw_rows > inner.height as usize && inner.width > 2;
            let content_area = if show_sb {
                Rect::new(inner.x, inner.y, inner.width.saturating_sub(1), inner.height)
            } else {
                inner
            };
            let total_rows = crate::ui::wrapped_line_count(&all_lines, content_area.width);
            self.max_scroll_y = total_rows
                .saturating_sub(inner.height as usize)
                .min(u16::MAX as usize) as u16;
            self.scroll_y = self.scroll_y.min(self.max_scroll_y);

            frame.render_widget(
                Paragraph::new(all_lines)
                    .wrap(Wrap { trim: false })
                    .scroll((self.scroll_y, 0)),
                content_area,
            );
            if show_sb {
                let sb_area = Rect::new(inner.right().saturating_sub(1), inner.y, 1, inner.height);
                let mut sb_state =
                    ScrollbarState::new(self.max_scroll_y.saturating_add(1) as usize)
                        .position(self.scroll_y as usize);
                let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .thumb_style(Style::default().fg(theme.accent))
                    .track_style(theme.muted_style());
                frame.render_stateful_widget(scrollbar, sb_area, &mut sb_state);
            }
        }
    }
}

/// Renders a clickable `[✕ Esc]` close pill on the top-right border of a modal when space allows.
pub fn draw_overlay_close_pill(frame: &mut Frame, rect: Rect, ui: &UiState, hitmap: &mut HitMap) {
    let close_label = "[✕ Esc]";
    let close_w = close_label.width() as u16;
    if rect.width <= close_w + 6 || rect.height == 0 {
        return;
    }
    let close_x = rect.right().saturating_sub(close_w + 1);
    let close_rect = Rect::new(close_x, rect.y, close_w, 1);
    let theme = &ui.theme;
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
        HitTarget::Button(ButtonId::OverlayClose),
    );
}

pub fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}
