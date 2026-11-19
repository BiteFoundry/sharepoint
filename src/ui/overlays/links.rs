//! Link Extractor Overlay (`u`): extracts all URLs (HTML `<a href>`, Markdown `[label](url)`,
//! and bare `https://...` / `http://...` links) from the currently open email or calendar event.
//!
//! Controls: `j`/`k`/`↑`/`↓` navigate, `Enter` open in browser, `y` copy URL, `Esc`/`q`/`u` close.

use std::collections::HashMap;

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};
use scraper::{Html, Selector};

use crate::{
    models::DriveItem,
    ui::{
        action::Action,
        app::UiState,
        hitmap::{HitMap, HitTarget},
        overlays::{centered_rect, OverlayOutcome},
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedLink {
    pub label: String,
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct LinkExtractorOverlay {
    pub context_title: String,
    pub links: Vec<ExtractedLink>,
    pub selected: usize,
}

impl LinkExtractorOverlay {
    pub fn from_drive_item(
        item: &DriveItem,
        share_url: Option<&str>,
        preview_text: Option<&str>,
    ) -> Self {
        let links = extract_links_from_drive_item(item, share_url, preview_text);
        Self {
            context_title: item.name.clone(),
            links,
            selected: 0,
        }
    }

    pub fn current_url(&self) -> Option<&str> {
        let idx = self.selected.min(self.links.len().saturating_sub(1));
        self.links.get(idx).map(|l| l.url.as_str())
    }

    pub fn select(&mut self, index: usize) {
        if index < self.links.len() {
            self.selected = index;
        }
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::CloseOverlay
            | Action::OpenLinkExtractor
            | Action::Input('q')
            | Action::Input('u') => OverlayOutcome::Close,
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            Action::MoveUp | Action::CursorUp | Action::Input('k') => {
                if !self.links.is_empty() {
                    let cur = self.selected.min(self.links.len() - 1);
                    self.selected = (cur + self.links.len() - 1) % self.links.len();
                }
                OverlayOutcome::Handled
            }
            Action::MoveDown | Action::CursorDown | Action::Input('j') => {
                if !self.links.is_empty() {
                    let cur = self.selected.min(self.links.len() - 1);
                    self.selected = (cur + 1) % self.links.len();
                }
                OverlayOutcome::Handled
            }
            Action::CursorHome | Action::ScrollTop => {
                self.selected = 0;
                OverlayOutcome::Handled
            }
            Action::CursorEnd | Action::ScrollBottom => {
                self.selected = self.links.len().saturating_sub(1);
                OverlayOutcome::Handled
            }
            Action::Activate | Action::OverlaySubmit => {
                if let Some(url) = self.current_url() {
                    OverlayOutcome::CloseAndDispatch(Action::OpenUrl(url.to_string()))
                } else {
                    OverlayOutcome::Close
                }
            }
            Action::Input('y') => {
                if let Some(url) = self.current_url() {
                    OverlayOutcome::Dispatch(Action::CopyUrlToClipboard(url.to_string()))
                } else {
                    OverlayOutcome::Handled
                }
            }
            Action::Input(ch) if ch.is_ascii_digit() && *ch != '0' => {
                let idx = (*ch as usize) - ('1' as usize);
                if idx < self.links.len() {
                    self.selected = idx;
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
            area.width.saturating_sub(6).min(88),
            area.height.saturating_sub(4).min(22),
        );
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let title = format!(
            " Links ({}) · Enter Open · y Copy · Esc Close ",
            self.links.len()
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

        if self.links.is_empty() {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    " No URLs found in the currently open item",
                    theme.muted_style(),
                )),
                inner,
            );
            return;
        }

        let active_sel = self.selected.min(self.links.len() - 1);
        let list_h = inner.height.saturating_sub(1).max(1) as usize;
        let skip = active_sel.saturating_sub(list_h.saturating_sub(1));

        for (row, (idx, item)) in self
            .links
            .iter()
            .enumerate()
            .skip(skip)
            .take(list_h)
            .enumerate()
        {
            let y = inner.y + row as u16;
            if y >= inner.bottom().saturating_sub(1) && inner.height > 1 {
                break;
            }
            let is_sel = idx == active_sel;
            let num = format!(" {:>2}. ", idx + 1);
            let has_distinct_label = !item.label.is_empty()
                && !item.label.eq_ignore_ascii_case(&item.url)
                && !item
                    .label
                    .trim_end_matches('/')
                    .eq_ignore_ascii_case(item.url.trim_end_matches('/'));

            let row_text = if has_distinct_label {
                format!("{num}{} — {}", item.label, item.url)
            } else {
                format!("{num}{}", item.url)
            };
            let truncated = crate::ui::truncate_width(&row_text, inner.width as usize);

            let line = if is_sel {
                Line::from(Span::styled(truncated, theme.selection()))
            } else if has_distinct_label {
                let prefix = format!("{num}{}", item.label);
                let avail = inner.width as usize;
                let prefix_trunc = crate::ui::truncate_width(&prefix, avail);
                let used = unicode_width::UnicodeWidthStr::width(prefix_trunc.as_str());
                let rem = avail.saturating_sub(used);
                let mut spans = vec![
                    Span::styled(
                        num,
                        theme.muted_style(),
                    ),
                    Span::styled(
                        crate::ui::truncate_width(&item.label, avail.saturating_sub(5)),
                        Style::default()
                            .fg(theme.fg_primary)
                            .add_modifier(Modifier::BOLD),
                    ),
                ];
                if rem > 4 {
                    spans.push(Span::styled(
                        crate::ui::truncate_width(&format!(" — {}", item.url), rem),
                        theme.accent_style(),
                    ));
                }
                Line::from(spans)
            } else {
                Line::from(vec![
                    Span::styled(num, theme.muted_style()),
                    Span::styled(
                        crate::ui::truncate_width(&item.url, (inner.width as usize).saturating_sub(5)),
                        theme.accent_style(),
                    ),
                ])
            };

            frame.render_widget(Paragraph::new(line), Rect::new(inner.x, y, inner.width, 1));
            hitmap.add_line(inner.x, y, inner.width, HitTarget::OverlayItem { index: idx });
        }

        if inner.height > 1 {
            let footer_y = inner.bottom() - 1;
            let hint = self
                .current_url()
                .map(|u| format!(" Selected: {u}"))
                .unwrap_or_default();
            frame.render_widget(
                Paragraph::new(Span::styled(
                    crate::ui::truncate_width(&hint, inner.width as usize),
                    theme.muted_style(),
                )),
                Rect::new(inner.x, footer_y, inner.width, 1),
            );
        }
    }
}

/// Extracts all unique URLs from a DriveItem (web URL, created share link, and URLs inside previewed HTML/Markdown/text).
pub fn extract_links_from_drive_item(
    item: &DriveItem,
    share_url: Option<&str>,
    preview_text: Option<&str>,
) -> Vec<ExtractedLink> {
    let mut acc = LinkAccumulator::default();
    if let Some(url) = share_url {
        acc.push("Share Link", url);
    }
    if let Some(ref url) = item.web_url {
        acc.push("Open in SharePoint / OneDrive Web", url);
    }
    if let Some(text) = preview_text {
        let is_html = matches!(item.extension().as_deref(), Some("html" | "htm"));
        if is_html {
            extract_into_acc(&mut acc, text, "");
        } else {
            extract_into_acc(&mut acc, "", text);
        }
    }
    acc.finish()
}

/// Extracts all unique URLs from HTML (`<a href>`) and plain/Markdown text.
pub fn extract_links_from_html_and_text(html: &str, plain: &str) -> Vec<ExtractedLink> {
    let mut acc = LinkAccumulator::default();
    extract_into_acc(&mut acc, html, plain);
    acc.finish()
}

fn extract_into_acc(acc: &mut LinkAccumulator, html: &str, plain: &str) {
    if !html.trim().is_empty() {
        let fragment = Html::parse_fragment(html);
        if let Ok(sel) = Selector::parse("a[href]") {
            for el in fragment.select(&sel) {
                if let Some(href) = el.value().attr("href") {
                    let label: String = el
                        .text()
                        .collect::<Vec<_>>()
                        .join(" ")
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ");
                    acc.push(&label, href);
                }
            }
        }
        // Also scan visible text nodes inside HTML for Markdown or bare URLs not wrapped in <a>
        let html_text: String = fragment
            .root_element()
            .text()
            .collect::<Vec<_>>()
            .join(" ");
        extract_markdown_and_bare_urls(acc, &html_text);
    }
    if !plain.trim().is_empty() {
        extract_markdown_and_bare_urls(acc, plain);
    }
}

fn extract_markdown_and_bare_urls(acc: &mut LinkAccumulator, text: &str) {
    // 1. Markdown links: [label](https://...)
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'[' {
            if let Some(close_bracket) = find_next_byte(bytes, i + 1, b']') {
                if close_bracket + 1 < bytes.len() && bytes[close_bracket + 1] == b'(' {
                    if let Some(close_paren) = find_matching_paren(bytes, close_bracket + 2) {
                        let label = text[i + 1..close_bracket].trim();
                        let raw_target = text[close_bracket + 2..close_paren].trim();
                        // Markdown links may have an optional title after whitespace: [x](url "title")
                        let url_part = raw_target
                            .split_whitespace()
                            .next()
                            .unwrap_or("")
                            .trim_matches(|c| c == '<' || c == '>');
                        if is_http_url(url_part) {
                            acc.push(label, url_part);
                            i = close_paren + 1;
                            continue;
                        }
                    }
                }
            }
        }
        i += 1;
    }

    // 2. Bare http:// and https:// URLs
    let mut scan = 0;
    while scan < text.len() {
        let rest = &text[scan..];
        let next_https = rest.find("https://");
        let next_http = rest.find("http://");
        let rel = match (next_https, next_http) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => break,
        };
        let start = scan + rel;
        let tail = &text[start..];
        let mut end = tail.len();
        for (idx, ch) in tail.char_indices() {
            if ch.is_whitespace() || matches!(ch, '<' | '>' | '"' | '\'' | '`') {
                end = idx;
                break;
            }
        }
        let cleaned = trim_trailing_url_punct(&tail[..end]);
        if is_http_url(cleaned) {
            acc.push("", cleaned);
        }
        scan = start + end.max(1);
    }
}

fn find_next_byte(bytes: &[u8], start: usize, needle: u8) -> Option<usize> {
    let mut i = start;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            return None;
        }
        if bytes[i] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn find_matching_paren(bytes: &[u8], start: usize) -> Option<usize> {
    let mut depth = 1usize;
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => return None,
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn trim_trailing_url_punct(raw: &str) -> &str {
    let mut s = raw;
    loop {
        let Some(last) = s.chars().last() else {
            break;
        };
        let should_strip = match last {
            '.' | ',' | ';' | ':' | '!' | '?' => true,
            ')' => {
                let opens = s.chars().filter(|&c| c == '(').count();
                let closes = s.chars().filter(|&c| c == ')').count();
                closes > opens
            }
            ']' => {
                let opens = s.chars().filter(|&c| c == '[').count();
                let closes = s.chars().filter(|&c| c == ']').count();
                closes > opens
            }
            '}' => {
                let opens = s.chars().filter(|&c| c == '{').count();
                let closes = s.chars().filter(|&c| c == '}').count();
                closes > opens
            }
            _ => false,
        };
        if should_strip {
            s = &s[..s.len() - last.len_utf8()];
        } else {
            break;
        }
    }
    s
}

fn is_http_url(raw: &str) -> bool {
    let trimmed = raw.trim();
    let lower = trimmed.to_ascii_lowercase();
    (lower.starts_with("https://") && trimmed.len() > 8)
        || (lower.starts_with("http://") && trimmed.len() > 7)
}

#[derive(Default)]
struct LinkAccumulator {
    links: Vec<ExtractedLink>,
    index_by_url: HashMap<String, usize>,
}

impl LinkAccumulator {
    fn push(&mut self, label: &str, raw_url: &str) {
        let url = raw_url.trim();
        if !is_http_url(url) {
            return;
        }
        let clean_label = label.trim().to_string();
        if let Some(&existing_idx) = self.index_by_url.get(url) {
            // Upgrade label if previously bare and now we have a human-readable label
            if self.links[existing_idx].label.is_empty()
                && !clean_label.is_empty()
                && !clean_label.eq_ignore_ascii_case(url)
            {
                self.links[existing_idx].label = clean_label;
            }
            return;
        }
        let idx = self.links.len();
        self.index_by_url.insert(url.to_string(), idx);
        self.links.push(ExtractedLink {
            label: clean_label,
            url: url.to_string(),
        });
    }

    fn finish(self) -> Vec<ExtractedLink> {
        self.links
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_html_markdown_and_bare_urls_with_dedup_and_balanced_parens() {
        let html = r#"
            <p>Read the <a href="https://docs.example.com/q4">Q4 Report</a>
            and check <a href="mailto:ignored@example.com">email</a>
            plus <a href="javascript:void(0)">noop</a>.</p>
            <p>Also see https://en.wikipedia.org/wiki/Rust_(programming_language).</p>
        "#;
        let plain = "Check [Design Doc](https://design.example.com/spec) and https://docs.example.com/q4, or http://status.example.org/health!";

        let links = extract_links_from_html_and_text(html, plain);
        assert_eq!(links.len(), 4);
        assert_eq!(links[0].label, "Q4 Report");
        assert_eq!(links[0].url, "https://docs.example.com/q4");
        assert_eq!(
            links[1].url,
            "https://en.wikipedia.org/wiki/Rust_(programming_language)"
        );
        assert_eq!(links[2].label, "Design Doc");
        assert_eq!(links[2].url, "https://design.example.com/spec");
        assert_eq!(links[3].url, "http://status.example.org/health");
    }

    #[test]
    fn overlay_navigation_open_copy_and_close() {
        let mut overlay = LinkExtractorOverlay {
            context_title: "Test".into(),
            links: vec![
                ExtractedLink {
                    label: "First".into(),
                    url: "https://one.example.com".into(),
                },
                ExtractedLink {
                    label: "Second".into(),
                    url: "https://two.example.com".into(),
                },
            ],
            selected: 0,
        };

        assert_eq!(overlay.handle(&Action::MoveDown), OverlayOutcome::Handled);
        assert_eq!(overlay.selected, 1);
        assert_eq!(
            overlay.handle(&Action::Input('y')),
            OverlayOutcome::Dispatch(Action::CopyUrlToClipboard(
                "https://two.example.com".into()
            ))
        );
        assert_eq!(
            overlay.handle(&Action::Activate),
            OverlayOutcome::CloseAndDispatch(Action::OpenUrl("https://two.example.com".into()))
        );
        assert_eq!(overlay.handle(&Action::CloseOverlay), OverlayOutcome::Close);
    }
}
