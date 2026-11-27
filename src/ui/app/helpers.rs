//! UI preference persistence and clipboard helpers.

use crate::config::ImageProtocolPreference;
use crate::img_preview::TerminalImageProtocol;

use super::super::toast::ToastKind;
use super::state::{AppState, UiMsg, UiState};

impl AppState {
    /// Saves the preferences the user can change in the TUI, and nothing else: not the
    /// rest of the config, and not the image protocol the terminal probe came up with.
    pub fn save_ui_prefs(&mut self, ui: &UiState) {
        // `render_markup` may have come from the environment. It becomes the user's own
        // setting, worth writing down, the first time they toggle it.
        if ui.render_markup != self.config.render_markup {
            self.render_markup_user_set = true;
        }
        self.config.notify = ui.notify;
        self.config.show_nav_rail = ui.show_nav_rail;
        self.config.inline_images = ui.inline_images;
        self.config.render_markup = ui.render_markup;
        self.config.density = ui.density;
        self.config.theme = ui.theme_preset;
        self.persist_prefs(&ui_pref_updates(ui, self.render_markup_user_set));
    }

    /// Saves the image protocol the user picked by hand. `auto` stays `auto` until then.
    pub fn save_image_protocol_choice(&mut self, ui: &UiState) {
        let choice = image_protocol_preference(ui.graphics_caps.protocol);
        self.config.image_protocol = choice;
        self.persist_prefs(&[("image_protocol", pref_value(choice))]);
    }

    fn persist_prefs(&self, updates: &[(&'static str, toml::Value)]) {
        if let Err(err) = crate::config::persist_config_keys(updates) {
            let _ = self.ui_tx.send(UiMsg::Toast {
                message: format!("Settings not saved: {err:#}"),
                kind: ToastKind::Error,
            });
        }
    }
}

fn image_protocol_preference(protocol: TerminalImageProtocol) -> ImageProtocolPreference {
    match protocol {
        TerminalImageProtocol::Kitty => ImageProtocolPreference::Kitty,
        TerminalImageProtocol::ITerm2 => ImageProtocolPreference::Iterm2,
        TerminalImageProtocol::Sixel => ImageProtocolPreference::Sixel,
        TerminalImageProtocol::Halfblock => ImageProtocolPreference::Halfblock,
    }
}

/// A preference as it is written in `config.toml`.
fn pref_value<T: serde::Serialize>(value: T) -> toml::Value {
    // Preferences are booleans and unit enums; neither can fail to become a TOML value.
    toml::Value::try_from(value).expect("a preference serializes to a TOML value")
}

/// The config keys `save_ui_prefs` writes, with their values.
///
/// `image_protocol` is never among them: what the UI holds is the probe's answer for this
/// terminal, not a choice. `render_markup` is included only once the user has toggled it.
pub fn ui_pref_updates(ui: &UiState, render_markup_user_set: bool) -> Vec<(&'static str, toml::Value)> {
    let mut updates = vec![
        ("notify", pref_value(ui.notify)),
        ("show_nav_rail", pref_value(ui.show_nav_rail)),
        ("inline_images", pref_value(ui.inline_images)),
        ("density", pref_value(ui.density)),
        ("theme", pref_value(ui.theme_preset)),
    ];
    if render_markup_user_set {
        updates.push(("render_markup", pref_value(ui.render_markup)));
    }
    updates
}

pub(super) fn text_preview_key_matches(key: &str, drive_id: &str, item_id: &str) -> bool {
    key.strip_prefix(drive_id)
        .and_then(|rest| rest.strip_prefix(':'))
        .is_some_and(|rest| rest == item_id)
}

pub fn copy_text_to_clipboard(text: &str) -> anyhow::Result<()> {
    let mut cb = arboard::Clipboard::new()?;
    cb.set_text(text.to_string())?;
    Ok(())
}

pub(super) fn read_clipboard_png_bytes() -> anyhow::Result<Vec<u8>> {
    let mut cb = arboard::Clipboard::new()?;
    let img = cb.get_image()?;
    let rgba = image::RgbaImage::from_raw(
        img.width as u32,
        img.height as u32,
        img.bytes.into_owned(),
    )
    .ok_or_else(|| anyhow::anyhow!("Invalid RGBA clipboard image buffer"))?;
    let mut out = Vec::new();
    let dyn_img = image::DynamicImage::ImageRgba8(rgba);
    dyn_img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)?;
    Ok(out)
}
