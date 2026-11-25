//! Unified Command Palette & Omni-Switcher (`Ctrl+P` / `Ctrl+K`).
//!
//! Prefix modes:
//! - (none): Sites, Drives, OneDrive views, and Commands
//! - `>`: Commands only (`>Upload`, `>New Folder`, `>Cycle Theme`, ...)
//! - `/`: Hands off query to `SearchOverlay` on Enter

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::ui::{
    action::Action,
    app::{AppState, UiState},
    hitmap::{HitMap, HitTarget},
    overlays::{centered_rect, OverlayKind, OverlayOutcome},
};

#[derive(Debug, Clone)]
pub struct PaletteItem {
    pub label: String,
    search_alias: String,
    pub shortcut: String,
    pub action: Action,
    base_priority: i32,
}

fn palette_commands() -> Vec<PaletteItem> {
    let cmds = [
        ("Go to OneDrive: My Files", "", Action::OpenOneDriveRoot),
        ("Go to OneDrive: Recent", "", Action::OpenRecent),
        ("Go to OneDrive: Shared with Me", "", Action::OpenSharedWithMe),
        ("Upload File (File Picker + zoxide)", "Ctrl+F", Action::OpenFilePicker),
        ("Paste Clipboard Image as PNG", "Ctrl+V", Action::UploadClipboardImage),
        ("New Folder", "n", Action::OpenNewFolder),
        ("Download Selected File", "d", Action::DownloadSelected),
        ("Rename Selected Item", "r", Action::OpenRenameItem),
        ("Delete Selected Item", "x / Del", Action::ConfirmDeleteSelected),
        ("Create & Copy Share Link", "s", Action::CreateAndCopyShareLink),
        ("Open Selected Item in Browser", "o", Action::OpenActiveWebUrl),
        ("HD Image / Interactive HTML Preview", "v / Alt+v", Action::OpenImagePreview),
        ("Serve Selected HTML Locally in Browser", "o", Action::ServeActiveHtml),
        ("Extract Links from File", "u", Action::OpenLinkExtractor),
        ("Toggle Inline Link URLs (href)", "h", Action::ToggleShowLinkUrls),
        ("Toggle Markdown/HTML Rendered Preview", "m", Action::TogglePreviewRenderMode),
        ("Cycle Sort Mode (Name / Modified / Size / Type)", "S", Action::CycleSortMode),
        ("Load More Items in This Folder", "L", Action::LoadMoreItems),
        ("Search Files (FTS5 + Graph)", "/", Action::OpenSearch),
        ("Sync / Refresh All", "Ctrl+R / F5", Action::RefreshAll),
        ("Toggle Left Sidebar", "Ctrl+B", Action::ToggleNavRail),
        ("Toggle List Density (Cozy / Compact)", "Ctrl+D", Action::ToggleDensity),
        ("Cycle Color Theme", "", Action::CycleTheme),
        ("Cycle Image Protocol", "", Action::CycleImageProtocol),
        ("Toggle Inline Image Previews", "", Action::ToggleInlineImages),
        ("Toggle Notifications", "", Action::ToggleNotify),
        ("Check for Updates", "", Action::CheckForUpdates),
        ("Settings", "Ctrl+,", Action::OpenOverlay(OverlayKind::Settings)),
        ("Keyboard Shortcuts Help", "?", Action::OpenOverlay(OverlayKind::Help)),
        ("Quit", "Ctrl+Q", Action::QuitConfirm),
    ];

    cmds.into_iter()
        .map(|(label, shortcut, action)| PaletteItem {
            label: label.to_string(),
            search_alias: String::new(),
            shortcut: shortcut.to_string(),
            action,
            base_priority: 0,
        })
        .collect()
}

fn drive_items(app: &AppState) -> Vec<PaletteItem> {
    let mut items = Vec::new();
    for site in &app.sites {
        if let Some(drives) = app.site_drives.get(&site.id) {
            for d in drives {
                items.push(PaletteItem {
                    label: format!("Library: {} / {}", site.effective_name(), d.name),
                    search_alias: format!("{} {}", site.name, d.name),
                    shortcut: "SharePoint".to_string(),
                    action: Action::SelectDrive {
                        drive_id: d.id.clone(),
                        label: d.display_label(),
                    },
                    base_priority: if site.is_followed { 140 } else { 90 },
                });
            }
        }
    }
    items
}

#[derive(Debug, Clone, Default)]
pub struct CommandPalette {
    pub query: String,
    pub selected: usize,
}

impl CommandPalette {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn filtered(&self, app: &AppState, _ui: &UiState) -> Vec<PaletteItem> {
        let raw = self.query.trim();
        let (mode, needle) = if let Some(rest) = raw.strip_prefix('>') {
            ('>', rest.trim().to_lowercase())
        } else if let Some(rest) = raw.strip_prefix('/') {
            let search_q = rest.trim().to_string();
            let label = if search_q.is_empty() {
                "Search all files…".to_string()
            } else {
                format!("Search files for \"{search_q}\"")
            };
            return vec![PaletteItem {
                label,
                search_alias: String::new(),
                shortcut: "Enter".to_string(),
                action: Action::OpenSearchWithQuery(search_q),
                base_priority: 500,
            }];
        } else {
            (' ', raw.to_lowercase())
        };

        let mut pool = Vec::new();
        match mode {
            '>' => pool.extend(palette_commands()),
            _ => {
                pool.extend(drive_items(app));
                pool.extend(palette_commands());
            }
        }

        let mut scored: Vec<(i32, PaletteItem)> = pool
            .into_iter()
            .filter_map(|item| {
                if needle.is_empty() {
                    return Some((item.base_priority, item));
                }
                let label_l = item.label.to_lowercase();
                let alias_l = item.search_alias.to_lowercase();
                let short_l = item.shortcut.to_lowercase();
                let match_score = if label_l.starts_with(&needle) || alias_l.starts_with(&needle) {
                    400
                } else if label_l.contains(&needle)
                    || alias_l.contains(&needle)
                    || short_l.contains(&needle)
                {
                    200
                } else if fuzzy_matches(&label_l, &needle)
                    || (!alias_l.is_empty() && fuzzy_matches(&alias_l, &needle))
                {
                    80
                } else {
                    return None;
                };
                Some((match_score + item.base_priority, item))
            })
            .collect();

        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.label.cmp(&b.1.label)));
        scored.into_iter().map(|(_, item)| item).collect()
    }

    pub fn handle(&mut self, action: &Action, app: &AppState, ui: &UiState) -> OverlayOutcome {
        match action {
            Action::Input(ch) => {
                self.query.push(*ch);
                self.selected = 0;
                OverlayOutcome::Handled
            }
            Action::Paste(text) => {
                self.query.push_str(&text.replace(['\r', '\n'], " "));
                self.selected = 0;
                OverlayOutcome::Handled
            }
            Action::Backspace => {
                self.query.pop();
                self.selected = 0;
                OverlayOutcome::Handled
            }
            Action::MoveUp => {
                let len = self.filtered(app, ui).len();
                if len > 0 {
                    let cur = self.selected.min(len - 1);
                    self.selected = (cur + len - 1) % len;
                }
                OverlayOutcome::Handled
            }
            Action::MoveDown => {
                let len = self.filtered(app, ui).len();
                if len > 0 {
                    let cur = self.selected.min(len - 1);
                    self.selected = (cur + 1) % len;
                }
                OverlayOutcome::Handled
            }
            Action::Activate | Action::OverlaySubmit => {
                let items = self.filtered(app, ui);
                let idx = self.selected.min(items.len().saturating_sub(1));
                match items.get(idx) {
                    Some(item) => OverlayOutcome::CloseAndDispatch(item.action.clone()),
                    None => OverlayOutcome::Handled,
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
            area.width.saturating_sub(6).min(74),
            area.height.saturating_sub(4).min(20),
        );
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                " Command Palette · > cmd  / search · Esc Close ",
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        crate::ui::overlays::draw_overlay_close_pill(frame, rect, ui, hitmap);
        if inner.height == 0 || inner.width == 0 {
            return;
        }

        let query_line = Line::from(vec![
            Span::styled("> ", theme.title(true)),
            Span::raw(self.query.clone()),
            Span::styled("▏", theme.muted_style()),
        ]);
        frame.render_widget(
            Paragraph::new(query_line),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );

        let items = self.filtered(app, ui);
        let active_sel = if items.is_empty() {
            0
        } else {
            self.selected.min(items.len() - 1)
        };
        let list_y = inner.y.saturating_add(2);
        let visible = inner.height.saturating_sub(2) as usize;
        if visible == 0 {
            return;
        }
        if items.is_empty() {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    " No matching libraries or commands",
                    theme.muted_style(),
                )),
                Rect::new(inner.x, list_y, inner.width, 1),
            );
            return;
        }

        let skip = active_sel.saturating_sub(visible.saturating_sub(1));
        for (row, (index, item)) in items
            .iter()
            .enumerate()
            .skip(skip)
            .take(visible)
            .enumerate()
        {
            let y = list_y + row as u16;
            if y >= inner.y + inner.height {
                break;
            }
            let is_selected = index == active_sel;
            let style = if is_selected {
                theme.selection()
            } else {
                Style::default()
            };
            let right_w = unicode_width::UnicodeWidthStr::width(item.shortcut.as_str())
                .min(20)
                .max(8);
            let label_w = (inner.width as usize).saturating_sub(right_w + 3).max(10);
            let truncated_label = crate::ui::truncate_width(&item.label, label_w);
            let used_w = unicode_width::UnicodeWidthStr::width(truncated_label.as_str());
            let pad = label_w.saturating_sub(used_w);
            let line = Line::from(vec![
                Span::styled(format!(" {truncated_label}{}", " ".repeat(pad)), style),
                Span::styled(
                    crate::ui::truncate_width(&item.shortcut, right_w),
                    theme.muted_style(),
                ),
            ]);
            frame.render_widget(Paragraph::new(line), Rect::new(inner.x, y, inner.width, 1));
            hitmap.add_line(inner.x, y, inner.width, HitTarget::OverlayItem { index });
        }
    }

    pub fn select(&mut self, index: usize) {
        self.selected = index;
    }
}

fn fuzzy_matches(haystack: &str, needle: &str) -> bool {
    let mut chars = needle.chars();
    let Some(mut target) = chars.next() else {
        return true;
    };
    for ch in haystack.chars() {
        if ch == target {
            match chars.next() {
                Some(next) => target = next,
                None => return true,
            }
        }
    }
    false
}
