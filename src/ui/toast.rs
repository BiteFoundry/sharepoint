//! Transient toast queue and background task board.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone)]
pub struct Toast {
    pub message: String,
    pub kind: ToastKind,
    pub created: Instant,
    pub ttl: Duration,
}

impl Toast {
    pub fn info(message: impl Into<String>) -> Self {
        Self::new(message, ToastKind::Info)
    }

    pub fn success(message: impl Into<String>) -> Self {
        Self::new(message, ToastKind::Success)
    }

    pub fn warning(message: impl Into<String>) -> Self {
        Self::new(message, ToastKind::Warning)
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self::new(message, ToastKind::Error)
    }

    pub fn new(message: impl Into<String>, kind: ToastKind) -> Self {
        Self {
            message: message.into(),
            kind,
            created: Instant::now(),
            ttl: Duration::from_secs(4),
        }
    }

    pub fn expired(&self, now: Instant) -> bool {
        now.duration_since(self.created) >= self.ttl
    }
}

#[derive(Debug, Default)]
pub struct ToastQueue {
    toasts: VecDeque<Toast>,
    max: usize,
}

impl ToastQueue {
    pub fn new() -> Self {
        Self {
            toasts: VecDeque::new(),
            max: 4,
        }
    }

    pub fn push(&mut self, toast: Toast) {
        if let Some(existing) = self
            .toasts
            .iter_mut()
            .find(|t| t.kind == toast.kind && t.message == toast.message)
        {
            existing.created = Instant::now();
            return;
        }
        self.toasts.push_back(toast);
        while self.toasts.len() > self.max {
            self.toasts.pop_front();
        }
    }

    pub fn info(&mut self, message: impl Into<String>) {
        self.push(Toast::info(message));
    }

    pub fn success(&mut self, message: impl Into<String>) {
        self.push(Toast::success(message));
    }

    pub fn error(&mut self, message: impl Into<String>) {
        self.push(Toast::error(message));
    }

    pub fn current(&self) -> Option<&Toast> {
        self.toasts.back()
    }

    pub fn latest(&self) -> Option<&Toast> {
        self.current()
    }

    pub fn prune_expired(&mut self) {
        self.tick(Instant::now());
    }

    pub fn len(&self) -> usize {
        self.toasts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.toasts.is_empty()
    }

    pub fn tick(&mut self, now: Instant) {
        while self.toasts.front().is_some_and(|toast| toast.expired(now)) {
            self.toasts.pop_front();
        }
    }

    pub fn clear(&mut self) {
        self.toasts.clear();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskState {
    Running,
    Done,
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct TaskStatus {
    pub label: String,
    pub state: TaskState,
}

impl TaskStatus {
    pub fn display(&self) -> String {
        match &self.state {
            TaskState::Running => format!("{}…", self.label),
            TaskState::Done => format!("{} done", self.label),
            TaskState::Failed(err) => format!("{} failed: {err}", self.label),
        }
    }

    pub fn running(&self) -> bool {
        matches!(self.state, TaskState::Running)
    }
}

#[derive(Debug, Default)]
pub struct TaskBoard {
    tasks: Vec<TaskStatus>,
}

impl TaskBoard {
    pub fn begin(&mut self, label: impl Into<String>) {
        let label = label.into();
        self.tasks.retain(|task| task.label != label);
        self.tasks.push(TaskStatus {
            label,
            state: TaskState::Running,
        });
    }

    pub fn finish(&mut self, label: &str, error: Option<String>) {
        self.tasks.retain(|task| task.label != label);
        self.tasks.push(TaskStatus {
            label: label.to_string(),
            state: match error {
                Some(err) => TaskState::Failed(err),
                None => TaskState::Done,
            },
        });
        let mut done = 0usize;
        let mut keep = Vec::with_capacity(self.tasks.len());
        for task in self.tasks.drain(..).rev() {
            if task.running() {
                keep.push(task);
            } else if done < 3 {
                done += 1;
                keep.push(task);
            }
        }
        keep.reverse();
        self.tasks = keep;
    }

    pub fn running(&self) -> impl Iterator<Item = &TaskStatus> {
        self.tasks.iter().filter(|task| task.running())
    }

    pub fn first_running(&self) -> Option<&TaskStatus> {
        self.running().next()
    }

    pub fn last(&self) -> Option<&TaskStatus> {
        self.tasks.last()
    }

    pub fn clear(&mut self) {
        self.tasks.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.first_running().is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toasts_expire_in_order() {
        let mut queue = ToastQueue::new();
        queue.info("a");
        queue.info("b");
        assert_eq!(queue.current().unwrap().message, "b");
        queue.tick(Instant::now());
        assert_eq!(queue.len(), 2);
    }

    #[test]
    fn toasts_deduplicate_active_same_kind_and_message() {
        let mut queue = ToastQueue::new();
        let older = Instant::now() - Duration::from_secs(3);
        queue.push(Toast {
            message: "Sync failed".to_string(),
            kind: ToastKind::Error,
            created: older,
            ttl: Duration::from_secs(4),
        });
        assert_eq!(queue.len(), 1);

        // Same kind + same message refreshes `created` in place without adding a duplicate.
        queue.error("Sync failed");
        assert_eq!(queue.len(), 1);
        assert!(queue.current().unwrap().created > older);

        // Same message with a different kind is kept distinct.
        queue.info("Sync failed");
        assert_eq!(queue.len(), 2);

        // Different message of same kind is also added.
        queue.error("Another error");
        assert_eq!(queue.len(), 3);
    }

    #[test]
    fn task_board_tracks_running_and_done() {
        let mut board = TaskBoard::default();
        board.begin("history");
        assert_eq!(board.first_running().unwrap().label, "history");
        board.finish("history", None);
        assert!(board.first_running().is_none());
        assert_eq!(board.last().unwrap().state, TaskState::Done);
    }
}
