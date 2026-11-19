//! Rendered screen region -> clickable target mapping (`HitMap`).
//!
//! Rebuilt every frame; later registrations (drawn on top) win on overlap.

use ratatui::layout::{Position, Rect};

/// Toolbar, header, and modal button identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonId {
    Search,
    CommandPalette,
    DensityToggle,
    SortToggle,
    Help,
    Settings,
    Refresh,
    UploadFile,
    NewFolder,
    DownloadItem,
    ShareItem,
    RenameItem,
    DeleteItem,
    ImagePreview,
    LinksModal,
    OpenInBrowser,
    // Narrow responsive bar pills
    NarrowSidebar,
    NarrowFiles,
    NarrowInspector,
    // Confirm dialog
    ConfirmOk,
    ConfirmCancel,
}

/// Clickable targets on the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitTarget {
    /// A row in the left sidebar (`nav_pane`).
    NavRow(usize),
    /// A row in the middle pane file/folder list.
    ListItem(usize),
    /// Breadcrumb parent navigation button (`..` / Backspace).
    BreadcrumbUp,
    /// Clickable `[Open in Browser]` link in Inspector pane.
    WebLinkUrl,
    /// Clickable `[HD Preview]` chip in Inspector pane.
    PreviewImageChip,
    /// Header / toolbar / status / modal button.
    Button(ButtonId),
    /// Modal / overlay outer frame (prevents clicks inside modal padding from closing the modal).
    OverlayBody,
    /// Item inside the top overlay.
    OverlayItem { index: usize },
}

#[derive(Debug, Clone, Default)]
pub struct HitMap {
    regions: Vec<(Rect, HitTarget)>,
}

impl HitMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.regions.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }

    pub fn len(&self) -> usize {
        self.regions.len()
    }

    /// Registers a clickable rectangle. Zero-sized areas are ignored.
    pub fn add(&mut self, area: Rect, target: HitTarget) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        self.regions.push((area, target));
    }

    /// Registers a single-row clickable span.
    pub fn add_line(&mut self, x: u16, row: u16, width: u16, target: HitTarget) {
        self.add(Rect::new(x, row, width, 1), target);
    }

    /// Resolves `(row, col)` to the top-most target.
    pub fn hit(&self, row: u16, col: u16) -> Option<HitTarget> {
        let position = Position::new(col, row);
        self.regions
            .iter()
            .rev()
            .find(|(area, _)| area.contains(position))
            .map(|(_, target)| *target)
    }

    pub fn region_of(&self, target: HitTarget) -> Option<Rect> {
        self.regions
            .iter()
            .rev()
            .find(|(_, item)| *item == target)
            .map(|(area, _)| *area)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_inside_and_overlap_rules() {
        let mut map = HitMap::new();
        map.add(Rect::new(0, 0, 20, 4), HitTarget::ListItem(1));
        map.add_line(2, 2, 8, HitTarget::Button(ButtonId::DownloadItem));
        assert_eq!(map.hit(2, 3), Some(HitTarget::Button(ButtonId::DownloadItem)));
        assert_eq!(map.hit(0, 0), Some(HitTarget::ListItem(1)));
        assert_eq!(map.hit(10, 10), None);
    }
}
