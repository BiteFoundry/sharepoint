//! Semantic multi-depth color & style tokens for the SharePoint / OneDrive TUI.
//!
//! Supports Truecolor (`Midnight Dark` / `Daylight Light`), `Ansi256`, `Ansi16`, and `NO_COLOR`.

use ratatui::style::{Color, Modifier, Style};

use crate::config::ThemePreset;

#[derive(Debug, Clone)]
pub struct Theme {
    pub no_color: bool,
    pub ansi16_only: bool,
    // Surfaces
    pub bg_canvas: Color,
    pub bg_sidebar: Color,
    pub bg_header: Color,
    pub bg_composer: Color,
    pub bg_elevated: Color,
    pub bg_selected: Color,
    pub bg_code: Color,
    pub bg_card: Color,
    pub bg_mention_me: Color,
    pub bg_reaction_me: Color,
    // Foregrounds
    pub fg_primary: Color,
    pub fg_secondary: Color,
    pub fg_muted: Color,
    pub fg_subtle: Color,
    // Brand & Semantic
    pub accent: Color,
    pub accent_soft: Color,
    pub own: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub unread: Color,
    pub unread_divider: Color,
    pub fg_mention_me: Color,
    pub code_fg: Color,
    pub reply: Color,
    // Legacy-compatible aliases used across TUI views/overlays
    pub fg: Color,
    pub muted: Color,
    pub border: Color,
    pub border_focus: Color,
    pub selection_bg: Color,
    pub selection_fg: Color,
    pub date_separator: Color,
    pub divider: Color,
    pub toast: Color,
    pub code_bg: Color,
    pub subject: Color,
    pub author_palette: [Color; 8],
}

impl Default for Theme {
    fn default() -> Self {
        Self::midnight_dark()
    }
}

impl Theme {
    /// Detects terminal color capabilities when `preset == ThemePreset::Auto`, or applies explicit preset.
    pub fn detect(preset: ThemePreset) -> Self {
        if preset == ThemePreset::NoColor
            || std::env::var("NO_COLOR").is_ok_and(|v| !v.trim().is_empty())
        {
            return Self::no_color();
        }
        match preset {
            ThemePreset::Dark => Self::midnight_dark(),
            ThemePreset::Light => Self::daylight_light(),
            ThemePreset::Ansi256 => Self::ansi256(),
            ThemePreset::Ansi16 => Self::ansi16(),
            ThemePreset::NoColor => Self::no_color(),
            ThemePreset::Auto => {
                let colorterm = std::env::var("COLORTERM")
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if colorterm.contains("truecolor") || colorterm.contains("24bit") {
                    return Self::midnight_dark();
                }
                let term = std::env::var("TERM")
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if term == "dumb" {
                    return Self::no_color();
                }
                if term.contains("256color") {
                    return Self::ansi256();
                }
                if cfg!(windows) || std::env::var("WT_SESSION").is_ok() {
                    return Self::midnight_dark();
                }
                if term.is_empty() {
                    Self::ansi256()
                } else {
                    Self::ansi16()
                }
            }
        }
    }

    pub fn midnight_dark() -> Self {
        let bg_selected = Color::Rgb(42, 48, 72);
        let fg_primary = Color::Rgb(226, 230, 243);
        let fg_muted = Color::Rgb(110, 119, 153);
        let fg_subtle = Color::Rgb(70, 78, 107);
        let accent = Color::Rgb(122, 162, 247);
        let accent_soft = Color::Rgb(137, 221, 255);
        let bg_code = Color::Rgb(29, 32, 48);
        let code_fg = Color::Rgb(224, 175, 104);
        Self {
            no_color: false,
            ansi16_only: false,
            bg_canvas: Color::Reset,
            bg_sidebar: Color::Rgb(27, 30, 43),
            bg_header: Color::Rgb(31, 35, 51),
            bg_composer: Color::Rgb(27, 30, 43),
            bg_elevated: Color::Rgb(37, 42, 61),
            bg_selected,
            bg_code,
            bg_card: Color::Rgb(30, 34, 50),
            bg_mention_me: Color::Rgb(59, 46, 30),
            bg_reaction_me: Color::Rgb(36, 52, 77),
            fg_primary,
            fg_secondary: Color::Rgb(169, 177, 208),
            fg_muted,
            fg_subtle,
            accent,
            accent_soft,
            own: Color::Rgb(125, 207, 255),
            success: Color::Rgb(115, 218, 202),
            warning: Color::Rgb(224, 175, 104),
            error: Color::Rgb(247, 118, 142),
            unread: Color::Rgb(187, 154, 247),
            unread_divider: Color::Rgb(247, 118, 142),
            fg_mention_me: Color::Rgb(255, 203, 107),
            code_fg,
            reply: Color::Rgb(122, 162, 247),
            fg: fg_primary,
            muted: fg_muted,
            border: fg_subtle,
            border_focus: accent,
            selection_bg: bg_selected,
            selection_fg: fg_primary,
            date_separator: fg_subtle,
            divider: fg_subtle,
            toast: accent_soft,
            code_bg: bg_code,
            subject: accent_soft,
            author_palette: [
                Color::Rgb(158, 206, 106),
                Color::Rgb(224, 175, 104),
                Color::Rgb(187, 154, 247),
                Color::Rgb(125, 207, 255),
                Color::Rgb(247, 118, 142),
                Color::Rgb(115, 218, 202),
                Color::Rgb(255, 158, 100),
                Color::Rgb(180, 249, 248),
            ],
        }
    }

    pub fn daylight_light() -> Self {
        let bg_selected = Color::Rgb(220, 228, 247);
        let fg_primary = Color::Rgb(30, 36, 51);
        let fg_muted = Color::Rgb(107, 114, 128);
        let fg_subtle = Color::Rgb(203, 213, 225);
        let accent = Color::Rgb(59, 91, 219);
        let accent_soft = Color::Rgb(12, 166, 120);
        let bg_code = Color::Rgb(235, 238, 245);
        let code_fg = Color::Rgb(156, 54, 181);
        Self {
            no_color: false,
            ansi16_only: false,
            bg_canvas: Color::Reset,
            bg_sidebar: Color::Rgb(238, 241, 247),
            bg_header: Color::Rgb(228, 232, 242),
            bg_composer: Color::Rgb(255, 255, 255),
            bg_elevated: Color::Rgb(224, 229, 240),
            bg_selected,
            bg_code,
            bg_card: Color::Rgb(240, 243, 250),
            bg_mention_me: Color::Rgb(255, 243, 205),
            bg_reaction_me: Color::Rgb(215, 231, 255),
            fg_primary,
            fg_secondary: Color::Rgb(74, 85, 104),
            fg_muted,
            fg_subtle,
            accent,
            accent_soft,
            own: Color::Rgb(11, 114, 133),
            success: Color::Rgb(18, 128, 92),
            warning: Color::Rgb(180, 83, 9),
            error: Color::Rgb(201, 42, 42),
            unread: Color::Rgb(103, 65, 217),
            unread_divider: Color::Rgb(224, 49, 49),
            fg_mention_me: Color::Rgb(124, 45, 18),
            code_fg,
            reply: Color::Rgb(59, 91, 219),
            fg: fg_primary,
            muted: fg_muted,
            border: fg_subtle,
            border_focus: accent,
            selection_bg: bg_selected,
            selection_fg: fg_primary,
            date_separator: fg_subtle,
            divider: fg_subtle,
            toast: accent,
            code_bg: bg_code,
            subject: accent_soft,
            author_palette: [
                Color::Rgb(43, 138, 62),
                Color::Rgb(180, 83, 9),
                Color::Rgb(103, 65, 217),
                Color::Rgb(25, 113, 194),
                Color::Rgb(201, 42, 42),
                Color::Rgb(12, 133, 153),
                Color::Rgb(217, 72, 15),
                Color::Rgb(95, 61, 196),
            ],
        }
    }

    pub fn ansi256() -> Self {
        let bg_selected = Color::Indexed(237);
        let fg_primary = Color::Indexed(254);
        let fg_muted = Color::Indexed(244);
        let fg_subtle = Color::Indexed(239);
        let accent = Color::Indexed(111);
        let accent_soft = Color::Indexed(117);
        let bg_code = Color::Indexed(235);
        let code_fg = Color::Indexed(180);
        Self {
            no_color: false,
            ansi16_only: false,
            bg_canvas: Color::Reset,
            bg_sidebar: Color::Indexed(234),
            bg_header: Color::Indexed(235),
            bg_composer: Color::Indexed(234),
            bg_elevated: Color::Indexed(236),
            bg_selected,
            bg_code,
            bg_card: Color::Indexed(235),
            bg_mention_me: Color::Indexed(58),
            bg_reaction_me: Color::Indexed(24),
            fg_primary,
            fg_secondary: Color::Indexed(249),
            fg_muted,
            fg_subtle,
            accent,
            accent_soft,
            own: Color::Indexed(81),
            success: Color::Indexed(79),
            warning: Color::Indexed(179),
            error: Color::Indexed(204),
            unread: Color::Indexed(141),
            unread_divider: Color::Indexed(204),
            fg_mention_me: Color::Indexed(221),
            code_fg,
            reply: Color::Indexed(75),
            fg: fg_primary,
            muted: fg_muted,
            border: fg_subtle,
            border_focus: accent,
            selection_bg: bg_selected,
            selection_fg: fg_primary,
            date_separator: fg_subtle,
            divider: fg_subtle,
            toast: accent_soft,
            code_bg: bg_code,
            subject: accent_soft,
            author_palette: [
                Color::Indexed(114),
                Color::Indexed(179),
                Color::Indexed(141),
                Color::Indexed(81),
                Color::Indexed(204),
                Color::Indexed(79),
                Color::Indexed(209),
                Color::Indexed(117),
            ],
        }
    }

    pub fn ansi16() -> Self {
        Self {
            no_color: false,
            ansi16_only: true,
            bg_canvas: Color::Reset,
            bg_sidebar: Color::Reset,
            bg_header: Color::Reset,
            bg_composer: Color::Reset,
            bg_elevated: Color::Reset,
            bg_selected: Color::DarkGray,
            bg_code: Color::Black,
            bg_card: Color::Reset,
            bg_mention_me: Color::Yellow,
            bg_reaction_me: Color::Blue,
            fg_primary: Color::Reset,
            fg_secondary: Color::White,
            fg_muted: Color::DarkGray,
            fg_subtle: Color::DarkGray,
            accent: Color::Cyan,
            accent_soft: Color::LightCyan,
            own: Color::LightCyan,
            success: Color::Green,
            warning: Color::Yellow,
            error: Color::Red,
            unread: Color::LightMagenta,
            unread_divider: Color::LightRed,
            fg_mention_me: Color::Black,
            code_fg: Color::Yellow,
            reply: Color::Blue,
            fg: Color::Reset,
            muted: Color::DarkGray,
            border: Color::DarkGray,
            border_focus: Color::Cyan,
            selection_bg: Color::DarkGray,
            selection_fg: Color::White,
            date_separator: Color::DarkGray,
            divider: Color::DarkGray,
            toast: Color::LightCyan,
            code_bg: Color::Black,
            subject: Color::LightCyan,
            author_palette: [
                Color::Green,
                Color::Yellow,
                Color::Magenta,
                Color::Cyan,
                Color::Red,
                Color::LightGreen,
                Color::LightRed,
                Color::LightBlue,
            ],
        }
    }

    pub fn no_color() -> Self {
        Self {
            no_color: true,
            ansi16_only: true,
            bg_canvas: Color::Reset,
            bg_sidebar: Color::Reset,
            bg_header: Color::Reset,
            bg_composer: Color::Reset,
            bg_elevated: Color::Reset,
            bg_selected: Color::Reset,
            bg_code: Color::Reset,
            bg_card: Color::Reset,
            bg_mention_me: Color::Reset,
            bg_reaction_me: Color::Reset,
            fg_primary: Color::Reset,
            fg_secondary: Color::Reset,
            fg_muted: Color::Reset,
            fg_subtle: Color::Reset,
            accent: Color::Reset,
            accent_soft: Color::Reset,
            own: Color::Reset,
            success: Color::Reset,
            warning: Color::Reset,
            error: Color::Reset,
            unread: Color::Reset,
            unread_divider: Color::Reset,
            fg_mention_me: Color::Reset,
            code_fg: Color::Reset,
            reply: Color::Reset,
            fg: Color::Reset,
            muted: Color::Reset,
            border: Color::Reset,
            border_focus: Color::Reset,
            selection_bg: Color::Reset,
            selection_fg: Color::Reset,
            date_separator: Color::Reset,
            divider: Color::Reset,
            toast: Color::Reset,
            code_bg: Color::Reset,
            subject: Color::Reset,
            author_palette: [Color::Reset; 8],
        }
    }

    /// Returns a stable color for an author ID or display name.
    pub fn author(&self, key: &str) -> Color {
        let mut hash: u64 = 5381;
        for byte in key.bytes() {
            hash = hash.wrapping_mul(33).wrapping_add(byte as u64);
        }
        self.author_palette[(hash as usize) % self.author_palette.len()]
    }

    pub fn border_style(&self, focused: bool) -> Style {
        if self.no_color {
            return if focused {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default().add_modifier(Modifier::DIM)
            };
        }
        let color = if focused {
            self.border_focus
        } else {
            self.border
        };
        Style::default().fg(color)
    }

    /// Non-destructive selection style: tints background without clobbering child span foreground colors!
    pub fn selection(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default().bg(self.bg_selected)
        }
    }

    /// Single-line list/overlay selection (uses `REVERSED` in `no_color` / `ansi16` modes).
    pub fn nav_selection(&self) -> Style {
        if self.no_color || self.ansi16_only {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default()
                .bg(self.bg_selected)
                .fg(self.fg_primary)
                .add_modifier(Modifier::BOLD)
        }
    }

    pub fn muted_style(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(self.fg_muted)
        }
    }

    pub fn subtle_style(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(self.fg_subtle)
        }
    }

    pub fn title(&self, focused: bool) -> Style {
        if focused {
            Style::default()
                .fg(self.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            self.muted_style()
        }
    }

    pub fn accent_style(&self) -> Style {
        Style::default().fg(self.accent)
    }

    pub fn subject_style(&self) -> Style {
        Style::default()
            .fg(self.accent_soft)
            .add_modifier(Modifier::BOLD)
    }

    pub fn code_style(&self) -> Style {
        if self.no_color {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(self.code_fg).bg(self.bg_code)
        }
    }

    pub fn quote_style(&self) -> Style {
        Style::default()
            .fg(self.fg_secondary)
            .add_modifier(Modifier::ITALIC)
    }

    pub fn reaction_style(&self, me_reacted: bool) -> Style {
        if self.no_color {
            if me_reacted {
                Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else {
                Style::default()
            }
        } else if me_reacted {
            Style::default()
                .fg(self.accent)
                .bg(self.bg_reaction_me)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(self.fg_secondary)
                .bg(self.bg_elevated)
        }
    }

    /// Maps Microsoft Graph presence availability to a colorblind-safe glyph and color.
    pub fn presence_indicator(&self, availability: Option<&str>) -> (&'static str, Color) {
        match availability {
            Some("Available" | "AvailableIdle") => ("●", self.success),
            Some("Busy" | "BusyIdle") => ("●", self.error),
            Some("DoNotDisturb") => ("⊘", self.error),
            Some("Away" | "BeRightBack") => ("◐", self.warning),
            _ => ("○", self.fg_muted),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn author_colors_are_stable_and_empty_safe() {
        let theme = Theme::default();
        assert_eq!(
            theme.author("alice@contoso.com"),
            theme.author("alice@contoso.com")
        );
        let _ = theme.author("");
    }

    #[test]
    fn presence_indicators_map_teams_states_with_accessible_glyphs() {
        let theme = Theme::default();
        assert_eq!(theme.presence_indicator(Some("Available")), ("●", theme.success));
        assert_eq!(theme.presence_indicator(Some("Busy")), ("●", theme.error));
        assert_eq!(
            theme.presence_indicator(Some("DoNotDisturb")),
            ("⊘", theme.error)
        );
        assert_eq!(theme.presence_indicator(Some("Away")), ("◐", theme.warning));
        assert_eq!(
            theme.presence_indicator(Some("Offline")),
            ("○", theme.fg_muted)
        );
        assert_eq!(theme.presence_indicator(None), ("○", theme.fg_muted));
    }

    #[test]
    fn selection_preserves_child_foreground_and_border_uses_accents() {
        let theme = Theme::default();
        assert_eq!(theme.selection().fg, None);
        assert_eq!(theme.selection().bg, Some(theme.bg_selected));
        assert_ne!(theme.border_style(true).fg, theme.border_style(false).fg);
    }

    #[test]
    fn presets_construct_cleanly() {
        assert!(!Theme::detect(ThemePreset::Dark).no_color);
        assert!(!Theme::detect(ThemePreset::Light).no_color);
        assert!(!Theme::detect(ThemePreset::Ansi256).no_color);
        assert!(Theme::detect(ThemePreset::Ansi16).ansi16_only);
        assert!(Theme::detect(ThemePreset::NoColor).no_color);
    }
}
