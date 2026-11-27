//! Inspector preview caches, revision keys, invalidation, and raw image LRU storage.

use std::sync::Arc;

use crate::img_preview::{self, TerminalGraphicsCaps};
use crate::models::{DriveItem, DriveItemVersion};

use super::helpers::text_preview_key_matches;
use super::state::{AppState, UiState, MAX_RAW_IMAGE_ENTRIES};

impl AppState {
    pub fn item_revision_key(item: &DriveItem) -> String {
        format!(
            "{}:{}:{}",
            item.effective_drive_id(),
            item.effective_item_id(),
            item.content_revision()
        )
    }

    pub fn text_preview_key(drive_id: &str, item_id: &str) -> String {
        format!("{drive_id}:{item_id}")
    }

    pub fn text_preview_inflight_key(drive_id: &str, item_id: &str) -> String {
        format!("txt:{drive_id}:{item_id}")
    }

    pub fn text_preview_for(&self, item: &DriveItem) -> Option<&str> {
        let (key, content) = self.text_previews.as_ref()?;
        let rev_ok = self
            .text_preview_revision
            .as_deref()
            .is_none_or(|rev| rev == item.content_revision());
        (text_preview_key_matches(key, item.effective_drive_id(), &item.id) && rev_ok)
            .then_some(content.as_str())
    }

    pub fn text_preview_error_for(&self, item: &DriveItem) -> Option<&str> {
        let key = Self::text_preview_inflight_key(item.effective_drive_id(), &item.id);
        self.preview_errors.get(&key).map(String::as_str)
    }

    pub fn versions_for(&self, item: &DriveItem) -> Option<&[DriveItemVersion]> {
        self.item_versions
            .get(&Self::item_revision_key(item))
            .map(Vec::as_slice)
    }

    pub fn image_preview_error_for(&self, item: &DriveItem) -> Option<&str> {
        self.preview_errors
            .get(&Self::item_revision_key(item))
            .or_else(|| self.preview_errors.get(&item.id))
            .map(String::as_str)
    }

    pub fn set_text_preview(&mut self, drive_id: &str, item_id: &str, content: String) {
        let rev = self
            .selected_item_ref()
            .filter(|item| item.effective_drive_id() == drive_id && item.id == item_id)
            .map(DriveItem::content_revision);
        self.text_previews = Some((Self::text_preview_key(drive_id, item_id), content));
        self.text_preview_revision = rev;
        self.text_preview_gen = self.text_preview_gen.wrapping_add(1);
    }

    pub fn clear_inspector_preview(&mut self, ui: &mut UiState) {
        self.text_previews = None;
        self.text_preview_revision = None;
        self.text_preview_gen = self.text_preview_gen.wrapping_add(1);
        ui.markup_cache = None;
        self.preview_errors.retain(|k, _| !k.starts_with("txt:"));
    }

    pub fn invalidate_active_item_caches(&mut self, ui: &mut UiState) {
        let active = self.active_item();
        self.clear_inspector_preview(ui);
        if let Some(item) = active {
            let rev_key = Self::item_revision_key(&item);
            let drive_item_prefix = format!("{}:{}:", item.effective_drive_id(), item.effective_item_id());
            self.item_versions.retain(|k, _| !k.starts_with(&drive_item_prefix));
            self.raw_image_bytes
                .retain(|(k, _)| !k.starts_with(&drive_item_prefix));
            self.rendered_images.retain(|k, _| {
                !k.starts_with(&format!("inline:{drive_item_prefix}"))
                    && !k.starts_with(&format!("modal:{drive_item_prefix}"))
            });
            self.preview_errors.remove(&rev_key);
            self.preview_errors.remove(&item.id);
            let tkey = Self::text_preview_inflight_key(item.effective_drive_id(), &item.id);
            self.inflight_previews.remove(&tkey);
            self.inflight_text_revisions.remove(&tkey);
            self.pending_html_overlay_fetches.remove(&tkey);
            self.inflight_previews.remove(&format!("ver:{rev_key}"));
        }
    }

    pub fn sync_inspector_preview_for(&mut self, selected: Option<&DriveItem>, ui: &mut UiState) {
        let active_triple = selected
            .filter(|item| !item.is_folder && item.is_text_previewable())
            .map(|item| {
                (
                    Self::text_preview_key(item.effective_drive_id(), &item.id),
                    Self::text_preview_inflight_key(item.effective_drive_id(), &item.id),
                    item.content_revision(),
                )
            });
        match active_triple {
            Some((ref expected_key, ref expected_err_key, ref expected_rev)) => {
                let key_mismatch = self
                    .text_previews
                    .as_ref()
                    .is_some_and(|(stored, _)| stored != expected_key);
                let rev_mismatch = self
                    .text_preview_revision
                    .as_deref()
                    .is_some_and(|rev| rev != expected_rev.as_str());
                if key_mismatch || rev_mismatch {
                    self.text_previews = None;
                    self.text_preview_revision = None;
                    self.text_preview_gen = self.text_preview_gen.wrapping_add(1);
                    ui.markup_cache = None;
                    if rev_mismatch {
                        let inflight_same_rev = self
                            .inflight_text_revisions
                            .get(expected_err_key)
                            .is_some_and(|r| r == expected_rev);
                        if !inflight_same_rev {
                            self.inflight_previews.remove(expected_err_key);
                            self.inflight_text_revisions.remove(expected_err_key);
                        }
                        self.preview_errors.remove(expected_err_key);
                    }
                }
                if ui
                    .markup_cache
                    .as_ref()
                    .is_some_and(|cached| cached.key.item_id != *expected_key)
                {
                    ui.markup_cache = None;
                }
                self.preview_errors
                    .retain(|k, _| !k.starts_with("txt:") || k == expected_err_key);
            }
            None => {
                self.clear_inspector_preview(ui);
            }
        }
    }

    pub(super) fn is_active_item(&self, drive_id: &str, item_id: &str) -> bool {
        self.selected_item_ref()
            .is_some_and(|item| item.effective_drive_id() == drive_id && item.id == item_id)
    }

    pub(super) fn is_active_text_item(&self, drive_id: &str, item_id: &str) -> bool {
        self.selected_item_ref().is_some_and(|item| {
            !item.is_folder
                && item.is_text_previewable()
                && item.effective_drive_id() == drive_id
                && item.id == item_id
        })
    }


    pub fn inline_image_cache_key(&self, item: &DriveItem) -> String {
        format!("inline:{}", Self::item_revision_key(item))
    }

    pub fn modal_image_cache_key(
        &self,
        item: &DriveItem,
        cols: u16,
        rows: u16,
        zoom_pct: u16,
        caps: TerminalGraphicsCaps,
    ) -> String {
        img_preview::preview_image_cache_key(
            "modal",
            &Self::item_revision_key(item),
            cols,
            rows,
            zoom_pct,
            caps.cell_px,
            caps.protocol,
        )
    }


    pub(crate) fn cache_raw_image(&mut self, cache_key: String, bytes: Arc<[u8]>) {
        if bytes.len() > img_preview::MAX_IMAGE_CACHE_BYTES {
            return;
        }
        self.raw_image_bytes.retain(|(key, _)| key != &cache_key);
        let mut total_bytes: usize = self.raw_image_bytes.iter().map(|(_, raw)| raw.len()).sum();
        while self.raw_image_bytes.len() >= MAX_RAW_IMAGE_ENTRIES
            || total_bytes + bytes.len() > img_preview::MAX_IMAGE_CACHE_BYTES
        {
            let (_, evicted) = self.raw_image_bytes.pop_front().expect("cache has entries to evict");
            total_bytes -= evicted.len();
        }
        self.raw_image_bytes.push_back((cache_key, bytes));
    }

}
