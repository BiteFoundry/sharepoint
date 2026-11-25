//! Styled terminal Markdown (CommonMark + GFM + YAML frontmatter + Math + Footnotes)
//! and HTML5 renderer for the TUI inspector, delegated to `bitefoundry-html`.

use bitefoundry_html::{DomDocument, GraphicKind, HtmlTheme};
use ratatui::text::{Line, Span};

use crate::config::ThemePreset;
pub use crate::models::MarkupKind;
use crate::ui::overlays::html_preview::HtmlPreviewOverlay;
use crate::ui::theme::Theme;

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

/// Maps the SharePoint TUI `Theme` to `bitefoundry_html::HtmlTheme`.
pub fn to_html_theme(theme: &Theme) -> HtmlTheme {
    HtmlPreviewOverlay::html_theme(theme)
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
    let html_theme = to_html_theme(theme);
    bitefoundry_html::markdown::render_markdown(raw, width.max(12), &html_theme, show_link_urls)
}

/// Renders an HTML document or fragment into styled terminal `Line`s wrapped to `width`.
pub fn render_html(
    raw: &str,
    width: u16,
    theme: &Theme,
    show_link_urls: bool,
) -> Vec<Line<'static>> {
    let html_theme = to_html_theme(theme);
    let doc = DomDocument::parse(raw);
    let mut rendered = bitefoundry_html::render_dom(
        &doc,
        width.max(12),
        &html_theme,
        None,
        None,
        show_link_urls,
    );

    // The static inspector renders `Vec<Line>` without blitting inline pixel graphics;
    // replace reserved graphic placeholder slots with clean text chips.
    if !rendered.graphics.is_empty() {
        let mut graphics = rendered.graphics;
        graphics.sort_by(|a, b| b.line_start.cmp(&a.line_start));
        for g in graphics {
            let start = g.line_start.min(rendered.lines.len());
            let end = start.saturating_add(g.line_count).min(rendered.lines.len());
            let chip = match g.kind {
                GraphicKind::InlineSvg => "[◇ Vector Graphic · v: Interactive]".to_string(),
                GraphicKind::Canvas2d => "[▣ Canvas · v: Interactive]".to_string(),
                GraphicKind::DataImage => match g.alt.as_deref().map(str::trim) {
                    Some(alt) if !alt.is_empty() => format!("[🖼 {alt} · v: Interactive]"),
                    _ => "[🖼 Image · v: Interactive]".to_string(),
                },
            };
            let chip_line = Line::from(Span::styled(chip, theme.muted_style()));
            rendered.lines.splice(start..end, std::iter::once(chip_line));
        }
    }

    rendered.lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::DriveItem;
    use ratatui::style::Modifier;

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
        let html = r##"<!DOCTYPE html>
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
  <svg width="64" height="32"><rect width="64" height="32" fill="#2563eb"/><text>svg noise</text></svg>
  <canvas>canvas noise</canvas>
  <template><p>template noise</p></template>
  <pre><code class="language-rust">fn check() {
    let x = 1;
}</code></pre>
  <table>
    <thead><tr><th>Region</th><th>Revenue</th></tr></thead>
    <tbody><tr><td>APAC</td><td>$4.2M</td></tr></tbody>
  </table>
</body>
</html>"##;

        let theme = Theme::midnight_dark();
        let lines = render_html(html, 80, &theme, false);
        let plain = lines_to_plain(&lines);

        // Non-visual noise must be completely absent
        assert!(!plain.contains("alert(1)"), "script must be stripped: {plain}");
        assert!(!plain.contains("color: red"), "style must be stripped: {plain}");
        assert!(!plain.contains("hidden noscript"), "noscript must be stripped");
        assert!(!plain.contains("svg noise"), "svg inner XML/text must be stripped");
        assert!(!plain.contains("canvas noise"), "canvas fallback text must be stripped");
        assert!(
            plain.contains("[◇ Vector Graphic"),
            "svg must render clean graphic chip: {plain}"
        );
        assert!(
            plain.contains("[▣"),
            "canvas must render clean chip: {plain}"
        );
        assert!(!plain.contains("template noise"), "template must be stripped");

        // Entities decoded cleanly
        assert!(plain.contains("Summary & Metrics"), "{plain}");
        assert!(plain.contains("Value < 100"), "{plain}");
        assert!(plain.contains("approved & verified"), "{plain}");

        // Pre/code preserves indentation
        assert!(plain.contains("fn check()"), "{plain}");
        assert!(plain.contains("│     let x = 1;"), "{plain}");

        // Table rendered with columns and divider
        assert!(plain.contains("Region"), "{plain}");
        assert!(plain.contains("Revenue"), "{plain}");
        assert!(plain.contains("─┼─"), "{plain}");
        assert!(plain.contains("APAC"), "{plain}");
        assert!(plain.contains("$4.2M"), "{plain}");
    }
}
