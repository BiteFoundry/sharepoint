//! Root UI module: 3-tier responsive 3-pane Minimal-Chrome layout, rendering pipeline, and shared helpers.

pub mod action;
pub mod app;
pub mod drive_view;
pub mod event;
pub mod hitmap;
pub mod keymap;
pub mod markup;
pub mod nav_pane;
pub mod notification;
pub mod overlays;
pub mod theme;
pub mod toast;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::Span,
    widgets::{Clear, Paragraph},
    Frame,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use self::app::{AppState, NarrowPane, UiState};
use self::hitmap::{ButtonId, HitMap, HitTarget};
use self::toast::ToastKind;

/// 3-tier responsive terminal width modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutMode {
    /// >= 120 columns: Left Nav + Middle File List + Right Inspector Pane.
    Wide,
    /// 80..=119 columns: Left Nav + stacked File List (45%) / Inspector (55%).
    Medium,
    /// < 80 columns: Compact top switcher + single active pane (`NarrowPane`).
    Narrow,
}

pub fn layout_mode(width: u16) -> LayoutMode {
    if width >= 120 {
        LayoutMode::Wide
    } else if width >= 80 {
        LayoutMode::Medium
    } else {
        LayoutMode::Narrow
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LayoutPlan {
    pub mode: Option<LayoutMode>,
    pub compact_bar: Option<Rect>,
    pub nav: Option<Rect>,
    pub list: Option<Rect>,
    pub inspector: Option<Rect>,
    pub status: Rect,
}

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn compute_layout(
    area: Rect,
    show_nav_rail: bool,
    narrow_pane: NarrowPane,
) -> LayoutPlan {
    if area.width < 10 || area.height < 4 {
        return LayoutPlan {
            status: Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1),
            ..Default::default()
        };
    }

    let mode = layout_mode(area.width);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(area);
    let body = rows[0];
    let status = rows[1];

    match mode {
        LayoutMode::Wide => {
            let nav_w = if show_nav_rail { 26 } else { 0 };
            let nav_sep = if nav_w > 0 { 1 } else { 0 };
            let remaining = area.width.saturating_sub(nav_w + nav_sep);
            let list_w = remaining.saturating_sub(36).clamp(32, 54);

            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(nav_w),
                    Constraint::Length(nav_sep),
                    Constraint::Length(list_w),
                    Constraint::Length(1),
                    Constraint::Min(28),
                ])
                .split(body);

            LayoutPlan {
                mode: Some(mode),
                compact_bar: None,
                nav: (nav_w > 0).then_some(cols[0]),
                list: Some(cols[2]),
                inspector: Some(cols[4]),
                status,
            }
        }
        LayoutMode::Medium => {
            let nav_w = if show_nav_rail { 22 } else { 0 };
            let nav_sep = if nav_w > 0 { 1 } else { 0 };
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(nav_w),
                    Constraint::Length(nav_sep),
                    Constraint::Min(30),
                ])
                .split(body);

            let right = cols[2];
            let split = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Percentage(45),
                    Constraint::Length(1),
                    Constraint::Min(4),
                ])
                .split(right);

            LayoutPlan {
                mode: Some(mode),
                compact_bar: None,
                nav: (nav_w > 0).then_some(cols[0]),
                list: Some(split[0]),
                inspector: Some(split[2]),
                status,
            }
        }
        LayoutMode::Narrow => {
            let split = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(3)])
                .split(body);
            let compact_bar = Some(split[0]);
            let content = split[1];

            match narrow_pane {
                NarrowPane::Sidebar => LayoutPlan {
                    mode: Some(mode),
                    compact_bar,
                    nav: Some(content),
                    list: None,
                    inspector: None,
                    status,
                },
                NarrowPane::Files => LayoutPlan {
                    mode: Some(mode),
                    compact_bar,
                    nav: None,
                    list: Some(content),
                    inspector: None,
                    status,
                },
                NarrowPane::Inspector => LayoutPlan {
                    mode: Some(mode),
                    compact_bar,
                    nav: None,
                    list: None,
                    inspector: Some(content),
                    status,
                },
            }
        }
    }
}

pub fn truncate_width(s: &str, max_w: usize) -> String {
    if max_w == 0 {
        return String::new();
    }
    if s.width() <= max_w {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    let budget = max_w.saturating_sub(1);
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if used + cw > budget {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push('…');
    out
}

pub fn draw(frame: &mut Frame, app: &mut AppState, ui: &mut UiState) -> Option<(u16, u16)> {
    let area = frame.area();
    ui.term_width = area.width;
    ui.term_height = area.height;
    ui.pending_pixel_graphics.clear();

    let base_bg = if ui.theme.no_color {
        Style::default()
    } else {
        Style::default()
            .bg(ui.theme.bg_canvas)
            .fg(ui.theme.fg_primary)
    };
    frame.render_widget(Paragraph::new("").style(base_bg), area);

    let plan = compute_layout(area, ui.show_nav_rail, ui.narrow_pane);
    ui.nav_area = plan.nav.unwrap_or_default();

    let mut hitmap = HitMap::default();

    if let Some(bar_area) = plan.compact_bar {
        draw_compact_bar(frame, bar_area, ui, &mut hitmap);
    }

    if let Some(nav_area) = plan.nav {
        nav_pane::draw(frame, nav_area, app, ui, &mut hitmap);
    }

    if let Some(list_area) = plan.list {
        drive_view::draw_list(frame, list_area, app, ui, &mut hitmap);
    }

    if let Some(insp_area) = plan.inspector {
        drive_view::draw_inspector(frame, insp_area, app, ui, &mut hitmap);
    }

    draw_status_bar(frame, plan.status, app, ui, &mut hitmap);
    draw_toasts(frame, area, ui);

    let mut cursor_pos = None;
    let mut overlays = std::mem::take(&mut ui.overlays);
    for overlay in overlays.iter_mut() {
        if let Some(pos) = overlay.draw(frame, area, app, ui, &mut hitmap) {
            cursor_pos = Some(pos);
        }
    }
    ui.overlays = overlays;
    ui.hitmap = hitmap;
    cursor_pos
}

fn draw_compact_bar(frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
    let pills = [
        (NarrowPane::Sidebar, " [1] Tree ", ButtonId::NarrowSidebar),
        (NarrowPane::Files, " [2] Files ", ButtonId::NarrowFiles),
        (NarrowPane::Inspector, " [3] Info ", ButtonId::NarrowInspector),
    ];
    let mut x = area.x;
    for (pane, label, btn) in pills {
        let style = if ui.narrow_pane == pane {
            ui.theme.selection()
        } else {
            ui.theme.muted_style()
        };
        let w = label.len() as u16;
        if x + w <= area.right() {
            frame.render_widget(Paragraph::new(Span::styled(label, style)), Rect::new(x, area.y, w, 1));
            hitmap.add_line(x, area.y, w, HitTarget::Button(btn));
        }
        x = x.saturating_add(w + 1);
    }
}

fn draw_status_bar(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &UiState,
    hitmap: &mut HitMap,
) {
    let theme = &ui.theme;
    let bg = if theme.no_color {
        Style::default()
    } else {
        Style::default().bg(theme.bg_header).fg(theme.fg_secondary)
    };
    let spinner = if app.sync_busy || !ui.tasks.is_empty() {
        let ch = SPINNER_FRAMES[ui.spinner_tick % SPINNER_FRAMES.len()];
        format!("{ch} ")
    } else {
        String::new()
    };

    let pills: [(&str, ButtonId); 6] = [
        ("[/ Search]", ButtonId::Search),
        ("[Ctrl+P Palette]", ButtonId::CommandPalette),
        ("[Ctrl+F Upload]", ButtonId::UploadFile),
        ("[Ctrl+R Sync]", ButtonId::Refresh),
        ("[Ctrl+, Prefs]", ButtonId::Settings),
        ("[? Help]", ButtonId::Help),
    ];

    let left_prefix = format!(
        " {} · {}{}  ",
        app.persona.display_name(),
        spinner,
        app.sync_summary
    );

    let mut chosen: Vec<(&str, ButtonId, u16)> = Vec::new();
    let mut right_w: u16 = 0;
    for (label, btn) in pills.iter().rev() {
        let w = label.width() as u16;
        let gap: u16 = if chosen.is_empty() { 1 } else { 1 };
        if area.width > right_w + w + gap + 12 {
            chosen.push((*label, *btn, w));
            right_w = right_w.saturating_add(w + gap);
        }
    }
    chosen.reverse();

    let left_budget = (area.width as usize).saturating_sub(right_w as usize + 1);
    let left_trunc = truncate_width(&left_prefix, left_budget);
    let left_w = left_trunc.width();
    let pad = (area.width as usize)
        .saturating_sub(left_w)
        .saturating_sub(right_w as usize);

    let mut spans = vec![
        Span::styled(left_trunc, bg),
        Span::styled(" ".repeat(pad), bg),
    ];
    let mut pill_x = area.x + area.width.saturating_sub(right_w);
    for (label, btn, w) in &chosen {
        spans.push(Span::styled((*label).to_string(), bg));
        hitmap.add_line(pill_x, area.y, *w, HitTarget::Button(*btn));
        pill_x = pill_x.saturating_add(*w);
        spans.push(Span::styled(" ", bg));
        pill_x = pill_x.saturating_add(1);
    }

    frame.render_widget(
        Paragraph::new(ratatui::text::Line::from(spans)).style(bg),
        area,
    );
}

fn draw_toasts(frame: &mut Frame, area: Rect, ui: &UiState) {
    let Some(toast) = ui.toasts.latest() else {
        return;
    };
    let theme = &ui.theme;
    let color = match toast.kind {
        ToastKind::Info => theme.accent,
        ToastKind::Success => theme.success,
        ToastKind::Warning => theme.warning,
        ToastKind::Error => theme.error,
    };
    let text = format!(" {} ", toast.message);
    let w = (text.width() as u16 + 2).min(area.width.saturating_sub(2));
    let rect = Rect::new(
        area.right().saturating_sub(w + 1),
        area.bottom().saturating_sub(3),
        w,
        1,
    );
    frame.render_widget(Clear, rect);
    let style = if theme.no_color {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default().bg(theme.bg_elevated).fg(color)
    };
    frame.render_widget(
        Paragraph::new(Span::styled(truncate_width(&text, w as usize), style)),
        rect,
    );
}
