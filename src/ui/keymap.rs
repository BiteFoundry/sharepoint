//! Data-driven keymap for Microsoft SharePoint & OneDrive TUI.
//!
//! Focus-specific bindings take precedence over global bindings.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::ui::{action::Action, app::Focus, overlays::OverlayKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl Key {
    pub const fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self { code, modifiers }
    }
}

#[derive(Debug, Clone)]
pub struct Binding {
    pub focus: Option<Focus>,
    pub key: Key,
    pub action: Action,
}

#[derive(Debug, Clone, Default)]
pub struct Keymap {
    bindings: Vec<Binding>,
}

impl Keymap {
    pub fn default_sharepoint() -> Self {
        use KeyCode::*;
        use KeyModifiers as M;

        let mut map = Keymap::default();
        let any = None;

        // --- Overlay (highest priority) ---
        map.bind(Some(Focus::Overlay), Esc, M::NONE, Action::CloseOverlay);
        map.bind(Some(Focus::Overlay), Up, M::NONE, Action::MoveUp);
        map.bind(Some(Focus::Overlay), Down, M::NONE, Action::MoveDown);
        map.bind(Some(Focus::Overlay), Left, M::NONE, Action::CursorLeft);
        map.bind(Some(Focus::Overlay), Right, M::NONE, Action::CursorRight);
        map.bind(Some(Focus::Overlay), Home, M::NONE, Action::CursorHome);
        map.bind(Some(Focus::Overlay), End, M::NONE, Action::CursorEnd);
        map.bind(Some(Focus::Overlay), Tab, M::NONE, Action::Tab);
        map.bind(Some(Focus::Overlay), BackTab, M::SHIFT, Action::PrevFocus);
        map.bind(Some(Focus::Overlay), Enter, M::NONE, Action::Activate);
        map.bind(Some(Focus::Overlay), Enter, M::CONTROL, Action::OverlaySubmit);
        map.bind(Some(Focus::Overlay), Enter, M::ALT, Action::OverlaySubmit);
        map.bind(Some(Focus::Overlay), Backspace, M::NONE, Action::Backspace);
        map.bind(Some(Focus::Overlay), Delete, M::NONE, Action::DeleteForward);

        // --- Browse modes (Nav / List / Inspector) ---
        // Sidebar tree: Right / l expands (never collapses), Left / h collapses or steps out
        // to the parent row; Enter / Space toggle.
        map.bind(Some(Focus::Nav), Char('l'), M::NONE, Action::NavExpand);
        map.bind(Some(Focus::Nav), Right, M::NONE, Action::NavExpand);
        map.bind(Some(Focus::Nav), Char('h'), M::NONE, Action::NavCollapse);
        map.bind(Some(Focus::Nav), Left, M::NONE, Action::NavCollapse);
        map.bind(Some(Focus::Nav), Char(' '), M::NONE, Action::Activate);

        // File list: Right / l opens the selected folder, mirroring Left / Backspace (up).
        map.bind(Some(Focus::List), Right, M::NONE, Action::Activate);
        map.bind(Some(Focus::List), Char('l'), M::NONE, Action::Activate);

        for focus in [Focus::List, Focus::Inspector] {
            map.bind(Some(focus), Backspace, M::NONE, Action::NavigateUp);
            map.bind(Some(focus), Left, M::NONE, Action::NavigateUp);
            map.bind(Some(focus), Char('d'), M::NONE, Action::DownloadSelected);
            map.bind(Some(focus), Char('n'), M::NONE, Action::OpenNewFolder);
            map.bind(Some(focus), Char('r'), M::NONE, Action::OpenRenameItem);
            map.bind(Some(focus), Char('x'), M::NONE, Action::ConfirmDeleteSelected);
            map.bind(Some(focus), Delete, M::NONE, Action::ConfirmDeleteSelected);
            map.bind(Some(focus), Char('s'), M::NONE, Action::CreateAndCopyShareLink);
            map.bind(Some(focus), Char('S'), M::SHIFT, Action::CycleSortMode);
            map.bind(Some(focus), Char('L'), M::SHIFT, Action::LoadMoreItems);
            map.bind(Some(focus), Char('u'), M::NONE, Action::OpenLinkExtractor);
            map.bind(Some(focus), Char('h'), M::NONE, Action::ToggleShowLinkUrls);
            map.bind(Some(focus), Char('m'), M::NONE, Action::TogglePreviewRenderMode);
            map.bind(Some(focus), Char('o'), M::NONE, Action::OpenActiveWebUrl);
            map.bind(Some(focus), Char('p'), M::NONE, Action::TogglePinCurrent);
            map.bind(Some(focus), Char('.'), M::NONE, Action::OpenContextMenuHere);
            map.bind(Some(focus), Char(' '), M::NONE, Action::ScrollPageDown);
        }

        // --- Global shortcuts ---
        map.bind(any, Char('c'), M::CONTROL, Action::Quit);
        map.bind(any, Char('q'), M::CONTROL, Action::QuitConfirm);
        map.bind(any, Esc, M::NONE, Action::CloseOverlay);
        map.bind(any, Tab, M::NONE, Action::NextFocus);
        map.bind(any, BackTab, M::SHIFT, Action::PrevFocus);
        map.bind(any, Enter, M::NONE, Action::Activate);
        map.bind(any, Char('k'), M::NONE, Action::MoveUp);
        map.bind(any, Char('j'), M::NONE, Action::MoveDown);
        map.bind(any, Up, M::NONE, Action::MoveUp);
        map.bind(any, Down, M::NONE, Action::MoveDown);
        map.bind(any, Home, M::NONE, Action::ScrollTop);
        map.bind(any, End, M::NONE, Action::ScrollBottom);
        map.bind(any, PageUp, M::NONE, Action::ScrollPageUp);
        map.bind(any, PageDown, M::NONE, Action::ScrollPageDown);
        map.bind(any, Char('v'), M::NONE, Action::OpenImagePreview);
        map.bind(any, Char('v'), M::ALT, Action::OpenImagePreview);
        map.bind(any, Char('v'), M::CONTROL, Action::UploadClipboardImage);
        map.bind(any, Char('f'), M::CONTROL, Action::OpenFilePicker);
        map.bind(any, Char('d'), M::CONTROL, Action::ToggleDensity);
        map.bind(any, Char('r'), M::CONTROL, Action::RefreshAll);
        map.bind(any, F(5), M::NONE, Action::RefreshAll);
        map.bind(any, Char('b'), M::CONTROL, Action::ToggleNavRail);
        map.bind(any, Char('/'), M::NONE, Action::OpenSearch);
        map.bind(
            any,
            Char('p'),
            M::CONTROL,
            Action::OpenOverlay(OverlayKind::CommandPalette),
        );
        map.bind(
            any,
            Char('k'),
            M::CONTROL,
            Action::OpenOverlay(OverlayKind::CommandPalette),
        );
        map.bind(
            any,
            Char(','),
            M::CONTROL,
            Action::OpenOverlay(OverlayKind::Settings),
        );
        map.bind(
            any,
            Char('?'),
            M::NONE,
            Action::OpenOverlay(OverlayKind::Help),
        );
        map.bind(any, Char('.'), M::NONE, Action::OpenContextMenuHere);

        map
    }

    pub fn bind(
        &mut self,
        focus: Option<Focus>,
        code: KeyCode,
        modifiers: KeyModifiers,
        action: Action,
    ) {
        self.bindings.push(Binding {
            focus,
            key: Key::new(code, modifiers),
            action,
        });
    }

    pub fn resolve(&self, focus: Focus, key: KeyEvent) -> Option<Action> {
        self.resolve_with(focus, key, true)
    }

    pub fn resolve_scoped(&self, focus: Focus, key: KeyEvent) -> Option<Action> {
        self.resolve_with(focus, key, false)
    }

    fn resolve_with(&self, focus: Focus, key: KeyEvent, include_global: bool) -> Option<Action> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        self.bindings
            .iter()
            .find(|binding| {
                (binding.focus == Some(focus) || (include_global && binding.focus.is_none()))
                    && key_matches(binding.key, key)
            })
            .map(|binding| binding.action.clone())
    }
}

fn key_matches(expected: Key, actual: KeyEvent) -> bool {
    match expected.code {
        KeyCode::Char(exp_ch) => {
            let KeyCode::Char(act_ch) = actual.code else {
                return false;
            };
            let mod_ok = actual.modifiers.contains(KeyModifiers::CONTROL)
                == expected.modifiers.contains(KeyModifiers::CONTROL)
                && actual.modifiers.contains(KeyModifiers::ALT)
                    == expected.modifiers.contains(KeyModifiers::ALT);
            if !mod_ok {
                return false;
            }
            if expected
                .modifiers
                .contains(KeyModifiers::CONTROL | KeyModifiers::SHIFT)
            {
                actual.modifiers.contains(KeyModifiers::SHIFT)
                    && act_ch.eq_ignore_ascii_case(&exp_ch)
            } else {
                act_ch == exp_ch
            }
        }
        _ => expected.code == actual.code && actual.modifiers == expected.modifiers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn maps_sharepoint_file_and_v074_shortcuts() {
        let map = Keymap::default_sharepoint();
        assert_eq!(
            map.resolve(Focus::List, key(KeyCode::Char('d'), KeyModifiers::NONE)),
            Some(Action::DownloadSelected)
        );
        assert_eq!(
            map.resolve(Focus::List, key(KeyCode::Char('n'), KeyModifiers::NONE)),
            Some(Action::OpenNewFolder)
        );
        assert_eq!(
            map.resolve(Focus::List, key(KeyCode::Char('r'), KeyModifiers::NONE)),
            Some(Action::OpenRenameItem)
        );
        assert_eq!(
            map.resolve(Focus::List, key(KeyCode::Char('x'), KeyModifiers::NONE)),
            Some(Action::ConfirmDeleteSelected)
        );
        assert_eq!(
            map.resolve(Focus::List, key(KeyCode::Char('s'), KeyModifiers::NONE)),
            Some(Action::CreateAndCopyShareLink)
        );
        assert_eq!(
            map.resolve(Focus::List, key(KeyCode::Char('u'), KeyModifiers::NONE)),
            Some(Action::OpenLinkExtractor)
        );
        assert_eq!(
            map.resolve(Focus::List, key(KeyCode::Char('h'), KeyModifiers::NONE)),
            Some(Action::ToggleShowLinkUrls)
        );
        assert_eq!(
            map.resolve(Focus::List, key(KeyCode::Char('v'), KeyModifiers::CONTROL)),
            Some(Action::UploadClipboardImage)
        );
        assert_eq!(
            map.resolve(Focus::List, key(KeyCode::Char('f'), KeyModifiers::CONTROL)),
            Some(Action::OpenFilePicker)
        );
        assert_eq!(
            map.resolve(Focus::List, key(KeyCode::Char('p'), KeyModifiers::CONTROL)),
            Some(Action::OpenOverlay(OverlayKind::CommandPalette))
        );
        assert_eq!(
            map.resolve(Focus::List, key(KeyCode::Char('m'), KeyModifiers::NONE)),
            Some(Action::TogglePreviewRenderMode)
        );
        assert_eq!(
            map.resolve(Focus::Inspector, key(KeyCode::Char('m'), KeyModifiers::NONE)),
            Some(Action::TogglePreviewRenderMode)
        );
    }

    #[test]
    fn m_key_does_not_hijack_text_input_overlays() {
        let map = Keymap::default_sharepoint();
        let m_key = key(KeyCode::Char('m'), KeyModifiers::NONE);

        // In List / Inspector browse modes, `m` toggles rendered vs raw preview
        assert_eq!(
            crate::ui::event::key_to_action(m_key, Focus::List, &map, false),
            Some(Action::TogglePreviewRenderMode)
        );
        assert_eq!(
            crate::ui::event::key_to_action(m_key, Focus::Inspector, &map, false),
            Some(Action::TogglePreviewRenderMode)
        );

        // When a text-input overlay (Search, CommandPalette, QuickInput) is active,
        // `m` must be delivered as character input, never hijacked by TogglePreviewRenderMode.
        assert_eq!(
            crate::ui::event::key_to_action(m_key, Focus::Overlay, &map, true),
            Some(Action::Input('m'))
        );
        // Even in non-text overlays, Focus::Overlay does not resolve List/Inspector `m`
        assert_eq!(
            map.resolve(Focus::Overlay, m_key),
            None
        );
    }
}
