//! Adaptive real-time synchronization engine for Microsoft SharePoint & OneDrive:
//! - Direct Socket.IO v4 WebSocket push (`GET /drives/{id}/root/subscriptions/socketIo`) for the active drive
//! - Per-drive `driveItem: delta` catch-up persisted atomically in SQLite `sync_cursors`
//! - Bootstrap gap closure (`?token=latest` -> folder load -> delta catch-up)
//! - HTTP `410 Gone` & overflow resync with root reconciliation
//! - Automatic active-folder delta fallback when Socket.IO is unavailable or blocked by a proxy
//! - Quiet background site/library discovery lane (Lane 2)

use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use anyhow::Result;
use rand::Rng;
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{mpsc, oneshot, watch, Notify},
    time::{self, MissedTickBehavior},
};

use crate::{
    config::AppConfig,
    graph::{
        drives::{
            is_delta_unsupported_error, is_socketio_unsupported_error, DriveDeltaChange,
            DriveDeltaPageResponse,
        },
        socketio::{self, SocketIoSignal},
        GraphClient,
    },
    models::{
        CoreEvent, DriveInfo, DriveItem, SharePointSite, SyncCursor, VirtualListAvailability,
        VirtualListKind,
        POLL_RETRY_SUMMARY,
    },
    storage::Storage,
};

const IDLE_THRESHOLD_SECS: u64 = 120;
const DEEP_IDLE_THRESHOLD_SECS: u64 = 600;
const AUTH_EXPIRY_PAUSE_SECS: u64 = 30;
const MAX_ERROR_BACKOFF_MS: u64 = 300_000;
/// Delta pages applied in one pull. A longer change feed is not abandoned: the pull
/// stores the next page's link as the cursor and the following pull continues there.
const MAX_DELTA_PAGES: usize = 20;
/// Pulls one sync call may chain while the drive is still behind, so a long backlog is
/// worked off quickly without holding the engine on one drive.
const MAX_DELTA_PASSES_PER_SYNC: usize = 3;
/// Pages a 410 resync collects in memory before it replaces the drive's cached rows.
const MAX_RESYNC_PAGES: usize = 200;
/// Folders above the active one that a partial resync re-lists.
const MAX_RESYNC_ANCESTORS: usize = 16;
/// Shown after a resync that could only re-list the folders on the way to the open one.
pub const PARTIAL_RESYNC_SUMMARY: &str = "Resynced the open folder; other folders refresh when opened";
const NOTIFICATION_COALESCE_MS: u64 = 80;
const SOCKET_SAFETY_DELTA_SECS: u64 = 120;
const SOCKET_RECONNECT_BASE_SECS: u64 = 2;
const SOCKET_RECONNECT_MAX_SECS: u64 = 60;
const SOCKET_UNSUPPORTED_RETRY_SECS: u64 = 300;
/// Sites listed for the sidebar besides the followed ones. A bounded bootstrap: sites
/// beyond it are reached through search (`/`) or `sharepoint sites <name>`.
const SIDEBAR_SITE_DISCOVERY_LIMIT: usize = 50;
/// Sites whose libraries are fetched up front. The rest load when their row is opened.
pub const SIDEBAR_DRIVE_PREFETCH_SITES: usize = 12;

/// Sidebar site list: followed sites first, then discovered sites not already present.
/// The order matters: `sites_for_drive_prefetch` takes the head of this list.
fn merge_sidebar_sites(
    followed: Vec<SharePointSite>,
    discovered: Vec<SharePointSite>,
) -> Vec<SharePointSite> {
    let mut sites = followed;
    for site in discovered {
        if !sites.iter().any(|existing| existing.id == site.id) {
            sites.push(site);
        }
    }
    sites
}

/// The sites whose libraries are prefetched: the head of the merged list, i.e. followed
/// sites before discovered ones.
fn sites_for_drive_prefetch(sites: &[SharePointSite]) -> &[SharePointSite] {
    &sites[..sites.len().min(SIDEBAR_DRIVE_PREFETCH_SITES)]
}

/// Sites whose libraries should be refreshed: the first `SIDEBAR_DRIVE_PREFETCH_SITES`
/// plus `active_site_id` if the user is actively viewing a site beyond the initial cap.
fn sites_for_drive_refresh<'a>(
    sites: &'a [SharePointSite],
    active_site_id: Option<&str>,
) -> Vec<&'a SharePointSite> {
    let mut selected: Vec<&'a SharePointSite> = sites_for_drive_prefetch(sites).iter().collect();
    if let Some(active_id) = active_site_id.map(str::trim).filter(|s| !s.is_empty()) {
        if !selected.iter().any(|s| s.id == active_id) {
            if let Some(site) = sites.iter().find(|s| s.id == active_id) {
                selected.push(site);
            }
        }
    }
    selected
}

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

/// Fallback active-folder idle multiplier: keeps an open visible TUI responsive on 10s fallback
/// and caps deep-idle backoff at 3x (30s) instead of stalling to 150s.
fn fallback_idle_multiplier(idle_duration: Duration) -> u64 {
    if idle_duration.as_secs() >= DEEP_IDLE_THRESHOLD_SECS {
        3
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
        self.on_tick_with(idle_duration, activity_seq, idle_multiplier)
    }

    fn on_tick_with(
        &mut self,
        idle_duration: Duration,
        activity_seq: u64,
        mult_fn: fn(Duration) -> u64,
    ) -> bool {
        if activity_seq != self.last_activity_seq {
            self.last_activity_seq = activity_seq;
            self.tick_count = 0;
        }
        let mult = mult_fn(idle_duration);
        if mult <= 1 {
            self.tick_count = 0;
            return true;
        }
        self.tick_count = self.tick_count.saturating_add(1);
        self.tick_count % mult == 0
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FocusViewKind {
    #[default]
    Folder,
    Recent,
    SharedWithMe,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FocusTarget {
    pub active_drive_id: Option<String>,
    pub active_parent_id: Option<String>,
    pub active_site_id: Option<String>,
    #[serde(default)]
    pub view_kind: FocusViewKind,
    /// Rows the client wants for the active folder; `None` means `FOLDER_PAGE_SIZE`.
    #[serde(default)]
    pub active_limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SyncCommand {
    Bootstrap,
    RefreshAll,
    SyncSites,
    SyncSiteDrives { site_id: String, site_name: Option<String> },
    SyncFolder {
        drive_id: String,
        parent_id: Option<String>,
        reuse_startup_root: bool,
        /// Rows to load; `None` means `FOLDER_PAGE_SIZE`.
        #[serde(default)]
        limit: Option<usize>,
    },
    SyncRecent,
    SyncShared,
    FocusGained,
    NotifyUserActivity,
    Shutdown,
}

#[derive(Debug, Clone)]
enum InternalTrigger {
    SocketConnected { drive_id: String },
    SocketNotification { drive_id: String },
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

pub fn drive_cursor_scope_key(drive_id: &str) -> String {
    format!("drive:{drive_id}")
}

fn encode_root_id(root_id: Option<&str>) -> Option<String> {
    root_id
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "root")
        .map(|s| format!("root_id:{s}"))
}

fn decode_root_id(cursor: Option<&SyncCursor>) -> Option<String> {
    cursor
        .and_then(|c| c.history_next_link.as_deref())
        .and_then(|s| s.strip_prefix("root_id:"))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Rows the TUI loads for one folder before the user asks for more.
pub const FOLDER_PAGE_SIZE: usize = 200;

/// Most rows the TUI will hold for one folder. Larger folders are a job for `ls --limit`.
pub const MAX_FOLDER_ITEMS: usize = 5000;

/// Row limit for a folder listing: the requested one within `1..=MAX_FOLDER_ITEMS`,
/// or `FOLDER_PAGE_SIZE` when the peer did not send one.
pub fn effective_folder_limit(requested: Option<usize>) -> usize {
    requested
        .unwrap_or(FOLDER_PAGE_SIZE)
        .clamp(1, MAX_FOLDER_ITEMS)
}

#[derive(Debug, Default)]
pub struct DriveSyncRuntime {
    pub delta_unsupported_drives: HashSet<String>,
    pub virtual_unavailable: HashSet<VirtualListKind>,
    pub last_delta_at: HashMap<String, Instant>,
    /// Folders whose last Graph listing stopped at the row limit, as `(drive_id, parent_id)`.
    /// The cache holds only part of such a folder, so counting cached rows cannot show
    /// that the server has more.
    pub incomplete_folders: HashSet<(String, Option<String>)>,
}

fn folder_key(drive_id: &str, parent_id: Option<&str>) -> (String, Option<String>) {
    let parent = parent_id
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "root")
        .map(str::to_string);
    (drive_id.to_string(), parent)
}

impl DriveSyncRuntime {
    /// Records where a delta pull left the drive. A drive that is still behind loses its
    /// timestamp, so the next tick continues at once instead of waiting out the interval.
    fn note_delta_progress(&mut self, drive_id: &str, progress: DeltaProgress) {
        match progress {
            DeltaProgress::CaughtUp => {
                self.last_delta_at.insert(drive_id.to_string(), Instant::now());
            }
            DeltaProgress::MorePending => {
                self.last_delta_at.remove(drive_id);
            }
        }
    }

    fn note_folder_listing(&mut self, drive_id: &str, parent_id: Option<&str>, complete: bool) {
        let key = folder_key(drive_id, parent_id);
        if complete {
            self.incomplete_folders.remove(&key);
        } else {
            self.incomplete_folders.insert(key);
        }
    }

    fn folder_is_incomplete(&self, drive_id: &str, parent_id: Option<&str>) -> bool {
        self.incomplete_folders.contains(&folder_key(drive_id, parent_id))
    }
}

/// Reads the active folder from the cache for a `FolderItemsUpdated` event.
/// The folder counts as truncated when the cache holds more than `limit` rows or when the
/// last Graph listing of it stopped early.
fn cached_folder_page(
    storage: &Storage,
    rt: &DriveSyncRuntime,
    drive_id: &str,
    parent_id: Option<&str>,
    limit: usize,
) -> Result<(Vec<DriveItem>, bool)> {
    let (items, cut) = storage.list_folder_children_page(drive_id, parent_id, limit)?;
    Ok((items, cut || rt.folder_is_incomplete(drive_id, parent_id)))
}

/// Performs an initial bootstrap of `/me/drive`, `/me/followedSites`, `/sites?search=*`,
/// personal OneDrive root children (with delta seed), recent items, and shared items into SQLite.
pub async fn bootstrap_sharepoint(graph: &GraphClient, storage: &Storage) -> Result<()> {
    if let Ok(drive) = graph.get_my_drive().await {
        let _ = storage.upsert_drives(std::slice::from_ref(&drive));
        let (dummy_tx, _) = mpsc::unbounded_channel();
        let mut rt = DriveSyncRuntime::default();
        let _ = sync_drive_delta_once(graph, storage, &dummy_tx, &mut rt, &drive.id, None, FOLDER_PAGE_SIZE).await;
    }

    let followed_res = graph.list_followed_sites().await;
    let authoritative_follow = followed_res.is_ok();
    let sites = merge_sidebar_sites(
        followed_res.unwrap_or_default(),
        graph
            .list_sites(Some("*"), SIDEBAR_SITE_DISCOVERY_LIMIT)
            .await
            .unwrap_or_default(),
    );
    if authoritative_follow {
        let _ = storage.upsert_sites_with_follow_state(&sites);
    } else if !sites.is_empty() {
        let _ = storage.upsert_sites(&sites);
    }
    if !sites.is_empty() {
        for site in sites_for_drive_prefetch(&sites) {
            if let Ok(drives) = graph
                .list_site_drives(&site.id, Some(site.effective_name()))
                .await
            {
                let _ = storage.upsert_drives(&drives);
            }
        }
    }

    if let Ok(recent) = graph.fetch_virtual_list(VirtualListKind::Recent, 50).await {
        if recent.status.is_available() {
            let _ = storage.replace_recent_items(&recent.items);
        }
    }
    if let Ok(shared) = graph
        .fetch_virtual_list(VirtualListKind::SharedWithMe, 50)
        .await
    {
        if shared.status.is_available() {
            let _ = storage.replace_shared_items(&shared.items);
        }
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
    let connected_socket_drive: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let socket_wakeup = Arc::new(Notify::new());
    let (internal_tx, internal_rx) = mpsc::unbounded_channel::<InternalTrigger>();

    let ctrl = SyncController {
        focus_tx,
        nudge_tx,
        shared: Arc::clone(&shared),
        shutdown_tx: Some(shutdown_tx),
    };

    // Socket.IO Direct Notification Manager (1 outbound WebSocket for active drive only)
    {
        let graph = graph.clone();
        let event_tx = event_tx.clone();
        let shared = Arc::clone(&shared);
        let focus_rx = focus_rx.clone();
        let shutdown_rx = shutdown_rx.clone();
        let connected_socket_drive = Arc::clone(&connected_socket_drive);
        let socket_wakeup = Arc::clone(&socket_wakeup);
        let internal_tx = internal_tx.clone();

        tokio::spawn(async move {
            run_socketio_manager(
                graph,
                event_tx,
                shared,
                focus_rx,
                shutdown_rx,
                connected_socket_drive,
                socket_wakeup,
                internal_tx,
            )
            .await;
        });
    }

    // Lane 1: Fast Interactive / Delta / Notification Lane
    {
        let graph = graph.clone();
        let storage = storage.clone();
        let event_tx = event_tx.clone();
        let shared = Arc::clone(&shared);
        let focus_rx = focus_rx.clone();
        let mut nudge_rx = nudge_rx;
        let mut internal_rx = internal_rx;
        let mut shutdown_rx = shutdown_rx.clone();
        let connected_socket_drive = Arc::clone(&connected_socket_drive);
        let socket_wakeup = Arc::clone(&socket_wakeup);
        let active_poll_ms = config.active_poll_ms.clamp(1_000, 120_000);

        tokio::spawn(async move {
            let mut ticker = time::interval(Duration::from_millis(active_poll_ms));
            ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
            // Skip immediate t=0 tick so startup relies on explicit Bootstrap/Socket connect.
            ticker.tick().await;
            let mut backoff = ErrorBackoffState::new(active_poll_ms);
            let mut tracker = PeriodicBackoffTracker::default();
            let mut rt = DriveSyncRuntime::default();
            let mut startup_root = None;

            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    maybe_internal = internal_rx.recv() => {
                        let Some(trigger) = maybe_internal else { break; };
                        match trigger {
                            InternalTrigger::SocketNotification { drive_id } => {
                                // Coalesce rapid notification bursts into a single delta pull.
                                let coalesce_until = time::Instant::now() + Duration::from_millis(NOTIFICATION_COALESCE_MS);
                                loop {
                                    let rem = coalesce_until.saturating_duration_since(time::Instant::now());
                                    if rem.is_zero() {
                                        break;
                                    }
                                    match time::timeout(rem, internal_rx.recv()).await {
                                        Ok(Some(InternalTrigger::SocketNotification { .. })) => continue,
                                        Ok(Some(InternalTrigger::SocketConnected { .. })) => continue,
                                        Ok(None) | Err(_) => break,
                                    }
                                }
                                let focus = focus_rx.borrow().clone();
                                if focus.view_kind == FocusViewKind::Folder
                                    && focus.active_drive_id.as_deref() == Some(drive_id.as_str())
                                {
                                    if let Err(err) = sync_drive_delta_once(
                                        &graph,
                                        &storage,
                                        &event_tx,
                                        &mut rt,
                                        &drive_id,
                                        focus.active_parent_id.as_deref(),
                                        effective_folder_limit(focus.active_limit),
                                    )
                                    .await
                                    {
                                        handle_poll_error(&err, &shared, &mut backoff, &event_tx);
                                    } else {
                                        shared.clear_auth_pause();
                                        backoff.record_success();
                                    }
                                }
                            }
                            InternalTrigger::SocketConnected { drive_id } => {
                                let focus = focus_rx.borrow().clone();
                                if focus.view_kind == FocusViewKind::Folder
                                    && focus.active_drive_id.as_deref() == Some(drive_id.as_str())
                                {
                                    if let Err(err) = sync_drive_delta_once(
                                        &graph,
                                        &storage,
                                        &event_tx,
                                        &mut rt,
                                        &drive_id,
                                        focus.active_parent_id.as_deref(),
                                        effective_folder_limit(focus.active_limit),
                                    )
                                    .await
                                    {
                                        handle_poll_error(&err, &shared, &mut backoff, &event_tx);
                                    } else {
                                        shared.clear_auth_pause();
                                        backoff.record_success();
                                    }
                                }
                            }
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
                                let active_site_id = focus_rx.borrow().active_site_id.clone();
                                let discovered_root = sync_core_discovery(
                                    &graph,
                                    &storage,
                                    &event_tx,
                                    &shared,
                                    &mut backoff,
                                    &mut rt,
                                    active_site_id.as_deref(),
                                )
                                .await;
                                let is_startup = matches!(cmd, SyncCommand::Bootstrap);
                                if is_startup {
                                    startup_root = discovered_root;
                                }
                                let focus = focus_rx.borrow().clone();
                                match focus.view_kind {
                                    FocusViewKind::Folder => {
                                        if let Some(did) = focus.active_drive_id {
                                            let discovery_handled_root = is_startup
                                                && focus.active_parent_id.is_none()
                                                && startup_root.as_ref().is_some_and(|(id, _)| *id == did);
                                            if !discovery_handled_root {
                                                if let Err(err) = sync_drive_delta_once(
                                                    &graph,
                                                    &storage,
                                                    &event_tx,
                                                    &mut rt,
                                                    &did,
                                                    focus.active_parent_id.as_deref(),
                                                    effective_folder_limit(focus.active_limit),
                                                )
                                                .await
                                                {
                                                    handle_poll_error(&err, &shared, &mut backoff, &event_tx);
                                                }
                                            }
                                        }
                                    }
                                    // Recent and SharedWithMe were already refreshed once by `sync_core_discovery`.
                                    FocusViewKind::Recent | FocusViewKind::SharedWithMe => {}
                                }
                                let summary = current_idle_summary(&connected_socket_drive, focus_rx.borrow().active_drive_id.as_deref());
                                let _ = event_tx.send(CoreEvent::SyncStateChanged {
                                    busy: false,
                                    summary,
                                });
                            }
                            SyncCommand::FocusGained => {
                                shared.record_activity(Instant::now());
                                socket_wakeup.notify_one();
                                let focus = focus_rx.borrow().clone();
                                match focus.view_kind {
                                    FocusViewKind::Folder => {
                                        if let Some(did) = focus.active_drive_id {
                                            if let Err(err) = sync_drive_delta_once(
                                                &graph,
                                                &storage,
                                                &event_tx,
                                                &mut rt,
                                                &did,
                                                focus.active_parent_id.as_deref(),
                                                effective_folder_limit(focus.active_limit),
                                            )
                                            .await
                                            {
                                                handle_poll_error(&err, &shared, &mut backoff, &event_tx);
                                            } else {
                                                shared.clear_auth_pause();
                                                backoff.record_success();
                                            }
                                        }
                                    }
                                    FocusViewKind::Recent => {
                                        if !rt.virtual_unavailable.contains(&VirtualListKind::Recent) {
                                            sync_virtual_list_once(&graph, &storage, &event_tx, &mut rt, VirtualListKind::Recent).await;
                                        }
                                    }
                                    FocusViewKind::SharedWithMe => {
                                        if !rt.virtual_unavailable.contains(&VirtualListKind::SharedWithMe) {
                                            sync_virtual_list_once(&graph, &storage, &event_tx, &mut rt, VirtualListKind::SharedWithMe).await;
                                        }
                                    }
                                }
                            }
                            SyncCommand::SyncSites => {
                                let active_site_id = focus_rx.borrow().active_site_id.clone();
                                sync_sites_once(&graph, &storage, &event_tx, active_site_id.as_deref()).await;
                            }
                            SyncCommand::SyncSiteDrives { site_id, site_name } => {
                                if let Ok(drives) = graph.list_site_drives(&site_id, site_name.as_deref()).await {
                                    let _ = storage.upsert_drives(&drives);
                                    let _ = event_tx.send(CoreEvent::DrivesUpdated { site_id, drives });
                                }
                            }
                            SyncCommand::SyncFolder { drive_id, parent_id, reuse_startup_root, limit } => {
                                let limit = effective_folder_limit(limit);
                                let initial_result = startup_root.as_ref()
                                    .filter(|(id, _)| reuse_startup_root && parent_id.is_none() && *id == drive_id)
                                    .map(|(_, loaded)| *loaded);
                                let result = match initial_result {
                                    Some(true) => Ok(()),
                                    Some(false) => sync_folder_once(&graph, &storage, &event_tx, &mut rt, &drive_id, None, limit).await,
                                    None => match storage.get_sync_cursor(&drive_cursor_scope_key(&drive_id)) {
                                        Ok(None) if !rt.delta_unsupported_drives.contains(&drive_id) => {
                                            bootstrap_drive_with_delta(&graph, &storage, &event_tx, &mut rt, &drive_id, parent_id.as_deref(), limit).await
                                        }
                                        Ok(_) => sync_folder_once(&graph, &storage, &event_tx, &mut rt, &drive_id, parent_id.as_deref(), limit).await,
                                        Err(err) => Err(err),
                                    },
                                };
                                if let Err(err) = result {
                                    let _ = event_tx.send(CoreEvent::SyncError(format!("List folder: {err:#}")));
                                }
                            }
                            SyncCommand::SyncRecent => {
                                sync_virtual_list_once(&graph, &storage, &event_tx, &mut rt, VirtualListKind::Recent).await;
                            }
                            SyncCommand::SyncShared => {
                                sync_virtual_list_once(&graph, &storage, &event_tx, &mut rt, VirtualListKind::SharedWithMe).await;
                            }
                        }
                    }
                    _ = ticker.tick() => {
                        let now = Instant::now();
                        if shared.is_auth_paused(now) || backoff.is_backed_off(now) {
                            continue;
                        }
                        let (idle, seq) = shared.idle_snapshot(now);
                        if !tracker.on_tick_with(idle, seq, fallback_idle_multiplier) {
                            continue;
                        }
                        let focus = focus_rx.borrow().clone();
                        match focus.view_kind {
                            FocusViewKind::Folder => {
                                if let Some(did) = focus.active_drive_id {
                                    let socket_live = connected_socket_drive
                                        .lock()
                                        .ok()
                                        .and_then(|g| g.clone())
                                        .as_deref()
                                        == Some(did.as_str());
                                    if socket_live {
                                        let due_safety = rt
                                            .last_delta_at
                                            .get(&did)
                                            .is_none_or(|t| t.elapsed() >= Duration::from_secs(SOCKET_SAFETY_DELTA_SECS));
                                        if !due_safety {
                                            continue;
                                        }
                                    }
                                    match sync_drive_delta_once(
                                        &graph,
                                        &storage,
                                        &event_tx,
                                        &mut rt,
                                        &did,
                                        focus.active_parent_id.as_deref(),
                                        effective_folder_limit(focus.active_limit),
                                    )
                                    .await
                                    {
                                        Ok(()) => {
                                            shared.clear_auth_pause();
                                            if backoff.record_success() {
                                                let summary = current_idle_summary(
                                                    &connected_socket_drive,
                                                    Some(did.as_str()),
                                                );
                                                let _ = event_tx.send(CoreEvent::SyncStateChanged {
                                                    busy: false,
                                                    summary,
                                                });
                                            }
                                        }
                                        Err(err) => {
                                            handle_poll_error(&err, &shared, &mut backoff, &event_tx);
                                        }
                                    }
                                }
                            }
                            FocusViewKind::Recent => {
                                if !rt.virtual_unavailable.contains(&VirtualListKind::Recent) {
                                    sync_virtual_list_once(&graph, &storage, &event_tx, &mut rt, VirtualListKind::Recent).await;
                                }
                            }
                            FocusViewKind::SharedWithMe => {
                                if !rt.virtual_unavailable.contains(&VirtualListKind::SharedWithMe) {
                                    sync_virtual_list_once(&graph, &storage, &event_tx, &mut rt, VirtualListKind::SharedWithMe).await;
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    // Lane 2: Quiet Background Site/Library Discovery Lane
    {
        let bg_ms = config.background_poll_ms.clamp(15_000, 600_000);
        let focus_rx = focus_rx;
        let mut shutdown_rx = shutdown_rx;
        tokio::spawn(async move {
            let mut ticker = time::interval(Duration::from_millis(bg_ms));
            ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
            // Skip immediate t=0 tick (Bootstrap handles initial discovery).
            ticker.tick().await;
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
                        let active_site_id = focus_rx.borrow().active_site_id.clone();
                        sync_sites_once(&graph, &storage, &event_tx, active_site_id.as_deref()).await;
                    }
                }
            }
        });
    }

    ctrl
}

fn current_idle_summary(
    connected_socket_drive: &Arc<Mutex<Option<String>>>,
    active_drive_id: Option<&str>,
) -> String {
    let live = active_drive_id.is_some()
        && connected_socket_drive
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .as_deref()
            == active_drive_id;
    if live {
        "Live (Socket.IO)".to_string()
    } else {
        "Synced".to_string()
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_socketio_manager(
    graph: GraphClient,
    event_tx: mpsc::UnboundedSender<CoreEvent>,
    shared: Arc<SyncSharedState>,
    mut focus_rx: watch::Receiver<FocusTarget>,
    mut shutdown_rx: watch::Receiver<bool>,
    connected_socket_drive: Arc<Mutex<Option<String>>>,
    socket_wakeup: Arc<Notify>,
    internal_tx: mpsc::UnboundedSender<InternalTrigger>,
) {
    let mut consecutive_failures: u32 = 0;
    let mut last_drive: Option<String> = None;

    loop {
        if *shutdown_rx.borrow() {
            break;
        }

        let target_drive = {
            let f = focus_rx.borrow();
            if f.view_kind == FocusViewKind::Folder {
                f.active_drive_id.clone()
            } else {
                None
            }
        };

        if target_drive != last_drive {
            consecutive_failures = 0;
            last_drive = target_drive.clone();
        }

        let Some(drive_id) = target_drive else {
            if let Ok(mut g) = connected_socket_drive.lock() {
                *g = None;
            }
            tokio::select! {
                _ = shutdown_rx.changed() => if *shutdown_rx.borrow() { break; },
                res = focus_rx.changed() => if res.is_err() { break; },
            }
            continue;
        };

        if shared.is_auth_paused(Instant::now()) {
            tokio::select! {
                _ = shutdown_rx.changed() => if *shutdown_rx.borrow() { break; },
                res = focus_rx.changed() => if res.is_err() { break; },
                _ = socket_wakeup.notified() => {}
                _ = time::sleep(Duration::from_secs(5)) => {}
            }
            continue;
        }

        let sub_res = tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() { break; }
                continue;
            }
            res = focus_rx.changed() => {
                if res.is_err() { break; }
                continue;
            }
            _ = socket_wakeup.notified() => {
                consecutive_failures = 0;
                continue;
            }
            res = graph.get_drive_socketio_subscription(&drive_id) => res,
        };

        let sub = match sub_res {
            Ok(s) => s,
            Err(err) => {
                let delay = if is_socketio_unsupported_error(&err) {
                    Duration::from_secs(SOCKET_UNSUPPORTED_RETRY_SECS)
                } else {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    let secs = SOCKET_RECONNECT_BASE_SECS
                        .saturating_mul(1u64 << consecutive_failures.min(5))
                        .min(SOCKET_RECONNECT_MAX_SECS);
                    Duration::from_secs(secs)
                };
                tokio::select! {
                    _ = shutdown_rx.changed() => if *shutdown_rx.borrow() { break; },
                    res = focus_rx.changed() => if res.is_err() { break; },
                    _ = socket_wakeup.notified() => { consecutive_failures = 0; }
                    _ = time::sleep(delay) => {}
                }
                continue;
            }
        };

        let (sess_stop_tx, mut sess_stop_rx) = watch::channel(false);
        let (conn_tx, conn_rx) = oneshot::channel();
        let (sig_tx, mut sig_rx) = mpsc::unbounded_channel::<SocketIoSignal>();
        let did_for_session = drive_id.clone();
        let url_for_session = sub.notification_url.clone();

        let mut session_handle = tokio::spawn(async move {
            socketio::run_socketio_session(
                did_for_session,
                &url_for_session,
                Some(conn_tx),
                sig_tx,
                &mut sess_stop_rx,
            )
            .await
        });

        let mut was_connected = false;
        let mut conn_rx = Some(conn_rx);
        let mut focus_changed = false;
        let mut shutting_down = false;
        let mut session_joined = false;

        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        shutting_down = true;
                        let _ = sess_stop_tx.send(true);
                        break;
                    }
                }
                res = focus_rx.changed() => {
                    if res.is_err() {
                        shutting_down = true;
                        let _ = sess_stop_tx.send(true);
                        break;
                    }
                    let still_same_drive = {
                        let f = focus_rx.borrow();
                        f.view_kind == FocusViewKind::Folder
                            && f.active_drive_id.as_deref() == Some(drive_id.as_str())
                    };
                    if !still_same_drive {
                        focus_changed = true;
                        let _ = sess_stop_tx.send(true);
                        break;
                    }
                }
                Ok(()) = async {
                    match conn_rx.as_mut() {
                        Some(rx) => rx.await,
                        None => std::future::pending().await,
                    }
                }, if conn_rx.is_some() => {
                    conn_rx = None;
                    was_connected = true;
                    consecutive_failures = 0;
                    if let Ok(mut g) = connected_socket_drive.lock() {
                        *g = Some(drive_id.clone());
                    }
                    let _ = event_tx.send(CoreEvent::SyncStateChanged {
                        busy: false,
                        summary: "Live (Socket.IO)".to_string(),
                    });
                    let _ = internal_tx.send(InternalTrigger::SocketConnected {
                        drive_id: drive_id.clone(),
                    });
                }
                maybe_sig = sig_rx.recv() => {
                    match maybe_sig {
                        Some(SocketIoSignal::Notification { drive_id: notif_did }) => {
                            let _ = internal_tx.send(InternalTrigger::SocketNotification {
                                drive_id: notif_did,
                            });
                        }
                        None => break,
                    }
                }
                _ = &mut session_handle => {
                    session_joined = true;
                    break;
                }
            }
        }

        if !session_joined
            && tokio::time::timeout(Duration::from_millis(500), &mut session_handle)
                .await
                .is_err()
        {
            session_handle.abort();
            let _ = session_handle.await;
        }
        if let Ok(mut g) = connected_socket_drive.lock() {
            if g.as_deref() == Some(drive_id.as_str()) {
                *g = None;
            }
        }
        if shutting_down {
            break;
        }
        if was_connected && !focus_changed {
            let _ = event_tx.send(CoreEvent::SyncStateChanged {
                busy: false,
                summary: "Synced (Poll fallback)".to_string(),
            });
        }
        if focus_changed {
            continue;
        }

        consecutive_failures = consecutive_failures.saturating_add(1);
        let delay_secs = SOCKET_RECONNECT_BASE_SECS
            .saturating_mul(1u64 << consecutive_failures.min(5))
            .min(SOCKET_RECONNECT_MAX_SECS);
        tokio::select! {
            _ = shutdown_rx.changed() => if *shutdown_rx.borrow() { break; },
            res = focus_rx.changed() => if res.is_err() { break; },
            _ = socket_wakeup.notified() => { consecutive_failures = 0; }
            _ = time::sleep(Duration::from_secs(delay_secs)) => {}
        }
    }
}

/// First-connection drive bootstrap with gap closure:
/// 1. Acquire latest delta start token (`?token=latest`) BEFORE reading the folder
/// 2. Read the open folder (`/children`) and persist/emit
/// 3. Immediately catch up any delta changes that occurred during the folder load
pub async fn bootstrap_drive_with_delta(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    rt: &mut DriveSyncRuntime,
    drive_id: &str,
    active_parent_id: Option<&str>,
    limit: usize,
) -> Result<()> {
    let seed_link = match graph.get_drive_delta_latest_link(drive_id).await {
        Ok(link) => Some(link),
        Err(err) if is_delta_unsupported_error(&err) => {
            rt.delta_unsupported_drives.insert(drive_id.to_string());
            None
        }
        Err(err) => return Err(err),
    };

    let page = graph
        .list_children_detailed(drive_id, active_parent_id, Some(limit))
        .await?;
    rt.note_folder_listing(drive_id, active_parent_id, page.complete);
    let mut inferred_root_id = if active_parent_id.is_none() {
        page.items
            .iter()
            .find_map(|i| i.parent_id.clone())
            .filter(|pid| !pid.trim().is_empty() && pid != "root")
    } else {
        None
    };

    if seed_link.is_some() && inferred_root_id.is_none() {
        inferred_root_id = graph.get_item_by_path(drive_id, "").await.ok().map(|root| root.id);
    }
    if page.complete {
        storage.replace_folder_children(drive_id, active_parent_id, &page.items)?;
    } else {
        storage.upsert_drive_items(&page.items)?;
    }
    let (items, truncated, progress) = if let Some(seed_url) = seed_link {
        let progress = pull_delta_pages_and_emit(
            graph,
            storage,
            event_tx,
            rt,
            drive_id,
            active_parent_id,
            limit,
            &seed_url,
            inferred_root_id,
        )
        .await?;
        let (items, truncated) =
            cached_folder_page(storage, rt, drive_id, active_parent_id, limit)?;
        (items, truncated, progress)
    } else {
        (page.items, !page.complete, DeltaProgress::CaughtUp)
    };
    let _ = event_tx.send(CoreEvent::FolderItemsUpdated {
        drive_id: drive_id.to_string(),
        parent_id: active_parent_id.map(str::to_string),
        items,
        truncated,
    });
    rt.note_delta_progress(drive_id, progress);
    Ok(())
}

/// Executes one per-drive delta synchronization pass using the persisted `sync_cursors` row.
/// Falls back to bootstrap-with-delta on first connection, handles `410 Gone`/overflow resync,
/// and advances the stored cursor ONLY after the final page succeeds in SQLite.
pub async fn sync_drive_delta_once(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    rt: &mut DriveSyncRuntime,
    drive_id: &str,
    active_parent_id: Option<&str>,
    limit: usize,
) -> Result<()> {
    if rt.delta_unsupported_drives.contains(drive_id) {
        sync_folder_once(graph, storage, event_tx, rt, drive_id, active_parent_id, limit).await?;
        rt.last_delta_at.insert(drive_id.to_string(), Instant::now());
        return Ok(());
    }

    let scope_key = drive_cursor_scope_key(drive_id);
    for _ in 0..MAX_DELTA_PASSES_PER_SYNC {
        let existing_cursor = storage.get_sync_cursor(&scope_key)?;
        let Some(saved_delta_link) = existing_cursor
            .as_ref()
            .and_then(|c| c.delta_link.clone())
            .filter(|s| !s.trim().is_empty())
        else {
            return bootstrap_drive_with_delta(
                graph,
                storage,
                event_tx,
                rt,
                drive_id,
                active_parent_id,
                limit,
            )
            .await;
        };

        let mut known_root_id = decode_root_id(existing_cursor.as_ref());
        if known_root_id.is_none() {
            known_root_id = graph.get_item_by_path(drive_id, "").await.ok().map(|root| root.id);
        }
        let progress = pull_delta_pages_and_emit(
            graph,
            storage,
            event_tx,
            rt,
            drive_id,
            active_parent_id,
            limit,
            &saved_delta_link,
            known_root_id,
        )
        .await?;
        rt.note_delta_progress(drive_id, progress);
        if progress == DeltaProgress::CaughtUp {
            break;
        }
        // Still behind: the cursor now points at the next unread page, so go again.
    }
    Ok(())
}

/// Where a delta pull left the drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeltaProgress {
    /// The change feed was read to its end; the cursor is a `deltaLink`.
    CaughtUp,
    /// The page budget ran out first; the cursor is the `nextLink` to continue from.
    MorePending,
}

enum DeltaPullStatus {
    Completed,
    /// The page budget ran out; the cursor was saved at the next unread page.
    MorePending,
    ResyncRequired {
        location: Option<String>,
        known_root_id: Option<String>,
    },
}

#[allow(clippy::too_many_arguments)]
async fn pull_delta_pages_once(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    rt: &DriveSyncRuntime,
    drive_id: &str,
    active_parent_id: Option<&str>,
    limit: usize,
    start_url: &str,
    mut known_root_id: Option<String>,
) -> Result<DeltaPullStatus> {
    let scope_key = drive_cursor_scope_key(drive_id);
    let mut next_url = start_url.to_string();
    let mut changed_items = false;
    let mut pages_read = 0usize;
    let mut more_pending = false;

    loop {
        pages_read += 1;

        match graph.get_drive_delta_page(drive_id, &next_url).await? {
            DriveDeltaPageResponse::ResyncRequired { location, .. } => {
                return Ok(DeltaPullStatus::ResyncRequired {
                    location,
                    known_root_id,
                });
            }
            DriveDeltaPageResponse::Page(page) => {
                for ch in &page.changes {
                    if let DriveDeltaChange::Root { id } = ch {
                        if !id.trim().is_empty() {
                            known_root_id = Some(id.clone());
                        }
                    }
                }

                // The cursor moves only with a page it belongs to, in that page's
                // transaction: to the deltaLink on the last page, or, when the budget for
                // this pull is spent, to the nextLink so the following pull continues
                // there. Pages in between leave it alone, so a failure replays from the
                // last saved position.
                let budget_spent = pages_read >= MAX_DELTA_PAGES;
                let resume_link = match &page.next_link {
                    None => Some(page.delta_link.as_ref().ok_or_else(|| {
                        anyhow::anyhow!("Graph final delta page returned no @odata.deltaLink")
                    })?),
                    Some(next) if budget_spent => Some(next),
                    Some(_) => None,
                };
                let commit_cursor = resume_link.map(|link| SyncCursor {
                    scope_key: scope_key.clone(),
                    delta_link: Some(link.clone()),
                    history_next_link: encode_root_id(known_root_id.as_deref()),
                    last_synced_at: now_rfc3339(),
                });

                let outcome = storage.apply_delta_page(
                    drive_id,
                    &page.changes,
                    known_root_id.as_deref(),
                    commit_cursor.as_ref(),
                )?;
                if let Some(det) = outcome.detected_root_id {
                    known_root_id = Some(det);
                }
                changed_items |= outcome.upserted_count > 0 || outcome.deleted_count > 0;

                match page.next_link {
                    Some(_) if budget_spent => {
                        more_pending = true;
                        break;
                    }
                    Some(next) => next_url = next,
                    None => break,
                }
            }
        }
    }

    // A replay might find an item's new parent already saved by a previous partial pull.
    // Read the active folder after any changed pass so the old parent loses moved items.
    if changed_items {
        let (items, truncated) =
            cached_folder_page(storage, rt, drive_id, active_parent_id, limit)?;
        let _ = event_tx.send(CoreEvent::FolderItemsUpdated {
            drive_id: drive_id.to_string(),
            parent_id: active_parent_id.map(str::to_string),
            items,
            truncated,
        });
    }

    Ok(if more_pending {
        DeltaPullStatus::MorePending
    } else {
        DeltaPullStatus::Completed
    })
}

#[allow(clippy::too_many_arguments)]
async fn pull_delta_pages_and_emit(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    rt: &mut DriveSyncRuntime,
    drive_id: &str,
    active_parent_id: Option<&str>,
    limit: usize,
    start_url: &str,
    known_root_id: Option<String>,
) -> Result<DeltaProgress> {
    match pull_delta_pages_once(
        graph,
        storage,
        event_tx,
        rt,
        drive_id,
        active_parent_id,
        limit,
        start_url,
        known_root_id,
    )
    .await?
    {
        DeltaPullStatus::Completed => Ok(DeltaProgress::CaughtUp),
        DeltaPullStatus::MorePending => Ok(DeltaProgress::MorePending),
        DeltaPullStatus::ResyncRequired {
            location,
            known_root_id,
        } => {
            resync_drive_after_410_or_overflow(
                graph,
                storage,
                event_tx,
                rt,
                drive_id,
                active_parent_id,
                limit,
                location,
                known_root_id,
            )
            .await?;
            // A resync ends on a fresh deltaLink, so the drive is caught up.
            Ok(DeltaProgress::CaughtUp)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn resync_drive_after_410_or_overflow(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    rt: &mut DriveSyncRuntime,
    drive_id: &str,
    active_parent_id: Option<&str>,
    limit: usize,
    location: Option<String>,
    mut known_root_id: Option<String>,
) -> Result<()> {
    let scope_key = drive_cursor_scope_key(drive_id);

    // Tier 1: If Graph returned a `Location` header on 410 Gone, attempt a bounded authoritative resync.
    if let Some(loc_url) = location.filter(|s| !s.trim().is_empty()) {
        let mut next_url = loc_url;
        let mut collected: Vec<DriveDeltaChange> = Vec::new();
        let mut final_delta_link: Option<String> = None;
        let mut bounded_ok = true;

        for _ in 0..MAX_RESYNC_PAGES {
            match graph.get_drive_delta_page(drive_id, &next_url).await {
                Ok(DriveDeltaPageResponse::Page(page)) => {
                    for ch in &page.changes {
                        if let DriveDeltaChange::Root { id } = ch {
                            if !id.trim().is_empty() {
                                known_root_id = Some(id.clone());
                            }
                        }
                    }
                    collected.extend(page.changes);
                    if let Some(next) = page.next_link {
                        next_url = next;
                    } else {
                        final_delta_link = page.delta_link;
                        break;
                    }
                }
                _ => {
                    bounded_ok = false;
                    break;
                }
            }
        }

        if bounded_ok {
            if let Some(dl) = final_delta_link {
                let cursor = SyncCursor {
                    scope_key: scope_key.clone(),
                    delta_link: Some(dl),
                    history_next_link: encode_root_id(known_root_id.as_deref()),
                    last_synced_at: now_rfc3339(),
                };
                storage.replace_drive_items_from_resync(
                    drive_id,
                    &collected,
                    known_root_id.as_deref(),
                    &cursor,
                )?;
                // The resync enumerated the whole drive, so the cache is now the full picture.
                rt.incomplete_folders.retain(|(drive, _)| drive != drive_id);
                let (items, truncated) =
                    cached_folder_page(storage, rt, drive_id, active_parent_id, limit)?;
                let _ = event_tx.send(CoreEvent::FolderItemsUpdated {
                    drive_id: drive_id.to_string(),
                    parent_id: active_parent_id.map(str::to_string),
                    items,
                    truncated,
                });
                return Ok(());
            }
        }
    }

    // Tier 2: Overflow or no/failed `Location` header — seed fresh `?token=latest` BEFORE
    // authoritatively reconciling root + active folder, then catch up any gap delta.
    let fresh_seed = graph.get_drive_delta_latest_link(drive_id).await?;

    // The folders between the root and the open one are re-listed too, so walking back up
    // the breadcrumb never shows rows the resync could not check. Read the chain from the
    // cache before the root re-list can drop part of it.
    let ancestors = match active_parent_id {
        Some(pid) => cached_ancestor_ids(storage, drive_id, pid, known_root_id.as_deref())?,
        None => Vec::new(),
    };

    // Always reconcile root on overflow/410 so deleted/added root items are never stale.
    // The root is listed in full only when it is the folder on screen.
    let root_limit = if active_parent_id.is_none() { limit } else { FOLDER_PAGE_SIZE };
    let root_page = graph
        .list_children_detailed(drive_id, None, Some(root_limit))
        .await?;
    rt.note_folder_listing(drive_id, None, root_page.complete);
    if let Some(det_root) = root_page
        .items
        .iter()
        .find_map(|i| i.parent_id.clone())
        .filter(|pid| !pid.trim().is_empty() && pid != "root")
    {
        known_root_id = Some(det_root);
    }
    if known_root_id.is_none() {
        known_root_id = graph.get_item_by_path(drive_id, "").await.ok().map(|root| root.id);
    }
    if root_page.complete {
        storage.replace_folder_children(drive_id, None, &root_page.items)?;
    } else {
        storage.upsert_drive_items(&root_page.items)?;
    }

    if let Some(pid) = active_parent_id {
        let active_page = graph
            .list_children_detailed(drive_id, Some(pid), Some(limit))
            .await?;
        rt.note_folder_listing(drive_id, Some(pid), active_page.complete);
        if active_page.complete {
            storage.replace_folder_children(drive_id, Some(pid), &active_page.items)?;
        } else {
            storage.upsert_drive_items(&active_page.items)?;
        }
        for ancestor in &ancestors {
            // An ancestor that is gone or unreadable is left for its own visit to settle;
            // it must not stop the open folder from being brought up to date.
            let Ok(page) = graph
                .list_children_detailed(drive_id, Some(ancestor), Some(FOLDER_PAGE_SIZE))
                .await
            else {
                continue;
            };
            rt.note_folder_listing(drive_id, Some(ancestor), page.complete);
            if page.complete {
                storage.replace_folder_children(drive_id, Some(ancestor), &page.items)?;
            } else {
                storage.upsert_drive_items(&page.items)?;
            }
        }
    }
    let (items, truncated) = cached_folder_page(storage, rt, drive_id, active_parent_id, limit)?;
    let _ = event_tx.send(CoreEvent::FolderItemsUpdated {
        drive_id: drive_id.to_string(),
        parent_id: active_parent_id.map(str::to_string),
        items,
        truncated,
    });

    // Catch up any edits that occurred while reconciling root + active folder and commit cursor.
    match pull_delta_pages_once(
        graph,
        storage,
        event_tx,
        rt,
        drive_id,
        active_parent_id,
        limit,
        &fresh_seed,
        known_root_id,
    )
    .await?
    {
        // Folders other than the root, the open one and its ancestors were not checked.
        // Their cached rows stay (stale beats missing offline) and refresh when opened.
        DeltaPullStatus::Completed | DeltaPullStatus::MorePending => {
            let _ = event_tx.send(CoreEvent::SyncStateChanged {
                busy: false,
                summary: PARTIAL_RESYNC_SUMMARY.to_string(),
            });
            Ok(())
        }
        DeltaPullStatus::ResyncRequired { .. } => anyhow::bail!("Graph requested another drive resync"),
    }
}

/// The cached folders strictly between the drive root and `folder_id`, nearest first.
/// Stops at the root, at a row the cache does not hold, or after `MAX_RESYNC_ANCESTORS`.
fn cached_ancestor_ids(
    storage: &Storage,
    drive_id: &str,
    folder_id: &str,
    root_id: Option<&str>,
) -> Result<Vec<String>> {
    let is_root = |id: &str| id.trim().is_empty() || id == "root" || Some(id) == root_id;
    let mut ancestors = Vec::new();
    let mut current = folder_id.to_string();
    while ancestors.len() < MAX_RESYNC_ANCESTORS {
        let Some(item) = storage.get_drive_item(drive_id, &current)? else {
            break;
        };
        let Some(parent) = item.parent_id.filter(|p| !is_root(p)) else {
            break;
        };
        if parent == folder_id || ancestors.contains(&parent) {
            break; // A cycle in a corrupt cache must not spin.
        }
        ancestors.push(parent.clone());
        current = parent;
    }
    Ok(ancestors)
}

async fn sync_core_discovery(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    shared: &Arc<SyncSharedState>,
    backoff: &mut ErrorBackoffState,
    rt: &mut DriveSyncRuntime,
    active_site_id: Option<&str>,
) -> Option<(String, bool)> {
    if let Ok(me) = graph.get_me().await {
        let _ = event_tx.send(CoreEvent::ProfileLoaded(me));
    }

    let root_load = match graph.get_my_drive().await {
        Ok(drive) => {
            let _ = storage.upsert_drives(std::slice::from_ref(&drive));
            let _ = event_tx.send(CoreEvent::MyDriveLoaded(drive.clone()));
            let root_load = if storage.get_sync_cursor(&drive_cursor_scope_key(&drive.id)).is_ok_and(|cursor| cursor.is_none())
                && !rt.delta_unsupported_drives.contains(&drive.id)
            {
                let loaded = bootstrap_drive_with_delta(graph, storage, event_tx, rt, &drive.id, None, FOLDER_PAGE_SIZE).await.is_ok();
                Some((drive.id, loaded))
            } else {
                None
            };
            shared.clear_auth_pause();
            backoff.record_success();
            root_load
        }
        Err(err) => {
            handle_poll_error(&err, shared, backoff, event_tx);
            None
        }
    };

    sync_sites_once(graph, storage, event_tx, active_site_id).await;
    sync_virtual_list_once(graph, storage, event_tx, rt, VirtualListKind::Recent).await;
    sync_virtual_list_once(graph, storage, event_tx, rt, VirtualListKind::SharedWithMe).await;
    root_load
}

async fn sync_virtual_list_once(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    rt: &mut DriveSyncRuntime,
    kind: VirtualListKind,
) {
    if let Ok(fetch) = graph.fetch_virtual_list(kind, 50).await {
        match &fetch.status.availability {
            VirtualListAvailability::Available => {
                rt.virtual_unavailable.remove(&kind);
                match kind {
                    VirtualListKind::Recent => {
                        let _ = storage.replace_recent_items(&fetch.items);
                        let _ = event_tx.send(CoreEvent::RecentItemsUpdated(fetch.items));
                    }
                    VirtualListKind::SharedWithMe => {
                        let _ = storage.replace_shared_items(&fetch.items);
                        let _ = event_tx.send(CoreEvent::SharedItemsUpdated(fetch.items));
                    }
                }
            }
            VirtualListAvailability::Unavailable { .. } => {
                rt.virtual_unavailable.insert(kind);
            }
        }
        let _ = event_tx.send(CoreEvent::VirtualListStatusUpdated {
            kind,
            status: fetch.status,
        });
    }
}

async fn sync_sites_once(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    active_site_id: Option<&str>,
) {
    let followed_res = graph.list_followed_sites().await;
    let authoritative_follow = followed_res.is_ok();
    let combined: Vec<SharePointSite> = merge_sidebar_sites(
        followed_res.unwrap_or_default(),
        graph
            .list_sites(Some("*"), SIDEBAR_SITE_DISCOVERY_LIMIT)
            .await
            .unwrap_or_default(),
    );
    if authoritative_follow {
        let _ = storage.upsert_sites_with_follow_state(&combined);
    } else if !combined.is_empty() {
        let _ = storage.upsert_sites(&combined);
    }
    if !combined.is_empty() || authoritative_follow {
        let _ = event_tx.send(CoreEvent::SitesUpdated(combined.clone()));
        let mut refreshed_active = false;
        for site in sites_for_drive_refresh(&combined, active_site_id) {
            if active_site_id == Some(site.id.as_str()) {
                refreshed_active = true;
            }
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
        if let Some(sid) = active_site_id.filter(|s| !s.trim().is_empty()) {
            if !refreshed_active {
                if let Ok(drives) = graph.list_site_drives(sid, None).await {
                    let _ = storage.upsert_drives(&drives);
                    let _ = event_tx.send(CoreEvent::DrivesUpdated {
                        site_id: sid.to_string(),
                        drives,
                    });
                }
            }
        }
    }
}

async fn sync_folder_once(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    rt: &mut DriveSyncRuntime,
    drive_id: &str,
    parent_id: Option<&str>,
    limit: usize,
) -> Result<()> {
    let page = graph
        .list_children_detailed(drive_id, parent_id, Some(limit))
        .await?;
    rt.note_folder_listing(drive_id, parent_id, page.complete);
    if page.complete {
        storage.replace_folder_children(drive_id, parent_id, &page.items)?;
    } else {
        storage.upsert_drive_items(&page.items)?;
    }
    let _ = event_tx.send(CoreEvent::FolderItemsUpdated {
        drive_id: drive_id.to_string(),
        parent_id: parent_id.map(str::to_string),
        items: page.items,
        truncated: !page.complete,
    });
    Ok(())
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
    fn folder_limit_fields_default_for_older_peers() {
        // Frames from a peer built before the limit existed carry neither field.
        let focus: FocusTarget = serde_json::from_value(serde_json::json!({
            "active_drive_id": "d1",
            "active_parent_id": null,
            "active_site_id": null,
            "view_kind": "folder"
        }))
        .expect("old focus frame parses");
        assert_eq!(focus.active_limit, None);

        let cmd: SyncCommand = serde_json::from_value(serde_json::json!({
            "SyncFolder": { "drive_id": "d1", "parent_id": null, "reuse_startup_root": false }
        }))
        .expect("old SyncFolder frame parses");
        assert!(matches!(cmd, SyncCommand::SyncFolder { limit: None, .. }));

        assert_eq!(effective_folder_limit(None), FOLDER_PAGE_SIZE);
        assert_eq!(effective_folder_limit(Some(400)), 400);
        assert_eq!(effective_folder_limit(Some(0)), 1);
        assert_eq!(effective_folder_limit(Some(usize::MAX)), MAX_FOLDER_ITEMS);
    }

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

    fn test_site(id: &str, is_followed: bool) -> SharePointSite {
        SharePointSite {
            id: id.to_string(),
            name: id.to_string(),
            display_name: id.to_string(),
            description: None,
            web_url: None,
            is_followed,
            created_at: None,
            last_modified_at: None,
        }
    }

    #[test]
    fn drive_prefetch_prioritises_followed_sites() {
        // More discovered sites than the prefetch budget, and two followed sites that the
        // discovery listing also returns (late in its order).
        let followed = vec![test_site("followed-a", true), test_site("followed-b", true)];
        let mut discovered: Vec<SharePointSite> = (0..SIDEBAR_SITE_DISCOVERY_LIMIT)
            .map(|i| test_site(&format!("discovered-{i}"), false))
            .collect();
        discovered.push(test_site("followed-b", false));

        let merged = merge_sidebar_sites(followed, discovered);
        assert_eq!(merged.len(), SIDEBAR_SITE_DISCOVERY_LIMIT + 2, "no duplicate of followed-b");
        assert_eq!(merged[0].id, "followed-a");
        assert_eq!(merged[1].id, "followed-b");
        assert!(merged[1].is_followed, "the followed entry wins over the discovered duplicate");

        let prefetch = sites_for_drive_prefetch(&merged);
        assert_eq!(prefetch.len(), SIDEBAR_DRIVE_PREFETCH_SITES);
        assert_eq!(prefetch[0].id, "followed-a");
        assert_eq!(prefetch[1].id, "followed-b");

        // Fewer sites than the budget: all of them, no panic.
        let few = merge_sidebar_sites(vec![test_site("only", true)], Vec::new());
        assert_eq!(sites_for_drive_prefetch(&few).len(), 1);
        assert!(sites_for_drive_prefetch(&[]).is_empty());

        // Active site past the #12 prefetch cap is still included in drive refresh.
        let refresh = sites_for_drive_refresh(&merged, Some("discovered-20"));
        assert_eq!(refresh.len(), SIDEBAR_DRIVE_PREFETCH_SITES + 1);
        assert_eq!(refresh.last().unwrap().id, "discovered-20");

        // Active site already within the first 12 is not duplicated.
        let refresh_dup = sites_for_drive_refresh(&merged, Some("followed-a"));
        assert_eq!(refresh_dup.len(), SIDEBAR_DRIVE_PREFETCH_SITES);
    }
}
