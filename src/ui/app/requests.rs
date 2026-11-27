//! Async fetchers for folders, previews, image decodes, overlays, and clipboard uploads.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bitefoundry_html::{sniff_content_kind, ContentKind};

use crate::cli::DEFAULT_TEXT_PREVIEW_MAX_BYTES;
use crate::img_preview::{self, FitBox, TerminalGraphicsCaps};
use crate::models::{DriveItem, VirtualListKind};
use crate::sync::{FocusTarget, FocusViewKind, FOLDER_PAGE_SIZE};

use super::super::overlays::Overlay;
use super::super::toast::ToastKind;
use super::helpers::read_clipboard_png_bytes;
use super::state::{
    ActiveViewSource, AppState, ImageDecodeRequest, LocalUploadPreview, PendingImageDownload,
    UiMsg, UiState,
};

impl AppState {
    /// Shared text/HTML content fetcher used by both the reading pane (inspector) and open
    /// `HtmlPreviewOverlay`, guaranteeing at most one `/content` download per item revision.
    pub fn spawn_shared_text_fetch(&mut self, item: &DriveItem, notify_html_overlay: bool) {
        let did = item.effective_drive_id().to_string();
        let iid = item.effective_item_id().to_string();
        let item_id = item.id.clone();
        let rev = item.content_revision();
        let tkey = Self::text_preview_inflight_key(&did, &item_id);

        // If already cached at this exact revision, serve synchronously with 0 HTTP calls.
        if let Some(cached) = self.text_preview_for(item).map(str::to_string) {
            if notify_html_overlay {
                let _ = self.ui_tx.send(UiMsg::HtmlPreviewReady {
                    drive_id: did,
                    item_id,
                    revision: Some(rev),
                    title: item.name.clone(),
                    html: cached,
                });
            }
            return;
        }

        // If the exact same revision is already in flight, coalesce instead of starting a second GET.
        if self
            .inflight_text_revisions
            .get(&tkey)
            .is_some_and(|in_rev| *in_rev == rev)
        {
            if notify_html_overlay {
                self.pending_html_overlay_fetches.insert(tkey);
            }
            return;
        }

        self.inflight_previews.insert(tkey.clone());
        self.inflight_text_revisions.insert(tkey.clone(), rev.clone());
        if notify_html_overlay {
            self.pending_html_overlay_fetches.insert(tkey.clone());
        }

        let graph = self.graph.clone();
        let tx = self.ui_tx.clone();
        let dl = item.download_url.clone();
        let drive_for_msg = did.clone();
        let id_for_msg = item_id.clone();
        let title = item.name.clone();
        let emit_html_ready = notify_html_overlay;

        tokio::spawn(async move {
            match graph
                .download_item_bytes(&did, &iid, dl.as_deref(), DEFAULT_TEXT_PREVIEW_MAX_BYTES)
                .await
            {
                Ok(bytes) => {
                    let content = String::from_utf8_lossy(&bytes).into_owned();
                    let _ = tx.send(UiMsg::TextPreviewLoaded {
                        drive_id: drive_for_msg.clone(),
                        item_id: id_for_msg.clone(),
                        revision: Some(rev.clone()),
                        content: content.clone(),
                    });
                    if emit_html_ready {
                        let _ = tx.send(UiMsg::HtmlPreviewReady {
                            drive_id: drive_for_msg,
                            item_id: id_for_msg,
                            revision: Some(rev),
                            title,
                            html: content,
                        });
                    }
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::PreviewFailed {
                        drive_id: drive_for_msg,
                        item_id: id_for_msg,
                        revision: Some(rev),
                        inflight_key: Some(tkey),
                        error: format!("{err:#}"),
                    });
                }
            }
        });
    }

    pub fn refresh_open_overlays_for_items(&mut self, ui: &mut UiState) {
        let mut html_refresh: Option<DriveItem> = None;
        let mut image_refresh: Option<(DriveItem, String, FitBox)> = None;

        for overlay in &mut ui.overlays {
            match overlay {
                Overlay::HtmlPreview(ov) => {
                    if let Some(ref target_id) = ov.item_id {
                        if let Some(updated) = self.current_items.iter().find(|it| {
                            it.id == *target_id
                                && ov
                                    .drive_id
                                    .as_deref()
                                    .is_none_or(|d| d == it.effective_drive_id())
                        }) {
                            let new_rev = updated.content_revision();
                            if ov.revision.as_deref() != Some(new_rev.as_str()) {
                                ov.revision = Some(new_rev);
                                html_refresh = Some(updated.clone());
                            } else if ov.title != updated.name {
                                // Metadata-only rename: update overlay title without re-downloading body.
                                ov.title = updated.name.clone();
                            }
                        }
                    }
                }
                Overlay::ImagePreview(ov) => {
                    let prev_cur = ov.current().cloned();
                    let new_images: Vec<DriveItem> = self
                        .sorted_visible_items()
                        .into_iter()
                        .filter(|i| i.is_image())
                        .collect();
                    if new_images.is_empty() {
                        ov.items.clear();
                        ov.index = 0;
                        ov.error = Some("No images remaining in this folder".to_string());
                    } else {
                        if let Some(ref prev) = prev_cur {
                            if let Some(pos) = new_images.iter().position(|i| i.id == prev.id) {
                                let updated = &new_images[pos];
                                let changed = updated.content_revision() != prev.content_revision();
                                ov.index = pos;
                                if changed {
                                    let fit = ov.fit_box(ui.term_width, ui.term_height);
                                    let key = self.modal_image_cache_key(
                                        updated,
                                        fit.view_cols,
                                        fit.view_rows,
                                        fit.zoom_pct,
                                        ui.graphics_caps,
                                    );
                                    image_refresh = Some((updated.clone(), key, fit));
                                }
                            } else {
                                ov.index = ov.index.min(new_images.len() - 1);
                                let next_img = &new_images[ov.index];
                                let fit = ov.fit_box(ui.term_width, ui.term_height);
                                let key = self.modal_image_cache_key(
                                    next_img,
                                    fit.view_cols,
                                    fit.view_rows,
                                    fit.zoom_pct,
                                    ui.graphics_caps,
                                );
                                image_refresh = Some((next_img.clone(), key, fit));
                            }
                        }
                        ov.items = new_images;
                    }
                }
                _ => {}
            }
        }

        if let Some(item) = html_refresh {
            self.spawn_shared_text_fetch(&item, true);
        }

        if let Some((item, key, fit)) = image_refresh {
            let bg_rgb = img_preview::color_to_rgb(ui.theme.bg_elevated, (18, 20, 26));
            self.request_image_decode(&item, key, fit, bg_rgb, ui.graphics_caps);
        }
    }

    pub fn spawn_bootstrap_from_cache(&self) {
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::task::spawn_blocking(move || {
            let sites = storage.list_sites().unwrap_or_default();
            let drives = storage.list_drives(None).unwrap_or_default();
            let my_drive = drives.iter().find(|d| d.is_personal_onedrive()).cloned();
            let pins = storage.list_pins().unwrap_or_default();
            let _ = tx.send(UiMsg::BootstrapLoaded {
                my_drive,
                sites,
                drives,
                pins,
            });
        });
    }

    pub fn load_active_folder(&mut self, ui: &mut UiState) {
        self.load_folder(ui, false);
    }

    pub(super) fn load_folder(&mut self, ui: &mut UiState, reuse_startup_root: bool) {
        let tx = self.ui_tx.clone();
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let vseq = self.bump_virtual_list_seq();
        self.loading_folder = true;

        // Truncation belongs to one folder. Moving to another folder or to a virtual view
        // starts clean; a refresh of the same folder keeps what the last listing said.
        let target = self.active_folder_target();
        if target != self.listed_folder {
            self.listed_folder = target;
            self.current_items_truncated = false;
            self.folder_item_limit = FOLDER_PAGE_SIZE;
        }

        match &self.active_source {
            ActiveViewSource::SearchResult { .. } => {
                self.loading_folder = false;
                return;
            }
            ActiveViewSource::Recent => {
                self.sync_ctrl.set_focus(FocusTarget {
                    active_drive_id: None,
                    active_parent_id: None,
                    active_site_id: None,
                    view_kind: FocusViewKind::Recent,
                    active_limit: None,
                });
                tokio::spawn(async move {
                    match graph.fetch_virtual_list(VirtualListKind::Recent, 50).await {
                        Ok(fetch) => {
                            if fetch.status.is_available() {
                                let _ = storage.replace_recent_items(&fetch.items);
                            }
                            let _ = tx.send(UiMsg::VirtualListLoaded {
                                seq: vseq,
                                kind: VirtualListKind::Recent,
                                items: fetch.items,
                                status: fetch.status,
                            });
                        }
                        Err(err) => {
                            let _ = tx.send(UiMsg::TaskError {
                                label: "Recent".into(),
                                error: format!("{err:#}"),
                            });
                        }
                    }
                });
                return;
            }
            ActiveViewSource::SharedWithMe => {
                self.sync_ctrl.set_focus(FocusTarget {
                    active_drive_id: None,
                    active_parent_id: None,
                    active_site_id: None,
                    view_kind: FocusViewKind::SharedWithMe,
                    active_limit: None,
                });
                tokio::spawn(async move {
                    match graph.fetch_virtual_list(VirtualListKind::SharedWithMe, 50).await {
                        Ok(fetch) => {
                            if fetch.status.is_available() {
                                let _ = storage.replace_shared_items(&fetch.items);
                            }
                            let _ = tx.send(UiMsg::VirtualListLoaded {
                                seq: vseq,
                                kind: VirtualListKind::SharedWithMe,
                                items: fetch.items,
                                status: fetch.status,
                            });
                        }
                        Err(err) => {
                            let _ = tx.send(UiMsg::TaskError {
                                label: "Shared".into(),
                                error: format!("{err:#}"),
                            });
                        }
                    }
                });
                return;
            }
            _ => {}
        }

        let Some(drive_id) = self.active_drive_id() else {
            self.loading_folder = false;
            return;
        };
        let parent_id = self.active_parent_id();
        self.sync_ctrl.set_focus(FocusTarget {
            active_drive_id: Some(drive_id.clone()),
            active_parent_id: parent_id.clone(),
            active_site_id: self.active_site_id(),
            view_kind: FocusViewKind::Folder,
            active_limit: self.requested_folder_limit(),
        });

        // Show cached children immediately, then refresh from Graph.
        // Also clear stale in-memory rows when the cached folder was just emptied (e.g., last item deleted).
        if let Ok((cached, cache_truncated)) = self.storage.list_folder_children_page(
            &drive_id,
            parent_id.as_deref(),
            self.folder_item_limit,
        ) {
            if !cached.is_empty() || !self.current_items.is_empty() {
                self.current_items = cached;
                // The cache can prove there is more, never that there is not: a folder whose
                // last listing stopped early has exactly one page cached.
                self.current_items_truncated |= cache_truncated;
                self.reconcile_selection_after_items_update(ui);
            }
        }

        self.sync_ctrl.request(crate::sync::SyncCommand::SyncFolder {
            drive_id,
            parent_id,
            reuse_startup_root,
            limit: self.requested_folder_limit(),
        });
    }

    pub fn ensure_inspector_loaded(&mut self, ui: &UiState) {
        let item = if let Some(selected) = self.selected_item_ref() {
            selected.clone()
        } else if let Some(fallback) = self.active_item() {
            self.active_selected_id = Some(fallback.id.clone());
            fallback
        } else {
            return;
        };
        if item.is_folder {
            return;
        }
        let item_id = item.id.clone();
        let rev_key = Self::item_revision_key(&item);

        // Fetch version history lazily (keyed by drive + item + revision)
        if !self.item_versions.contains_key(&rev_key) {
            let vkey = format!("ver:{rev_key}");
            if self.inflight_previews.insert(vkey) {
                let graph = self.graph.clone();
                let tx = self.ui_tx.clone();
                let did = item.effective_drive_id().to_string();
                let iid = item.effective_item_id().to_string();
                let id_for_msg = item_id.clone();
                let cache_key = rev_key.clone();
                tokio::spawn(async move {
                    match graph.list_item_versions(&did, &iid).await {
                        Ok(versions) => {
                            let _ = tx.send(UiMsg::VersionsLoaded {
                                cache_key,
                                item_id: id_for_msg,
                                versions,
                            });
                        }
                        Err(_) => {
                            let _ = tx.send(UiMsg::VersionsFailed { cache_key });
                        }
                    }
                });
            }
        }

        if item.is_text_previewable() && self.text_preview_for(&item).is_none() {
            self.spawn_shared_text_fetch(&item, false);
        } else if item.is_image() && ui.inline_images {
            let cache_key = self.inline_image_cache_key(&item);
            if !self.rendered_images.contains_key(&cache_key) {
                let cols = ui.inspector_scroll.area.width.saturating_sub(4).clamp(24, 72);
                let rows = ui.inspector_scroll.area.height.saturating_sub(12).clamp(8, 22);
                let bg_rgb = img_preview::color_to_rgb(
                    ui.theme.bg_canvas,
                    img_preview::color_to_rgb(ui.theme.bg_elevated, (18, 20, 26)),
                );
                self.request_image_decode(
                    &item,
                    cache_key,
                    FitBox::fixed(cols, rows),
                    bg_rgb,
                    ui.graphics_caps,
                );
            }
        }
    }

    pub fn request_image_decode(
        &mut self,
        item: &DriveItem,
        cache_key: String,
        fit: FitBox,
        bg_rgb: (u8, u8, u8),
        caps: TerminalGraphicsCaps,
    ) {
        if self.rendered_images.contains_key(&cache_key)
            || !self.inflight_previews.insert(cache_key.clone())
        {
            return;
        }

        let decode = ImageDecodeRequest {
            cache_key,
            fit,
            bg_rgb,
            caps,
        };
        let raw_cache_key = Self::item_revision_key(item);
        if let Some((_, bytes)) = self.raw_image_bytes.iter().find(|(key, _)| key == &raw_cache_key) {
            let kind = sniff_content_kind(item.mime_type.as_deref(), Some(&item.name), Some(bytes));
            self.spawn_image_decodes(item.clone(), Arc::clone(bytes), kind, vec![decode]);
            return;
        }

        if let Some(pending) = self.inflight_raw_images.get_mut(&raw_cache_key) {
            pending.decodes.push(decode);
            return;
        }
        self.inflight_raw_images.insert(
            raw_cache_key.clone(),
            PendingImageDownload {
                item: item.clone(),
                decodes: vec![decode],
            },
        );

        let tx = self.ui_tx.clone();
        let graph = self.graph.clone();
        let did = item.effective_drive_id().to_string();
        let iid = item.effective_item_id().to_string();
        let dl = item.download_url.clone();
        tokio::spawn(async move {
            let result = graph
                .download_item_bytes(&did, &iid, dl.as_deref(), 16 * 1024 * 1024)
                .await
                .map(Arc::from)
                .map_err(|err| format!("{err:#}"));
            let _ = tx.send(UiMsg::ImageBytesFetched {
                cache_key: raw_cache_key,
                result,
            });
        });
    }


    pub(super) fn spawn_image_decodes(
        &self,
        item: DriveItem,
        bytes: Arc<[u8]>,
        kind: ContentKind,
        decodes: Vec<ImageDecodeRequest>,
    ) {
        let tx = self.ui_tx.clone();
        tokio::task::spawn_blocking(move || {
            let drive_id = item.effective_drive_id().to_string();
            if kind == ContentKind::Html {
                let _ = tx.send(UiMsg::HtmlContentSniffed {
                    drive_id,
                    item_id: item.id,
                    title: item.name,
                    html: String::from_utf8_lossy(&bytes).into_owned(),
                    cache_keys: decodes.into_iter().map(|decode| decode.cache_key).collect(),
                });
                return;
            }
            for ImageDecodeRequest {
                cache_key,
                fit,
                bg_rgb,
                caps,
            } in decodes
            {
                let result = match kind {
                    ContentKind::RasterImage | ContentKind::Svg => {
                        img_preview::decode_and_render_for_protocol(&bytes, fit, bg_rgb, caps)
                            .map_err(|err| format!("{err:#}"))
                    }
                    _ => Err(format!("Unsupported image response ({kind:?}); refusing raster decode")),
                };
                let msg = match result {
                    Ok(rendered) => UiMsg::ImageDecoded { cache_key, rendered },
                    Err(error) => UiMsg::PreviewFailed {
                        drive_id: drive_id.clone(),
                        item_id: item.id.clone(),
                        revision: None,
                        inflight_key: Some(cache_key),
                        error,
                    },
                };
                let _ = tx.send(msg);
            }
        });
    }

    pub fn spawn_upload_clipboard_png(&self) {
        let Some(drive_id) = self.active_drive_id() else {
            let _ = self.ui_tx.send(UiMsg::Toast {
                message: "Select a OneDrive or SharePoint folder first".into(),
                kind: ToastKind::Warning,
            });
            return;
        };
        let parent_id = self.active_parent_id();
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();

        tokio::spawn(async move {
            let png_res = tokio::task::spawn_blocking(read_clipboard_png_bytes).await;
            let bytes = match png_res {
                Ok(Ok(b)) => b,
                Ok(Err(err)) => {
                    let _ = tx.send(UiMsg::Toast {
                        message: format!("Clipboard PNG paste failed: {err:#}"),
                        kind: ToastKind::Warning,
                    });
                    return;
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskError {
                        label: "Clipboard PNG".into(),
                        error: err.to_string(),
                    });
                    return;
                }
            };

            let unix_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let file_name = format!("pasted-image-{unix_ms}.png");

            let local_preview = (bytes.len() <= img_preview::MAX_IMAGE_CACHE_BYTES)
                .then(|| LocalUploadPreview::Image(Arc::from(bytes.as_slice())));

            match graph
                .upload_bytes(
                    &drive_id,
                    parent_id.as_deref(),
                    &file_name,
                    bytes,
                    Some("image/png"),
                )
                .await
            {
                Ok(item) => {
                    let _ = storage.upsert_drive_items(std::slice::from_ref(&item));
                    let _ = tx.send(UiMsg::ItemUploaded {
                        item,
                        parent_id,
                        local_preview,
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskError {
                        label: "Upload PNG".into(),
                        error: format!("{err:#}"),
                    });
                }
            }
        });
    }

}
