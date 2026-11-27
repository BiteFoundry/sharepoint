//! Application state (`AppState`), UI-only state (`UiState`), and async UI task messages (`UiMsg`).

mod cache;
mod events;
mod helpers;
mod messages;
mod navigation;
mod requests;
mod state;

#[cfg(test)]
mod tests;

pub use helpers::{copy_text_to_clipboard, ui_pref_updates};
pub use state::{
    ActiveViewSource, AppState, Focus, LoadMoreOutcome, LocalUploadPreview, NarrowPane,
    PendingPixelGraphic, ScrollState, SortMode, UiMsg, UiState,
};
