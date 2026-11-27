//! Active view routing, breadcrumbs, sorting, pagination, and selection reconciliation.

use crate::config::Persona;
use crate::models::{DriveInfo, DriveItem, VirtualListKind, VirtualListStatus};
use crate::sync::{FOLDER_PAGE_SIZE, MAX_FOLDER_ITEMS};

use super::state::{ActiveViewSource, AppState, LoadMoreOutcome, SortMode, UiState};

impl AppState {
    pub fn active_drive_id(&self) -> Option<String> {
        match &self.active_source {
            ActiveViewSource::OneDriveRoot => self.my_drive.as_ref().map(|d| d.id.clone()),
            ActiveViewSource::Drive { drive_id, .. }
            | ActiveViewSource::SearchResult { drive_id, .. } => Some(drive_id.clone()),
            ActiveViewSource::Recent | ActiveViewSource::SharedWithMe => None,
        }
    }

    /// Returns the SharePoint `site_id` owning the currently active library, if any.
    pub fn active_site_id(&self) -> Option<String> {
        let drive_id = self.active_drive_id()?;
        for (site_id, drives) in &self.site_drives {
            if drives.iter().any(|d| d.id == drive_id) {
                return Some(site_id.clone());
            }
        }
        None
    }

    /// Returns `Some((drive_id, parent_id))` only when viewing a real folder (`OneDriveRoot` or `Drive`),
    /// never for virtual views (`Recent` / `SharedWithMe`) or synthetic `SearchResult` views.
    pub fn active_folder_target(&self) -> Option<(String, Option<String>)> {
        match &self.active_source {
            ActiveViewSource::OneDriveRoot | ActiveViewSource::Drive { .. } => {
                let drive_id = self.active_drive_id()?;
                Some((drive_id, self.active_parent_id()))
            }
            ActiveViewSource::Recent
            | ActiveViewSource::SharedWithMe
            | ActiveViewSource::SearchResult { .. } => None,
        }
    }

    pub fn virtual_list_status_for_active(&self) -> Option<(VirtualListKind, Option<&VirtualListStatus>)> {
        match &self.active_source {
            ActiveViewSource::Recent => Some((VirtualListKind::Recent, self.recent_status.as_ref())),
            ActiveViewSource::SharedWithMe => {
                Some((VirtualListKind::SharedWithMe, self.shared_status.as_ref()))
            }
            _ => None,
        }
    }

    pub fn active_parent_id(&self) -> Option<String> {
        self.folder_stack.last().map(|(id, _)| id.clone())
    }

    /// True for the user's own OneDrive and for the libraries loaded for the sidebar's sites.
    pub fn is_known_drive(&self, drive_id: &str) -> bool {
        self.my_drive.as_ref().is_some_and(|d| d.id == drive_id)
            || self.site_drives.values().flatten().any(|d| d.id == drive_id)
    }

    /// Display name of a drive the sidebar knows: `My Files` for the user's OneDrive, the
    /// library label for a SharePoint document library.
    pub fn known_drive_label(&self, drive_id: &str) -> Option<String> {
        if self.my_drive.as_ref().is_some_and(|d| d.id == drive_id) {
            return Some("My Files".to_string());
        }
        self.site_drives
            .values()
            .flatten()
            .find(|d| d.id == drive_id)
            .map(DriveInfo::display_label)
    }

    /// Breadcrumb product prefix for a drive: `OneDrive` for personal drives, `SharePoint`
    /// for site libraries. A drive outside the sidebar falls back to the running persona.
    fn drive_family_label(&self, drive_id: &str) -> &'static str {
        if self.my_drive.as_ref().is_some_and(|d| d.id == drive_id) {
            return "OneDrive";
        }
        if let Some(d) = self.site_drives.values().flatten().find(|d| d.id == drive_id) {
            return if d.is_personal_onedrive() { "OneDrive" } else { "SharePoint" };
        }
        match self.persona {
            Persona::OneDrive => "OneDrive",
            Persona::SharePoint => "SharePoint",
        }
    }

    /// The view to come back to after opening a folder that lives in another drive: the
    /// current view, or, when the current view is itself such a hop, the view that hop started
    /// from (a search hit has no listing of its own, so it falls back to My Files).
    pub(crate) fn return_source_for_hop(&mut self) -> Option<ActiveViewSource> {
        match &self.active_source {
            ActiveViewSource::OneDriveRoot
            | ActiveViewSource::Recent
            | ActiveViewSource::SharedWithMe => Some(self.active_source.clone()),
            ActiveViewSource::Drive { .. } => self
                .origin_source
                .take()
                .or_else(|| Some(self.active_source.clone())),
            ActiveViewSource::SearchResult { .. } => self
                .origin_source
                .take()
                .or(Some(ActiveViewSource::OneDriveRoot)),
        }
    }

    pub fn breadcrumb_label(&self) -> String {
        let root = match &self.active_source {
            ActiveViewSource::OneDriveRoot => "OneDrive > My Files".to_string(),
            ActiveViewSource::Recent => "OneDrive > Recent".to_string(),
            ActiveViewSource::SharedWithMe => "OneDrive > Shared with Me".to_string(),
            ActiveViewSource::Drive { drive_id, label } => match &self.origin_source {
                // Opened from a list view: the path reads from that list.
                Some(ActiveViewSource::Recent) => "OneDrive > Recent".to_string(),
                Some(ActiveViewSource::SharedWithMe) => "OneDrive > Shared with Me".to_string(),
                _ => {
                    let family = self.drive_family_label(drive_id);
                    // A drive outside the sidebar is entered through one of its folders, and
                    // its label is that folder's name: the trail already says it.
                    let label_repeats_folder = !self.is_known_drive(drive_id)
                        && self.folder_stack.first().is_some_and(|(_, name)| name == label);
                    if label.trim().is_empty() || label_repeats_folder {
                        family.to_string()
                    } else {
                        format!("{family} > {label}")
                    }
                }
            },
            ActiveViewSource::SearchResult { label, .. } => label.clone(),
        };
        if self.folder_stack.is_empty() {
            root
        } else {
            let trail: Vec<&str> = self.folder_stack.iter().map(|(_, n)| n.as_str()).collect();
            format!("{root} > {}", trail.join(" > "))
        }
    }

    /// Header note for a folder that holds more rows than are loaded, as `(full, short)`.
    /// The short form is for a header too narrow for the full one.
    pub fn truncation_hint(&self) -> Option<(String, String)> {
        if !self.current_items_truncated || self.active_folder_target().is_none() {
            return None;
        }
        let shown = self.current_items.len();
        if self.folder_item_limit >= MAX_FOLDER_ITEMS {
            return Some((
                format!(" · first {shown} shown · rest: {} ls --limit", self.persona.bin_name()),
                format!(" · {shown}+"),
            ));
        }
        Some((
            format!(" · first {shown} shown · L loads more"),
            format!(" · {shown}+ (L)"),
        ))
    }

    /// Limit to send to the sync engine: `None` while the folder is at the default page, so
    /// the frames match what an older daemon expects.
    pub(super) fn requested_folder_limit(&self) -> Option<usize> {
        (self.folder_item_limit != FOLDER_PAGE_SIZE).then_some(self.folder_item_limit)
    }

    /// Asks for one more page of the active folder when the last listing was cut short.
    pub fn load_more_items(&mut self, ui: &mut UiState) -> LoadMoreOutcome {
        if !self.current_items_truncated || self.active_folder_target().is_none() {
            return LoadMoreOutcome::NothingMore;
        }
        if self.folder_item_limit >= MAX_FOLDER_ITEMS {
            return LoadMoreOutcome::AtMaximum(MAX_FOLDER_ITEMS);
        }
        self.folder_item_limit = self
            .folder_item_limit
            .saturating_add(FOLDER_PAGE_SIZE)
            .min(MAX_FOLDER_ITEMS);
        self.load_active_folder(ui);
        LoadMoreOutcome::Requested(self.folder_item_limit)
    }

    pub fn sorted_visible_items(&self) -> Vec<DriveItem> {
        let mut items = self.current_items.clone();
        items.sort_by(|a, b| {
            b.is_folder.cmp(&a.is_folder).then_with(|| match self.sort_mode {
                SortMode::NameAsc => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                SortMode::ModifiedDesc => b.last_modified_at.cmp(&a.last_modified_at),
                SortMode::SizeDesc => b.size.cmp(&a.size),
                SortMode::TypeAsc => a
                    .type_badge()
                    .cmp(b.type_badge())
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())),
            })
        });
        items
    }

    pub(super) fn selected_item_ref(&self) -> Option<&DriveItem> {
        let id = self.active_selected_id.as_deref()?;
        self.current_items.iter().find(|item| item.id == id)
    }

    pub fn active_item(&self) -> Option<DriveItem> {
        if let Some(selected) = self.selected_item_ref() {
            return Some(selected.clone());
        }
        self.sorted_visible_items().into_iter().next()
    }


    pub(crate) fn reconcile_selection_after_items_update(&mut self, ui: &mut UiState) {
        let items = self.sorted_visible_items();
        let prev_idx = ui.list_scroll.selected_idx;
        let selected = if items.is_empty() {
            ui.list_scroll.selected_idx = None;
            self.active_selected_id = None;
            None
        } else if let Some(ref selected_id) = self.active_selected_id {
            if let Some(pos) = items.iter().position(|item| item.id == *selected_id) {
                ui.list_scroll.selected_idx = Some(pos);
                items.get(pos)
            } else {
                let fallback = prev_idx.unwrap_or(0).min(items.len() - 1);
                ui.list_scroll.selected_idx = Some(fallback);
                self.active_selected_id = Some(items[fallback].id.clone());
                ui.inspector_scroll.top();
                items.get(fallback)
            }
        } else {
            let fallback = prev_idx.unwrap_or(0).min(items.len() - 1);
            ui.list_scroll.selected_idx = Some(fallback);
            self.active_selected_id = Some(items[fallback].id.clone());
            ui.inspector_scroll.top();
            items.get(fallback)
        };
        self.sync_inspector_preview_for(selected, ui);
    }

    pub fn navigate_to_active_folder(&mut self, ui: &mut UiState) {
        self.current_items.clear();
        self.active_selected_id = None;
        ui.list_scroll.selected_idx = None;
        ui.inspector_scroll.top();
        self.clear_inspector_preview(ui);
        self.load_active_folder(ui);
    }

}
