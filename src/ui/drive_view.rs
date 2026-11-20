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
        markup::{self, CachedMarkupPreview, MarkupCacheKey, MarkupKind},
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

    // Row 0: Breadcrumb bar + clickable right-side pills ([Sort], [Density], [↻])
    let breadcrumb = app.breadcrumb_label();
    let sort_label = format!("[Sort: {}]", app.sort_mode.label());
    let density_label = match ui.density {
        DensityMode::Cozy => "[Cozy]",
        DensityMode::Compact => "[Comp]",
    };
    let refresh_label = "[↻]";
    let header_bg = if theme.no_color {
        Style::default()
    } else {
        Style::default().bg(theme.bg_header)
    };

    let refresh_w = unicode_width::UnicodeWidthStr::width(refresh_label) as u16;
    let density_w = unicode_width::UnicodeWidthStr::width(density_label) as u16;
    let sort_w = unicode_width::UnicodeWidthStr::width(sort_label.as_str()) as u16;
    let show_refresh = area.width > sort_w + density_w + refresh_w + 10;
    let show_density = area.width > sort_w + density_w + 8;
    let pills_total = sort_w
        + if show_density { density_w + 1 } else { 0 }
        + if show_refresh { refresh_w + 1 } else { 0 }
        + 1;
    let bc_budget = (area.width as usize).saturating_sub(pills_total as usize + 2);
    let bc_text = format!(" {}", truncate_width(&breadcrumb, bc_budget));
    let bc_w = unicode_width::UnicodeWidthStr::width(bc_text.as_str()) as u16;
    let pad = area.width.saturating_sub(bc_w + pills_total) as usize;

    let mut header_spans = vec![
        Span::styled(bc_text, theme.title(focused)),
        Span::styled(" ".repeat(pad), header_bg),
        Span::styled(sort_label.clone(), theme.muted_style()),
    ];
    if show_density {
        header_spans.push(Span::styled(" ", header_bg));
        header_spans.push(Span::styled(density_label, theme.muted_style()));
    }
    if show_refresh {
        header_spans.push(Span::styled(" ", header_bg));
        header_spans.push(Span::styled(refresh_label, theme.muted_style()));
    }
    header_spans.push(Span::styled(" ", header_bg));
    frame.render_widget(
        Paragraph::new(Line::from(header_spans)).style(header_bg),
        Rect::new(area.x, area.y, area.width, 1),
    );

    let mut pill_x = area.x + area.width.saturating_sub(pills_total);
    hitmap.add_line(pill_x, area.y, sort_w, HitTarget::Button(ButtonId::SortToggle));
    pill_x = pill_x.saturating_add(sort_w + 1);
    if show_density {
        hitmap.add_line(
            pill_x,
            area.y,
            density_w,
            HitTarget::Button(ButtonId::DensityToggle),
        );
        pill_x = pill_x.saturating_add(density_w + 1);
    }
    if show_refresh {
        hitmap.add_line(pill_x, area.y, refresh_w, HitTarget::Button(ButtonId::Refresh));
    }
    if !app.folder_stack.is_empty() {
        hitmap.add_line(area.x, area.y, 12.min(area.width), HitTarget::BreadcrumbUp);
    }

    // Row 1: Clickable action toolbar strip
    let mut list_start_y = area.y + 1;
    if area.height >= 4 {
        let toolbar_pills: [(&str, ButtonId); 6] = [
            ("[n] New", ButtonId::NewFolder),
            ("[Ctrl+F] Up", ButtonId::UploadFile),
            ("[d] DL", ButtonId::DownloadItem),
            ("[s] Share", ButtonId::ShareItem),
            ("[r] Ren", ButtonId::RenameItem),
            ("[x] Del", ButtonId::DeleteItem),
        ];
        let mut spans: Vec<Span<'static>> = vec![Span::raw(" ")];
        let mut cur_x = area.x.saturating_add(1);
        for (idx, (label, btn)) in toolbar_pills.iter().enumerate() {
            let w = unicode_width::UnicodeWidthStr::width(*label) as u16;
            let sep_w: u16 = if idx > 0 { 1 } else { 0 };
            if cur_x + sep_w + w > area.right() {
                break;
            }
            if sep_w > 0 {
                spans.push(Span::raw(" "));
                cur_x = cur_x.saturating_add(1);
            }
            spans.push(Span::styled((*label).to_string(), theme.muted_style()));
            hitmap.add_line(cur_x, area.y + 1, w, HitTarget::Button(*btn));
            cur_x = cur_x.saturating_add(w);
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)),
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
        ui.markup_cache = None;
        frame.render_widget(
            Paragraph::new(Span::styled(
                " Select a file or folder to inspect metadata, preview images, or read text",
                theme.muted_style(),
            )),
            area,
        );
        return;
    };

    // Row 0: Inspector Title (+ clickable pills for HTML/Markdown/Image items)
    let markup_kind = MarkupKind::detect(&item);
    let header_bg = if theme.no_color {
        Style::default()
    } else {
        Style::default().bg(theme.bg_header)
    };
    let title = format!(" [{}] {} ", item.type_badge(), item.name);
    let mut right_pills: Vec<(&str, Style, ButtonId)> = Vec::new();
    if markup_kind == Some(MarkupKind::Html) {
        right_pills.push(("[⚡ Interactive (v)]", theme.reaction_style(true), ButtonId::HtmlPreview));
        right_pills.push(("[🌐 Serve (o)]", theme.reaction_style(false), ButtonId::ServeHtml));
        let mode_label = if ui.render_markup {
            "[m: Rendered]"
        } else {
            "[m: Raw]"
        };
        let mode_style = if ui.render_markup {
            theme.reaction_style(true)
        } else {
            theme.muted_style()
        };
        right_pills.push((mode_label, mode_style, ButtonId::PreviewRenderToggle));
    } else if markup_kind.is_some() {
        let mode_label = if ui.render_markup {
            " [m: Rendered] "
        } else {
            " [m: Raw] "
        };
        let mode_style = if ui.render_markup {
            theme.reaction_style(true)
        } else {
            theme.muted_style()
        };
        right_pills.push((mode_label, mode_style, ButtonId::PreviewRenderToggle));
    } else if item.is_image() {
        right_pills.push(("[v: HD Preview]", theme.reaction_style(true), ButtonId::ImagePreview));
    }

    // Select as many right pills as fit cleanly alongside the title
    let mut chosen_pills: Vec<(&str, Style, ButtonId, u16)> = Vec::new();
    let mut used_pills_w: u16 = 0;
    for (label, style, btn) in right_pills {
        let w = unicode_width::UnicodeWidthStr::width(label) as u16;
        let gap: u16 = if chosen_pills.is_empty() { 0 } else { 1 };
        if area.width > used_pills_w + gap + w + 6 {
            chosen_pills.push((label, style, btn, w));
            used_pills_w = used_pills_w.saturating_add(gap + w);
        }
    }

    if !chosen_pills.is_empty() {
        let trailing_space: u16 = if markup_kind == Some(MarkupKind::Markdown) { 0 } else { 1 };
        let total_right = used_pills_w.saturating_add(trailing_space);
        let title_budget = (area.width as usize).saturating_sub(total_right as usize + 1);
        let trunc_title = truncate_width(&title, title_budget);
        let title_w = unicode_width::UnicodeWidthStr::width(trunc_title.as_str());
        let pad = (area.width as usize)
            .saturating_sub(title_w)
            .saturating_sub(total_right as usize);
        let mut header_spans = vec![
            Span::styled(trunc_title, theme.title(focused)),
            Span::styled(" ".repeat(pad), header_bg),
        ];
        let mut pill_x = area.x + area.width.saturating_sub(total_right);
        for (idx, (label, style, btn, w)) in chosen_pills.iter().enumerate() {
            if idx > 0 {
                header_spans.push(Span::styled(" ", header_bg));
                pill_x = pill_x.saturating_add(1);
            }
            header_spans.push(Span::styled((*label).to_string(), *style));
            hitmap.add_line(pill_x, area.y, *w, HitTarget::Button(*btn));
            pill_x = pill_x.saturating_add(*w);
        }
        if trailing_space > 0 {
            header_spans.push(Span::styled(" ", header_bg));
        }
        frame.render_widget(
            Paragraph::new(Line::from(header_spans)).style(header_bg),
            Rect::new(area.x, area.y, area.width, 1),
        );
    } else {
        frame.render_widget(
            Paragraph::new(Span::styled(
                truncate_width(&title, area.width as usize),
                theme.title(focused),
            ))
            .style(header_bg),
            Rect::new(area.x, area.y, area.width, 1),
        );
    }

    // Row 1: Clickable Inspector Action Pills ([u: Links], [o: Open], [v: HD], [d: DL], [s: Share])
    let mut body_start_y = area.y + 1;
    if area.height >= 4 {
        let mut insp_pills: Vec<(&str, ButtonId)> = Vec::new();
        if item.is_image() {
            insp_pills.push(("[v: HD]", ButtonId::ImagePreview));
        } else if markup_kind == Some(MarkupKind::Html) {
            insp_pills.push(("[v: Interactive]", ButtonId::HtmlPreview));
            insp_pills.push(("[o: Serve]", ButtonId::ServeHtml));
        }
        insp_pills.push(("[u: Links]", ButtonId::LinksModal));
        insp_pills.push(("[o: Open]", ButtonId::OpenInBrowser));
        if !item.is_folder {
            insp_pills.push(("[d: DL]", ButtonId::DownloadItem));
        }
        insp_pills.push(("[s: Share]", ButtonId::ShareItem));

        let mut spans: Vec<Span<'static>> = vec![Span::raw(" ")];
        let mut cur_x = area.x.saturating_add(1);
        for (idx, (label, btn)) in insp_pills.iter().enumerate() {
            let w = unicode_width::UnicodeWidthStr::width(*label) as u16;
            let sep_w: u16 = if idx > 0 { 1 } else { 0 };
            if cur_x + sep_w + w > area.right() {
                break;
            }
            if sep_w > 0 {
                spans.push(Span::raw(" "));
                cur_x = cur_x.saturating_add(1);
            }
            spans.push(Span::styled((*label).to_string(), theme.muted_style()));
            hitmap.add_line(cur_x, body_start_y, w, HitTarget::Button(*btn));
            cur_x = cur_x.saturating_add(w);
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect::new(area.x, body_start_y, area.width, 1),
        );
        body_start_y = area.y + 2;
    }

    let body_area = Rect::new(
        area.x,
        body_start_y,
        area.width,
        area.bottom().saturating_sub(body_start_y),
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
    let active_preview_key = AppState::text_preview_key(item.effective_drive_id(), &item.id);
    if markup_kind.is_none()
        || ui
            .markup_cache
            .as_ref()
            .is_some_and(|cached| cached.key.item_id != active_preview_key)
    {
        ui.markup_cache = None;
    }

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
        if let Some(content) = app.text_preview_for(&item) {
            if let Some(kind) = markup_kind {
                if ui.render_markup {
                    lines.push(Line::from(Span::styled(
                        format!(
                            " {} Preview (Rendered · m Raw · u Links · h href {}):",
                            kind.label(),
                            if ui.show_link_urls { "ON" } else { "OFF" }
                        ),
                        theme.accent_style(),
                    )));
                    let render_w = body_area.width.saturating_sub(2);
                    let cache_key = MarkupCacheKey::new(
                        &active_preview_key,
                        kind,
                        content,
                        render_w,
                        ui.theme_preset,
                        theme.no_color,
                        ui.show_link_urls,
                    );
                    let cache_hit = ui
                        .markup_cache
                        .as_ref()
                        .is_some_and(|c| c.key == cache_key);
                    if !cache_hit {
                        let rendered = markup::render_markup(
                            kind,
                            content,
                            render_w,
                            ui.show_link_urls,
                            theme,
                        );
                        ui.markup_cache = Some(CachedMarkupPreview {
                            key: cache_key,
                            lines: rendered,
                        });
                    }
                    if let Some(ref cached) = ui.markup_cache {
                        lines.extend(cached.lines.iter().cloned());
                    }
                } else {
                    lines.push(Line::from(Span::styled(
                        format!(
                            " {} Source (Raw · {} lines · m Render · u Links):",
                            kind.label(),
                            content.lines().count()
                        ),
                        theme.accent_style(),
                    )));
                    for raw_line in content.lines().take(500) {
                        lines.push(Line::from(Span::styled(
                            format!(" {raw_line}"),
                            Style::default().fg(theme.fg_primary),
                        )));
                    }
                }
            } else {
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
            }
        } else if let Some(err) = app.text_preview_error_for(&item) {
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
    ui.inspector_scroll.offset = ui.inspector_scroll.offset.min(max_off);
    let scroll_y = ui.inspector_scroll.offset as u16;
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};
    use tempfile::tempdir;
    use tokio::sync::{mpsc, watch};

    use crate::{
        auth::AuthManager,
        config::{AppConfig, EnvVarGuard, Persona, TEST_ENV_LOCK},
        graph::GraphClient,
        models::DriveItem,
        storage::Storage,
        sync::{FocusTarget, SyncCommand, SyncController},
        ui::{
            action::{dispatch, Action},
            app::UiMsg,
            overlays::{
                command_palette::CommandPalette,
                context_menu::{ContextMenu, ContextTarget},
                settings::SettingsState,
                OverlayOutcome,
            },
        },
    };

    fn build_test_app_and_ui() -> (AppState, UiState, mpsc::UnboundedReceiver<UiMsg>) {
        let cfg = AppConfig::default();
        let auth = AuthManager::new(cfg.clone()).expect("auth");
        let graph = GraphClient::new(cfg.clone(), auth.clone()).expect("graph");
        let storage = Storage::open_in_memory().expect("storage");
        let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
        let (nudge_tx, _nudge_rx) = mpsc::unbounded_channel::<SyncCommand>();
        let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx);
        let (ui_tx, ui_rx) = mpsc::unbounded_channel::<UiMsg>();
        let ui = UiState::from_config(&cfg);
        let app = AppState::new(cfg, Persona::SharePoint, auth, graph, storage, sync_ctrl, ui_tx);
        (app, ui, ui_rx)
    }

    fn render_inspector_to_string(
        app: &AppState,
        ui: &mut UiState,
        hitmap: &mut HitMap,
        width: u16,
        height: u16,
    ) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw_inspector(frame, area, app, ui, hitmap);
            })
            .expect("draw");
        let buf = terminal.backend().buffer();
        let mut lines = Vec::with_capacity(height as usize);
        for y in 0..height {
            let mut row = String::new();
            for x in 0..width {
                row.push_str(buf[(x, y)].symbol());
            }
            lines.push(row);
        }
        lines.join("\n")
    }

    #[tokio::test]
    async fn inspector_renders_markdown_and_toggles_raw_via_hitmap_and_action() {
        let _lock = TEST_ENV_LOCK.lock().await;
        let cfg_dir = tempdir().unwrap();
        let _env = EnvVarGuard::set(&[(
            "SHAREPOINT_CONFIG_DIR",
            Some(cfg_dir.path().to_str().unwrap()),
        )]);

        let (mut app, mut ui, _rx) = build_test_app_and_ui();
        let md_item = DriveItem {
            id: "md-1".into(),
            drive_id: "d1".into(),
            name: "README.md".into(),
            size: 120,
            ..Default::default()
        };
        app.current_items = vec![md_item.clone()];
        app.active_selected_id = Some(md_item.id.clone());
        app.set_text_preview(
            md_item.effective_drive_id(),
            &md_item.id,
            "# Launch Plan\n\nCheck **bold** and [Docs](https://example.com/docs).\n".into(),
        );

        assert!(ui.render_markup);

        // 1. Rendered mode: pill `[m: Rendered]`, HitMap button registered, Markdown rendered
        let mut hitmap = HitMap::new();
        let rendered_screen = render_inspector_to_string(&app, &mut ui, &mut hitmap, 80, 24);
        assert!(rendered_screen.contains("[m: Rendered]"), "{rendered_screen}");
        assert!(rendered_screen.contains("Markdown Preview (Rendered"), "{rendered_screen}");
        assert!(rendered_screen.contains("▌ Launch Plan"), "{rendered_screen}");
        assert!(
            !rendered_screen.contains("# Launch Plan"),
            "rendered view should not show raw markdown heading hashes:\n{rendered_screen}"
        );

        let pill_rect = hitmap
            .region_of(HitTarget::Button(ButtonId::PreviewRenderToggle))
            .expect("PreviewRenderToggle hitmap region must be registered on .md item");
        ui.hitmap = hitmap;

        // 2. Clicking the header pill switches to Raw mode
        let quit = dispatch(
            Action::Click {
                column: pill_rect.x + 1,
                row: pill_rect.y,
            },
            &mut app,
            &mut ui,
        );
        assert!(!quit);
        assert!(!ui.render_markup);

        let mut hitmap_raw = HitMap::new();
        let raw_screen = render_inspector_to_string(&app, &mut ui, &mut hitmap_raw, 80, 24);
        assert!(raw_screen.contains("[m: Raw]"), "{raw_screen}");
        assert!(raw_screen.contains("Markdown Source (Raw"), "{raw_screen}");
        assert!(raw_screen.contains("# Launch Plan"), "{raw_screen}");
        assert!(
            hitmap_raw
                .region_of(HitTarget::Button(ButtonId::PreviewRenderToggle))
                .is_some()
        );

        // 3. Pressing `m` (`TogglePreviewRenderMode`) toggles back to Rendered
        dispatch(Action::TogglePreviewRenderMode, &mut app, &mut ui);
        assert!(ui.render_markup);

        // 4. ContextMenu, CommandPalette, and Settings all wire TogglePreviewRenderMode
        let menu = ContextMenu::build(ContextTarget::DriveItem(0), (0, 0), &app, &ui);
        assert!(menu
            .items
            .iter()
            .any(|item| item.action == Action::TogglePreviewRenderMode));

        let palette = CommandPalette {
            query: ">markdown".into(),
            selected: 0,
        };
        assert!(palette
            .filtered(&app, &ui)
            .iter()
            .any(|item| item.action == Action::TogglePreviewRenderMode));

        let mut settings = SettingsState { selected: 3 };
        assert_eq!(
            settings.handle(&Action::Activate, &app),
            OverlayOutcome::Dispatch(Action::TogglePreviewRenderMode)
        );
    }

    #[tokio::test]
    async fn inspector_renders_html_and_omits_toggle_pill_for_plain_code_files() {
        let _lock = TEST_ENV_LOCK.lock().await;
        let cfg_dir = tempdir().unwrap();
        let _env = EnvVarGuard::set(&[(
            "SHAREPOINT_CONFIG_DIR",
            Some(cfg_dir.path().to_str().unwrap()),
        )]);

        let (mut app, mut ui, _rx) = build_test_app_and_ui();
        let html_item = DriveItem {
            id: "html-1".into(),
            drive_id: "d1".into(),
            name: "report.html".into(),
            size: 160,
            ..Default::default()
        };
        app.current_items = vec![html_item.clone()];
        app.active_selected_id = Some(html_item.id.clone());
        app.set_text_preview(
            html_item.effective_drive_id(),
            &html_item.id,
            "<html><head><script>evil()</script></head><body><h1>Q4 &amp; Review</h1><p>All good.</p></body></html>"
                .into(),
        );

        let mut hitmap = HitMap::new();
        let html_screen = render_inspector_to_string(&app, &mut ui, &mut hitmap, 80, 24);
        assert!(html_screen.contains("[m: Rendered]"), "{html_screen}");
        assert!(html_screen.contains("HTML Preview (Rendered"), "{html_screen}");
        assert!(html_screen.contains("Q4 & Review"), "{html_screen}");
        assert!(!html_screen.contains("evil()"), "{html_screen}");
        assert!(hitmap
            .region_of(HitTarget::Button(ButtonId::PreviewRenderToggle))
            .is_some());

        // Plain code file (.rs) must not show the markup toggle pill
        let rs_item = DriveItem {
            id: "rs-1".into(),
            drive_id: "d1".into(),
            name: "main.rs".into(),
            size: 40,
            ..Default::default()
        };
        app.current_items = vec![rs_item.clone()];
        app.active_selected_id = Some(rs_item.id.clone());
        app.set_text_preview(rs_item.effective_drive_id(), &rs_item.id, "fn main() {}\n".into());

        let mut rs_hitmap = HitMap::new();
        let rs_screen = render_inspector_to_string(&app, &mut ui, &mut rs_hitmap, 80, 24);
        assert!(!rs_screen.contains("[m: Rendered]"), "{rs_screen}");
        assert!(rs_screen.contains("fn main() {}"), "{rs_screen}");
        assert!(rs_hitmap
            .region_of(HitTarget::Button(ButtonId::PreviewRenderToggle))
            .is_none());
    }

    #[tokio::test]
    async fn html_item_header_pills_open_interactive_overlay_and_v_opens_html_preview() {
        let _lock = TEST_ENV_LOCK.lock().await;
        let cfg_dir = tempdir().unwrap();
        let _env = EnvVarGuard::set(&[(
            "SHAREPOINT_CONFIG_DIR",
            Some(cfg_dir.path().to_str().unwrap()),
        )]);

        let (mut app, mut ui, mut rx) = build_test_app_and_ui();
        let html_item = DriveItem {
            id: "html-v-1".into(),
            drive_id: "d1".into(),
            name: "demo.html".into(),
            size: 180,
            ..Default::default()
        };
        app.current_items = vec![html_item.clone()];
        app.active_selected_id = Some(html_item.id.clone());
        app.set_text_preview(
            html_item.effective_drive_id(),
            &html_item.id,
            "<html><body><p id=\"out\">Ready</p><button onclick=\"document.getElementById('out').textContent='Done'\">Go</button></body></html>"
                .into(),
        );

        let mut hitmap = HitMap::new();
        let screen = render_inspector_to_string(&app, &mut ui, &mut hitmap, 100, 24);
        assert!(screen.contains("⚡") && screen.contains("Interactive (v)]"), "{screen}");
        assert!(screen.contains("🌐") && screen.contains("Serve (o)]"), "{screen}");
        assert!(screen.contains("[m: Rendered]"), "{screen}");
        let interactive_rect = hitmap
            .region_of(HitTarget::Button(ButtonId::HtmlPreview))
            .expect("HtmlPreview pill must be registered");
        let serve_rect = hitmap
            .region_of(HitTarget::Button(ButtonId::ServeHtml))
            .expect("ServeHtml pill must be registered");
        assert!(hitmap
            .region_of(HitTarget::Button(ButtonId::PreviewRenderToggle))
            .is_some());

        // Clicking [⚡ Interactive (v)] opens Overlay::HtmlPreview
        ui.hitmap = hitmap;
        dispatch(
            Action::Click {
                column: interactive_rect.x + 1,
                row: interactive_rect.y,
            },
            &mut app,
            &mut ui,
        );
        assert!(matches!(
            ui.overlays.last(),
            Some(crate::ui::overlays::Overlay::HtmlPreview(_))
        ));

        // Close overlay, then pressing `v` (`Action::OpenImagePreview`) on HTML item opens HtmlPreview (not "No images")
        dispatch(Action::CloseOverlay, &mut app, &mut ui);
        assert!(ui.overlays.is_empty());
        dispatch(Action::OpenImagePreview, &mut app, &mut ui);
        assert!(matches!(
            ui.overlays.last(),
            Some(crate::ui::overlays::Overlay::HtmlPreview(_))
        ));
        dispatch(Action::CloseOverlay, &mut app, &mut ui);

        // Clicking [🌐 Serve (o)] serves HTML via lazy NativeHtmlServer and emits UiMsg::HtmlServed
        dispatch(
            Action::Click {
                column: serve_rect.x + 1,
                row: serve_rect.y,
            },
            &mut app,
            &mut ui,
        );
        let served_msg = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
            .await
            .expect("expected UiMsg::HtmlServed")
            .expect("channel open");
        let url = match served_msg {
            UiMsg::HtmlServed { url } => url,
            _ => panic!("expected UiMsg::HtmlServed"),
        };
        assert!(url.starts_with("http://127.0.0.1:"), "{url}");
        app.html_server.shutdown();
    }

    #[tokio::test]
    async fn html_and_svg_sniff_routing_from_image_decode_path() {
        let (mut app, mut ui, mut rx) = build_test_app_and_ui();

        // 1. HTML bytes masquerading as .png -> routed cleanly to Overlay::HtmlPreview, never raster-decoded
        let fake_png = DriveItem {
            id: "fake-png-1".into(),
            drive_id: "d1".into(),
            name: "avatar.png".into(),
            mime_type: Some("image/png".into()),
            ..Default::default()
        };
        app.current_items = vec![fake_png.clone()];
        app.active_selected_id = Some(fake_png.id.clone());
        app.raw_image_bytes.insert(
            fake_png.id.clone(),
            std::sync::Arc::from(b"<!DOCTYPE html><html><body><h1>Sign In</h1></body></html>".as_slice()),
        );
        let key = app.modal_image_cache_key(&fake_png, 40, 20, crate::img_preview::TerminalImageProtocol::Halfblock);
        app.request_image_decode(&fake_png, key, 40, 20, ui.graphics_caps);

        let msg = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
            .await
            .expect("expected UiMsg from request_image_decode")
            .expect("channel open");
        assert!(matches!(msg, UiMsg::HtmlContentSniffed { .. }));
        app.apply_ui_msg(msg, &mut ui);
        assert!(matches!(
            ui.overlays.last(),
            Some(crate::ui::overlays::Overlay::HtmlPreview(_))
        ));
        assert!(app.rendered_images.is_empty());

        // 2. SVG bytes masquerading as .png -> informative SVG preview error, never raster-decoded
        let fake_svg = DriveItem {
            id: "fake-svg-1".into(),
            drive_id: "d1".into(),
            name: "logo.png".into(),
            mime_type: Some("image/png".into()),
            ..Default::default()
        };
        app.raw_image_bytes.insert(
            fake_svg.id.clone(),
            std::sync::Arc::from(b"<svg xmlns=\"http://www.w3.org/2000/svg\"><rect width=\"10\" height=\"10\"/></svg>".as_slice()),
        );
        let svg_key = app.modal_image_cache_key(&fake_svg, 40, 20, crate::img_preview::TerminalImageProtocol::Halfblock);
        app.request_image_decode(&fake_svg, svg_key, 40, 20, ui.graphics_caps);

        let svg_msg = tokio::time::timeout(std::time::Duration::from_secs(3), rx.recv())
            .await
            .expect("expected SVG PreviewFailed")
            .expect("channel open");
        app.apply_ui_msg(svg_msg, &mut ui);
        let err = app
            .preview_errors
            .get(&fake_svg.id)
            .expect("expected SVG preview error");
        assert!(err.contains("SVG"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn wired_button_ids_register_in_full_ui_and_dispatch_on_click() {
        let _lock = TEST_ENV_LOCK.lock().await;
        let cfg_dir = tempdir().unwrap();
        let _env = EnvVarGuard::set(&[(
            "SHAREPOINT_CONFIG_DIR",
            Some(cfg_dir.path().to_str().unwrap()),
        )]);

        let (mut app, mut ui, _rx) = build_test_app_and_ui();
        let img_item = DriveItem {
            id: "btn-img-1".into(),
            drive_id: "d1".into(),
            name: "photo.png".into(),
            mime_type: Some("image/png".into()),
            size: 1024,
            web_url: Some("https://contoso.sharepoint.com/photo.png".into()),
            ..Default::default()
        };
        app.current_items = vec![img_item.clone()];
        app.active_selected_id = Some(img_item.id.clone());

        let mut terminal = Terminal::new(TestBackend::new(140, 32)).unwrap();
        terminal
            .draw(|frame| {
                let _ = crate::ui::draw(frame, &mut app, &mut ui);
            })
            .unwrap();

        let expected_buttons = [
            ButtonId::Search,
            ButtonId::CommandPalette,
            ButtonId::DensityToggle,
            ButtonId::SortToggle,
            ButtonId::Help,
            ButtonId::Settings,
            ButtonId::Refresh,
            ButtonId::UploadFile,
            ButtonId::NewFolder,
            ButtonId::DownloadItem,
            ButtonId::ShareItem,
            ButtonId::RenameItem,
            ButtonId::DeleteItem,
            ButtonId::ImagePreview,
            ButtonId::LinksModal,
            ButtonId::OpenInBrowser,
        ];
        for btn in expected_buttons {
            assert!(
                ui.hitmap.region_of(HitTarget::Button(btn)).is_some(),
                "expected ButtonId::{btn:?} to be registered in full UI hitmap"
            );
        }

        // Click Search -> opens Search overlay
        let r = ui.hitmap.region_of(HitTarget::Button(ButtonId::Search)).unwrap();
        dispatch(Action::Click { column: r.x + 1, row: r.y }, &mut app, &mut ui);
        assert!(matches!(ui.overlays.last(), Some(crate::ui::overlays::Overlay::Search(_))));
        ui.pop_overlay();

        // Click CommandPalette -> opens CommandPalette overlay
        let r = ui.hitmap.region_of(HitTarget::Button(ButtonId::CommandPalette)).unwrap();
        dispatch(Action::Click { column: r.x + 1, row: r.y }, &mut app, &mut ui);
        assert!(matches!(ui.overlays.last(), Some(crate::ui::overlays::Overlay::CommandPalette(_))));
        ui.pop_overlay();

        // Click NewFolder -> opens QuickInput overlay
        let r = ui.hitmap.region_of(HitTarget::Button(ButtonId::NewFolder)).unwrap();
        dispatch(Action::Click { column: r.x + 1, row: r.y }, &mut app, &mut ui);
        assert!(matches!(ui.overlays.last(), Some(crate::ui::overlays::Overlay::QuickInput(_))));
        ui.pop_overlay();

        // Click RenameItem -> opens QuickInput overlay
        let r = ui.hitmap.region_of(HitTarget::Button(ButtonId::RenameItem)).unwrap();
        dispatch(Action::Click { column: r.x + 1, row: r.y }, &mut app, &mut ui);
        assert!(matches!(ui.overlays.last(), Some(crate::ui::overlays::Overlay::QuickInput(_))));
        ui.pop_overlay();

        // Click DeleteItem -> opens Confirm overlay
        let r = ui.hitmap.region_of(HitTarget::Button(ButtonId::DeleteItem)).unwrap();
        dispatch(Action::Click { column: r.x + 1, row: r.y }, &mut app, &mut ui);
        assert!(matches!(ui.overlays.last(), Some(crate::ui::overlays::Overlay::Confirm(_))));
        ui.pop_overlay();

        // Click UploadFile -> opens FilePicker overlay
        let r = ui.hitmap.region_of(HitTarget::Button(ButtonId::UploadFile)).unwrap();
        dispatch(Action::Click { column: r.x + 1, row: r.y }, &mut app, &mut ui);
        assert!(matches!(ui.overlays.last(), Some(crate::ui::overlays::Overlay::FilePicker(_))));
        ui.pop_overlay();

        // Click LinksModal -> opens Links overlay
        let r = ui.hitmap.region_of(HitTarget::Button(ButtonId::LinksModal)).unwrap();
        dispatch(Action::Click { column: r.x + 1, row: r.y }, &mut app, &mut ui);
        assert!(matches!(ui.overlays.last(), Some(crate::ui::overlays::Overlay::Links(_))));
        ui.pop_overlay();

        // Click ImagePreview -> opens ImagePreview overlay
        let r = ui.hitmap.region_of(HitTarget::Button(ButtonId::ImagePreview)).unwrap();
        dispatch(Action::Click { column: r.x + 1, row: r.y }, &mut app, &mut ui);
        assert!(matches!(ui.overlays.last(), Some(crate::ui::overlays::Overlay::ImagePreview(_))));
        ui.pop_overlay();

        // Click DensityToggle -> toggles density mode
        let prev_density = ui.density;
        let r = ui.hitmap.region_of(HitTarget::Button(ButtonId::DensityToggle)).unwrap();
        dispatch(Action::Click { column: r.x + 1, row: r.y }, &mut app, &mut ui);
        assert_ne!(ui.density, prev_density);

        // Click Settings -> opens Settings overlay
        let r = ui.hitmap.region_of(HitTarget::Button(ButtonId::Settings)).unwrap();
        dispatch(Action::Click { column: r.x + 1, row: r.y }, &mut app, &mut ui);
        assert!(matches!(ui.overlays.last(), Some(crate::ui::overlays::Overlay::Settings(_))));
        ui.pop_overlay();
    }
}
