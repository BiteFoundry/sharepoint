//! Adaptive two-lane background synchronization engine for Microsoft SharePoint & OneDrive
//! (Lane 1: Fast Interactive Lane + Lane 2: Quiet Background Poll Lane).

use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use anyhow::Result;
use rand::Rng;
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{mpsc, watch},
    time::{self, MissedTickBehavior},
};

use crate::{
    config::AppConfig,
    graph::GraphClient,
    models::{CoreEvent, DriveInfo, SharePointSite, POLL_RETRY_SUMMARY},
    storage::Storage,
};

const IDLE_THRESHOLD_SECS: u64 = 120;
const DEEP_IDLE_THRESHOLD_SECS: u64 = 600;
const AUTH_EXPIRY_PAUSE_SECS: u64 = 30;
const MAX_ERROR_BACKOFF_MS: u64 = 300_000;

pub(crate) struct SyncSharedState {
    epoch: Instant,
    last_activity_ms: AtomicU64,
    activity_seq: AtomicU64,
    auth_pause_until_ms: AtomicU64,
}

impl SyncSharedState {
    pub(crate) fn new() -> Self {
        Self::with_epoch(Instant::now())
    }

    pub(crate) fn with_epoch(epoch: Instant) -> Self {
        Self {
            epoch,
            last_activity_ms: AtomicU64::new(0),
            activity_seq: AtomicU64::new(0),
            auth_pause_until_ms: AtomicU64::new(0),
        }
    }

    pub(crate) fn record_activity(&self, now: Instant) {
        let ms = now.saturating_duration_since(self.epoch).as_millis() as u64;
        self.last_activity_ms.store(ms, Ordering::Release);
        self.activity_seq.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn idle_snapshot(&self, now: Instant) -> (Duration, u64) {
        let seq = self.activity_seq.load(Ordering::Acquire);
        let last_ms = self.last_activity_ms.load(Ordering::Acquire);
        let now_ms = now.saturating_duration_since(self.epoch).as_millis() as u64;
        (Duration::from_millis(now_ms.saturating_sub(last_ms)), seq)
    }

    pub(crate) fn pause_auth(&self, now: Instant, duration: Duration) {
        let until_ms = (now.saturating_duration_since(self.epoch) + duration).as_millis() as u64;
        self.auth_pause_until_ms
            .store(until_ms.max(1), Ordering::Release);
    }

    pub(crate) fn clear_auth_pause(&self) {
        self.auth_pause_until_ms.store(0, Ordering::Release);
    }

    pub(crate) fn is_auth_paused(&self, now: Instant) -> bool {
        let until_ms = self.auth_pause_until_ms.load(Ordering::Acquire);
        if until_ms == 0 {
            return false;
        }
        let now_ms = now.saturating_duration_since(self.epoch).as_millis() as u64;
        now_ms < until_ms
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyncErrorKind {
    Forbidden403,
    Unauthorized401,
    Transient,
}

fn extract_error_detail_for_status(lower: &str) -> &str {
    if let Some(idx) = lower.find("graph api error (") {
        let rest = &lower[idx..];
        if let Some((_, tail)) = rest.split_once("): ") {
            return tail;
        }
        return rest;
    }
    lower
}

pub(crate) fn classify_sync_error(err: &anyhow::Error) -> SyncErrorKind {
    let lower = format!("{err:#}").to_ascii_lowercase();
    let detail = extract_error_detail_for_status(&lower);

    if detail.contains("403")
        || lower.contains("forbidden")
        || lower.contains("authorization_requestdenied")
        || lower.contains("insufficient")
        || lower.contains("accessdenied")
    {
        return SyncErrorKind::Forbidden403;
    }

    if detail.contains("401")
        || lower.contains("unauthorized")
        || lower.contains("token expired")
    {
        return SyncErrorKind::Unauthorized401;
    }

    SyncErrorKind::Transient
}

#[derive(Debug)]
pub(crate) struct ErrorBackoffState {
    base_ms: u64,
    consecutive_failures: u32,
    next_retry_at: Option<Instant>,
    degraded: bool,
}

impl ErrorBackoffState {
    pub(crate) fn new(base_ms: u64) -> Self {
        Self {
            base_ms: base_ms.max(1),
            consecutive_failures: 0,
            next_retry_at: None,
            degraded: false,
        }
    }

    pub(crate) fn compute_backoff_ms(base_ms: u64, failures: u32, jitter_factor: f64) -> u64 {
        let shift = failures.min(6);
        let raw_ms = base_ms
            .saturating_mul(1u64 << shift)
            .min(MAX_ERROR_BACKOFF_MS);
        let jittered = (raw_ms as f64 * jitter_factor).round() as u64;
        jittered.clamp(1, MAX_ERROR_BACKOFF_MS)
    }

    pub(crate) fn record_transient_failure(&mut self, now: Instant) -> bool {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        let jitter = rand::thread_rng().gen_range(0.8_f64..=1.2_f64);
        let delay_ms = Self::compute_backoff_ms(self.base_ms, self.consecutive_failures, jitter);
        self.next_retry_at = Some(now + Duration::from_millis(delay_ms));
        if !self.degraded {
            self.degraded = true;
            true
        } else {
            false
        }
    }

    pub(crate) fn record_success(&mut self) -> bool {
        self.consecutive_failures = 0;
        self.next_retry_at = None;
        if self.degraded {
            self.degraded = false;
            true
        } else {
            false
        }
    }

    pub(crate) fn is_backed_off(&self, now: Instant) -> bool {
        self.next_retry_at.is_some_and(|until| now < until)
    }
}

pub(crate) fn idle_multiplier(idle_duration: Duration) -> u64 {
    let secs = idle_duration.as_secs();
    if secs >= DEEP_IDLE_THRESHOLD_SECS {
        10
    } else if secs >= IDLE_THRESHOLD_SECS {
        4
    } else {
        1
    }
}

#[derive(Debug, Default)]
pub(crate) struct PeriodicBackoffTracker {
    tick_count: u64,
    last_activity_seq: u64,
}

impl PeriodicBackoffTracker {
    pub(crate) fn on_tick(&mut self, idle_duration: Duration, activity_seq: u64) -> bool {
        if activity_seq != self.last_activity_seq {
            self.last_activity_seq = activity_seq;
            self.tick_count = 0;
        }
        let mult = idle_multiplier(idle_duration);
        if mult <= 1 {
            self.tick_count = 0;
            return true;
        }
        self.tick_count = self.tick_count.saturating_add(1);
        self.tick_count % mult == 0
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FocusTarget {
    pub active_drive_id: Option<String>,
    pub active_parent_id: Option<String>,
    pub active_site_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SyncCommand {
    Bootstrap,
    RefreshAll,
    SyncSites,
    SyncSiteDrives { site_id: String, site_name: Option<String> },
    SyncFolder { drive_id: String, parent_id: Option<String> },
    SyncRecent,
    SyncShared,
    NotifyUserActivity,
    Shutdown,
}

#[derive(Clone)]
pub struct SyncController {
    focus_tx: watch::Sender<FocusTarget>,
    nudge_tx: mpsc::UnboundedSender<SyncCommand>,
    shared: Arc<SyncSharedState>,
    shutdown_tx: Option<watch::Sender<bool>>,
}

impl SyncController {
    pub fn from_channels(
        focus_tx: watch::Sender<FocusTarget>,
        nudge_tx: mpsc::UnboundedSender<SyncCommand>,
    ) -> Self {
        Self {
            focus_tx,
            nudge_tx,
            shared: Arc::new(SyncSharedState::new()),
            shutdown_tx: None,
        }
    }

    pub fn set_focus(&self, target: FocusTarget) {
        self.shared.record_activity(Instant::now());
        let _ = self.focus_tx.send(target);
    }

    pub fn notify_activity(&self) {
        self.shared.record_activity(Instant::now());
        let _ = self.nudge_tx.send(SyncCommand::NotifyUserActivity);
    }

    pub fn request(&self, cmd: SyncCommand) {
        if !matches!(cmd, SyncCommand::Shutdown) {
            self.shared.record_activity(Instant::now());
        } else if let Some(tx) = &self.shutdown_tx {
            let _ = tx.send(true);
        }
        let _ = self.nudge_tx.send(cmd);
    }
}

/// Performs an initial bootstrap of `/me/drive`, `/me/followedSites`, `/sites?search=*`,
/// personal OneDrive root children, recent items, and shared items into SQLite.
pub async fn bootstrap_sharepoint(graph: &GraphClient, storage: &Storage) -> Result<()> {
    if let Ok(drive) = graph.get_my_drive().await {
        let _ = storage.upsert_drives(std::slice::from_ref(&drive));
        if let Ok(children) = graph.list_children(&drive.id, None, Some(200)).await {
            let _ = storage.replace_folder_children(&drive.id, None, &children);
        }
    }

    let mut sites = graph.list_followed_sites().await.unwrap_or_default();
    if let Ok(discovered) = graph.list_sites(Some("*"), 50).await {
        for s in discovered {
            if !sites.iter().any(|existing| existing.id == s.id) {
                sites.push(s);
            }
        }
    }
    if !sites.is_empty() {
        let _ = storage.upsert_sites(&sites);
        for site in sites.iter().take(6) {
            if let Ok(drives) = graph
                .list_site_drives(&site.id, Some(site.effective_name()))
                .await
            {
                let _ = storage.upsert_drives(&drives);
            }
        }
    }

    if let Ok(recent) = graph.list_recent_items(50).await {
        let _ = storage.replace_recent_items(&recent);
    }
    if let Ok(shared) = graph.list_shared_with_me(50).await {
        let _ = storage.replace_shared_items(&shared);
    }

    Ok(())
}

pub fn spawn_sync_engine(
    config: AppConfig,
    graph: GraphClient,
    storage: Storage,
    event_tx: mpsc::UnboundedSender<CoreEvent>,
) -> SyncController {
    let (focus_tx, focus_rx) = watch::channel(FocusTarget::default());
    let (nudge_tx, nudge_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let shared = Arc::new(SyncSharedState::new());

    let ctrl = SyncController {
        focus_tx,
        nudge_tx,
        shared: Arc::clone(&shared),
        shutdown_tx: Some(shutdown_tx),
    };

    // Lane 1: Fast Interactive Lane
    {
        let graph = graph.clone();
        let storage = storage.clone();
        let event_tx = event_tx.clone();
        let shared = Arc::clone(&shared);
        let focus_rx = focus_rx.clone();
        let mut nudge_rx = nudge_rx;
        let mut shutdown_rx = shutdown_rx.clone();
        let active_poll_ms = config.active_poll_ms.clamp(2_000, 120_000);

        tokio::spawn(async move {
            let mut ticker = time::interval(Duration::from_millis(active_poll_ms));
            ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
            let mut backoff = ErrorBackoffState::new(active_poll_ms);
            let mut tracker = PeriodicBackoffTracker::default();

            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    maybe_cmd = nudge_rx.recv() => {
                        let Some(cmd) = maybe_cmd else { break; };
                        match cmd {
                            SyncCommand::Shutdown => break,
                            SyncCommand::NotifyUserActivity => {
                                shared.record_activity(Instant::now());
                            }
                            SyncCommand::Bootstrap | SyncCommand::RefreshAll => {
                                let _ = event_tx.send(CoreEvent::SyncStateChanged {
                                    busy: true,
                                    summary: "Syncing OneDrive & SharePoint...".to_string(),
                                });
                                sync_core_discovery(&graph, &storage, &event_tx, &shared, &mut backoff).await;
                                let focus = focus_rx.borrow().clone();
                                if let Some(did) = focus.active_drive_id {
                                    sync_folder_once(&graph, &storage, &event_tx, &did, focus.active_parent_id.as_deref()).await;
                                }
                                let _ = event_tx.send(CoreEvent::SyncStateChanged {
                                    busy: false,
                                    summary: "Synced".to_string(),
                                });
                            }
                            SyncCommand::SyncSites => {
                                sync_sites_once(&graph, &storage, &event_tx).await;
                            }
                            SyncCommand::SyncSiteDrives { site_id, site_name } => {
                                if let Ok(drives) = graph.list_site_drives(&site_id, site_name.as_deref()).await {
                                    let _ = storage.upsert_drives(&drives);
                                    let _ = event_tx.send(CoreEvent::DrivesUpdated { site_id, drives });
                                }
                            }
                            SyncCommand::SyncFolder { drive_id, parent_id } => {
                                sync_folder_once(&graph, &storage, &event_tx, &drive_id, parent_id.as_deref()).await;
                            }
                            SyncCommand::SyncRecent => {
                                if let Ok(items) = graph.list_recent_items(50).await {
                                    let _ = storage.replace_recent_items(&items);
                                    let _ = event_tx.send(CoreEvent::RecentItemsUpdated(items));
                                }
                            }
                            SyncCommand::SyncShared => {
                                if let Ok(items) = graph.list_shared_with_me(50).await {
                                    let _ = storage.replace_shared_items(&items);
                                    let _ = event_tx.send(CoreEvent::SharedItemsUpdated(items));
                                }
                            }
                        }
                    }
                    _ = ticker.tick() => {
                        let now = Instant::now();
                        if shared.is_auth_paused(now) || backoff.is_backed_off(now) {
                            continue;
                        }
                        let (idle, seq) = shared.idle_snapshot(now);
                        if !tracker.on_tick(idle, seq) {
                            continue;
                        }
                        let focus = focus_rx.borrow().clone();
                        if let Some(did) = focus.active_drive_id {
                            match graph.list_children(&did, focus.active_parent_id.as_deref(), Some(200)).await {
                                Ok(items) => {
                                    let _ = storage.replace_folder_children(&did, focus.active_parent_id.as_deref(), &items);
                                    let _ = event_tx.send(CoreEvent::FolderItemsUpdated {
                                        drive_id: did,
                                        parent_id: focus.active_parent_id,
                                        items,
                                    });
                                    shared.clear_auth_pause();
                                    if backoff.record_success() {
                                        let _ = event_tx.send(CoreEvent::SyncStateChanged {
                                            busy: false,
                                            summary: "Synced".to_string(),
                                        });
                                    }
                                }
                                Err(err) => {
                                    handle_poll_error(&err, &shared, &mut backoff, &event_tx);
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    // Lane 2: Quiet Background Poll Lane
    {
        let bg_ms = config.background_poll_ms.clamp(15_000, 300_000);
        let mut shutdown_rx = shutdown_rx;
        tokio::spawn(async move {
            let mut ticker = time::interval(Duration::from_millis(bg_ms));
            ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
            let mut tracker = PeriodicBackoffTracker::default();
            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    _ = ticker.tick() => {
                        let now = Instant::now();
                        if shared.is_auth_paused(now) {
                            continue;
                        }
                        let (idle, seq) = shared.idle_snapshot(now);
                        if !tracker.on_tick(idle, seq) {
                            continue;
                        }
                        sync_sites_once(&graph, &storage, &event_tx).await;
                    }
                }
            }
        });
    }

    ctrl
}

async fn sync_core_discovery(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    shared: &Arc<SyncSharedState>,
    backoff: &mut ErrorBackoffState,
) {
    if let Ok(me) = graph.get_me().await {
        let _ = event_tx.send(CoreEvent::ProfileLoaded(me));
    }

    match graph.get_my_drive().await {
        Ok(drive) => {
            let _ = storage.upsert_drives(std::slice::from_ref(&drive));
            let _ = event_tx.send(CoreEvent::MyDriveLoaded(drive.clone()));
            sync_folder_once(graph, storage, event_tx, &drive.id, None).await;
            shared.clear_auth_pause();
            backoff.record_success();
        }
        Err(err) => {
            handle_poll_error(&err, shared, backoff, event_tx);
        }
    }

    sync_sites_once(graph, storage, event_tx).await;

    if let Ok(recent) = graph.list_recent_items(50).await {
        let _ = storage.replace_recent_items(&recent);
        let _ = event_tx.send(CoreEvent::RecentItemsUpdated(recent));
    }
    if let Ok(shared_items) = graph.list_shared_with_me(50).await {
        let _ = storage.replace_shared_items(&shared_items);
        let _ = event_tx.send(CoreEvent::SharedItemsUpdated(shared_items));
    }
}

async fn sync_sites_once(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
) {
    let mut combined: Vec<SharePointSite> = graph.list_followed_sites().await.unwrap_or_default();
    if let Ok(discovered) = graph.list_sites(Some("*"), 50).await {
        for site in discovered {
            if !combined.iter().any(|existing| existing.id == site.id) {
                combined.push(site);
            }
        }
    }
    if !combined.is_empty() {
        let _ = storage.upsert_sites(&combined);
        let _ = event_tx.send(CoreEvent::SitesUpdated(combined.clone()));
        for site in combined.iter().take(6) {
            if let Ok(drives) = graph
                .list_site_drives(&site.id, Some(site.effective_name()))
                .await
            {
                let _ = storage.upsert_drives(&drives);
                let _ = event_tx.send(CoreEvent::DrivesUpdated {
                    site_id: site.id.clone(),
                    drives,
                });
            }
        }
    }
}

async fn sync_folder_once(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    drive_id: &str,
    parent_id: Option<&str>,
) {
    if let Ok(items) = graph.list_children(drive_id, parent_id, Some(200)).await {
        let _ = storage.replace_folder_children(drive_id, parent_id, &items);
        let _ = event_tx.send(CoreEvent::FolderItemsUpdated {
            drive_id: drive_id.to_string(),
            parent_id: parent_id.map(str::to_string),
            items,
        });
    }
}

fn handle_poll_error(
    err: &anyhow::Error,
    shared: &Arc<SyncSharedState>,
    backoff: &mut ErrorBackoffState,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
) {
    let now = Instant::now();
    match classify_sync_error(err) {
        SyncErrorKind::Unauthorized401 => {
            shared.pause_auth(now, Duration::from_secs(AUTH_EXPIRY_PAUSE_SECS));
            let _ = event_tx.send(CoreEvent::SyncError(format!("{err:#}")));
        }
        SyncErrorKind::Forbidden403 => {
            let _ = event_tx.send(CoreEvent::SyncError(format!("{err:#}")));
        }
        SyncErrorKind::Transient => {
            if backoff.record_transient_failure(now) {
                let _ = event_tx.send(CoreEvent::SyncStateChanged {
                    busy: false,
                    summary: POLL_RETRY_SUMMARY.to_string(),
                });
            }
        }
    }
}

/// Helper to combine personal drive and site drives for callers.
pub fn sort_drives_for_persona(drives: &mut [DriveInfo], onedrive_first: bool) {
    drives.sort_by(|a, b| {
        let a_personal = a.is_personal_onedrive();
        let b_personal = b.is_personal_onedrive();
        if onedrive_first {
            b_personal
                .cmp(&a_personal)
                .then_with(|| a.display_label().cmp(&b.display_label()))
        } else {
            a_personal
                .cmp(&b_personal)
                .then_with(|| a.display_label().cmp(&b.display_label()))
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backoff_and_idle_multiplier() {
        assert_eq!(idle_multiplier(Duration::from_secs(10)), 1);
        assert_eq!(idle_multiplier(Duration::from_secs(150)), 4);
        assert_eq!(idle_multiplier(Duration::from_secs(700)), 10);

        let mut b = ErrorBackoffState::new(1000);
        let now = Instant::now();
        assert!(b.record_transient_failure(now));
        assert!(!b.record_transient_failure(now));
        assert!(b.is_backed_off(now));
        assert!(b.record_success());
        assert!(!b.is_backed_off(now));
    }
}
