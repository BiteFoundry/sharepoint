//! Local SQLite WAL persistence + 4-reader pool + FTS5 (`trigram` + `LIKE` hybrid) search repository.

pub mod db;
pub mod drives;
pub mod search;

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::Result;

use crate::{
    graph::drives::DriveDeltaChange,
    models::{
        DownloadBaseline, DriveInfo, DriveItem, DriveSearchHit, PinnedTarget, SharePointSite,
        SyncCursor,
    },
};

const MAX_READER_POOL_SIZE: usize = 4;

#[derive(Clone)]
pub struct Storage {
    path: PathBuf,
    writer: Arc<Mutex<rusqlite::Connection>>,
    readers: Option<Arc<Mutex<Vec<rusqlite::Connection>>>>,
    trigram_enabled: bool,
}

enum ReadConnInner<'a> {
    Pooled {
        conn: Option<rusqlite::Connection>,
        pool: &'a Arc<Mutex<Vec<rusqlite::Connection>>>,
    },
    Writer(std::sync::MutexGuard<'a, rusqlite::Connection>),
}

struct ReadConnGuard<'a>(ReadConnInner<'a>);

impl std::ops::Deref for ReadConnGuard<'_> {
    type Target = rusqlite::Connection;

    fn deref(&self) -> &Self::Target {
        match &self.0 {
            ReadConnInner::Pooled { conn, .. } => conn
                .as_ref()
                .expect("pooled reader connection available until Drop"),
            ReadConnInner::Writer(guard) => guard,
        }
    }
}

impl Drop for ReadConnGuard<'_> {
    fn drop(&mut self) {
        if let ReadConnInner::Pooled { conn, pool } = &mut self.0 {
            if let Some(c) = conn.take() {
                if let Ok(mut guard) = pool.lock() {
                    if guard.len() < MAX_READER_POOL_SIZE {
                        guard.push(c);
                    }
                }
            }
        }
    }
}

impl Storage {
    pub fn open(path: &Path) -> Result<Self> {
        crate::config::ensure_parent(path)?;
        let conn = rusqlite::Connection::open(path)?;
        let trigram_enabled = db::init_connection(&conn, false)?;
        Ok(Self {
            path: path.to_path_buf(),
            writer: Arc::new(Mutex::new(conn)),
            readers: Some(Arc::new(Mutex::new(Vec::with_capacity(
                MAX_READER_POOL_SIZE,
            )))),
            trigram_enabled,
        })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = rusqlite::Connection::open_in_memory()?;
        let trigram_enabled = db::init_connection(&conn, true)?;
        Ok(Self {
            path: PathBuf::from(":memory:"),
            writer: Arc::new(Mutex::new(conn)),
            readers: None,
            trigram_enabled,
        })
    }

    pub fn open_readonly(path: &Path) -> Result<Self> {
        let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = rusqlite::Connection::open_with_flags(path, flags)?;
        db::init_reader_connection(&conn)?;
        let trigram_enabled: bool = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'drive_items_fts'",
                [],
                |row| row.get::<_, String>(0),
            )
            .map(|s| s.to_ascii_lowercase().contains("trigram"))
            .unwrap_or(false);
        Ok(Self {
            path: path.to_path_buf(),
            writer: Arc::new(Mutex::new(conn)),
            readers: None,
            trigram_enabled,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn conn(&self) -> &Arc<Mutex<rusqlite::Connection>> {
        &self.writer
    }

    pub fn trigram_enabled(&self) -> bool {
        self.trigram_enabled
    }

    pub fn shrink_memory(&self) {
        if let Ok(conn) = self.writer.lock() {
            let _ = conn.execute_batch("PRAGMA optimize;");
            db::shrink_connection_memory(&conn);
        }
        if let Some(pool) = &self.readers {
            if let Ok(guard) = pool.lock() {
                for c in guard.iter() {
                    db::shrink_connection_memory(c);
                }
            }
        }
    }

    fn lock_conn(&self) -> Result<std::sync::MutexGuard<'_, rusqlite::Connection>> {
        self.writer
            .lock()
            .map_err(|_| anyhow::anyhow!("Failed to acquire SQLite connection lock (poisoned mutex)"))
    }

    fn read_conn(&self) -> Result<ReadConnGuard<'_>> {
        if let Some(pool) = &self.readers {
            let existing = {
                let mut guard = pool.lock().map_err(|_| {
                    anyhow::anyhow!("Failed to acquire SQLite reader pool lock (poisoned mutex)")
                })?;
                guard.pop()
            };
            let conn = match existing {
                Some(c) => c,
                None => {
                    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                        | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
                    let c = rusqlite::Connection::open_with_flags(&self.path, flags)?;
                    db::init_reader_connection(&c)?;
                    c
                }
            };
            Ok(ReadConnGuard(ReadConnInner::Pooled {
                conn: Some(conn),
                pool,
            }))
        } else {
            Ok(ReadConnGuard(ReadConnInner::Writer(self.lock_conn()?)))
        }
    }

    // --- Sites ---

    pub fn upsert_sites(&self, sites: &[SharePointSite]) -> Result<()> {
        let mut conn = self.lock_conn()?;
        drives::upsert_sites(&mut conn, sites)
    }

    pub fn upsert_sites_with_follow_state(&self, sites: &[SharePointSite]) -> Result<()> {
        let mut conn = self.lock_conn()?;
        drives::upsert_sites_with_follow_state(&mut conn, sites)
    }

    pub fn list_sites(&self) -> Result<Vec<SharePointSite>> {
        let conn = self.read_conn()?;
        drives::list_sites(&conn)
    }

    // --- Drives ---

    pub fn upsert_drives(&self, drives_list: &[DriveInfo]) -> Result<()> {
        let mut conn = self.lock_conn()?;
        drives::upsert_drives(&mut conn, drives_list)
    }

    pub fn list_drives(&self, site_id: Option<&str>) -> Result<Vec<DriveInfo>> {
        let conn = self.read_conn()?;
        drives::list_drives(&conn, site_id)
    }

    // --- Drive Items ---

    pub fn upsert_drive_items(&self, items: &[DriveItem]) -> Result<usize> {
        let mut conn = self.lock_conn()?;
        drives::upsert_drive_items(&mut conn, items)
    }

    pub fn upsert_drive_item_after_move(
        &self,
        item: &DriveItem,
        old_folder_path: Option<&str>,
    ) -> Result<usize> {
        let mut conn = self.lock_conn()?;
        drives::upsert_drive_item_after_move(&mut conn, item, old_folder_path)
    }

    pub fn has_cached_child_for_parent(&self, drive_id: &str, parent_id: &str) -> Result<bool> {
        let conn = self.read_conn()?;
        drives::has_cached_child_for_parent(&conn, drive_id, parent_id)
    }

    pub fn has_nested_drive_items(&self, drive_id: &str) -> Result<bool> {
        let conn = self.read_conn()?;
        drives::has_nested_drive_items(&conn, drive_id)
    }

    pub fn replace_folder_children(
        &self,
        drive_id: &str,
        parent_id: Option<&str>,
        items: &[DriveItem],
    ) -> Result<()> {
        let mut conn = self.lock_conn()?;
        drives::replace_folder_children(&mut conn, drive_id, parent_id, items)
    }

    pub fn replace_folder_children_by_path(
        &self,
        drive_id: &str,
        folder_path: &str,
        items: &[DriveItem],
    ) -> Result<()> {
        let mut conn = self.lock_conn()?;
        drives::replace_folder_children_by_path(&mut conn, drive_id, folder_path, items)
    }

    pub fn get_drive_item_by_path(
        &self,
        drive_id: &str,
        path: &str,
    ) -> Result<Option<DriveItem>> {
        let conn = self.read_conn()?;
        drives::get_drive_item_by_path(&conn, drive_id, path)
    }

    pub fn list_folder_children(
        &self,
        drive_id: &str,
        parent_id: Option<&str>,
        parent_path: Option<&str>,
        limit: usize,
    ) -> Result<Vec<DriveItem>> {
        let conn = self.read_conn()?;
        drives::list_folder_children(&conn, drive_id, parent_id, parent_path, limit)
    }

    /// Reads up to `limit` cached children and reports whether the cache holds more than that.
    pub fn list_folder_children_page(
        &self,
        drive_id: &str,
        parent_id: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<DriveItem>, bool)> {
        let mut items =
            self.list_folder_children(drive_id, parent_id, None, limit.saturating_add(1))?;
        let truncated = items.len() > limit;
        items.truncate(limit);
        Ok((items, truncated))
    }

    pub fn get_drive_item(&self, drive_id: &str, item_id: &str) -> Result<Option<DriveItem>> {
        let conn = self.read_conn()?;
        drives::get_drive_item(&conn, drive_id, item_id)
    }

    pub fn get_drive_item_by_id_any_drive(&self, item_id: &str) -> Result<Option<DriveItem>> {
        let conn = self.read_conn()?;
        drives::get_drive_item_by_id_any_drive(&conn, item_id)
    }

    pub fn delete_drive_item(&self, drive_id: &str, item_id: &str) -> Result<()> {
        let mut conn = self.lock_conn()?;
        drives::delete_drive_item(&mut conn, drive_id, item_id)
    }

    pub fn search_items(
        &self,
        query: &str,
        drive_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<DriveSearchHit>> {
        let conn = self.read_conn()?;
        search::search_items(&conn, query, drive_id, limit, self.trigram_enabled)
    }

    // --- Recent & Shared Lists ---

    pub fn replace_recent_items(&self, items: &[DriveItem]) -> Result<()> {
        let mut conn = self.lock_conn()?;
        drives::replace_named_item_list(&mut conn, "recent", items)
    }

    pub fn list_recent_items(&self, limit: usize) -> Result<Vec<DriveItem>> {
        let conn = self.read_conn()?;
        drives::list_named_items(&conn, "recent", limit)
    }

    pub fn replace_shared_items(&self, items: &[DriveItem]) -> Result<()> {
        let mut conn = self.lock_conn()?;
        drives::replace_named_item_list(&mut conn, "shared", items)
    }

    pub fn list_shared_items(&self, limit: usize) -> Result<Vec<DriveItem>> {
        let conn = self.read_conn()?;
        drives::list_named_items(&conn, "shared", limit)
    }

    // --- Pins ---

    pub fn upsert_pin(&self, pin: &PinnedTarget) -> Result<()> {
        let conn = self.lock_conn()?;
        drives::upsert_pin(&conn, pin)
    }

    pub fn remove_pin(&self, key: &str) -> Result<()> {
        let conn = self.lock_conn()?;
        drives::remove_pin(&conn, key)
    }

    pub fn list_pins(&self) -> Result<Vec<PinnedTarget>> {
        let conn = self.read_conn()?;
        drives::list_pins(&conn)
    }

    // --- Sync Cursors ---

    pub fn get_sync_cursor(&self, scope_key: &str) -> Result<Option<SyncCursor>> {
        let conn = self.read_conn()?;
        drives::get_sync_cursor(&conn, scope_key)
    }

    pub fn upsert_sync_cursor(&self, cursor: &SyncCursor) -> Result<()> {
        let conn = self.lock_conn()?;
        drives::upsert_sync_cursor(&conn, cursor)
    }

    pub fn delete_sync_cursor(&self, scope_key: &str) -> Result<()> {
        let conn = self.lock_conn()?;
        drives::delete_sync_cursor(&conn, scope_key)
    }

    pub fn apply_delta_page(
        &self,
        drive_id: &str,
        changes: &[DriveDeltaChange],
        known_root_id: Option<&str>,
        commit_cursor: Option<&SyncCursor>,
    ) -> Result<drives::DeltaApplyOutcome> {
        let mut conn = self.lock_conn()?;
        drives::apply_delta_page(&mut conn, drive_id, changes, known_root_id, commit_cursor)
    }

    pub fn replace_drive_items_from_resync(
        &self,
        drive_id: &str,
        changes: &[DriveDeltaChange],
        known_root_id: Option<&str>,
        commit_cursor: &SyncCursor,
    ) -> Result<drives::DeltaApplyOutcome> {
        let mut conn = self.lock_conn()?;
        drives::replace_drive_items_from_resync(
            &mut conn,
            drive_id,
            changes,
            known_root_id,
            commit_cursor,
        )
    }

    // --- Download Baselines ---

    pub fn upsert_download_baseline(&self, baseline: &DownloadBaseline) -> Result<()> {
        let conn = self.lock_conn()?;
        drives::upsert_download_baseline(&conn, baseline)
    }

    pub fn update_download_baseline_if_same_item(&self, baseline: &DownloadBaseline) -> Result<bool> {
        let conn = self.lock_conn()?;
        drives::update_download_baseline_if_same_item(&conn, baseline)
    }

    pub fn get_download_baseline(&self, local_path: &Path) -> Result<Option<DownloadBaseline>> {
        let conn = self.read_conn()?;
        drives::get_download_baseline(&conn, local_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sites_drives_items_fts5_and_korean_short_like_fallback() {
        let storage = Storage::open_in_memory().unwrap();
        storage
            .upsert_sites(&[SharePointSite {
                id: "site-eng".into(),
                name: "engineering".into(),
                display_name: "Engineering Hub".into(),
                description: Some("Core docs".into()),
                web_url: Some("https://contoso.sharepoint.com/sites/eng".into()),
                is_followed: true,
                created_at: None,
                last_modified_at: None,
            }])
            .unwrap();
        assert_eq!(storage.list_sites().unwrap().len(), 1);

        storage
            .upsert_drives(&[DriveInfo {
                id: "drv-1".into(),
                name: "Documents".into(),
                drive_type: "documentLibrary".into(),
                site_id: Some("site-eng".into()),
                site_name: Some("Engineering Hub".into()),
                ..Default::default()
            }])
            .unwrap();
        assert_eq!(storage.list_drives(Some("site-eng")).unwrap().len(), 1);

        let items = vec![
            DriveItem {
                id: "item-1".into(),
                drive_id: "drv-1".into(),
                parent_id: Some("root".into()),
                parent_path: "/Architecture".into(),
                name: "kubernetes-plan.md".into(),
                size: 1024,
                mime_type: Some("text/markdown".into()),
                last_modified_by: Some("Alice Kim".into()),
                ..Default::default()
            },
            DriveItem {
                id: "item-2".into(),
                drive_id: "drv-1".into(),
                parent_id: Some("root".into()),
                parent_path: "/보고서".into(),
                name: "기획서.docx".into(),
                size: 4096,
                last_modified_by: Some("홍길동".into()),
                ..Default::default()
            },
        ];
        storage.upsert_drive_items(&items).unwrap();

        // FTS5 trigram match (>= 3 chars)
        let fts_hits = storage.search_items("kubernetes", Some("drv-1"), 10).unwrap();
        assert_eq!(fts_hits.len(), 1);
        assert_eq!(fts_hits[0].item.id, "item-1");

        // Korean < 3 char LIKE fallback!
        let ko_hits = storage.search_items("기획", Some("drv-1"), 10).unwrap();
        assert_eq!(ko_hits.len(), 1);
        assert_eq!(ko_hits[0].item.id, "item-2");

        // Author search
        let author_hits = storage.search_items("홍길", None, 10).unwrap();
        assert_eq!(author_hits.len(), 1);
        assert_eq!(author_hits[0].item.id, "item-2");

        // Recent & Pins
        storage.replace_recent_items(&items).unwrap();
        assert_eq!(storage.list_recent_items(10).unwrap().len(), 2);

        storage
            .upsert_pin(&PinnedTarget {
                key: "site:site-eng".into(),
                kind: "site".into(),
                drive_id: None,
                item_id: None,
                site_id: Some("site-eng".into()),
                label: "Engineering Hub".into(),
            })
            .unwrap();
        assert_eq!(storage.list_pins().unwrap().len(), 1);

        // Delete removes item and FTS entry cleanly
        storage.delete_drive_item("drv-1", "item-1").unwrap();
        assert!(storage.get_drive_item("drv-1", "item-1").unwrap().is_none());
        assert!(storage.search_items("kubernetes", None, 10).unwrap().is_empty());
    }

    #[test]
    fn move_to_root_path_prune_recursive_folder_delete_and_site_unfollow() {
        let storage = Storage::open_in_memory().unwrap();

        // S1: Move from /Projects to root ("") updates drive_items + FTS5 + offline listing + root prune
        let folder = DriveItem {
            id: "fld-proj".into(),
            drive_id: "drv-me".into(),
            parent_id: Some("root".into()),
            parent_path: "".into(),
            name: "Projects".into(),
            is_folder: true,
            ..Default::default()
        };
        let spec_proj = DriveItem {
            id: "item-spec".into(),
            drive_id: "drv-me".into(),
            parent_id: Some("fld-proj".into()),
            parent_path: "/Projects".into(),
            name: "spec.md".into(),
            size: 12,
            ..Default::default()
        };
        let keep_proj = DriveItem {
            id: "item-keep".into(),
            drive_id: "drv-me".into(),
            parent_id: Some("fld-proj".into()),
            parent_path: "/Projects".into(),
            name: "keep.md".into(),
            size: 8,
            ..Default::default()
        };
        let gone_proj = DriveItem {
            id: "item-gone".into(),
            drive_id: "drv-me".into(),
            parent_id: Some("fld-proj".into()),
            parent_path: "/Projects".into(),
            name: "gone.md".into(),
            size: 8,
            ..Default::default()
        };
        storage
            .upsert_drive_items(&[
                folder.clone(),
                spec_proj.clone(),
                keep_proj.clone(),
                gone_proj.clone(),
            ])
            .unwrap();

        let spec_root = DriveItem {
            parent_id: Some("root".into()),
            parent_path: "".into(),
            ..spec_proj
        };
        storage.upsert_drive_items(&[spec_root]).unwrap();

        let stored_spec = storage.get_drive_item("drv-me", "item-spec").unwrap().unwrap();
        assert_eq!(stored_spec.parent_path, "");
        assert_eq!(stored_spec.display_path(), "/spec.md");

        let root_children: Vec<String> = storage
            .list_folder_children("drv-me", None, Some(""), 50)
            .unwrap()
            .into_iter()
            .map(|i| i.id)
            .collect();
        assert!(root_children.contains(&"item-spec".to_string()));
        let proj_children: Vec<String> = storage
            .list_folder_children("drv-me", None, Some("/Projects"), 50)
            .unwrap()
            .into_iter()
            .map(|i| i.id)
            .collect();
        assert!(!proj_children.contains(&"item-spec".to_string()));

        // Remote delete of root spec.md pruned cleanly by authoritative root replace
        storage
            .replace_folder_children("drv-me", None, std::slice::from_ref(&folder))
            .unwrap();
        assert!(storage.get_drive_item("drv-me", "item-spec").unwrap().is_none());
        assert!(storage
            .search_items("spec.md", Some("drv-me"), 10)
            .unwrap()
            .is_empty());

        // S2: Path-based folder replace prunes gone.md, then deleting fld-proj removes keep.md descendant
        storage
            .replace_folder_children_by_path("drv-me", "/Projects", std::slice::from_ref(&keep_proj))
            .unwrap();
        assert!(storage.get_drive_item("drv-me", "item-gone").unwrap().is_none());
        assert!(storage
            .search_items("gone.md", Some("drv-me"), 10)
            .unwrap()
            .is_empty());
        assert!(storage.get_drive_item("drv-me", "item-keep").unwrap().is_some());

        storage.delete_drive_item("drv-me", "fld-proj").unwrap();
        assert!(storage.get_drive_item("drv-me", "fld-proj").unwrap().is_none());
        assert!(storage.get_drive_item("drv-me", "item-keep").unwrap().is_none());
        assert!(storage
            .search_items("keep.md", Some("drv-me"), 10)
            .unwrap()
            .is_empty());

        // S4: Authoritative follow-state update clears unfollowed site flag
        let site_eng_followed = SharePointSite {
            id: "site-eng".into(),
            name: "engineering".into(),
            display_name: "Engineering Hub".into(),
            is_followed: true,
            ..Default::default()
        };
        let site_mkt = SharePointSite {
            id: "site-mkt".into(),
            name: "marketing".into(),
            display_name: "Marketing Portal".into(),
            is_followed: false,
            ..Default::default()
        };
        storage
            .upsert_sites_with_follow_state(&[site_eng_followed.clone(), site_mkt.clone()])
            .unwrap();
        assert!(storage
            .list_sites()
            .unwrap()
            .into_iter()
            .find(|s| s.id == "site-eng")
            .unwrap()
            .is_followed);

        let site_eng_unfollowed = SharePointSite {
            is_followed: false,
            ..site_eng_followed
        };
        storage
            .upsert_sites_with_follow_state(&[site_eng_unfollowed, site_mkt])
            .unwrap();
        assert!(!storage
            .list_sites()
            .unwrap()
            .into_iter()
            .find(|s| s.id == "site-eng")
            .unwrap()
            .is_followed);
    }
}
