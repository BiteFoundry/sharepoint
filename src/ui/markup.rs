//! Styled terminal Markdown (CommonMark + GFM) and HTML5 renderer for the TUI inspector.
//!
//! Composes `pulldown-cmark` (Markdown) and `scraper` (HTML5 DOM) into styled
//! `Vec<ratatui::text::Line<'static>>` bound to `Theme` semantic tokens and `show_link_urls`.

use pulldown_cmark::{
    Alignment, BlockQuoteKind, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd,
    TextMergeStream,
};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use scraper::{ElementRef, Html, Node, Selector};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::config::ThemePreset;
pub use crate::models::MarkupKind;
use crate::ui::theme::Theme;
use crate::ui::truncate_width;

const MAX_RENDERED_LINES: usize = 1_000;

/// Cache key uniquely identifying a rendered markup preview for O(1) redraw/scroll reuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkupCacheKey {
    pub item_id: String,
    pub kind: MarkupKind,
    pub content_len: usize,
    pub content_hash: u64,
    pub width: u16,
    pub theme_preset: ThemePreset,
    pub theme_no_color: bool,
    pub show_link_urls: bool,
}

impl MarkupCacheKey {
    pub fn new(
        item_id: &str,
        kind: MarkupKind,
        content: &str,
        width: u16,
        theme_preset: ThemePreset,
        theme_no_color: bool,
        show_link_urls: bool,
    ) -> Self {
        Self {
            item_id: item_id.to_string(),
            kind,
            content_len: content.len(),
            content_hash: fnv1a_64(content.as_bytes()),
            width,
            theme_preset,
            theme_no_color,
            show_link_urls,
        }
    }
}

/// Cached rendered `Line`s for the active inspector markup file.
#[derive(Debug, Clone)]
pub struct CachedMarkupPreview {
    pub key: MarkupCacheKey,
    pub lines: Vec<Line<'static>>,
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Renders Markdown or HTML source into styled terminal `Line`s fitted to `max_width`.
pub fn render_markup(
    kind: MarkupKind,
    source: &str,
    max_width: u16,
    show_link_urls: bool,
    theme: &Theme,
) -> Vec<Line<'static>> {
    match kind {
        MarkupKind::Markdown => render_markdown(source, max_width, theme, show_link_urls),
        MarkupKind::Html => render_html(source, max_width, theme, show_link_urls),
    }
}

/// Renders CommonMark + GFM Markdown into styled terminal `Line`s wrapped to `width`.
pub fn render_markdown(
    raw: &str,
    width: u16,
    theme: &Theme,
    show_link_urls: bool,
) -> Vec<Line<'static>> {
    let mut out = LineSink::new(width, theme, show_link_urls);
    render_markdown_into(raw, &mut out);
    out.finish()
}

/// Renders an HTML document or fragment into styled terminal `Line`s wrapped to `width`.
pub fn render_html(
    raw: &str,
    width: u16,
    theme: &Theme,
    show_link_urls: bool,
) -> Vec<Line<'static>> {
    let mut out = LineSink::new(width, theme, show_link_urls);
    render_html_into(raw, &mut out, 0);
    out.finish()
}

// ---------------------------------------------------------------------------
// Shared Line Sink, Word-Wrapper, Code Block & Table Layout
// ---------------------------------------------------------------------------

struct LineSink<'a> {
    lines: Vec<Line<'static>>,
    max_width: usize,
    theme: &'a Theme,
    show_link_urls: bool,
    truncated: bool,
}

impl<'a> LineSink<'a> {
    fn new(width: u16, theme: &'a Theme, show_link_urls: bool) -> Self {
        Self {
            lines: Vec::new(),
            max_width: (width as usize).max(16),
            theme,
            show_link_urls,
            truncated: false,
        }
    }

    fn body_style(&self) -> Style {
        if self.theme.no_color {
            Style::default()
        } else {
            Style::default().fg(self.theme.fg_primary)
        }
    }

    fn link_style(&self) -> Style {
        if self.theme.no_color {
            Style::default().add_modifier(Modifier::UNDERLINED)
        } else {
            self.theme.accent_style().add_modifier(Modifier::UNDERLINED)
        }
    }

    fn quote_bar_style(&self) -> Style {
        if self.theme.no_color {
            Style::default().add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(self.theme.accent)
        }
    }

    fn quote_text_style(&self) -> Style {
        if self.theme.no_color {
            Style::default().add_modifier(Modifier::ITALIC)
        } else {
            self.theme.quote_style()
        }
    }

    fn heading_style(&self, level: u8) -> Style {
        if self.theme.no_color {
            return match level {
                1 => Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                2 | 3 => Style::default().add_modifier(Modifier::BOLD),
                _ => Style::default().add_modifier(Modifier::BOLD | Modifier::DIM),
            };
        }
        match level {
            1 => Style::default()
                .fg(self.theme.accent)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            2 => self.theme.subject_style(),
            3 => Style::default()
                .fg(self.theme.fg_primary)
                .add_modifier(Modifier::BOLD),
            _ => Style::default()
                .fg(self.theme.fg_secondary)
                .add_modifier(Modifier::BOLD),
        }
    }

    fn heading_prefix(&self, level: u8) -> Span<'static> {
        let (glyph, style) = match level {
            1 => (
                "▌ ",
                if self.theme.no_color {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                        .fg(self.theme.accent)
                        .add_modifier(Modifier::BOLD)
                },
            ),
            2 => (
                "▎ ",
                if self.theme.no_color {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    self.theme.subject_style()
                },
            ),
            3 => (
                "▸ ",
                if self.theme.no_color {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    self.theme.accent_style()
                },
            ),
            _ => ("▪ ", self.theme.muted_style()),
        };
        Span::styled(glyph, style)
    }

    fn inline_code_span(&self, text: &str) -> Span<'static> {
        if self.theme.no_color {
            Span::styled(format!("`{text}`"), self.theme.code_style())
        } else {
            Span::styled(format!(" {text} "), self.theme.code_style())
        }
    }

    fn checkbox_span(&self, checked: bool) -> Span<'static> {
        if self.theme.no_color {
            if checked {
                Span::styled("[x] ", Style::default().add_modifier(Modifier::BOLD))
            } else {
                Span::styled("[ ] ", Style::default().add_modifier(Modifier::DIM))
            }
        } else if checked {
            Span::styled("☑ ", Style::default().fg(self.theme.success))
        } else {
            Span::styled("☐ ", self.theme.muted_style())
        }
    }

    fn push_line(&mut self, line: Line<'static>) {
        if self.lines.len() >= MAX_RENDERED_LINES {
            self.truncated = true;
            return;
        }
        self.lines.push(line);
    }

    fn push_blank_line(&mut self) {
        if self.lines.is_empty() || self.truncated {
            return;
        }
        let last_empty = self
            .lines
            .last()
            .is_some_and(|l| l.spans.iter().all(|s| s.content.trim().is_empty()));
        if !last_empty {
            self.push_line(Line::from(""));
        }
    }

    fn quote_prefix_spans(&self, quote_depth: usize) -> Vec<Span<'static>> {
        let mut out = Vec::with_capacity(quote_depth + 1);
        out.push(Span::raw(" "));
        for _ in 0..quote_depth {
            out.push(Span::styled("│ ", self.quote_bar_style()));
        }
        out
    }

    fn push_rule(&mut self, quote_depth: usize) {
        self.push_blank_line();
        let mut spans = self.quote_prefix_spans(quote_depth);
        let used_w: usize = spans.iter().map(|s| s.content.width()).sum();
        let rule_w = self.max_width.saturating_sub(used_w + 1).clamp(8, 60);
        spans.push(Span::styled("─".repeat(rule_w), self.theme.subtle_style()));
        self.push_line(Line::from(spans));
        self.push_blank_line();
    }

    fn push_code_block(&mut self, lang: &str, code: &str, quote_depth: usize) {
        self.push_blank_line();
        let label = lang.trim().split_whitespace().next().unwrap_or("");
        let header_label = if label.is_empty() { "code" } else { label };
        let mut header_spans = self.quote_prefix_spans(quote_depth);
        header_spans.push(Span::styled(
            format!("┌─ {header_label}"),
            self.theme.muted_style(),
        ));
        self.push_line(Line::from(header_spans));

        let trimmed_code = code.strip_suffix('\n').unwrap_or(code);
        let code_style = self.theme.code_style();
        if trimmed_code.is_empty() {
            let mut empty_spans = self.quote_prefix_spans(quote_depth);
            empty_spans.push(Span::styled("│ ", self.theme.muted_style()));
            self.push_line(Line::from(empty_spans));
        } else {
            for raw_line in trimmed_code.split('\n') {
                let line_text = raw_line.trim_end_matches('\r');
                let mut row_spans = self.quote_prefix_spans(quote_depth);
                row_spans.push(Span::styled("│ ", self.theme.muted_style()));
                row_spans.push(Span::styled(line_text.to_string(), code_style));
                self.push_line(Line::from(row_spans));
                if self.truncated {
                    return;
                }
            }
        }

        let mut footer_spans = self.quote_prefix_spans(quote_depth);
        footer_spans.push(Span::styled("└─", self.theme.muted_style()));
        self.push_line(Line::from(footer_spans));
        self.push_blank_line();
    }

    fn push_table(&mut self, table: &TableData, quote_depth: usize) {
        let num_cols = table
            .headers
            .len()
            .max(table.rows.iter().map(Vec::len).max().unwrap_or(0));
        if num_cols == 0 {
            return;
        }
        self.push_blank_line();

        let prefix = self.quote_prefix_spans(quote_depth);
        let prefix_w: usize = prefix.iter().map(|s| s.content.width()).sum();
        let sep_total_w = num_cols.saturating_sub(1) * 3;
        let avail = self
            .max_width
            .saturating_sub(prefix_w + sep_total_w + 1)
            .max(num_cols * 3);

        let mut col_widths = vec![3usize; num_cols];
        for (i, cell) in table.headers.iter().enumerate() {
            col_widths[i] = col_widths[i].max(cell.trim().width());
        }
        for row in &table.rows {
            for (i, cell) in row.iter().enumerate() {
                if i < num_cols {
                    col_widths[i] = col_widths[i].max(cell.trim().width());
                }
            }
        }

        let total_w: usize = col_widths.iter().sum();
        if total_w > avail {
            let mut remaining_excess = total_w - avail;
            while remaining_excess > 0 {
                if let Some((max_idx, &max_val)) = col_widths
                    .iter()
                    .enumerate()
                    .max_by_key(|(_, &w)| w)
                {
                    if max_val <= 3 {
                        break;
                    }
                    col_widths[max_idx] -= 1;
                    remaining_excess -= 1;
                } else {
                    break;
                }
            }
        }

        let header_style = if self.theme.no_color {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            self.theme.subject_style()
        };
        let sep_style = self.theme.subtle_style();
        let body_style = self.body_style();

        if !table.headers.is_empty() {
            let mut spans = prefix.clone();
            for (idx, &w) in col_widths.iter().enumerate() {
                if idx > 0 {
                    spans.push(Span::styled(" │ ", sep_style));
                }
                let raw = table.headers.get(idx).map(String::as_str).unwrap_or("");
                let align = table
                    .alignments
                    .get(idx)
                    .copied()
                    .unwrap_or(Alignment::Left);
                spans.push(Span::styled(format_cell(raw, w, align), header_style));
            }
            self.push_line(Line::from(spans));

            let mut div_spans = prefix.clone();
            for (idx, &w) in col_widths.iter().enumerate() {
                if idx > 0 {
                    div_spans.push(Span::styled("─┼─", sep_style));
                }
                div_spans.push(Span::styled("─".repeat(w), sep_style));
            }
            self.push_line(Line::from(div_spans));
        }

        for row in &table.rows {
            let mut spans = prefix.clone();
            for (idx, &w) in col_widths.iter().enumerate() {
                if idx > 0 {
                    spans.push(Span::styled(" │ ", sep_style));
                }
                let raw = row.get(idx).map(String::as_str).unwrap_or("");
                let align = table
                    .alignments
                    .get(idx)
                    .copied()
                    .unwrap_or(Alignment::Left);
                spans.push(Span::styled(format_cell(raw, w, align), body_style));
            }
            self.push_line(Line::from(spans));
            if self.truncated {
                return;
            }
        }

        self.push_blank_line();
    }

    fn push_wrapped_spans(
        &mut self,
        first_prefix: Vec<Span<'static>>,
        cont_prefix: Vec<Span<'static>>,
        spans: Vec<Span<'static>>,
    ) {
        if spans.is_empty() {
            return;
        }
        let wrapped = wrap_styled_spans(first_prefix, cont_prefix, spans, self.max_width);
        for line in wrapped {
            self.push_line(line);
            if self.truncated {
                return;
            }
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        while self
            .lines
            .last()
            .is_some_and(|l| l.spans.iter().all(|s| s.content.trim().is_empty()))
        {
            self.lines.pop();
        }
        if self.truncated {
            self.lines.push(Line::from(Span::styled(
                " … (preview truncated)",
                self.theme.muted_style(),
            )));
        }
        self.lines
    }
}

#[derive(Default)]
struct TableData {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
    alignments: Vec<Alignment>,
}

fn format_cell(raw: &str, width: usize, align: Alignment) -> String {
    let trimmed = raw.trim();
    let trunc = truncate_width(trimmed, width);
    let w = trunc.width();
    let pad = width.saturating_sub(w);
    match align {
        Alignment::Right => format!("{}{}", " ".repeat(pad), trunc),
        Alignment::Center => {
            let left = pad / 2;
            let right = pad.saturating_sub(left);
            format!("{}{}{}", " ".repeat(left), trunc, " ".repeat(right))
        }
        Alignment::None | Alignment::Left => format!("{}{}", trunc, " ".repeat(pad)),
    }
}

fn wrap_styled_spans(
    first_prefix: Vec<Span<'static>>,
    cont_prefix: Vec<Span<'static>>,
    content: Vec<Span<'static>>,
    max_width: usize,
) -> Vec<Line<'static>> {
    let first_w: usize = first_prefix.iter().map(|s| s.content.width()).sum();
    let cont_w: usize = cont_prefix.iter().map(|s| s.content.width()).sum();

    let mut out: Vec<Line<'static>> = Vec::new();
    let mut cur_spans: Vec<Span<'static>> = first_prefix;
    let mut cur_w: usize = first_w;
    let mut has_content = false;

    for span in content {
        let style = span.style;
        let text = span.content.into_owned();
        for token in split_wrap_tokens(&text) {
            if token.is_empty() {
                continue;
            }
            let is_ws = token.chars().all(char::is_whitespace);
            let tok_w = token.width();

            if is_ws {
                if !has_content {
                    continue;
                }
                if cur_w + tok_w <= max_width {
                    push_or_merge_span(&mut cur_spans, token, style);
                    cur_w += tok_w;
                } else {
                    out.push(Line::from(std::mem::replace(
                        &mut cur_spans,
                        cont_prefix.clone(),
                    )));
                    cur_w = cont_w;
                    has_content = false;
                }
                continue;
            }

            if cur_w + tok_w <= max_width {
                push_or_merge_span(&mut cur_spans, token, style);
                cur_w += tok_w;
                has_content = true;
                continue;
            }

            // Wrap to next line if current line already has content
            if has_content {
                trim_trailing_ws_spans(&mut cur_spans);
                out.push(Line::from(std::mem::replace(
                    &mut cur_spans,
                    cont_prefix.clone(),
                )));
                cur_w = cont_w;
                has_content = false;
            }

            // If token fits on fresh line, place it directly; otherwise split by display width
            if cur_w + tok_w <= max_width {
                push_or_merge_span(&mut cur_spans, token, style);
                cur_w += tok_w;
                has_content = true;
            } else {
                let mut chunk = String::new();
                let mut chunk_w = 0usize;
                for ch in token.chars() {
                    let cw = ch.width().unwrap_or(0);
                    let avail = max_width.saturating_sub(cur_w).max(1);
                    if chunk_w > 0 && chunk_w + cw > avail {
                        push_or_merge_span(&mut cur_spans, std::mem::take(&mut chunk), style);
                        out.push(Line::from(std::mem::replace(
                            &mut cur_spans,
                            cont_prefix.clone(),
                        )));
                        cur_w = cont_w;
                        chunk_w = 0;
                    }
                    chunk.push(ch);
                    chunk_w += cw;
                }
                if !chunk.is_empty() {
                    push_or_merge_span(&mut cur_spans, chunk, style);
                    cur_w += chunk_w;
                    has_content = true;
                }
            }
        }
    }

    if has_content || out.is_empty() {
        trim_trailing_ws_spans(&mut cur_spans);
        out.push(Line::from(cur_spans));
    }
    out
}

fn split_wrap_tokens(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut cur_ws: Option<bool> = None;
    for ch in s.chars() {
        let is_ws = ch.is_whitespace();
        if let Some(prev_ws) = cur_ws {
            if prev_ws != is_ws {
                tokens.push(std::mem::take(&mut cur));
            }
        }
        cur.push(ch);
        cur_ws = Some(is_ws);
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

fn push_or_merge_span(spans: &mut Vec<Span<'static>>, text: String, style: Style) {
    if let Some(last) = spans.last_mut() {
        if last.style == style {
            last.content.to_mut().push_str(&text);
            return;
        }
    }
    spans.push(Span::styled(text, style));
}

fn trim_trailing_ws_spans(spans: &mut Vec<Span<'static>>) {
    while let Some(last) = spans.last_mut() {
        let trimmed = last.content.trim_end();
        if trimmed.len() == last.content.len() {
            break;
        }
        if trimmed.is_empty() {
            spans.pop();
        } else {
            let owned = trimmed.to_string();
            last.content = owned.into();
            break;
        }
    }
}

fn format_link_suffix(label: &str, url: &str, show_link_urls: bool) -> Option<String> {
    let trimmed_url = url.trim();
    if trimmed_url.is_empty()
        || trimmed_url
            .to_ascii_lowercase()
            .starts_with("javascript:")
    {
        return None;
    }
    let trimmed_label = label.trim();
    let same = trimmed_label.eq_ignore_ascii_case(trimmed_url)
        || trimmed_label
            .trim_end_matches('/')
            .eq_ignore_ascii_case(trimmed_url.trim_end_matches('/'));
    if same {
        return None;
    }
    if show_link_urls {
        Some(format!(" ({trimmed_url})"))
    } else {
        Some(" ↗".to_string())
    }
}

fn image_label(alt: &str, src: &str) -> String {
    let trimmed = alt.trim();
    if !trimmed.is_empty() {
        return trimmed.to_string();
    }
    let from_src = src
        .trim()
        .split('?')
        .next()
        .unwrap_or("")
        .rsplit('/')
        .next()
        .unwrap_or("")
        .trim();
    if !from_src.is_empty() {
        from_src.to_string()
    } else {
        "image".to_string()
    }
}

// ---------------------------------------------------------------------------
// Markdown Renderer (`pulldown-cmark`)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum ListState {
    Unordered,
    Ordered(u64),
}

struct ActiveLink {
    url: String,
    label_buf: String,
}

struct ActiveImage {
    src: String,
    alt_buf: String,
}

fn render_markdown_into(raw: &str, out: &mut LineSink<'_>) {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_GFM;
    let parser = TextMergeStream::new(Parser::new_ext(raw, options));

    let mut style_stack: Vec<Style> = vec![out.body_style()];
    let mut inline_buf: Vec<Span<'static>> = Vec::new();
    let mut quote_depth: usize = 0;
    let mut list_stack: Vec<ListState> = Vec::new();
    let mut pending_list_marker: Option<(usize, String, Option<bool>)> = None;
    let mut active_heading: Option<u8> = None;
    let mut in_code_block: Option<(String, String)> = None;
    let mut in_html_block: Option<String> = None;
    let mut active_link: Option<ActiveLink> = None;
    let mut active_image: Option<ActiveImage> = None;

    // Table state
    let mut active_table: Option<TableData> = None;
    let mut in_table_head = false;
    let mut cur_row: Vec<String> = Vec::new();
    let mut cur_cell: Option<String> = None;

    let flush_inline = |inline_buf: &mut Vec<Span<'static>>,
                        out: &mut LineSink<'_>,
                        quote_depth: usize,
                        pending_list_marker: &mut Option<(usize, String, Option<bool>)>,
                        list_depth: usize,
                        active_heading: Option<u8>| {
        if inline_buf.is_empty() && pending_list_marker.is_none() {
            return;
        }
        let spans = std::mem::take(inline_buf);
        let mut first_prefix = out.quote_prefix_spans(quote_depth);
        let mut cont_prefix = out.quote_prefix_spans(quote_depth);

        if let Some(level) = active_heading {
            first_prefix.push(out.heading_prefix(level));
            cont_prefix.push(Span::raw("  "));
        } else if let Some((depth, marker, task)) = pending_list_marker.take() {
            let indent = " ".repeat(depth.saturating_sub(1) * 2);
            if !indent.is_empty() {
                first_prefix.push(Span::raw(indent.clone()));
            }
            let marker_w = if let Some(checked) = task {
                let cb = out.checkbox_span(checked);
                let w = cb.content.width();
                first_prefix.push(cb);
                w
            } else {
                let style = if out.theme.no_color {
                    Style::default()
                } else {
                    out.theme.accent_style()
                };
                let text = format!("{marker} ");
                let w = text.width();
                first_prefix.push(Span::styled(text, style));
                w
            };
            cont_prefix.push(Span::raw(" ".repeat(indent.len() + marker_w)));
        } else if list_depth > 0 {
            let indent = " ".repeat(list_depth * 2);
            first_prefix.push(Span::raw(indent.clone()));
            cont_prefix.push(Span::raw(indent));
        }

        if spans.is_empty() {
            out.push_line(Line::from(first_prefix));
        } else {
            out.push_wrapped_spans(first_prefix, cont_prefix, spans);
        }
    };

    for event in parser {
        if out.truncated {
            break;
        }
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {}
                Tag::Heading { level, .. } => {
                    flush_inline(
                        &mut inline_buf,
                        out,
                        quote_depth,
                        &mut pending_list_marker,
                        list_stack.len(),
                        active_heading,
                    );
                    out.push_blank_line();
                    let lvl = heading_level_u8(level);
                    active_heading = Some(lvl);
                    style_stack.push(out.heading_style(lvl));
                }
                Tag::BlockQuote(kind) => {
                    flush_inline(
                        &mut inline_buf,
                        out,
                        quote_depth,
                        &mut pending_list_marker,
                        list_stack.len(),
                        active_heading,
                    );
                    if quote_depth == 0 {
                        out.push_blank_line();
                    }
                    quote_depth += 1;
                    style_stack.push(out.quote_text_style());
                    if let Some(alert) = kind {
                        let (badge, color) = match alert {
                            BlockQuoteKind::Note => ("NOTE", out.theme.accent),
                            BlockQuoteKind::Tip => ("TIP", out.theme.success),
                            BlockQuoteKind::Important => ("IMPORTANT", out.theme.unread),
                            BlockQuoteKind::Warning => ("WARNING", out.theme.warning),
                            BlockQuoteKind::Caution => ("CAUTION", out.theme.error),
                        };
                        let mut spans = out.quote_prefix_spans(quote_depth);
                        let badge_style = if out.theme.no_color {
                            Style::default().add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(color).add_modifier(Modifier::BOLD)
                        };
                        spans.push(Span::styled(format!("[{badge}]"), badge_style));
                        out.push_line(Line::from(spans));
                    }
                }
                Tag::CodeBlock(kind) => {
                    flush_inline(
                        &mut inline_buf,
                        out,
                        quote_depth,
                        &mut pending_list_marker,
                        list_stack.len(),
                        active_heading,
                    );
                    let lang = match kind {
                        CodeBlockKind::Indented => String::new(),
                        CodeBlockKind::Fenced(info) => info.into_string(),
                    };
                    in_code_block = Some((lang, String::new()));
                }
                Tag::HtmlBlock => {
                    flush_inline(
                        &mut inline_buf,
                        out,
                        quote_depth,
                        &mut pending_list_marker,
                        list_stack.len(),
                        active_heading,
                    );
                    in_html_block = Some(String::new());
                }
                Tag::List(start) => {
                    flush_inline(
                        &mut inline_buf,
                        out,
                        quote_depth,
                        &mut pending_list_marker,
                        list_stack.len(),
                        active_heading,
                    );
                    if list_stack.is_empty() {
                        out.push_blank_line();
                    }
                    let state = match start {
                        Some(n) => ListState::Ordered(n),
                        None => ListState::Unordered,
                    };
                    list_stack.push(state);
                }
                Tag::Item => {
                    flush_inline(
                        &mut inline_buf,
                        out,
                        quote_depth,
                        &mut pending_list_marker,
                        list_stack.len(),
                        active_heading,
                    );
                    let depth = list_stack.len().max(1);
                    let marker = match list_stack.last_mut() {
                        Some(ListState::Ordered(n)) => {
                            let s = format!("{n}.");
                            *n += 1;
                            s
                        }
                        _ => {
                            let bullets = ["•", "◦", "▪"];
                            bullets[(depth - 1) % bullets.len()].to_string()
                        }
                    };
                    pending_list_marker = Some((depth, marker, None));
                }
                Tag::Table(alignments) => {
                    flush_inline(
                        &mut inline_buf,
                        out,
                        quote_depth,
                        &mut pending_list_marker,
                        list_stack.len(),
                        active_heading,
                    );
                    active_table = Some(TableData {
                        headers: Vec::new(),
                        rows: Vec::new(),
                        alignments,
                    });
                }
                Tag::TableHead => {
                    in_table_head = true;
                    cur_row.clear();
                }
                Tag::TableRow => {
                    cur_row.clear();
                }
                Tag::TableCell => {
                    cur_cell = Some(String::new());
                }
                Tag::Strong => {
                    let top = *style_stack.last().unwrap_or(&out.body_style());
                    style_stack.push(top.add_modifier(Modifier::BOLD));
                }
                Tag::Emphasis => {
                    let top = *style_stack.last().unwrap_or(&out.body_style());
                    style_stack.push(top.add_modifier(Modifier::ITALIC));
                }
                Tag::Strikethrough => {
                    let top = *style_stack.last().unwrap_or(&out.body_style());
                    style_stack.push(top.add_modifier(Modifier::CROSSED_OUT));
                }
                Tag::Link { dest_url, .. } => {
                    active_link = Some(ActiveLink {
                        url: dest_url.into_string(),
                        label_buf: String::new(),
                    });
                    style_stack.push(out.link_style());
                }
                Tag::Image { dest_url, .. } => {
                    active_image = Some(ActiveImage {
                        src: dest_url.into_string(),
                        alt_buf: String::new(),
                    });
                }
                _ => {}
            },
            Event::End(tag_end) => match tag_end {
                TagEnd::Paragraph => {
                    flush_inline(
                        &mut inline_buf,
                        out,
                        quote_depth,
                        &mut pending_list_marker,
                        list_stack.len(),
                        active_heading,
                    );
                    if list_stack.is_empty() {
                        out.push_blank_line();
                    }
                }
                TagEnd::Heading(_) => {
                    flush_inline(
                        &mut inline_buf,
                        out,
                        quote_depth,
                        &mut pending_list_marker,
                        list_stack.len(),
                        active_heading,
                    );
                    active_heading = None;
                    if style_stack.len() > 1 {
                        style_stack.pop();
                    }
                    out.push_blank_line();
                }
                TagEnd::BlockQuote(_) => {
                    flush_inline(
                        &mut inline_buf,
                        out,
                        quote_depth,
                        &mut pending_list_marker,
                        list_stack.len(),
                        active_heading,
                    );
                    quote_depth = quote_depth.saturating_sub(1);
                    if style_stack.len() > 1 {
                        style_stack.pop();
                    }
                    if quote_depth == 0 {
                        out.push_blank_line();
                    }
                }
                TagEnd::CodeBlock => {
                    if let Some((lang, code)) = in_code_block.take() {
                        out.push_code_block(&lang, &code, quote_depth);
                    }
                }
                TagEnd::HtmlBlock => {
                    if let Some(html) = in_html_block.take() {
                        render_html_into(&html, out, quote_depth);
                    }
                }
                TagEnd::List(_) => {
                    flush_inline(
                        &mut inline_buf,
                        out,
                        quote_depth,
                        &mut pending_list_marker,
                        list_stack.len(),
                        active_heading,
                    );
                    list_stack.pop();
                    if list_stack.is_empty() {
                        out.push_blank_line();
                    }
                }
                TagEnd::Item => {
                    flush_inline(
                        &mut inline_buf,
                        out,
                        quote_depth,
                        &mut pending_list_marker,
                        list_stack.len(),
                        active_heading,
                    );
                }
                TagEnd::Table => {
                    if let Some(table) = active_table.take() {
                        out.push_table(&table, quote_depth);
                    }
                }
                TagEnd::TableHead => {
                    if let Some(table) = active_table.as_mut() {
                        table.headers = std::mem::take(&mut cur_row);
                    }
                    in_table_head = false;
                }
                TagEnd::TableRow => {
                    if let Some(table) = active_table.as_mut() {
                        let row = std::mem::take(&mut cur_row);
                        if in_table_head {
                            table.headers = row;
                        } else {
                            table.rows.push(row);
                        }
                    }
                }
                TagEnd::TableCell => {
                    if let Some(cell) = cur_cell.take() {
                        cur_row.push(cell);
                    }
                }
                TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough => {
                    if style_stack.len() > 1 {
                        style_stack.pop();
                    }
                }
                TagEnd::Link => {
                    if style_stack.len() > 1 {
                        style_stack.pop();
                    }
                    if let Some(link) = active_link.take() {
                        if let Some(cell) = cur_cell.as_mut() {
                            if let Some(suffix) =
                                format_link_suffix(&link.label_buf, &link.url, out.show_link_urls)
                            {
                                cell.push_str(&suffix);
                            }
                        } else {
                            if link.label_buf.trim().is_empty() && !link.url.trim().is_empty() {
                                inline_buf.push(Span::styled(link.url.clone(), out.link_style()));
                            } else if let Some(suffix) =
                                format_link_suffix(&link.label_buf, &link.url, out.show_link_urls)
                            {
                                inline_buf.push(Span::styled(suffix, out.theme.muted_style()));
                            }
                        }
                    }
                }
                TagEnd::Image => {
                    if let Some(img) = active_image.take() {
                        let tag_text = format!("[img: {}]", image_label(&img.alt_buf, &img.src));
                        if let Some(cell) = cur_cell.as_mut() {
                            cell.push_str(&tag_text);
                        } else {
                            inline_buf.push(Span::styled(tag_text, out.theme.muted_style()));
                        }
                    }
                }
                _ => {}
            },
            Event::Text(text) => {
                if let Some((_, ref mut buf)) = in_code_block {
                    buf.push_str(&text);
                } else if let Some(ref mut html_buf) = in_html_block {
                    html_buf.push_str(&text);
                } else if let Some(ref mut img) = active_image {
                    img.alt_buf.push_str(&text);
                } else if let Some(ref mut cell) = cur_cell {
                    if let Some(ref mut link) = active_link {
                        link.label_buf.push_str(&text);
                    }
                    cell.push_str(&text);
                } else {
                    if let Some(ref mut link) = active_link {
                        link.label_buf.push_str(&text);
                    }
                    let style = *style_stack.last().unwrap_or(&out.body_style());
                    push_or_merge_span(&mut inline_buf, text.into_string(), style);
                }
            }
            Event::Code(code) => {
                if let Some(ref mut img) = active_image {
                    img.alt_buf.push_str(&code);
                } else if let Some(ref mut cell) = cur_cell {
                    if let Some(ref mut link) = active_link {
                        link.label_buf.push_str(&code);
                    }
                    cell.push('`');
                    cell.push_str(&code);
                    cell.push('`');
                } else {
                    if let Some(ref mut link) = active_link {
                        link.label_buf.push_str(&code);
                    }
                    inline_buf.push(out.inline_code_span(&code));
                }
            }
            Event::SoftBreak => {
                if let Some((_, ref mut buf)) = in_code_block {
                    buf.push('\n');
                } else if let Some(ref mut cell) = cur_cell {
                    cell.push(' ');
                } else {
                    let style = *style_stack.last().unwrap_or(&out.body_style());
                    push_or_merge_span(&mut inline_buf, " ".to_string(), style);
                }
            }
            Event::HardBreak => {
                if let Some(ref mut cell) = cur_cell {
                    cell.push(' ');
                } else {
                    flush_inline(
                        &mut inline_buf,
                        out,
                        quote_depth,
                        &mut pending_list_marker,
                        list_stack.len(),
                        active_heading,
                    );
                }
            }
            Event::Rule => {
                flush_inline(
                    &mut inline_buf,
                    out,
                    quote_depth,
                    &mut pending_list_marker,
                    list_stack.len(),
                    active_heading,
                );
                out.push_rule(quote_depth);
            }
            Event::TaskListMarker(checked) => {
                if let Some((_, _, ref mut task)) = pending_list_marker {
                    *task = Some(checked);
                } else {
                    inline_buf.push(out.checkbox_span(checked));
                }
            }
            Event::Html(html) => {
                if let Some(ref mut html_buf) = in_html_block {
                    html_buf.push_str(&html);
                } else {
                    flush_inline(
                        &mut inline_buf,
                        out,
                        quote_depth,
                        &mut pending_list_marker,
                        list_stack.len(),
                        active_heading,
                    );
                    render_html_into(&html, out, quote_depth);
                }
            }
            Event::InlineHtml(html) => {
                let lower = html.trim().to_ascii_lowercase();
                if lower.starts_with("<!--") {
                    continue;
                }
                if lower.starts_with("<br") {
                    flush_inline(
                        &mut inline_buf,
                        out,
                        quote_depth,
                        &mut pending_list_marker,
                        list_stack.len(),
                        active_heading,
                    );
                } else if lower.starts_with("<hr") {
                    flush_inline(
                        &mut inline_buf,
                        out,
                        quote_depth,
                        &mut pending_list_marker,
                        list_stack.len(),
                        active_heading,
                    );
                    out.push_rule(quote_depth);
                } else if matches!(lower.as_str(), "<b>" | "<strong>") {
                    let top = *style_stack.last().unwrap_or(&out.body_style());
                    style_stack.push(top.add_modifier(Modifier::BOLD));
                } else if matches!(lower.as_str(), "<i>" | "<em>") {
                    let top = *style_stack.last().unwrap_or(&out.body_style());
                    style_stack.push(top.add_modifier(Modifier::ITALIC));
                } else if matches!(lower.as_str(), "<s>" | "<del>" | "<strike>") {
                    let top = *style_stack.last().unwrap_or(&out.body_style());
                    style_stack.push(top.add_modifier(Modifier::CROSSED_OUT));
                } else if matches!(lower.as_str(), "<u>" | "<ins>") {
                    let top = *style_stack.last().unwrap_or(&out.body_style());
                    style_stack.push(top.add_modifier(Modifier::UNDERLINED));
                } else if matches!(lower.as_str(), "<code>" | "<kbd>" | "<samp>") {
                    style_stack.push(out.theme.code_style());
                } else if matches!(
                    lower.as_str(),
                    "</b>"
                        | "</strong>"
                        | "</i>"
                        | "</em>"
                        | "</s>"
                        | "</del>"
                        | "</strike>"
                        | "</u>"
                        | "</ins>"
                        | "</code>"
                        | "</kbd>"
                        | "</samp>"
                ) && style_stack.len() > 1
                {
                    style_stack.pop();
                }
            }
            Event::FootnoteReference(name) => {
                inline_buf.push(Span::styled(format!("[^{name}]"), out.theme.muted_style()));
            }
            Event::InlineMath(math) | Event::DisplayMath(math) => {
                inline_buf.push(out.inline_code_span(&math));
            }
        }
    }

    flush_inline(
        &mut inline_buf,
        out,
        quote_depth,
        &mut pending_list_marker,
        list_stack.len(),
        active_heading,
    );
}

fn heading_level_u8(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

// ---------------------------------------------------------------------------
// HTML5 Renderer (`scraper`)
// ---------------------------------------------------------------------------

struct HtmlWalker<'a, 'b> {
    out: &'a mut LineSink<'b>,
    style_stack: Vec<Style>,
    inline_buf: Vec<Span<'static>>,
    quote_depth: usize,
    list_stack: Vec<ListState>,
    pending_list_marker: Option<(usize, String, Option<bool>)>,
    active_heading: Option<u8>,
    dd_indent: usize,
}

impl<'a, 'b> HtmlWalker<'a, 'b> {
    fn new(out: &'a mut LineSink<'b>, base_quote_depth: usize) -> Self {
        let base_style = if base_quote_depth > 0 {
            out.quote_text_style()
        } else {
            out.body_style()
        };
        Self {
            out,
            style_stack: vec![base_style],
            inline_buf: Vec::new(),
            quote_depth: base_quote_depth,
            list_stack: Vec::new(),
            pending_list_marker: None,
            active_heading: None,
            dd_indent: 0,
        }
    }

    fn current_style(&self) -> Style {
        *self.style_stack.last().unwrap_or(&self.out.body_style())
    }

    fn push_inline_text(&mut self, raw: &str) {
        let normalized = collapse_html_whitespace(raw);
        if normalized.is_empty() {
            return;
        }
        if normalized == " " {
            let needs_space = self
                .inline_buf
                .last()
                .is_some_and(|s| !s.content.ends_with(char::is_whitespace));
            if !needs_space {
                return;
            }
        }
        let text = if self.inline_buf.is_empty() {
            normalized.trim_start().to_string()
        } else if normalized.starts_with(' ')
            && self
                .inline_buf
                .last()
                .is_some_and(|s| s.content.ends_with(char::is_whitespace))
        {
            normalized.trim_start().to_string()
        } else {
            normalized
        };
        if text.is_empty() {
            return;
        }
        let style = self.current_style();
        push_or_merge_span(&mut self.inline_buf, text, style);
    }

    fn flush_inline(&mut self) {
        if self.inline_buf.is_empty() && self.pending_list_marker.is_none() {
            return;
        }
        let spans = std::mem::take(&mut self.inline_buf);
        let mut first_prefix = self.out.quote_prefix_spans(self.quote_depth);
        let mut cont_prefix = self.out.quote_prefix_spans(self.quote_depth);

        if self.dd_indent > 0 {
            let pad = " ".repeat(self.dd_indent * 2);
            first_prefix.push(Span::raw(pad.clone()));
            cont_prefix.push(Span::raw(pad));
        }

        if let Some(level) = self.active_heading {
            first_prefix.push(self.out.heading_prefix(level));
            cont_prefix.push(Span::raw("  "));
        } else if let Some((depth, marker, task)) = self.pending_list_marker.take() {
            let indent = " ".repeat(depth.saturating_sub(1) * 2);
            if !indent.is_empty() {
                first_prefix.push(Span::raw(indent.clone()));
            }
            let marker_w = if let Some(checked) = task {
                let cb = self.out.checkbox_span(checked);
                let w = cb.content.width();
                first_prefix.push(cb);
                w
            } else {
                let style = if self.out.theme.no_color {
                    Style::default()
                } else {
                    self.out.theme.accent_style()
                };
                let text = format!("{marker} ");
                let w = text.width();
                first_prefix.push(Span::styled(text, style));
                w
            };
            cont_prefix.push(Span::raw(" ".repeat(indent.len() + marker_w)));
        } else if !self.list_stack.is_empty() {
            let indent = " ".repeat(self.list_stack.len() * 2);
            first_prefix.push(Span::raw(indent.clone()));
            cont_prefix.push(Span::raw(indent));
        }

        if spans.is_empty() {
            self.out.push_line(Line::from(first_prefix));
        } else {
            self.out.push_wrapped_spans(first_prefix, cont_prefix, spans);
        }
    }

    fn walk_children(&mut self, el: ElementRef<'_>) {
        for child in el.children() {
            if self.out.truncated {
                return;
            }
            match child.value() {
                Node::Text(text) => {
                    self.push_inline_text(text);
                }
                Node::Element(_) => {
                    if let Some(child_el) = ElementRef::wrap(child) {
                        self.walk_element(child_el);
                    }
                }
                _ => {}
            }
        }
    }

    fn walk_element(&mut self, el: ElementRef<'_>) {
        if self.out.truncated {
            return;
        }
        let tag = el.value().name().to_ascii_lowercase();
        match tag.as_str() {
            // Non-visual subtrees stripped completely
            "script" | "style" | "head" | "noscript" | "svg" | "template" | "meta" | "link"
            | "iframe" | "object" | "canvas" => {}

            // Headings
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.flush_inline();
                self.out.push_blank_line();
                let level = tag.as_bytes()[1].saturating_sub(b'0').clamp(1, 6);
                self.active_heading = Some(level);
                self.style_stack.push(self.out.heading_style(level));
                self.walk_children(el);
                self.flush_inline();
                self.active_heading = None;
                self.style_stack.pop();
                self.out.push_blank_line();
            }

            // Block containers / paragraphs
            "p" | "section" | "article" | "header" | "footer" | "main" | "aside" | "nav"
            | "figure" | "figcaption" | "address" => {
                self.flush_inline();
                if self.list_stack.is_empty() {
                    self.out.push_blank_line();
                }
                self.walk_children(el);
                self.flush_inline();
                if self.list_stack.is_empty() {
                    self.out.push_blank_line();
                }
            }
            "div" | "body" | "html" => {
                self.flush_inline();
                self.walk_children(el);
                self.flush_inline();
            }

            // Line break & horizontal rule
            "br" => {
                if self.inline_buf.is_empty() {
                    self.out.push_line(Line::from(
                        self.out.quote_prefix_spans(self.quote_depth),
                    ));
                } else {
                    self.flush_inline();
                }
            }
            "hr" => {
                self.flush_inline();
                self.out.push_rule(self.quote_depth);
            }

            // Preformatted code block
            "pre" => {
                self.flush_inline();
                let lang = detect_html_code_lang(el);
                let mut raw_text = String::new();
                collect_pre_text(el, &mut raw_text);
                self.out.push_code_block(&lang, &raw_text, self.quote_depth);
            }

            // Blockquote
            "blockquote" => {
                self.flush_inline();
                if self.quote_depth == 0 {
                    self.out.push_blank_line();
                }
                self.quote_depth += 1;
                self.style_stack.push(self.out.quote_text_style());
                self.walk_children(el);
                self.flush_inline();
                self.quote_depth = self.quote_depth.saturating_sub(1);
                self.style_stack.pop();
                if self.quote_depth == 0 {
                    self.out.push_blank_line();
                }
            }

            // Lists
            "ul" => {
                self.flush_inline();
                if self.list_stack.is_empty() {
                    self.out.push_blank_line();
                }
                self.list_stack.push(ListState::Unordered);
                self.walk_children(el);
                self.flush_inline();
                self.list_stack.pop();
                if self.list_stack.is_empty() {
                    self.out.push_blank_line();
                }
            }
            "ol" => {
                self.flush_inline();
                if self.list_stack.is_empty() {
                    self.out.push_blank_line();
                }
                let start = el
                    .value()
                    .attr("start")
                    .and_then(|s| s.trim().parse::<u64>().ok())
                    .unwrap_or(1);
                self.list_stack.push(ListState::Ordered(start));
                self.walk_children(el);
                self.flush_inline();
                self.list_stack.pop();
                if self.list_stack.is_empty() {
                    self.out.push_blank_line();
                }
            }
            "li" => {
                self.flush_inline();
                let depth = self.list_stack.len().max(1);
                let marker = match self.list_stack.last_mut() {
                    Some(ListState::Ordered(n)) => {
                        let s = format!("{n}.");
                        *n += 1;
                        s
                    }
                    _ => {
                        let bullets = ["•", "◦", "▪"];
                        bullets[(depth - 1) % bullets.len()].to_string()
                    }
                };
                self.pending_list_marker = Some((depth, marker, None));
                self.walk_children(el);
                self.flush_inline();
            }

            // Definition lists
            "dl" => {
                self.flush_inline();
                self.out.push_blank_line();
                self.walk_children(el);
                self.flush_inline();
                self.out.push_blank_line();
            }
            "dt" => {
                self.flush_inline();
                let bold = self.current_style().add_modifier(Modifier::BOLD);
                self.style_stack.push(bold);
                self.walk_children(el);
                self.flush_inline();
                self.style_stack.pop();
            }
            "dd" => {
                self.flush_inline();
                self.dd_indent += 1;
                self.walk_children(el);
                self.flush_inline();
                self.dd_indent = self.dd_indent.saturating_sub(1);
            }

            // Tables
            "table" => {
                self.flush_inline();
                let table = extract_html_table(el, self.out.show_link_urls);
                self.out.push_table(&table, self.quote_depth);
            }

            // Inline formatting
            "strong" | "b" => {
                let s = self.current_style().add_modifier(Modifier::BOLD);
                self.style_stack.push(s);
                self.walk_children(el);
                self.style_stack.pop();
            }
            "em" | "i" | "cite" | "var" => {
                let s = self.current_style().add_modifier(Modifier::ITALIC);
                self.style_stack.push(s);
                self.walk_children(el);
                self.style_stack.pop();
            }
            "u" | "ins" => {
                let s = self.current_style().add_modifier(Modifier::UNDERLINED);
                self.style_stack.push(s);
                self.walk_children(el);
                self.style_stack.pop();
            }
            "s" | "del" | "strike" => {
                let s = self.current_style().add_modifier(Modifier::CROSSED_OUT);
                self.style_stack.push(s);
                self.walk_children(el);
                self.style_stack.pop();
            }
            "code" | "kbd" | "samp" | "tt" => {
                let text = collapse_html_whitespace(&collect_plain_text(el));
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    self.inline_buf.push(self.out.inline_code_span(trimmed));
                }
            }
            "a" => {
                let href = el.value().attr("href").unwrap_or("").trim();
                let label_raw = collapse_html_whitespace(&collect_plain_text(el));
                let label = label_raw.trim();
                let safe_href = if href.to_ascii_lowercase().starts_with("javascript:") {
                    ""
                } else {
                    href
                };
                if label.is_empty() {
                    if !safe_href.is_empty() {
                        self.inline_buf
                            .push(Span::styled(safe_href.to_string(), self.out.link_style()));
                    }
                } else {
                    let style = if safe_href.is_empty() {
                        self.current_style()
                    } else {
                        self.out.link_style()
                    };
                    self.style_stack.push(style);
                    self.walk_children(el);
                    self.style_stack.pop();
                    if let Some(suffix) =
                        format_link_suffix(label, safe_href, self.out.show_link_urls)
                    {
                        self.inline_buf
                            .push(Span::styled(suffix, self.out.theme.muted_style()));
                    }
                }
            }
            "img" => {
                let alt = el.value().attr("alt").unwrap_or("");
                let src = el.value().attr("src").unwrap_or("");
                let label = format!("[img: {}]", image_label(alt, src));
                self.inline_buf
                    .push(Span::styled(label, self.out.theme.muted_style()));
            }
            "input" => {
                let kind = el.value().attr("type").unwrap_or("").to_ascii_lowercase();
                if kind == "checkbox" {
                    let checked = el.value().attr("checked").is_some();
                    if let Some((_, _, ref mut task)) = self.pending_list_marker {
                        *task = Some(checked);
                    } else {
                        self.inline_buf.push(self.out.checkbox_span(checked));
                    }
                }
            }
            _ => {
                self.walk_children(el);
            }
        }
    }
}

fn render_html_into(raw: &str, out: &mut LineSink<'_>, base_quote_depth: usize) {
    let lower_probe = raw.to_ascii_lowercase();
    let is_full_doc = lower_probe.contains("<html")
        || lower_probe.contains("<body")
        || lower_probe.contains("<!doctype");

    if is_full_doc {
        let doc = Html::parse_document(raw);
        if let Ok(title_sel) = Selector::parse("head > title") {
            if let Some(title_el) = doc.select(&title_sel).next() {
                let title = collapse_html_whitespace(&collect_plain_text(title_el));
                let title = title.trim();
                if !title.is_empty() {
                    let style = if out.theme.no_color {
                        Style::default().add_modifier(Modifier::BOLD)
                    } else {
                        out.theme.subject_style()
                    };
                    let mut spans = out.quote_prefix_spans(base_quote_depth);
                    spans.push(Span::styled(title.to_string(), style));
                    out.push_line(Line::from(spans));
                    out.push_blank_line();
                }
            }
        }
        let mut walker = HtmlWalker::new(out, base_quote_depth);
        if let Ok(body_sel) = Selector::parse("body") {
            if let Some(body) = doc.select(&body_sel).next() {
                walker.walk_children(body);
                walker.flush_inline();
                return;
            }
        }
        walker.walk_children(doc.root_element());
        walker.flush_inline();
    } else {
        let frag = Html::parse_fragment(raw);
        let mut walker = HtmlWalker::new(out, base_quote_depth);
        walker.walk_children(frag.root_element());
        walker.flush_inline();
    }
}

fn collapse_html_whitespace(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut prev_ws = false;
    for ch in input.chars() {
        let is_ws = ch.is_ascii_whitespace() || ch == '\u{00a0}';
        if is_ws {
            if !prev_ws {
                out.push(' ');
                prev_ws = true;
            }
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
    out
}

fn collect_pre_text(el: ElementRef<'_>, out: &mut String) {
    for child in el.children() {
        match child.value() {
            Node::Text(t) => out.push_str(t),
            Node::Element(e) => {
                let name = e.name().to_ascii_lowercase();
                if name == "br" {
                    out.push('\n');
                } else if !matches!(name.as_str(), "script" | "style") {
                    if let Some(child_el) = ElementRef::wrap(child) {
                        collect_pre_text(child_el, out);
                    }
                }
            }
            _ => {}
        }
    }
}

fn collect_plain_text(el: ElementRef<'_>) -> String {
    let mut buf = String::new();
    collect_plain_text_into(el, &mut buf, false);
    buf
}

fn collect_plain_text_into(el: ElementRef<'_>, out: &mut String, show_link_urls: bool) {
    for child in el.children() {
        match child.value() {
            Node::Text(t) => out.push_str(t),
            Node::Element(e) => {
                let name = e.name().to_ascii_lowercase();
                if matches!(
                    name.as_str(),
                    "script" | "style" | "head" | "noscript" | "svg" | "template"
                ) {
                    continue;
                }
                if name == "br" {
                    out.push(' ');
                    continue;
                }
                if let Some(child_el) = ElementRef::wrap(child) {
                    if name == "a" && show_link_urls {
                        let mut label = String::new();
                        collect_plain_text_into(child_el, &mut label, false);
                        let label_t = collapse_html_whitespace(&label);
                        out.push_str(label_t.trim());
                        if let Some(href) = e.attr("href") {
                            if let Some(suffix) = format_link_suffix(label_t.trim(), href, true) {
                                out.push_str(&suffix);
                            }
                        }
                    } else {
                        collect_plain_text_into(child_el, out, show_link_urls);
                    }
                }
            }
            _ => {}
        }
    }
}

fn detect_html_code_lang(pre: ElementRef<'_>) -> String {
    for candidate in std::iter::once(pre).chain(
        pre.children()
            .filter_map(ElementRef::wrap)
            .filter(|c| c.value().name().eq_ignore_ascii_case("code")),
    ) {
        if let Some(class) = candidate.value().attr("class") {
            for tok in class.split_whitespace() {
                if let Some(lang) = tok
                    .strip_prefix("language-")
                    .or_else(|| tok.strip_prefix("lang-"))
                {
                    if !lang.is_empty() {
                        return lang.to_string();
                    }
                }
            }
        }
    }
    String::new()
}

fn extract_html_table(table_el: ElementRef<'_>, show_link_urls: bool) -> TableData {
    let mut table = TableData::default();
    let mut tr_nodes: Vec<(bool, ElementRef<'_>)> = Vec::new();
    collect_table_rows(table_el, false, &mut tr_nodes);

    for (in_thead, tr) in tr_nodes {
        let mut cells = Vec::new();
        let mut has_th = false;
        for child in tr.children() {
            if let Some(cell_el) = ElementRef::wrap(child) {
                let cname = cell_el.value().name().to_ascii_lowercase();
                if cname == "th" || cname == "td" {
                    if cname == "th" {
                        has_th = true;
                    }
                    let mut raw = String::new();
                    collect_plain_text_into(cell_el, &mut raw, show_link_urls);
                    cells.push(collapse_html_whitespace(&raw).trim().to_string());
                }
            }
        }
        if cells.is_empty() {
            continue;
        }
        if (in_thead || has_th) && table.headers.is_empty() {
            table.headers = cells;
        } else {
            table.rows.push(cells);
        }
    }
    table
}

fn collect_table_rows<'a>(
    el: ElementRef<'a>,
    in_thead: bool,
    out: &mut Vec<(bool, ElementRef<'a>)>,
) {
    for child in el.children() {
        if let Some(child_el) = ElementRef::wrap(child) {
            let name = child_el.value().name().to_ascii_lowercase();
            match name.as_str() {
                "tr" => out.push((in_thead, child_el)),
                "thead" => collect_table_rows(child_el, true, out),
                "tbody" | "tfoot" => collect_table_rows(child_el, false, out),
                // Do not descend into nested tables
                "table" => {}
                _ => collect_table_rows(child_el, in_thead, out),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::DriveItem;

    fn lines_to_plain(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn test_markup_kind_detection_extensions_and_mimes() {
        let cases = [
            ("README.md", None, Some(MarkupKind::Markdown)),
            ("notes.markdown", None, Some(MarkupKind::Markdown)),
            ("doc.mdown", None, Some(MarkupKind::Markdown)),
            ("page.mdx", None, Some(MarkupKind::Markdown)),
            ("index.html", None, Some(MarkupKind::Html)),
            ("legacy.htm", None, Some(MarkupKind::Html)),
            ("spec.xhtml", None, Some(MarkupKind::Html)),
            ("untitled", Some("text/markdown; charset=utf-8"), Some(MarkupKind::Markdown)),
            ("untitled", Some("text/x-markdown"), Some(MarkupKind::Markdown)),
            ("untitled", Some("text/html; charset=utf-8"), Some(MarkupKind::Html)),
            ("untitled", Some("application/xhtml+xml"), Some(MarkupKind::Html)),
            ("main.rs", Some("text/x-rust"), None),
            ("data.json", Some("application/json"), None),
        ];

        for (name, mime, expected) in cases {
            let item = DriveItem {
                id: "id".into(),
                name: name.into(),
                mime_type: mime.map(str::to_string),
                ..Default::default()
            };
            assert_eq!(MarkupKind::detect(&item), expected, "case {name} / {mime:?}");
            assert_eq!(item.markup_kind(), expected);
            if expected.is_some() {
                assert!(
                    item.is_text_previewable(),
                    "markup item {name} should be text previewable"
                );
            }
        }

        let folder = DriveItem {
            id: "f1".into(),
            name: "docs.md".into(),
            is_folder: true,
            ..Default::default()
        };
        assert_eq!(MarkupKind::detect(&folder), None);
        assert!(!folder.is_text_previewable());
    }

    #[test]
    fn test_markdown_renders_headings_inline_styles_lists_quotes_code_and_tables() {
        let md = "\
# Architecture Overview

## Core Components

Paragraph with **bold**, *italic*, ~~old~~, and `inline_code`.

> Quoted insight line

- [x] Ship renderer
- [ ] Add polish
  - Nested bullet

```rust
fn main() {
    println!(\"hi\");
}
```

| Service | Status |
| --- | --- |
| Graph | Ready |
| Cache | Warm |
";

        for theme in [Theme::midnight_dark(), Theme::no_color()] {
            let lines = render_markdown(md, 72, &theme, false);
            let plain = lines_to_plain(&lines);

            assert!(plain.contains("Architecture Overview"), "{plain}");
            assert!(plain.contains("Core Components"), "{plain}");
            assert!(plain.contains("bold"), "{plain}");
            assert!(plain.contains("italic"), "{plain}");
            assert!(plain.contains("old"), "{plain}");
            assert!(plain.contains("inline_code"), "{plain}");
            assert!(plain.contains("│ Quoted insight line"), "{plain}");
            assert!(plain.contains("Ship renderer"), "{plain}");
            assert!(plain.contains("Add polish"), "{plain}");
            assert!(plain.contains("Nested bullet"), "{plain}");
            assert!(plain.contains("┌─ rust"), "{plain}");
            assert!(plain.contains("fn main()"), "{plain}");
            assert!(plain.contains("Service"), "{plain}");
            assert!(plain.contains("─┼─"), "{plain}");
            assert!(plain.contains("Graph"), "{plain}");

            // Verify semantic ratatui modifiers are applied
            let has_h1_bold = lines.iter().any(|l| {
                l.spans.iter().any(|s| {
                    s.content.contains("Architecture Overview")
                        && s.style.add_modifier.contains(Modifier::BOLD)
                        && s.style.add_modifier.contains(Modifier::UNDERLINED)
                })
            });
            assert!(has_h1_bold, "H1 should have BOLD | UNDERLINED");

            let has_bold_span = lines.iter().any(|l| {
                l.spans.iter().any(|s| {
                    s.content.contains("bold") && s.style.add_modifier.contains(Modifier::BOLD)
                })
            });
            assert!(has_bold_span, "**bold** should have BOLD modifier");

            let has_italic_span = lines.iter().any(|l| {
                l.spans.iter().any(|s| {
                    s.content.contains("italic") && s.style.add_modifier.contains(Modifier::ITALIC)
                })
            });
            assert!(has_italic_span, "*italic* should have ITALIC modifier");

            let has_strike_span = lines.iter().any(|l| {
                l.spans.iter().any(|s| {
                    s.content.contains("old")
                        && s.style.add_modifier.contains(Modifier::CROSSED_OUT)
                })
            });
            assert!(has_strike_span, "~~old~~ should have CROSSED_OUT modifier");

            if theme.no_color {
                assert!(plain.contains("[x] "), "no_color checkbox should use [x]");
                assert!(plain.contains("[ ] "), "no_color checkbox should use [ ]");
            } else {
                assert!(plain.contains("☑ "), "color checkbox should use ☑");
                assert!(plain.contains("☐ "), "color checkbox should use ☐");
            }
        }
    }

    #[test]
    fn test_markdown_and_html_links_respect_show_link_urls_toggle() {
        let theme = Theme::midnight_dark();
        let md = "Read [SharePoint Spec](https://contoso.sharepoint.com/spec) today.";
        let html = r#"<p>Read <a href="https://contoso.sharepoint.com/spec">SharePoint Spec</a> today.</p>"#;

        let md_hidden = lines_to_plain(&render_markdown(md, 80, &theme, false));
        assert!(md_hidden.contains("SharePoint Spec"));
        assert!(md_hidden.contains('↗'));
        assert!(!md_hidden.contains("https://contoso.sharepoint.com/spec"));

        let md_shown = lines_to_plain(&render_markdown(md, 80, &theme, true));
        assert!(md_shown.contains("SharePoint Spec"));
        assert!(md_shown.contains("(https://contoso.sharepoint.com/spec)"));

        let html_hidden = lines_to_plain(&render_html(html, 80, &theme, false));
        assert!(html_hidden.contains("SharePoint Spec"));
        assert!(html_hidden.contains('↗'));
        assert!(!html_hidden.contains("https://contoso.sharepoint.com/spec"));

        let html_shown = lines_to_plain(&render_html(html, 80, &theme, true));
        assert!(html_shown.contains("SharePoint Spec"));
        assert!(html_shown.contains("(https://contoso.sharepoint.com/spec)"));
    }

    #[test]
    fn test_html_strips_script_style_decodes_entities_and_renders_structure() {
        let html = r#"<!DOCTYPE html>
<html>
<head>
  <title>Quarterly Report</title>
  <style>body { color: red; }</style>
  <script>alert(1);</script>
</head>
<body>
  <h1>Summary &amp; Metrics</h1>
  <p>Value &lt; 100 &nbsp; <strong>approved</strong> &amp; <em>verified</em>.</p>
  <noscript>hidden noscript</noscript>
  <svg><text>svg noise</text></svg>
  <template><p>template noise</p></template>
  <pre><code class="language-rust">fn check() {
    let x = 1;
}</code></pre>
  <table>
    <thead><tr><th>Region</th><th>Revenue</th></tr></thead>
    <tbody><tr><td>APAC</td><td>$4.2M</td></tr></tbody>
  </table>
</body>
</html>"#;

        let theme = Theme::midnight_dark();
        let lines = render_html(html, 80, &theme, false);
        let plain = lines_to_plain(&lines);

        // Non-visual noise must be completely absent
        assert!(!plain.contains("alert(1)"), "script must be stripped: {plain}");
        assert!(!plain.contains("color: red"), "style must be stripped: {plain}");
        assert!(!plain.contains("hidden noscript"), "noscript must be stripped");
        assert!(!plain.contains("svg noise"), "svg must be stripped");
        assert!(!plain.contains("template noise"), "template must be stripped");

        // Entities decoded cleanly
        assert!(plain.contains("Summary & Metrics"), "{plain}");
        assert!(plain.contains("Value < 100"), "{plain}");
        assert!(plain.contains("approved & verified"), "{plain}");

        // Pre/code preserves indentation and language header
        assert!(plain.contains("┌─ rust"), "{plain}");
        assert!(plain.contains("│     let x = 1;"), "{plain}");

        // Table rendered with columns and divider
        assert!(plain.contains("Region"), "{plain}");
        assert!(plain.contains("Revenue"), "{plain}");
        assert!(plain.contains("─┼─"), "{plain}");
        assert!(plain.contains("APAC"), "{plain}");
        assert!(plain.contains("$4.2M"), "{plain}");
    }
}
