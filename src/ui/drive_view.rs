//! Minimal-Chrome middle file list (breadcrumb + sorted entries) and right inspector pane
//! (file metadata + version history + live inline image preview + live text/code preview).

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
    Frame,
};

use crate::{
    config::DensityMode,
    models::format_human_size,
    ui::{
        app::{AppState, Focus, PendingPixelGraphic, UiState},
        hitmap::{ButtonId, HitMap, HitTarget},
        truncate_width,
    },
};

pub fn draw_list(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &mut UiState,
    hitmap: &mut HitMap,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    ui.list_scroll.area = area;
    let theme = &ui.theme;
    let focused = ui.focus == Focus::List;

    // Row 0: Breadcrumb bar + sort badge
    let breadcrumb = app.breadcrumb_label();
    let sort_label = format!(" [Sort: {}] ", app.sort_mode.label());
    let bc_budget = (area.width as usize).saturating_sub(sort_label.len() + 2);
    let header_line = Line::from(vec![
        Span::styled(
            format!(" {}", truncate_width(&breadcrumb, bc_budget)),
            theme.title(focused),
        ),
        Span::styled(sort_label.clone(), theme.muted_style()),
    ]);
    let header_bg = if theme.no_color {
        Style::default()
    } else {
        Style::default().bg(theme.bg_header)
    };
    frame.render_widget(
        Paragraph::new(header_line).style(header_bg),
        Rect::new(area.x, area.y, area.width, 1),
    );
    let sort_x = area.x + area.width.saturating_sub(sort_label.len() as u16);
    hitmap.add_line(
        sort_x,
        area.y,
        sort_label.len() as u16,
        HitTarget::Button(ButtonId::SortToggle),
    );
    if !app.folder_stack.is_empty() {
        hitmap.add_line(area.x, area.y, 12.min(area.width), HitTarget::BreadcrumbUp);
    }

    // Row 1: Action toolbar strip
    let mut list_start_y = area.y + 1;
    if area.height >= 4 {
        let toolbar = Line::from(vec![
            Span::styled(" [n] NewDir ", theme.muted_style()),
            Span::styled(" [Ctrl+F] Upload ", theme.muted_style()),
            Span::styled(" [Ctrl+V] PastePNG ", theme.muted_style()),
            Span::styled(" [d] DL ", theme.muted_style()),
            Span::styled(" [s] Share ", theme.muted_style()),
        ]);
        frame.render_widget(
            Paragraph::new(toolbar),
            Rect::new(area.x, area.y + 1, area.width, 1),
        );
        list_start_y = area.y + 2;
    }

    let list_h = area.bottom().saturating_sub(list_start_y) as usize;
    if list_h == 0 {
        return;
    }

    let items = app.sorted_visible_items();
    if items.is_empty() {
        let empty_msg = if app.loading_folder {
            " Loading items..."
        } else {
            " (Empty folder — press Ctrl+F to upload or n to create a folder)"
        };
        frame.render_widget(
            Paragraph::new(Span::styled(empty_msg, theme.muted_style())),
            Rect::new(area.x, list_start_y, area.width, 1),
        );
        return;
    }

    let selected = ui
        .list_scroll
        .selected_idx
        .unwrap_or(0)
        .min(items.len().saturating_sub(1));
    let row_height: usize = if ui.density == DensityMode::Cozy { 2 } else { 1 };
    let visible_items = (list_h / row_height).max(1);
    let skip = selected.saturating_sub(visible_items.saturating_sub(1));

    for (vis_idx, (idx, item)) in items
        .iter()
        .enumerate()
        .skip(skip)
        .take(visible_items)
        .enumerate()
    {
        let y = list_start_y + (vis_idx * row_height) as u16;
        if y >= area.bottom() {
            break;
        }
        let is_sel = idx == selected;
        let row_style = if is_sel && focused {
            theme.selection()
        } else if is_sel {
            if theme.no_color {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default().bg(theme.bg_selected).fg(theme.fg_primary)
            }
        } else {
            Style::default().fg(theme.fg_primary)
        };

        let badge = format!("[{:<4}]", item.type_badge());
        let size_str = if item.is_folder {
            match item.child_count {
                Some(n) => format!("{n} items"),
                None => "folder".to_string(),
            }
        } else {
            format_human_size(item.size)
        };
        let date_str = item
            .last_modified_at
            .as_deref()
            .map(|s| s.get(..10).unwrap_or(s))
            .unwrap_or("");

        let right_meta = format!("{size_str:>9}  {date_str}");
        let name_budget = (area.width as usize).saturating_sub(badge.len() + right_meta.len() + 5);
        let name_display = if item.is_folder {
            format!("{}/", item.name)
        } else {
            item.name.clone()
        };
        let trunc_name = truncate_width(&name_display, name_budget.max(8));

        let badge_style = if is_sel && focused {
            row_style
        } else if item.is_folder {
            theme.accent_style().add_modifier(Modifier::BOLD)
        } else if item.is_image() {
            Style::default().fg(theme.success)
        } else {
            theme.muted_style()
        };

        let line1 = Line::from(vec![
            Span::styled(" ", row_style),
            Span::styled(badge, badge_style),
            Span::styled(format!(" {trunc_name}"), row_style),
            Span::styled(
                format!(
                    "{:>width$}",
                    right_meta,
                    width = (area.width as usize)
                        .saturating_sub(
                            unicode_width::UnicodeWidthStr::width(trunc_name.as_str()) + 9
                        )
                        .max(right_meta.len() + 1)
                ),
                if is_sel && focused {
                    row_style
                } else {
                    theme.muted_style()
                },
            ),
        ]);
        frame.render_widget(Paragraph::new(line1), Rect::new(area.x, y, area.width, 1));

        if row_height == 2 && y + 1 < area.bottom() {
            let author = item
                .last_modified_by
                .as_deref()
                .or(item.created_by.as_deref())
                .unwrap_or("");
            let sub_text = if !author.is_empty() {
                format!("        {} · {}", item.display_path(), author)
            } else {
                format!("        {}", item.display_path())
            };
            frame.render_widget(
                Paragraph::new(Span::styled(
                    truncate_width(&sub_text, area.width as usize),
                    if is_sel && focused {
                        row_style
                    } else {
                        theme.muted_style()
                    },
                )),
                Rect::new(area.x, y + 1, area.width, 1),
            );
        }

        hitmap.add(
            Rect::new(area.x, y, area.width, row_height as u16),
            HitTarget::ListItem(idx),
        );
    }
}

pub fn draw_inspector(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &mut UiState,
    hitmap: &mut HitMap,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    ui.inspector_scroll.area = area;
    let theme = &ui.theme;
    let focused = ui.focus == Focus::Inspector;

    let Some(item) = app.active_item() else {
        frame.render_widget(
            Paragraph::new(Span::styled(
                " Select a file or folder to inspect metadata, preview images, or read text",
                theme.muted_style(),
            )),
            area,
        );
        return;
    };

    // Row 0: Inspector Title
    let header_bg = if theme.no_color {
        Style::default()
    } else {
        Style::default().bg(theme.bg_header)
    };
    let title = format!(" [{}] {} ", item.type_badge(), item.name);
    frame.render_widget(
        Paragraph::new(Span::styled(
            truncate_width(&title, area.width as usize),
            theme.title(focused),
        ))
        .style(header_bg),
        Rect::new(area.x, area.y, area.width, 1),
    );

    let body_area = Rect::new(
        area.x,
        area.y + 1,
        area.width,
        area.height.saturating_sub(1),
    );
    if body_area.height == 0 {
        return;
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(meta_line("Path", &item.display_path(), theme));
    lines.push(meta_line(
        "Size",
        &if item.is_folder {
            format!(
                "Folder ({} items)",
                item.child_count
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "?".to_string())
            )
        } else {
            format!("{} ({} bytes)", format_human_size(item.size), item.size)
        },
        theme,
    ));
    if let Some(ref mime) = item.mime_type {
        lines.push(meta_line("MIME", mime, theme));
    }
    if let Some(ref author) = item.last_modified_by.as_ref().or(item.created_by.as_ref()) {
        lines.push(meta_line("Author", author, theme));
    }
    if let Some(ref modified) = item.last_modified_at {
        lines.push(meta_line("Modified", modified, theme));
    }
    if let Some(ref scope) = item.shared_scope {
        lines.push(meta_line("Sharing", scope, theme));
    }
    if let Some(ref share_url) = app.created_share_links.get(&item.id) {
        lines.push(meta_line("Share Link", share_url, theme));
    }
    if let Some(ref url) = item.web_url {
        let shown = if ui.show_link_urls {
            url.clone()
        } else {
            "[Open in Browser] (press o, h to reveal URL, u for Links)".to_string()
        };
        lines.push(meta_line("Web URL", &shown, theme));
    }

    // Version history summary if loaded
    if let Some(versions) = app.item_versions.get(&item.id) {
        if !versions.is_empty() {
            lines.push(Line::from(Span::styled(
                format!(" Versions ({}):", versions.len()),
                theme.accent_style().add_modifier(Modifier::BOLD),
            )));
            for v in versions.iter().take(4) {
                lines.push(Line::from(Span::styled(
                    format!(
                        "   v{} · {} · {} · {}",
                        v.id,
                        format_human_size(v.size),
                        v.last_modified_at
                            .as_deref()
                            .map(|s| s.get(..16).unwrap_or(s))
                            .unwrap_or(""),
                        v.last_modified_by.as_deref().unwrap_or("")
                    ),
                    theme.muted_style(),
                )));
            }
        }
    }

    lines.push(Line::from(Span::styled(
        " ────────────────────────────────────────",
        theme.muted_style(),
    )));

    let meta_rows_used = lines.len() as u16;

    // Live inline image preview for image files
    if item.is_image() && ui.inline_images {
        let preview_key = app.inline_image_cache_key(&item);
        if let Some(rendered) = app.rendered_images.get(&preview_key) {
            lines.push(Line::from(Span::styled(
                format!(
                    " Image Preview ({}x{} · {} · press v for HD overlay):",
                    rendered.width_px,
                    rendered.height_px,
                    rendered.protocol.label()
                ),
                theme.accent_style(),
            )));
            let img_y = body_area.y.saturating_add(meta_rows_used + 1);
            if rendered.protocol.is_pixel()
                && rendered.pixel_payload.is_some()
                && ui.overlays.is_empty()
                && img_y < body_area.bottom()
            {
                let avail_h = body_area.bottom().saturating_sub(img_y);
                let rect = Rect::new(
                    body_area.x + 1,
                    img_y,
                    body_area.width.saturating_sub(2).min(rendered.cols),
                    avail_h.min(rendered.rows),
                );
                if rect.width > 0 && rect.height > 0 {
                    ui.pending_pixel_graphics.push(PendingPixelGraphic {
                        cache_key: preview_key,
                        rect,
                        protocol: rendered.protocol,
                    });
                    hitmap.add(rect, HitTarget::PreviewImageChip);
                }
            } else {
                for img_line in &rendered.lines {
                    lines.push(img_line.clone());
                }
            }
        } else if let Some(err) = app.preview_errors.get(&item.id) {
            lines.push(Line::from(Span::styled(
                format!(" Image preview unavailable: {err}"),
                Style::default().fg(theme.warning),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                " Loading inline image preview...",
                theme.muted_style(),
            )));
        }
    } else if item.is_text_previewable() {
        if let Some(content) = app.text_previews.get(&item.id) {
            lines.push(Line::from(Span::styled(
                format!(
                    " File Preview ({} lines · u Links · h href {}):",
                    content.lines().count(),
                    if ui.show_link_urls { "ON" } else { "OFF" }
                ),
                theme.accent_style(),
            )));
            for raw_line in content.lines().take(500) {
                lines.push(Line::from(Span::styled(
                    format!(" {raw_line}"),
                    Style::default().fg(theme.fg_primary),
                )));
            }
        } else if let Some(err) = app.preview_errors.get(&item.id) {
            lines.push(Line::from(Span::styled(
                format!(" Text preview unavailable: {err}"),
                Style::default().fg(theme.warning),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                " Loading text preview...",
                theme.muted_style(),
            )));
        }
    } else if item.is_folder {
        lines.push(Line::from(Span::styled(
            " Press Enter to open folder, n to create subfolder, Ctrl+F to upload file, Ctrl+V to paste PNG.",
            theme.muted_style(),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            " Binary file — press d to download, s to create share link, o to open in browser.",
            theme.muted_style(),
        )));
    }

    ui.inspector_scroll.total_lines = lines.len();
    let max_off = lines.len().saturating_sub(body_area.height as usize);
    let scroll_y = ui.inspector_scroll.offset.min(max_off) as u16;
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll_y, 0)),
        body_area,
    );
}

fn meta_line(label: &str, value: &str, theme: &crate::ui::theme::Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!(" {:<10} ", label), theme.muted_style()),
        Span::styled(value.to_string(), Style::default().fg(theme.fg_primary)),
    ])
}
