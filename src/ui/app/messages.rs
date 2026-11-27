//! Async `UiMsg` router and domain handlers for `AppState`.

use std::sync::Arc;

use bitefoundry_html::{sniff_content_kind, ContentKind};

use crate::config::Persona;
use crate::models::{
    DriveInfo, DriveItem, PinnedTarget, SharePointSite, VirtualListKind, VirtualListStatus,
};

use super::super::notification;
use super::super::overlays::html_preview::HtmlPreviewOverlay;
use super::super::overlays::Overlay;
use super::super::toast::ToastKind;
use super::helpers::copy_text_to_clipboard;
use super::state::{ActiveViewSource, AppState, LocalUploadPreview, UiMsg, UiState};

impl AppState {
    pub fn apply_ui_msg(&mut self, msg: UiMsg, ui: &mut UiState) {
        match msg {
            UiMsg::BootstrapLoaded {
                my_drive,
                sites,
                drives,
                pins,
            } => self.handle_bootstrap_loaded(my_drive, sites, drives, pins, ui),
            UiMsg::RecentLoaded(items) => {
                self.handle_virtual_list_items(VirtualListKind::Recent, items, ui);
            }
            UiMsg::SharedLoaded(items) => {
                self.handle_virtual_list_items(VirtualListKind::SharedWithMe, items, ui);
            }
            UiMsg::VirtualListLoaded {
                seq,
                kind,
                items,
                status,
            } => self.handle_virtual_list_loaded(seq, kind, items, status, ui),
            UiMsg::SiteDrivesLoaded { site_id, drives } => {
                self.site_drives.insert(site_id, drives);
            }
            UiMsg::TextPreviewLoaded {
                drive_id,
                item_id,
                revision,
                content,
            } => self.handle_text_preview_loaded(drive_id, item_id, revision, content, ui),
            UiMsg::VersionsLoaded {
                cache_key,
                versions,
                ..
            } => {
                self.inflight_previews.remove(&format!("ver:{cache_key}"));
                self.item_versions.insert(cache_key, versions);
            }
            UiMsg::VersionsFailed { cache_key } => {
                self.inflight_previews.remove(&format!("ver:{cache_key}"));
            }
            UiMsg::PreviewFailed {
                drive_id,
                item_id,
                revision,
                inflight_key,
                error,
            } => self.handle_preview_failed(drive_id, item_id, revision, inflight_key, error),
            UiMsg::ImageBytesFetched { cache_key, result } => {
                self.handle_image_bytes_fetched(cache_key, result);
            }
            UiMsg::ImageDecoded {
                cache_key,
                rendered,
            } => {
                self.inflight_previews.remove(&cache_key);
                if self.rendered_images.len() >= 12 {
                    self.rendered_images.clear();
                }
                self.rendered_images.insert(cache_key, rendered);
            }
            UiMsg::HtmlContentSniffed {
                drive_id,
                item_id,
                title,
                html,
                cache_keys,
            } => self.handle_html_content_sniffed(drive_id, item_id, title, html, cache_keys, ui),
            UiMsg::HtmlPreviewReady {
                drive_id,
                item_id,
                revision,
                title,
                html,
            } => self.handle_html_preview_ready(drive_id, item_id, revision, title, html, ui),
            UiMsg::HtmlServed { url } => match open::that(&url) {
                Ok(()) => ui.toast(format!("Serving HTML at {url}"), ToastKind::Success),
                Err(err) => ui.toast(
                    format!("HTML ready at {url} (browser open failed: {err})"),
                    ToastKind::Warning,
                ),
            },
            UiMsg::ItemDownloaded(dl) => {
                let msg = format!("Downloaded {} -> {}", dl.file_name, dl.path.display());
                ui.toast(msg.clone(), ToastKind::Success);
                if ui.notify {
                    notification::show("SharePoint Download Complete", &msg);
                }
            }
            UiMsg::ItemUploaded {
                item,
                parent_id,
                local_preview,
            } => self.handle_item_uploaded(item, parent_id, local_preview, ui),
            UiMsg::FolderCreated { folder, parent_id } => {
                self.handle_folder_created(folder, parent_id, ui);
            }
            UiMsg::ItemRenamed(item) => self.handle_item_renamed(item, ui),
            UiMsg::ItemDeleted {
                drive_id,
                item_id,
                raw_drive_id,
                raw_item_id,
                name,
            } => self.handle_item_deleted(drive_id, item_id, raw_drive_id, raw_item_id, name, ui),
            UiMsg::ShareLinkCreated { item_id, link } => {
                let copied = copy_text_to_clipboard(&link.web_url).is_ok();
                self.created_share_links.insert(item_id, link.web_url.clone());
                let suffix = if copied { " (copied to clipboard)" } else { "" };
                ui.toast(
                    format!("Share link: {}{suffix}", link.web_url),
                    ToastKind::Success,
                );
            }
            UiMsg::SearchResults { seq, items } => {
                if seq == self.search_seq {
                    self.search_results = items;
                }
            }
            UiMsg::UpdateChecked(res) => {
                if res.update_available {
                    ui.toast(
                        format!(
                            "Update available: v{} -> v{}",
                            res.current_version, res.latest_version
                        ),
                        ToastKind::Info,
                    );
                } else {
                    ui.toast(
                        format!("Up to date (v{})", res.current_version),
                        ToastKind::Success,
                    );
                }
            }
            UiMsg::TaskError { label, error } => {
                let was_loading = self.loading_folder;
                self.loading_folder = false;
                if was_loading {
                    self.ensure_inspector_loaded(ui);
                }
                ui.toast(format!("{label} failed: {error}"), ToastKind::Error);
            }
            UiMsg::Toast { message, kind } => {
                ui.toast(message, kind);
            }
        }
    }

    fn handle_bootstrap_loaded(
        &mut self,
        my_drive: Option<DriveInfo>,
        sites: Vec<SharePointSite>,
        drives: Vec<DriveInfo>,
        pins: Vec<PinnedTarget>,
        ui: &mut UiState,
    ) {
        let loaded_drive = self.my_drive.is_none() && my_drive.is_some();
        if self.my_drive.is_none() {
            self.my_drive = my_drive;
        }
        if self.sites.is_empty() {
            self.sites = sites;
        }
        for d in drives {
            if let Some(ref sid) = d.site_id {
                let list = self.site_drives.entry(sid.clone()).or_default();
                if !list.iter().any(|existing| existing.id == d.id) {
                    list.push(d);
                }
            }
        }
        self.pins = pins;
        if self.persona == Persona::SharePoint
            && !self.initial_persona_library_selected
            && matches!(self.active_source, ActiveViewSource::OneDriveRoot)
            && self.folder_stack.is_empty()
        {
            if let Some(site) = self.sites.first() {
                if let Some(ds) = self.site_drives.get(&site.id) {
                    if let Some(d) = ds.first() {
                        self.initial_persona_library_selected = true;
                        self.active_source = ActiveViewSource::Drive {
                            drive_id: d.id.clone(),
                            label: d.display_label(),
                        };
                        self.load_active_folder(ui);
                        return;
                    }
                }
            }
        }
        if loaded_drive && matches!(self.active_source, ActiveViewSource::OneDriveRoot) {
            self.load_folder(ui, self.folder_stack.is_empty());
        }
    }

    fn handle_virtual_list_items(
        &mut self,
        kind: VirtualListKind,
        items: Vec<DriveItem>,
        ui: &mut UiState,
    ) {
        let active_matches = match kind {
            VirtualListKind::Recent => matches!(self.active_source, ActiveViewSource::Recent),
            VirtualListKind::SharedWithMe => {
                matches!(self.active_source, ActiveViewSource::SharedWithMe)
            }
        };
        if active_matches {
            self.current_items = items;
            self.loading_folder = false;
            self.reconcile_selection_after_items_update(ui);
            self.refresh_open_overlays_for_items(ui);
            self.ensure_inspector_loaded(ui);
        }
    }

    fn handle_virtual_list_loaded(
        &mut self,
        seq: u64,
        kind: VirtualListKind,
        items: Vec<DriveItem>,
        status: VirtualListStatus,
        ui: &mut UiState,
    ) {
        if seq != self.virtual_list_seq {
            return;
        }
        let is_avail = status.is_available();
        let active_matches = match kind {
            VirtualListKind::Recent => {
                self.recent_status = Some(status);
                matches!(self.active_source, ActiveViewSource::Recent)
            }
            VirtualListKind::SharedWithMe => {
                self.shared_status = Some(status);
                matches!(self.active_source, ActiveViewSource::SharedWithMe)
            }
        };
        if active_matches {
            self.loading_folder = false;
            if is_avail {
                self.current_items = items;
                self.reconcile_selection_after_items_update(ui);
                self.refresh_open_overlays_for_items(ui);
                self.ensure_inspector_loaded(ui);
            } else {
                self.current_items.clear();
                self.active_selected_id = None;
                ui.list_scroll.selected_idx = None;
                self.clear_inspector_preview(ui);
            }
        }
    }

    fn handle_text_preview_loaded(
        &mut self,
        drive_id: String,
        item_id: String,
        revision: Option<String>,
        content: String,
        ui: &mut UiState,
    ) {
        let err_key = Self::text_preview_inflight_key(&drive_id, &item_id);
        if revision
            .as_ref()
            .is_some_and(|rev| self.inflight_text_revisions.get(&err_key) != Some(rev))
        {
            return;
        }
        self.inflight_previews.remove(&err_key);
        self.inflight_text_revisions.remove(&err_key);
        let want_html_overlay = self.pending_html_overlay_fetches.remove(&err_key);
        let matched_item = self
            .current_items
            .iter()
            .find(|it| it.effective_drive_id() == drive_id && it.id == item_id)
            .cloned();
        let current_rev = matched_item.as_ref().map(DriveItem::content_revision);
        if revision.is_some() && current_rev != revision {
            return;
        }
        if self.is_active_text_item(&drive_id, &item_id) {
            self.preview_errors.remove(&err_key);
            self.set_text_preview(&drive_id, &item_id, content.clone());
        }
        let mut updated_open_overlay = false;
        for overlay in &mut ui.overlays {
            if let Overlay::HtmlPreview(ov) = overlay {
                if ov.item_id.as_deref() == Some(item_id.as_str())
                    && ov.drive_id.as_deref().is_none_or(|d| d == drive_id)
                {
                    let title = matched_item
                        .as_ref()
                        .map(|it| it.name.clone())
                        .unwrap_or_else(|| ov.title.clone());
                    ov.replace_html(title, content.clone(), current_rev.clone());
                    updated_open_overlay = true;
                }
            }
        }
        if want_html_overlay && !updated_open_overlay {
            if let Some(it) = matched_item {
                let mut ov = HtmlPreviewOverlay::new(it.name, content, Some(item_id));
                ov.drive_id = Some(drive_id);
                ov.revision = current_rev;
                ui.push_overlay(Overlay::HtmlPreview(Box::new(ov)));
            }
        }
    }

    fn handle_preview_failed(
        &mut self,
        drive_id: String,
        item_id: String,
        revision: Option<String>,
        inflight_key: Option<String>,
        error: String,
    ) {
        if let Some(rev) = revision.as_ref() {
            let tracked = inflight_key
                .as_ref()
                .and_then(|key| self.inflight_text_revisions.get(key));
            if tracked != Some(rev) {
                return;
            }
        }
        let is_text_req = inflight_key.as_deref().is_none_or(|k| k.starts_with("txt:"));
        if let Some(key) = inflight_key {
            self.inflight_previews.remove(&key);
            self.inflight_text_revisions.remove(&key);
            self.pending_html_overlay_fetches.remove(&key);
        }
        if is_text_req {
            if self.is_active_text_item(&drive_id, &item_id) {
                self.preview_errors.insert(
                    Self::text_preview_inflight_key(&drive_id, &item_id),
                    error,
                );
            }
        } else {
            let err_key = self
                .current_items
                .iter()
                .find(|it| it.effective_drive_id() == drive_id && it.id == item_id)
                .map(Self::item_revision_key)
                .unwrap_or(item_id);
            self.preview_errors.insert(err_key, error);
        }
    }

    fn handle_image_bytes_fetched(
        &mut self,
        cache_key: String,
        result: Result<Arc<[u8]>, String>,
    ) {
        let Some(pending) = self.inflight_raw_images.remove(&cache_key) else {
            return;
        };
        match result {
            Ok(bytes) => {
                let kind = sniff_content_kind(
                    pending.item.mime_type.as_deref(),
                    Some(&pending.item.name),
                    Some(&bytes),
                );
                if matches!(kind, ContentKind::RasterImage | ContentKind::Svg) {
                    self.cache_raw_image(cache_key, Arc::clone(&bytes));
                }
                self.spawn_image_decodes(pending.item, bytes, kind, pending.decodes);
            }
            Err(error) => {
                for decode in pending.decodes {
                    self.inflight_previews.remove(&decode.cache_key);
                }
                self.preview_errors.insert(cache_key, error);
            }
        }
    }

    fn handle_html_content_sniffed(
        &mut self,
        drive_id: String,
        item_id: String,
        title: String,
        html: String,
        cache_keys: Vec<String>,
        ui: &mut UiState,
    ) {
        for cache_key in cache_keys {
            self.inflight_previews.remove(&cache_key);
        }
        self.preview_errors.remove(&item_id);
        if self.is_active_item(&drive_id, &item_id) {
            self.set_text_preview(&drive_id, &item_id, html.clone());
        }
        if matches!(ui.overlays.last(), Some(Overlay::ImagePreview(_))) {
            ui.pop_overlay();
        }
        let already_open = matches!(
            ui.overlays.last(),
            Some(Overlay::HtmlPreview(ov)) if ov.item_id.as_deref() == Some(item_id.as_str())
        );
        if !already_open {
            ui.push_overlay(Overlay::HtmlPreview(Box::new(HtmlPreviewOverlay::new(
                title,
                html,
                Some(item_id),
            ))));
        }
        ui.toast(
            "HTML content detected — opened interactive HTML viewer",
            ToastKind::Info,
        );
    }

    fn handle_html_preview_ready(
        &mut self,
        drive_id: String,
        item_id: String,
        revision: Option<String>,
        title: String,
        html: String,
        ui: &mut UiState,
    ) {
        let current_rev = self
            .current_items
            .iter()
            .find(|it| it.effective_drive_id() == drive_id && it.id == item_id)
            .map(DriveItem::content_revision);
        if revision.is_some() && current_rev != revision {
            return;
        }
        if self.is_active_text_item(&drive_id, &item_id) {
            self.preview_errors
                .remove(&Self::text_preview_inflight_key(&drive_id, &item_id));
            self.set_text_preview(&drive_id, &item_id, html.clone());
        }
        if let Some(Overlay::HtmlPreview(ov)) = ui.overlays.last_mut() {
            if ov.item_id.as_deref() == Some(item_id.as_str())
                && ov.drive_id.as_deref().is_none_or(|d| d == drive_id)
            {
                ov.replace_html(title, html, current_rev);
                return;
            }
        }
        let mut ov = HtmlPreviewOverlay::new(title, html, Some(item_id));
        ov.drive_id = Some(drive_id);
        ov.revision = current_rev;
        ui.push_overlay(Overlay::HtmlPreview(Box::new(ov)));
    }

    fn handle_item_uploaded(
        &mut self,
        item: DriveItem,
        parent_id: Option<String>,
        local_preview: Option<LocalUploadPreview>,
        ui: &mut UiState,
    ) {
        let msg = format!(
            "Uploaded {} ({})",
            item.name,
            crate::models::format_human_size(item.size)
        );
        ui.toast(msg.clone(), ToastKind::Success);
        if ui.notify {
            notification::show("SharePoint Upload Complete", &msg);
        }
        if self.active_folder_target() == Some((item.drive_id.clone(), parent_id.clone())) {
            self.current_items.retain(|existing| existing.id != item.id);
            self.current_items.push(item.clone());
            self.active_selected_id = Some(item.id.clone());
            self.loading_folder = false;
            self.reconcile_selection_after_items_update(ui);
            match local_preview {
                Some(LocalUploadPreview::Text(text)) => {
                    if self.is_active_text_item(item.effective_drive_id(), &item.id) {
                        self.set_text_preview(item.effective_drive_id(), &item.id, text);
                    }
                }
                Some(LocalUploadPreview::Image(bytes)) => {
                    self.cache_raw_image(Self::item_revision_key(&item), bytes);
                }
                None => {}
            }
            self.refresh_open_overlays_for_items(ui);
            self.ensure_inspector_loaded(ui);
            self.sync_ctrl.request(crate::sync::SyncCommand::SyncFolder {
                drive_id: item.drive_id,
                parent_id,
                reuse_startup_root: false,
                limit: self.requested_folder_limit(),
            });
        }
    }

    fn handle_folder_created(
        &mut self,
        folder: DriveItem,
        parent_id: Option<String>,
        ui: &mut UiState,
    ) {
        ui.toast(format!("Created folder '{}'", folder.name), ToastKind::Success);
        if self.active_folder_target() == Some((folder.drive_id.clone(), parent_id.clone())) {
            self.current_items.retain(|existing| existing.id != folder.id);
            self.current_items.push(folder.clone());
            self.active_selected_id = Some(folder.id.clone());
            self.loading_folder = false;
            self.reconcile_selection_after_items_update(ui);
            self.sync_ctrl.request(crate::sync::SyncCommand::SyncFolder {
                drive_id: folder.drive_id,
                parent_id,
                reuse_startup_root: false,
                limit: self.requested_folder_limit(),
            });
        }
    }

    fn handle_item_renamed(&mut self, item: DriveItem, ui: &mut UiState) {
        ui.toast(format!("Renamed to '{}'", item.name), ToastKind::Success);
        let mut touched_active = self.active_selected_id.as_deref() == Some(item.id.as_str());
        for existing in &mut self.current_items {
            let is_same = (existing.drive_id == item.drive_id && existing.id == item.id)
                || (existing.effective_drive_id() == item.drive_id
                    && existing.effective_item_id() == item.id);
            if is_same {
                existing.name = item.name.clone();
                if self.active_selected_id.as_deref() == Some(existing.id.as_str()) {
                    touched_active = true;
                }
            }
        }
        for pin in &mut self.pins {
            if pin.drive_id.as_deref() == Some(item.drive_id.as_str())
                && pin.item_id.as_deref() == Some(item.id.as_str())
            {
                pin.label = item.name.clone();
                let _ = self.storage.upsert_pin(pin);
            }
        }
        if touched_active {
            ui.markup_cache = None;
        }
        self.reconcile_selection_after_items_update(ui);
        self.refresh_open_overlays_for_items(ui);
        self.load_active_folder(ui);
    }

    fn handle_item_deleted(
        &mut self,
        drive_id: String,
        item_id: String,
        raw_drive_id: Option<String>,
        raw_item_id: Option<String>,
        name: String,
        ui: &mut UiState,
    ) {
        ui.toast(format!("Deleted '{name}'"), ToastKind::Success);
        let raw_did = raw_drive_id.as_deref();
        let raw_iid = raw_item_id.as_deref();
        let active_deleted = self.active_selected_id.as_deref() == Some(item_id.as_str())
            || raw_iid.is_some_and(|rid| self.active_selected_id.as_deref() == Some(rid));
        if active_deleted {
            self.clear_inspector_preview(ui);
        }
        self.current_items.retain(|it| {
            let eff_match =
                it.effective_drive_id() == drive_id && it.effective_item_id() == item_id;
            let direct_match = it.drive_id == drive_id && it.id == item_id;
            let raw_match = matches!((raw_did, raw_iid), (Some(rd), Some(ri)) if it.drive_id == rd && it.id == ri);
            !(eff_match || direct_match || raw_match)
        });
        let eff_pin_key = format!("folder:{drive_id}:{item_id}");
        let raw_pin_key = match (raw_did, raw_iid) {
            (Some(rd), Some(ri)) => Some(format!("folder:{rd}:{ri}")),
            _ => None,
        };
        let mut removed_pin_keys: Vec<String> = Vec::new();
        self.pins.retain(|p| {
            let eff_pin = p.drive_id.as_deref() == Some(drive_id.as_str())
                && p.item_id.as_deref() == Some(item_id.as_str());
            let raw_pin = matches!(
                (raw_did, raw_iid),
                (Some(rd), Some(ri)) if p.drive_id.as_deref() == Some(rd) && p.item_id.as_deref() == Some(ri)
            );
            let key_pin =
                p.key == eff_pin_key || raw_pin_key.as_deref() == Some(p.key.as_str());
            if eff_pin || raw_pin || key_pin {
                removed_pin_keys.push(p.key.clone());
                false
            } else {
                true
            }
        });
        for key in removed_pin_keys {
            let _ = self.storage.remove_pin(&key);
        }
        self.reconcile_selection_after_items_update(ui);
        self.refresh_open_overlays_for_items(ui);
        self.ensure_inspector_loaded(ui);
        self.load_active_folder(ui);
    }
}
