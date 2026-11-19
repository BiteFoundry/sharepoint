//! Interactive File Picker Overlay (`Ctrl+F`): browse local directories and
//! files to upload into the active OneDrive/SharePoint folder, with type-to-filter,
//! Backspace parent navigation, and `z` key directory jumping via `zoxide query -l`.

use std::path::{Path, PathBuf};
use std::process::Command;

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePickerEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilePickerMode {
    Browse,
    ZoxideJump,
}

#[derive(Debug, Clone)]
pub struct FilePickerOverlay {
    pub current_dir: PathBuf,
    pub entries: Vec<FilePickerEntry>,
    pub filter: String,
    pub selected: usize,
    pub mode: FilePickerMode,
    pub zoxide_dirs: Vec<PathBuf>,
    pub zoxide_query: String,
    pub zoxide_selected: usize,
    pub status: Option<String>,
    pub error: Option<String>,
}

impl Default for FilePickerOverlay {
    fn default() -> Self {
        Self::new()
    }
}

impl FilePickerOverlay {
    pub fn new() -> Self {
        let start_dir = std::env::current_dir()
            .ok()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."));
        Self::new_in_dir(start_dir)
    }

    pub fn new_in_dir(dir: PathBuf) -> Self {
        let mut picker = Self {
            current_dir: dir.clone(),
            entries: Vec::new(),
            filter: String::new(),
            selected: 0,
            mode: FilePickerMode::Browse,
            zoxide_dirs: Vec::new(),
            zoxide_query: String::new(),
            zoxide_selected: 0,
            status: None,
            error: None,
        };
        picker.load_dir(dir);
        picker
    }

    pub fn load_dir(&mut self, dir: PathBuf) {
        self.error = None;
        self.status = None;
        match read_dir_entries(&dir) {
            Ok(entries) => {
                self.current_dir = dir;
                self.entries = entries;
                self.filter.clear();
                self.selected = 0;
            }
            Err(err) => {
                self.error = Some(format!("Cannot open {}: {err}", dir.display()));
            }
        }
    }

    pub fn filtered_entries(&self) -> Vec<&FilePickerEntry> {
        let q = self.filter.trim().to_ascii_lowercase();
        if q.is_empty() {
            return self.entries.iter().collect();
        }
        self.entries
            .iter()
            .filter(|e| e.name.to_ascii_lowercase().contains(&q))
            .collect()
    }

    pub fn filtered_zoxide_dirs(&self) -> Vec<&PathBuf> {
        let q = self.zoxide_query.trim().to_ascii_lowercase();
        if q.is_empty() {
            return self.zoxide_dirs.iter().collect();
        }
        self.zoxide_dirs
            .iter()
            .filter(|p| p.to_string_lossy().to_ascii_lowercase().contains(&q))
            .collect()
    }

    pub fn enter_zoxide_with_dirs(&mut self, dirs: Vec<PathBuf>) {
        if dirs.is_empty() {
            self.status = Some("zoxide returned no directories".to_string());
            return;
        }
        self.zoxide_dirs = dirs;
        self.zoxide_query.clear();
        self.zoxide_selected = 0;
        self.mode = FilePickerMode::ZoxideJump;
        self.status = None;
        self.error = None;
    }

    pub fn select(&mut self, index: usize) {
        match self.mode {
            FilePickerMode::Browse => {
                let len = self.filtered_entries().len();
                if index < len {
                    self.selected = index;
                }
            }
            FilePickerMode::ZoxideJump => {
                let len = self.filtered_zoxide_dirs().len();
                if index < len {
                    self.zoxide_selected = index;
                }
            }
        }
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match self.mode {
            FilePickerMode::Browse => self.handle_browse(action),
            FilePickerMode::ZoxideJump => self.handle_zoxide(action),
        }
    }

    fn handle_browse(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::CloseOverlay => OverlayOutcome::Close,
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            Action::MoveUp | Action::CursorUp => {
                let len = self.filtered_entries().len();
                if len > 0 {
                    let cur = self.selected.min(len - 1);
                    self.selected = (cur + len - 1) % len;
                }
                OverlayOutcome::Handled
            }
            Action::MoveDown | Action::CursorDown => {
                let len = self.filtered_entries().len();
                if len > 0 {
                    let cur = self.selected.min(len - 1);
                    self.selected = (cur + 1) % len;
                }
                OverlayOutcome::Handled
            }
            Action::CursorHome | Action::ScrollTop => {
                self.selected = 0;
                OverlayOutcome::Handled
            }
            Action::CursorEnd | Action::ScrollBottom => {
                let len = self.filtered_entries().len();
                self.selected = len.saturating_sub(1);
                OverlayOutcome::Handled
            }
            Action::Backspace => {
                if !self.filter.is_empty() {
                    self.filter.pop();
                    self.selected = 0;
                } else if let Some(parent) = self.current_dir.parent().map(Path::to_path_buf) {
                    if parent != self.current_dir {
                        self.load_dir(parent);
                    }
                }
                OverlayOutcome::Handled
            }
            Action::Activate | Action::OverlaySubmit => {
                let chosen = {
                    let items = self.filtered_entries();
                    let idx = self.selected.min(items.len().saturating_sub(1));
                    items.get(idx).cloned().cloned()
                };
                if let Some(entry) = chosen {
                    if entry.is_dir {
                        self.load_dir(entry.path);
                        OverlayOutcome::Handled
                    } else {
                        OverlayOutcome::CloseAndDispatch(Action::UploadLocalFile(entry.path))
                    }
                } else {
                    OverlayOutcome::Handled
                }
            }
            Action::Input('z') if self.filter.is_empty() => {
                match query_zoxide_dirs() {
                    Ok(dirs) if !dirs.is_empty() => self.enter_zoxide_with_dirs(dirs),
                    Ok(_) => {
                        self.status = Some("zoxide has no recorded directories yet".to_string());
                    }
                    Err(msg) => {
                        self.status = Some(msg);
                    }
                }
                OverlayOutcome::Handled
            }
            Action::Input(ch) => {
                self.filter.push(*ch);
                self.selected = 0;
                self.status = None;
                self.error = None;
                OverlayOutcome::Handled
            }
            Action::Paste(text) => {
                self.filter.push_str(&text.replace(['\r', '\n'], ""));
                self.selected = 0;
                self.status = None;
                self.error = None;
                OverlayOutcome::Handled
            }
            _ => OverlayOutcome::Handled,
        }
    }

    fn handle_zoxide(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::CloseOverlay => {
                self.mode = FilePickerMode::Browse;
                self.zoxide_query.clear();
                OverlayOutcome::Handled
            }
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            Action::MoveUp | Action::CursorUp => {
                let len = self.filtered_zoxide_dirs().len();
                if len > 0 {
                    let cur = self.zoxide_selected.min(len - 1);
                    self.zoxide_selected = (cur + len - 1) % len;
                }
                OverlayOutcome::Handled
            }
            Action::MoveDown | Action::CursorDown => {
                let len = self.filtered_zoxide_dirs().len();
                if len > 0 {
                    let cur = self.zoxide_selected.min(len - 1);
                    self.zoxide_selected = (cur + 1) % len;
                }
                OverlayOutcome::Handled
            }
            Action::Backspace => {
                if !self.zoxide_query.is_empty() {
                    self.zoxide_query.pop();
                    self.zoxide_selected = 0;
                } else {
                    self.mode = FilePickerMode::Browse;
                }
                OverlayOutcome::Handled
            }
            Action::Input(ch) => {
                self.zoxide_query.push(*ch);
                self.zoxide_selected = 0;
                OverlayOutcome::Handled
            }
            Action::Paste(text) => {
                self.zoxide_query.push_str(&text.replace(['\r', '\n'], ""));
                self.zoxide_selected = 0;
                OverlayOutcome::Handled
            }
            Action::Activate | Action::OverlaySubmit => {
                let chosen = {
                    let items = self.filtered_zoxide_dirs();
                    let idx = self.zoxide_selected.min(items.len().saturating_sub(1));
                    items.get(idx).cloned().cloned()
                };
                self.mode = FilePickerMode::Browse;
                self.zoxide_query.clear();
                if let Some(dir) = chosen {
                    self.load_dir(dir);
                }
                OverlayOutcome::Handled
            }
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
        let theme = &ui.theme;
        let rect = centered_rect(
            area,
            area.width.saturating_sub(6).min(86),
            area.height.saturating_sub(4).min(24),
        );
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let title = match self.mode {
            FilePickerMode::Browse => {
                " Upload File · Enter Open/Select · Backspace Up · z Zoxide · Esc Close "
            }
            FilePickerMode::ZoxideJump => {
                " Zoxide Jump · Type to Filter · Enter Jump · Esc Back "
            }
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                crate::ui::truncate_width(title, rect.width.saturating_sub(2) as usize),
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width == 0 || inner.height < 3 {
            return;
        }

        // Row 0: current path
        let dir_label = format!(" Dir: {}", self.current_dir.display());
        frame.render_widget(
            Paragraph::new(Span::styled(
                crate::ui::truncate_width(&dir_label, inner.width as usize),
                theme.accent_style().add_modifier(Modifier::BOLD),
            )),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );

        // Row 1: filter / zoxide search input
        let (prompt, query_val) = match self.mode {
            FilePickerMode::Browse => (" Filter: ", self.filter.as_str()),
            FilePickerMode::ZoxideJump => (" Zoxide: ", self.zoxide_query.as_str()),
        };
        let filter_line = Line::from(vec![
            Span::styled(prompt, theme.muted_style()),
            Span::raw(query_val.to_string()),
            Span::styled("▏", theme.accent_style()),
        ]);
        frame.render_widget(
            Paragraph::new(filter_line),
            Rect::new(inner.x, inner.y + 1, inner.width, 1),
        );

        let list_y = inner.y + 2;
        let has_footer = self.error.is_some() || self.status.is_some();
        let footer_rows: u16 = if has_footer { 1 } else { 0 };
        let list_h = inner.height.saturating_sub(2 + footer_rows) as usize;

        match self.mode {
            FilePickerMode::Browse => {
                let items = self.filtered_entries();
                if items.is_empty() {
                    frame.render_widget(
                        Paragraph::new(Span::styled(
                            " (empty directory or no matching files)",
                            theme.muted_style(),
                        )),
                        Rect::new(inner.x, list_y, inner.width, 1),
                    );
                } else if list_h > 0 {
                    let active_sel = self.selected.min(items.len() - 1);
                    let skip = active_sel.saturating_sub(list_h.saturating_sub(1));
                    for (row, (idx, entry)) in items
                        .iter()
                        .enumerate()
                        .skip(skip)
                        .take(list_h)
                        .enumerate()
                    {
                        let y = list_y + row as u16;
                        let is_sel = idx == active_sel;
                        let icon = if entry.is_dir { "📁 " } else { "📄 " };
                        let right = if entry.is_dir {
                            "[DIR]".to_string()
                        } else {
                            format_file_size(entry.size_bytes)
                        };
                        let right_w = right.len().max(6);
                        let left_budget = (inner.width as usize).saturating_sub(right_w + 3).max(8);
                        let left_raw = format!(" {icon}{}", entry.name);
                        let left_trunc = crate::ui::truncate_width(&left_raw, left_budget);
                        let left_used = unicode_width::UnicodeWidthStr::width(left_trunc.as_str());
                        let pad = left_budget.saturating_sub(left_used);

                        let line = if is_sel {
                            Line::from(Span::styled(
                                format!("{left_trunc}{} {right:>right_w$}", " ".repeat(pad)),
                                theme.selection(),
                            ))
                        } else {
                            let name_style = if entry.is_dir {
                                theme.accent_style().add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(theme.fg_primary)
                            };
                            Line::from(vec![
                                Span::styled(left_trunc, name_style),
                                Span::raw(" ".repeat(pad + 1)),
                                Span::styled(format!("{right:>right_w$}"), theme.muted_style()),
                            ])
                        };
                        frame.render_widget(
                            Paragraph::new(line),
                            Rect::new(inner.x, y, inner.width, 1),
                        );
                        hitmap.add_line(
                            inner.x,
                            y,
                            inner.width,
                            HitTarget::OverlayItem { index: idx },
                        );
                    }
                }
            }
            FilePickerMode::ZoxideJump => {
                let dirs = self.filtered_zoxide_dirs();
                if dirs.is_empty() {
                    frame.render_widget(
                        Paragraph::new(Span::styled(
                            " (no matching zoxide directories)",
                            theme.muted_style(),
                        )),
                        Rect::new(inner.x, list_y, inner.width, 1),
                    );
                } else if list_h > 0 {
                    let active_sel = self.zoxide_selected.min(dirs.len() - 1);
                    let skip = active_sel.saturating_sub(list_h.saturating_sub(1));
                    for (row, (idx, dir)) in dirs
                        .iter()
                        .enumerate()
                        .skip(skip)
                        .take(list_h)
                        .enumerate()
                    {
                        let y = list_y + row as u16;
                        let is_sel = idx == active_sel;
                        let label = format!(" ⚡ {}", dir.display());
                        let trunc = crate::ui::truncate_width(&label, inner.width as usize);
                        let style = if is_sel {
                            theme.selection()
                        } else {
                            Style::default().fg(theme.fg_primary)
                        };
                        frame.render_widget(
                            Paragraph::new(Span::styled(trunc, style)),
                            Rect::new(inner.x, y, inner.width, 1),
                        );
                        hitmap.add_line(
                            inner.x,
                            y,
                            inner.width,
                            HitTarget::OverlayItem { index: idx },
                        );
                    }
                }
            }
        }

        if let Some(ref err) = self.error {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    crate::ui::truncate_width(&format!(" Error: {err}"), inner.width as usize),
                    Style::default().fg(theme.error),
                )),
                Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
            );
        } else if let Some(ref status) = self.status {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    crate::ui::truncate_width(&format!(" {status}"), inner.width as usize),
                    theme.muted_style(),
                )),
                Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
            );
        }
    }
}

fn read_dir_entries(dir: &Path) -> std::io::Result<Vec<FilePickerEntry>> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let meta = entry.metadata().ok();
        let is_dir = meta.as_ref().map_or(path.is_dir(), |m| m.is_dir());
        let size_bytes = if is_dir {
            0
        } else {
            meta.as_ref().map_or(0, |m| m.len())
        };
        entries.push(FilePickerEntry {
            name,
            path,
            is_dir,
            size_bytes,
        });
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()))
    });
    Ok(entries)
}

/// Parses newline-delimited output from `zoxide query -l` into existing directory paths.
pub fn parse_zoxide_output(stdout: &str) -> Vec<PathBuf> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .collect()
}

fn query_zoxide_dirs() -> Result<Vec<PathBuf>, String> {
    match Command::new("zoxide").args(["query", "-l"]).output() {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            Ok(parse_zoxide_output(&stdout))
        }
        Ok(_) => Err("zoxide query returned an error".to_string()),
        Err(_) => Err("zoxide is not installed in PATH".to_string()),
    }
}

fn format_file_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browse_filter_descend_parent_and_select_file() {
        let root = tempfile::tempdir().unwrap();
        let sub = root.path().join("docs");
        std::fs::create_dir(&sub).unwrap();
        let report = sub.join("quarterly.pdf");
        std::fs::write(&report, b"%PDF-1.4 sample").unwrap();
        let notes = root.path().join("notes.txt");
        std::fs::write(&notes, b"hello").unwrap();

        let mut picker = FilePickerOverlay::new_in_dir(root.path().to_path_buf());
        // Directories sort before files
        assert_eq!(picker.entries.len(), 2);
        assert!(picker.entries[0].is_dir);
        assert_eq!(picker.entries[0].name, "docs");
        assert_eq!(picker.entries[1].name, "notes.txt");

        // Enter descends into `docs`
        assert_eq!(picker.handle(&Action::Activate), OverlayOutcome::Handled);
        assert_eq!(picker.current_dir, sub);
        assert_eq!(picker.entries.len(), 1);
        assert_eq!(picker.entries[0].name, "quarterly.pdf");

        // Backspace with empty filter navigates to parent
        assert_eq!(picker.handle(&Action::Backspace), OverlayOutcome::Handled);
        assert_eq!(picker.current_dir, root.path());

        // Type-to-filter narrows to `notes.txt`, Enter attaches it
        picker.handle(&Action::Input('n'));
        picker.handle(&Action::Input('o'));
        assert_eq!(picker.filtered_entries().len(), 1);
        assert_eq!(
            picker.handle(&Action::Activate),
            OverlayOutcome::CloseAndDispatch(Action::UploadLocalFile(notes))
        );
    }

    #[test]
    fn zoxide_parser_and_jump_mode() {
        let root = tempfile::tempdir().unwrap();
        let alpha = root.path().join("alpha");
        let beta = root.path().join("beta");
        std::fs::create_dir(&alpha).unwrap();
        std::fs::create_dir(&beta).unwrap();

        let fake_stdout = format!(
            "{}\n{}\n/nonexistent/path/that/cannot/exist\n",
            alpha.display(),
            beta.display()
        );
        let parsed = parse_zoxide_output(&fake_stdout);
        assert_eq!(parsed, vec![alpha.clone(), beta.clone()]);

        let mut picker = FilePickerOverlay::new_in_dir(root.path().to_path_buf());
        picker.enter_zoxide_with_dirs(parsed);
        assert_eq!(picker.mode, FilePickerMode::ZoxideJump);

        for ch in "beta".chars() {
            picker.handle(&Action::Input(ch));
        }
        assert_eq!(picker.filtered_zoxide_dirs().len(), 1);
        assert_eq!(picker.handle(&Action::Activate), OverlayOutcome::Handled);
        assert_eq!(picker.mode, FilePickerMode::Browse);
        assert_eq!(picker.current_dir, beta);
    }
}
