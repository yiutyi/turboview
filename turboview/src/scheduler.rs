//! Bounded, priority-aware decode task scheduling primitives.
//!
//! This module is deliberately independent from GPUI so queue semantics can be
//! tested deterministically. The UI integration owns task execution and must
//! call `complete` on every success, failure, or cancellation path.

use std::collections::HashSet;
use std::hash::Hash;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum TaskPriority {
    Prefetch,
    Roi,
    Preview,
    Current,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScheduleResult {
    Queued,
    Promoted,
    AlreadyRunning,
    AlreadyQueued,
    RejectedFull,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Pending<K> {
    key: K,
    priority: TaskPriority,
    sequence: u64,
}

/// A bounded pending queue plus a hard cap on running work.
///
/// Higher priorities are dispatched first; equal priorities preserve FIFO
/// order. Repeated keys are coalesced, and a higher-priority duplicate promotes
/// the existing queued item instead of starting a second decode.
pub struct TaskScheduler<K> {
    pending: Vec<Pending<K>>,
    running: HashSet<K>,
    max_pending: usize,
    max_running: usize,
    next_sequence: u64,
}

impl<K: Clone + Eq + Hash> TaskScheduler<K> {
    pub fn new(max_pending: usize, max_running: usize) -> Self {
        Self {
            pending: Vec::with_capacity(max_pending),
            running: HashSet::new(),
            max_pending,
            max_running: max_running.max(1),
            next_sequence: 0,
        }
    }

    pub fn schedule(&mut self, key: K, priority: TaskPriority) -> ScheduleResult {
        if self.running.contains(&key) {
            return ScheduleResult::AlreadyRunning;
        }
        if let Some(item) = self.pending.iter_mut().find(|item| item.key == key) {
            if priority > item.priority {
                item.priority = priority;
                return ScheduleResult::Promoted;
            }
            return ScheduleResult::AlreadyQueued;
        }
        if self.max_pending == 0 {
            return ScheduleResult::RejectedFull;
        }
        if self.pending.len() >= self.max_pending {
            let Some((worst_index, worst)) = self
                .pending
                .iter()
                .enumerate()
                .min_by_key(|(_, item)| (item.priority, std::cmp::Reverse(item.sequence)))
            else {
                return ScheduleResult::RejectedFull;
            };
            if priority <= worst.priority {
                return ScheduleResult::RejectedFull;
            }
            self.pending.swap_remove(worst_index);
        }
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1);
        self.pending.push(Pending {
            key,
            priority,
            sequence,
        });
        ScheduleResult::Queued
    }

    /// Dispatch the highest-priority task if a worker slot is available.
    pub fn start_next(&mut self) -> Option<K> {
        if self.running.len() >= self.max_running || self.pending.is_empty() {
            return None;
        }
        let index = self
            .pending
            .iter()
            .enumerate()
            .max_by_key(|(_, item)| (item.priority, std::cmp::Reverse(item.sequence)))
            .map(|(index, _)| index)?;
        let item = self.pending.swap_remove(index);
        self.running.insert(item.key.clone());
        Some(item.key)
    }

    /// Release a worker slot. Returns false for an unknown/already-finished key.
    pub fn complete(&mut self, key: &K) -> bool {
        self.running.remove(key)
    }

    /// Remove queued work matching a predicate; running work is cooperative
    /// and must be cancelled by the execution layer using its cancellation token.
    pub fn cancel_pending(&mut self, mut should_cancel: impl FnMut(&K) -> bool) -> usize {
        let before = self.pending.len();
        self.pending.retain(|item| !should_cancel(&item.key));
        before - self.pending.len()
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub fn running_len(&self) -> usize {
        self.running.len()
    }

    pub fn contains_pending(&self, key: &K) -> bool {
        self.pending.iter().any(|item| &item.key == key)
    }

    pub fn is_running(&self, key: &K) -> bool {
        self.running.contains(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_task_preempts_prefetch_fifo() {
        let mut scheduler = TaskScheduler::new(8, 1);
        assert_eq!(scheduler.schedule(1, TaskPriority::Prefetch), ScheduleResult::Queued);
        assert_eq!(scheduler.schedule(2, TaskPriority::Current), ScheduleResult::Queued);
        assert_eq!(scheduler.start_next(), Some(2));
        assert_eq!(scheduler.start_next(), None);
        assert!(scheduler.complete(&2));
        assert_eq!(scheduler.start_next(), Some(1));
    }

    #[test]
    fn selecting_prefetched_item_promotes_without_duplicate() {
        let mut scheduler = TaskScheduler::new(8, 2);
        assert_eq!(scheduler.schedule(7, TaskPriority::Prefetch), ScheduleResult::Queued);
        assert_eq!(scheduler.schedule(7, TaskPriority::Current), ScheduleResult::Promoted);
        assert_eq!(scheduler.pending_len(), 1);
        assert_eq!(scheduler.start_next(), Some(7));
        assert_eq!(scheduler.schedule(7, TaskPriority::Current), ScheduleResult::AlreadyRunning);
    }

    #[test]
    fn pending_queue_is_bounded_and_evicts_lowest_priority() {
        let mut scheduler = TaskScheduler::new(2, 1);
        scheduler.schedule(1, TaskPriority::Prefetch);
        scheduler.schedule(2, TaskPriority::Roi);
        assert_eq!(scheduler.schedule(3, TaskPriority::Current), ScheduleResult::Queued);
        assert_eq!(scheduler.pending_len(), 2);
        assert!(!scheduler.contains_pending(&1));
        assert!(scheduler.contains_pending(&3));
        assert_eq!(scheduler.schedule(4, TaskPriority::Prefetch), ScheduleResult::RejectedFull);
    }

    #[test]
    fn stale_queued_tasks_can_be_removed() {
        let mut scheduler = TaskScheduler::new(8, 1);
        scheduler.schedule(10, TaskPriority::Prefetch);
        scheduler.schedule(11, TaskPriority::Current);
        assert_eq!(scheduler.cancel_pending(|key| *key == 10), 1);
        assert_eq!(scheduler.pending_len(), 1);
        assert_eq!(scheduler.start_next(), Some(11));
    }

    #[test]
    fn running_limit_is_hard() {
        let mut scheduler = TaskScheduler::new(8, 2);
        for key in 0..4 {
            scheduler.schedule(key, TaskPriority::Current);
        }
        assert!(scheduler.start_next().is_some());
        assert!(scheduler.start_next().is_some());
        assert_eq!(scheduler.running_len(), 2);
        assert_eq!(scheduler.start_next(), None);
    }
}
