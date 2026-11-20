//! Decode task identity and cooperative cancellation primitives.
//!
//! The registry is independent from GPUI. It gives each execution a stable
//! identity, prevents an old completion from removing a newer task for the
//! same key, and lets navigation invalidate work without aborting executor
//! futures unsafely.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

static NEXT_TASK_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TaskId(u64);

impl TaskId {
    fn next() -> Self {
        Self(NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed))
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Returns true while the task is still allowed to publish its result.
    pub fn is_active(&self) -> bool {
        !self.is_cancelled()
    }
}

#[derive(Clone, Debug)]
pub struct TaskHandle {
    pub id: TaskId,
    pub cancellation: CancellationToken,
}

struct ActiveTask {
    id: TaskId,
    cancellation: CancellationToken,
}

/// Tracks one active execution per key. Replacing or cancelling a task signals
/// its token but remains registered until completion; this keeps in-flight
/// identity visible while native decoder calls finish cooperatively.
pub struct TaskRegistry<K> {
    active: HashMap<K, ActiveTask>,
}

impl<K: Clone + Eq + Hash> Default for TaskRegistry<K> {
    fn default() -> Self {
        Self {
            active: HashMap::new(),
        }
    }
}

impl<K: Clone + Eq + Hash> TaskRegistry<K> {
    pub fn start(&mut self, key: K) -> TaskHandle {
        if let Some(previous) = self.active.remove(&key) {
            previous.cancellation.cancel();
        }
        let handle = TaskHandle {
            id: TaskId::next(),
            cancellation: CancellationToken::new(),
        };
        self.active.insert(
            key,
            ActiveTask {
                id: handle.id,
                cancellation: handle.cancellation.clone(),
            },
        );
        handle
    }

    pub fn token(&self, key: &K) -> Option<CancellationToken> {
        self.active
            .get(key)
            .map(|task| task.cancellation.clone())
    }

    pub fn cancel(&mut self, key: &K) -> bool {
        if let Some(task) = self.active.get(key) {
            task.cancellation.cancel();
            true
        } else {
            false
        }
    }

    pub fn cancel_where(&mut self, mut predicate: impl FnMut(&K) -> bool) -> usize {
        let keys: Vec<K> = self
            .active
            .keys()
            .filter(|key| predicate(key))
            .cloned()
            .collect();
        let count = keys.len();
        for key in keys {
            self.cancel(&key);
        }
        count
    }

    /// Remove a completed task only if it is still the registered generation.
    pub fn finish(&mut self, key: &K, id: TaskId) -> bool {
        if self.active.get(key).is_some_and(|task| task.id == id) {
            self.active.remove(key);
            true
        } else {
            false
        }
    }

    pub fn len(&self) -> usize {
        self.active.len()
    }

    pub fn is_empty(&self) -> bool {
        self.active.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_is_visible_to_all_clones() {
        let token = CancellationToken::new();
        let worker_token = token.clone();
        assert!(worker_token.is_active());
        token.cancel();
        assert!(worker_token.is_cancelled());
        assert!(!worker_token.is_active());
    }

    #[test]
    fn replacing_same_key_cancels_previous_generation() {
        let mut registry = TaskRegistry::default();
        let old = registry.start("image.jpg");
        let new = registry.start("image.jpg");
        assert!(old.cancellation.is_cancelled());
        assert!(new.cancellation.is_active());
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn stale_completion_cannot_remove_new_task() {
        let mut registry = TaskRegistry::default();
        let old = registry.start("image.jpg");
        let new = registry.start("image.jpg");
        assert!(!registry.finish(&"image.jpg", old.id));
        assert!(registry.token(&"image.jpg").unwrap().is_active());
        assert!(registry.finish(&"image.jpg", new.id));
        assert!(registry.is_empty());
    }

    #[test]
    fn cancel_where_signals_only_matching_tasks() {
        let mut registry = TaskRegistry::default();
        let a = registry.start("a.jpg");
        let b = registry.start("b.jpg");
        assert_eq!(registry.cancel_where(|key| *key == "a.jpg"), 1);
        assert!(a.cancellation.is_cancelled());
        assert!(b.cancellation.is_active());
        assert_eq!(registry.len(), 2);
        assert!(registry.finish(&"a.jpg", a.id));
        assert_eq!(registry.len(), 1);
    }
}
