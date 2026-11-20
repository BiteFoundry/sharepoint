//! Right-click and `.` Context Menu for OneDrive / SharePoint items and global actions.

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::ui::{
    action::Action,
    app::{AppState, UiState},
    hitmap::{HitMap, HitTarget},
    overlays::{OverlayKind, OverlayOutcome},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextTarget {
    DriveItem(usize),
    Global,
}

#[derive(Debug, Clone)]
pub struct MenuItem {
    pub label: String,
    pub shortcut: Option<String>,
    pub action: Action,
}

impl MenuItem {
    fn new(label: impl Into<String>, shortcut: Option<&str>, action: Action) -> Self {
        Self {
            label: label.into(),
            shortcut: shortcut.map(str::to_string),
            action,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ContextMenu {
    pub context: ContextTarget,
    pub anchor: (u16, u16),
    pub items: Vec<MenuItem>,
    pub selected: usize,
}

impl ContextMenu {
    pub fn build(target: ContextTarget, anchor: (u16, u16), app: &AppState, _ui: &UiState) -> Self {
        let items = match target {
            ContextTarget::DriveItem(_) => {
                let mut list = Vec::new();
                if let Some(item) = app.active_item() {
                    if item.is_folder {
                        list.push(MenuItem::new("Open Folder", Some("Enter"), Action::Activate));
                    } else {
                        list.push(MenuItem::new("Download File", Some("d"), Action::DownloadSelected));
                    }
                    if item.is_image() {
                        list.push(MenuItem::new("HD Image Preview", Some("v"), Action::OpenImagePreview));
                    }
                    if item.markup_kind().is_some() {
                        list.push(MenuItem::new(
                            "Toggle Rendered / Raw Preview",
                            Some("m"),
                            Action::TogglePreviewRenderMode,
                        ));
                    }
                    list.push(MenuItem::new("Create & Copy Share Link", Some("s"), Action::CreateAndCopyShareLink));
                    list.push(MenuItem::new("Extract Links", Some("u"), Action::OpenLinkExtractor));
                    list.push(MenuItem::new("Open in Browser", Some("o"), Action::OpenActiveWebUrl));
                    list.push(MenuItem::new("Rename", Some("r"), Action::OpenRenameItem));
                    list.push(MenuItem::new("Pin / Unpin", Some("p"), Action::TogglePinCurrent));
                    list.push(MenuItem::new("Delete", Some("x / Del"), Action::ConfirmDeleteSelected));
                }
                list.push(MenuItem::new("Upload File…", Some("Ctrl+F"), Action::OpenFilePicker));
                list.push(MenuItem::new("Paste Clipboard PNG", Some("Ctrl+V"), Action::UploadClipboardImage));
                list.push(MenuItem::new("New Folder…", Some("n"), Action::OpenNewFolder));
                list
            }
            ContextTarget::Global => vec![
                MenuItem::new("Upload File…", Some("Ctrl+F"), Action::OpenFilePicker),
                MenuItem::new("Paste Clipboard PNG", Some("Ctrl+V"), Action::UploadClipboardImage),
                MenuItem::new("New Folder…", Some("n"), Action::OpenNewFolder),
                MenuItem::new("Search Files", Some("/"), Action::OpenSearch),
                MenuItem::new("Command Palette", Some("Ctrl+P"), Action::OpenOverlay(OverlayKind::CommandPalette)),
                MenuItem::new("Refresh", Some("Ctrl+R"), Action::RefreshAll),
                MenuItem::new("Settings", Some("Ctrl+,"), Action::OpenOverlay(OverlayKind::Settings)),
                MenuItem::new("Help", Some("?"), Action::OpenOverlay(OverlayKind::Help)),
                MenuItem::new("Quit", Some("Ctrl+Q"), Action::QuitConfirm),
            ],
        };

        Self {
            context: target,
            anchor,
            items,
            selected: 0,
        }
    }

    pub fn next(&mut self) {
        if !self.items.is_empty() {
            self.selected = (self.selected + 1) % self.items.len();
        }
    }

    pub fn prev(&mut self) {
        if !self.items.is_empty() {
            self.selected = (self.selected + self.items.len() - 1) % self.items.len();
        }
    }

    pub fn activate(&self) -> OverlayOutcome {
        match self.items.get(self.selected) {
            Some(item) => OverlayOutcome::CloseAndDispatch(item.action.clone()),
            None => OverlayOutcome::Close,
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
        let theme = &ui.theme;
        let label_width = self
            .items
            .iter()
            .map(|item| {
                unicode_width::UnicodeWidthStr::width(item.label.as_str())
                    + item
                        .shortcut
                        .as_deref()
                        .map_or(0, |shortcut| shortcut.len() + 2)
            })
            .max()
            .unwrap_or(20);
        let width = (label_width as u16 + 4).clamp(24, 48);
        let height = (self.items.len() as u16 + 2).min(area.height);
        let rect = anchored_rect(area, self.anchor, width, height);

        let title = match self.context {
            ContextTarget::DriveItem(_) => " File Menu ",
            ContextTarget::Global => " Menu ",
        };
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(title, theme.title(true))))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);

        for (index, item) in self.items.iter().enumerate() {
            if index as u16 >= inner.height {
                break;
            }
            let row = inner.y + index as u16;
            let selected = index == self.selected;
            let label_style = if selected {
                theme.selection().remove_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let hint_style = if selected {
                theme.selection()
            } else {
                theme.muted_style()
            };
            let hint = item.shortcut.clone().unwrap_or_default();
            let hint_w = unicode_width::UnicodeWidthStr::width(hint.as_str());
            let max_label_w = (inner.width as usize).saturating_sub(hint_w + 2);
            let label = crate::ui::truncate_width(&item.label, max_label_w);
            let used = unicode_width::UnicodeWidthStr::width(label.as_str());
            let pad = (inner.width as usize)
                .saturating_sub(used)
                .saturating_sub(hint_w)
                .saturating_sub(2);
            let line = Line::from(vec![
                Span::styled(format!(" {label}"), label_style),
                Span::styled(" ".repeat(pad), label_style),
                Span::styled(hint, hint_style),
                Span::styled(" ", label_style),
            ]);
            frame.render_widget(
                Paragraph::new(line),
                Rect::new(inner.x, row, inner.width, 1),
            );
            hitmap.add_line(inner.x, row, inner.width, HitTarget::OverlayItem { index });
        }
    }

    pub fn select(&mut self, index: usize) {
        if index < self.items.len() {
            self.selected = index;
        }
    }
}

fn anchored_rect(area: Rect, anchor: (u16, u16), width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let x = anchor.0.min(area.x + area.width.saturating_sub(width));
    let y = anchor.1.min(area.y + area.height.saturating_sub(height));
    Rect::new(x, y, width, height)
}
