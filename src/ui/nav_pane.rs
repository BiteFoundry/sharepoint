//! Left sidebar navigation tree (`nav_pane`): Persona-aware collapsible tree for
//! Personal OneDrive (`My Files`, `Recent`, `Shared with Me`) + SharePoint Sites -> Document Libraries + Pinned items.

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::config::Persona;
use crate::ui::{
    app::{ActiveViewSource, AppState, Focus, UiState},
    hitmap::{HitMap, HitTarget},
    truncate_width,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavRow {
    SectionHeader {
        section_id: &'static str,
        label: String,
        collapsed: bool,
    },
    OneDriveMyFiles,
    OneDriveRecent,
    OneDriveShared,
    SharePointSite {
        site_id: String,
        label: String,
        is_followed: bool,
        collapsed: bool,
    },
    SharePointLibrary {
        site_id: String,
        drive_id: String,
        label: String,
    },
    PinnedItem {
        key: String,
        label: String,
        drive_id: Option<String>,
        item_id: Option<String>,
        site_id: Option<String>,
    },
}

pub fn build_nav_rows(app: &AppState, ui: &UiState) -> Vec<NavRow> {
    let mut rows = Vec::new();

    let push_onedrive = |rows: &mut Vec<NavRow>| {
        let collapsed = ui.collapsed_sections.contains("onedrive");
        rows.push(NavRow::SectionHeader {
            section_id: "onedrive",
            label: "ONEDRIVE".to_string(),
            collapsed,
        });
        if !collapsed {
            rows.push(NavRow::OneDriveMyFiles);
            rows.push(NavRow::OneDriveRecent);
            rows.push(NavRow::OneDriveShared);
        }
    };

    let push_sharepoint = |rows: &mut Vec<NavRow>| {
        let collapsed = ui.collapsed_sections.contains("sharepoint");
        rows.push(NavRow::SectionHeader {
            section_id: "sharepoint",
            label: "SHAREPOINT".to_string(),
            collapsed,
        });
        if !collapsed {
            for site in &app.sites {
                let site_collapsed = ui.collapsed_sites.contains(&site.id);
                rows.push(NavRow::SharePointSite {
                    site_id: site.id.clone(),
                    label: site.effective_name().to_string(),
                    is_followed: site.is_followed,
                    collapsed: site_collapsed,
                });
                if !site_collapsed {
                    if let Some(drives) = app.site_drives.get(&site.id) {
                        for d in drives {
                            rows.push(NavRow::SharePointLibrary {
                                site_id: site.id.clone(),
                                drive_id: d.id.clone(),
                                label: d.name.clone(),
                            });
                        }
                    }
                }
            }
        }
    };

    if !app.pins.is_empty() {
        let collapsed = ui.collapsed_sections.contains("pinned");
        rows.push(NavRow::SectionHeader {
            section_id: "pinned",
            label: "PINNED".to_string(),
            collapsed,
        });
        if !collapsed {
            for pin in &app.pins {
                rows.push(NavRow::PinnedItem {
                    key: pin.key.clone(),
                    label: pin.label.clone(),
                    drive_id: pin.drive_id.clone(),
                    item_id: pin.item_id.clone(),
                    site_id: pin.site_id.clone(),
                });
            }
        }
    }

    match app.persona {
        Persona::OneDrive => {
            push_onedrive(&mut rows);
            push_sharepoint(&mut rows);
        }
        Persona::SharePoint => {
            push_sharepoint(&mut rows);
            push_onedrive(&mut rows);
        }
    }

    rows
}

pub fn draw(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &UiState,
    hitmap: &mut HitMap,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    hitmap.add(area, HitTarget::Pane(Focus::Nav));
    let theme = &ui.theme;
    let sidebar_style = if theme.no_color {
        Style::default()
    } else {
        Style::default().bg(theme.bg_sidebar).fg(theme.fg_primary)
    };
    frame.render_widget(Paragraph::new("").style(sidebar_style), area);

    let header_title = match app.persona {
        Persona::OneDrive => " OneDrive · SharePoint",
        Persona::SharePoint => " SharePoint · OneDrive",
    };
    let focused = ui.focus == Focus::Nav;
    frame.render_widget(
        Paragraph::new(Span::styled(
            truncate_width(header_title, area.width as usize),
            theme.title(focused),
        ))
        .style(sidebar_style),
        Rect::new(area.x, area.y, area.width, 1),
    );

    let rows = build_nav_rows(app, ui);
    let list_h = area.height.saturating_sub(2) as usize;
    if list_h == 0 {
        return;
    }
    let selected = ui.nav_selected.min(rows.len().saturating_sub(1));
    let skip = selected.saturating_sub(list_h.saturating_sub(1));

    for (vis_idx, (row_idx, row)) in rows
        .iter()
        .enumerate()
        .skip(skip)
        .take(list_h)
        .enumerate()
    {
        let y = area.y + 1 + vis_idx as u16;
        let is_selected = focused && row_idx == selected;
        let is_active = is_row_active(row, app);

        let (prefix, text, base_style) = match row {
            NavRow::SectionHeader {
                label, collapsed, ..
            } => {
                let arrow = if *collapsed { "▸" } else { "▾" };
                (
                    format!(" {arrow} "),
                    label.clone(),
                    theme.muted_style().add_modifier(Modifier::BOLD),
                )
            }
            NavRow::OneDriveMyFiles => ("   ● ".to_string(), "My Files".to_string(), sidebar_style),
            NavRow::OneDriveRecent => ("   ◷ ".to_string(), "Recent".to_string(), sidebar_style),
            NavRow::OneDriveShared => ("   ⇄ ".to_string(), "Shared with Me".to_string(), sidebar_style),
            NavRow::SharePointSite {
                label,
                is_followed,
                collapsed,
                ..
            } => {
                let arrow = if *collapsed { "▸" } else { "▾" };
                let star = if *is_followed { "★ " } else { "" };
                (
                    format!("  {arrow} {star}"),
                    label.clone(),
                    sidebar_style.add_modifier(Modifier::BOLD),
                )
            }
            NavRow::SharePointLibrary { label, .. } => {
                ("     ▸ ".to_string(), label.clone(), sidebar_style)
            }
            NavRow::PinnedItem { label, .. } => {
                ("   ★ ".to_string(), label.clone(), sidebar_style)
            }
        };

        let style = if is_selected {
            theme.selection()
        } else if is_active {
            theme.accent_style().add_modifier(Modifier::BOLD)
        } else {
            base_style
        };

        let full = format!("{prefix}{text}");
        let clipped = truncate_width(&full, area.width as usize);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(clipped, style))),
            Rect::new(area.x, y, area.width, 1),
        );
        hitmap.add_line(area.x, y, area.width, HitTarget::NavRow(row_idx));
    }

    // Bottom account indicator
    if area.height >= 3 {
        let foot_y = area.bottom() - 1;
        let acct = app
            .me
            .as_ref()
            .map(|u| format!(" {}", u.display_name))
            .unwrap_or_else(|| " Offline / Unsynced".to_string());
        frame.render_widget(
            Paragraph::new(Span::styled(
                truncate_width(&acct, area.width as usize),
                theme.muted_style(),
            ))
            .style(sidebar_style),
            Rect::new(area.x, foot_y, area.width, 1),
        );
    }
}

fn is_row_active(row: &NavRow, app: &AppState) -> bool {
    match (&app.active_source, row) {
        (ActiveViewSource::OneDriveRoot, NavRow::OneDriveMyFiles) => true,
        (ActiveViewSource::Recent, NavRow::OneDriveRecent) => true,
        (ActiveViewSource::SharedWithMe, NavRow::OneDriveShared) => true,
        (
            ActiveViewSource::Drive { drive_id, .. },
            NavRow::SharePointLibrary {
                drive_id: row_did, ..
            },
        ) => drive_id == row_did,
        _ => false,
    }
}
