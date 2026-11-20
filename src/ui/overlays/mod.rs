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
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
    Frame,
};

use crate::ui::{
    action::Action,
    app::{AppState, UiState},
    hitmap::{HitMap, HitTarget},
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
        Overlay::Help(HelpOverlay)
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
            Overlay::Help(_) => match action {
                Action::CloseOverlay | Action::Activate | Action::Input('q') => {
                    OverlayOutcome::Close
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
            Overlay::Help(_) => {
                draw_help(frame, area, ui, hitmap);
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
pub struct HelpOverlay;

const HELP_SECTIONS: [(&str, &[(&str, &str)]); 4] = [
    (
        "Navigation & Tree",
        &[
            ("Tab / Shift+Tab", "Switch Sidebar <-> Files <-> Inspector"),
            ("j / k · ↑ / ↓", "Move selection / scroll inspector"),
            ("Enter", "Open folder / toggle site / inspect file"),
            ("Backspace / ←", "Navigate to parent folder"),
            ("S", "Cycle sort (Name / Modified / Size / Type)"),
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

fn draw_help(frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
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
            " Microsoft SharePoint & OneDrive TUI Help · Esc Close ",
            theme.title(true),
        )))
        .border_style(theme.border_style(true));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(inner);

    for (index, (title, entries)) in HELP_SECTIONS.iter().enumerate() {
        let target = columns[index % 2];
        if target.width == 0 {
            continue;
        }
        let y_offset = if index < 2 { 0 } else { target.height / 2 };
        let mut lines = Vec::with_capacity(entries.len() + 1);
        lines.push(Line::from(Span::styled(
            format!(" {title}"),
            theme.title(true),
        )));
        for (key, desc) in entries.iter() {
            lines.push(Line::from(vec![
                Span::styled(format!("  {key:<20}"), theme.accent_style()),
                Span::raw(desc.to_string()),
            ]));
        }
        let sub = Rect::new(
            target.x,
            target.y + y_offset,
            target.width,
            target.height.saturating_sub(y_offset),
        );
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), sub);
    }
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
