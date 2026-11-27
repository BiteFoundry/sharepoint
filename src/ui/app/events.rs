//! Background `CoreEvent` application onto `AppState` and `UiState`.

use crate::config::Persona;
use crate::models::{CoreEvent, VirtualListKind, POLL_RETRY_SUMMARY};

use super::super::toast::ToastKind;
use super::state::{ActiveViewSource, AppState, UiState};

impl AppState {
    pub fn apply_core_event(&mut self, ev: CoreEvent, ui: &mut UiState) {
        match ev {
            CoreEvent::ProfileLoaded(me) => {
                self.me = Some(me);
            }
            CoreEvent::MyDriveLoaded(drive) => {
                let first_load = self.my_drive.is_none();
                self.my_drive = Some(drive);
                if first_load && matches!(self.active_source, ActiveViewSource::OneDriveRoot) {
                    self.load_folder(ui, self.folder_stack.is_empty());
                }
            }
            CoreEvent::SitesUpdated(sites) => {
                self.sites = sites;
                // Allow initial SharePoint startup library selection at most once, before user navigation.
                if self.persona == Persona::SharePoint
                    && !self.initial_persona_library_selected
                    && matches!(self.active_source, ActiveViewSource::OneDriveRoot)
                    && self.folder_stack.is_empty()
                {
                    if let Some(first_site) = self.sites.first() {
                        if let Some(drives) = self.site_drives.get(&first_site.id) {
                            if let Some(d) = drives.first() {
                                self.initial_persona_library_selected = true;
                                self.active_source = ActiveViewSource::Drive {
                                    drive_id: d.id.clone(),
                                    label: d.display_label(),
                                };
                                self.folder_stack.clear();
                                self.navigate_to_active_folder(ui);
                            }
                        }
                    }
                }
            }
            CoreEvent::DrivesUpdated { site_id, drives } => {
                let should_boot_sp = self.persona == Persona::SharePoint
                    && !self.initial_persona_library_selected
                    && matches!(self.active_source, ActiveViewSource::OneDriveRoot)
                    && self.folder_stack.is_empty()
                    && !drives.is_empty();
                self.site_drives.insert(site_id, drives.clone());
                if should_boot_sp {
                    self.initial_persona_library_selected = true;
                    let d = &drives[0];
                    self.active_source = ActiveViewSource::Drive {
                        drive_id: d.id.clone(),
                        label: d.display_label(),
                    };
                    self.folder_stack.clear();
                    self.navigate_to_active_folder(ui);
                }
            }
            CoreEvent::FolderItemsUpdated {
                drive_id,
                parent_id,
                items,
                truncated,
            } => {
                if self.active_folder_target() == Some((drive_id, parent_id)) {
                    // Guard against an older smaller-limit background poll truncating a
                    // folder whose user-requested page limit was already raised via `LoadMoreItems`.
                    if truncated && items.len() < self.folder_item_limit {
                        return;
                    }
                    self.current_items = items;
                    self.current_items_truncated = truncated;
                    self.loading_folder = false;
                    self.reconcile_selection_after_items_update(ui);
                    self.refresh_open_overlays_for_items(ui);
                    self.ensure_inspector_loaded(ui);
                }
            }
            CoreEvent::RecentItemsUpdated(items) => {
                if matches!(self.active_source, ActiveViewSource::Recent) {
                    self.current_items = items;
                    self.loading_folder = false;
                    self.reconcile_selection_after_items_update(ui);
                    self.refresh_open_overlays_for_items(ui);
                    self.ensure_inspector_loaded(ui);
                }
            }
            CoreEvent::SharedItemsUpdated(items) => {
                if matches!(self.active_source, ActiveViewSource::SharedWithMe) {
                    self.current_items = items;
                    self.loading_folder = false;
                    self.reconcile_selection_after_items_update(ui);
                    self.refresh_open_overlays_for_items(ui);
                    self.ensure_inspector_loaded(ui);
                }
            }
            CoreEvent::VirtualListStatusUpdated { kind, status } => {
                let unavailable = !status.is_available();
                match kind {
                    VirtualListKind::Recent => {
                        self.recent_status = Some(status);
                        if unavailable && matches!(self.active_source, ActiveViewSource::Recent) {
                            self.current_items.clear();
                            self.active_selected_id = None;
                            ui.list_scroll.selected_idx = None;
                            self.loading_folder = false;
                            self.clear_inspector_preview(ui);
                        }
                    }
                    VirtualListKind::SharedWithMe => {
                        self.shared_status = Some(status);
                        if unavailable && matches!(self.active_source, ActiveViewSource::SharedWithMe) {
                            self.current_items.clear();
                            self.active_selected_id = None;
                            ui.list_scroll.selected_idx = None;
                            self.loading_folder = false;
                            self.clear_inspector_preview(ui);
                        }
                    }
                }
            }
            CoreEvent::SyncStateChanged { busy, summary } => {
                let was_busy = self.sync_busy;
                self.sync_busy = busy;
                if was_busy && !busy {
                    self.loading_folder = false;
                    self.ensure_inspector_loaded(ui);
                }
                if summary != POLL_RETRY_SUMMARY {
                    self.sync_summary = summary;
                }
            }
            CoreEvent::SyncError(err) => {
                self.sync_busy = false;
                self.loading_folder = false;
                self.ensure_inspector_loaded(ui);
                ui.toast(format!("Sync error: {err}"), ToastKind::Error);
            }
        }
    }

}
