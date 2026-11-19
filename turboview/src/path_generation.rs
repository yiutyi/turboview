//! Immutable path-index generations with stale-scan protection.
//!
//! A scan builds a fresh `PathStore` off to the side. Publishing swaps the active
//! generation under a short write lock; existing readers keep their old generation
//! alive through `Arc` snapshots. Handles carry a generation number so an ID from
//! one scan can never silently address an entry in another scan.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use crate::path_store::{PathId, PathParts, PathStore};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct GenerationId(u64);

impl GenerationId {
    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PathHandle {
    generation: GenerationId,
    id: PathId,
}

impl PathHandle {
    pub fn generation(self) -> GenerationId {
        self.generation
    }

    pub fn index(self) -> usize {
        self.id.index()
    }
}

struct Generation {
    id: GenerationId,
    store: PathStore,
}

/// A cheap, immutable reader view. Cloning it only increments an Arc reference count.
#[derive(Clone)]
pub struct GenerationSnapshot {
    inner: Arc<Generation>,
}

impl GenerationSnapshot {
    pub fn id(&self) -> GenerationId {
        self.inner.id
    }

    pub fn len(&self) -> usize {
        self.inner.store.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.store.is_empty()
    }

    pub fn directory_count(&self) -> usize {
        self.inner.store.directory_count()
    }

    pub fn storage_bytes(&self) -> usize {
        self.inner.store.storage_bytes()
    }

    pub fn handle_at(&self, index: usize) -> Option<PathHandle> {
        self.inner.store.id_at(index).map(|id| PathHandle {
            generation: self.inner.id,
            id,
        })
    }

    /// Returns borrowed path components without constructing a PathBuf.
    /// Handles from another generation are rejected.
    #[inline]
    pub fn parts(&self, handle: PathHandle) -> Option<PathParts<'_>> {
        if handle.generation != self.inner.id {
            return None;
        }
        self.inner.store.parts(handle.id)
    }

    /// Materializes into a caller-owned buffer, reusing its allocation.
    /// The buffer is unchanged for a stale or out-of-range handle.
    #[inline]
    pub fn get_into(&self, handle: PathHandle, target: &mut PathBuf) -> bool {
        if handle.generation != self.inner.id {
            return false;
        }
        self.inner.store.get_into(handle.id, target)
    }

    /// Returns None for out-of-range handles and handles from another generation.
    pub fn get(&self, handle: PathHandle) -> Option<PathBuf> {
        if handle.generation != self.inner.id {
            return None;
        }
        self.inner.store.get(handle.id)
    }

    /// Materialize by dense position when a generation-scoped handle is not retained.
    pub fn path_at(&self, index: usize) -> Option<PathBuf> {
        self.handle_at(index).and_then(|handle| self.get(handle))
    }
}

/// Mutable, unpublished index owned by one scan operation.
pub struct GenerationBuilder {
    id: GenerationId,
    store: PathStore,
}

impl GenerationBuilder {
    pub fn id(&self) -> GenerationId {
        self.id
    }

    pub fn insert(&mut self, path: &Path) -> PathId {
        self.store.insert(path)
    }

    pub fn len(&self) -> usize {
        self.store.len()
    }

    pub fn is_empty(&self) -> bool {
        self.store.is_empty()
    }

    pub fn sort_ids(&self, ids: &mut [PathId]) {
        self.store.sort_ids(ids);
    }
}

/// Coordinates background builds and atomic generation publication.
///
/// The most recently started scan wins. If an older, slower scan finishes after a
/// newer scan has started, its publish is rejected instead of rolling the index back.
pub struct PathIndexManager {
    next_generation: AtomicU64,
    latest_requested: AtomicU64,
    active: RwLock<Option<Arc<Generation>>>,
}

impl Default for PathIndexManager {
    fn default() -> Self {
        Self::new()
    }
}

impl PathIndexManager {
    pub fn new() -> Self {
        Self {
            next_generation: AtomicU64::new(1),
            latest_requested: AtomicU64::new(0),
            active: RwLock::new(None),
        }
    }

    /// Start a new off-thread build. Starting it immediately supersedes older builds.
    pub fn begin_generation(
        &self,
        images_capacity: usize,
        name_bytes_capacity: usize,
        directories_capacity: usize,
    ) -> GenerationBuilder {
        // Serialize scan-start and publish decisions so an older concurrent
        // begin call cannot overwrite a newer request or race the active swap.
        let id = {
            let _active = self.active.write().expect("path index lock poisoned");
            let id = self.next_generation.fetch_add(1, Ordering::Relaxed);
            self.latest_requested.store(id, Ordering::Release);
            id
        };
        GenerationBuilder {
            id: GenerationId(id),
            store: PathStore::with_capacity(
                images_capacity,
                name_bytes_capacity,
                directories_capacity,
            ),
        }
    }

    /// Publish in O(1) with respect to the number of paths. Old snapshots remain valid.
    pub fn publish(
        &self,
        builder: GenerationBuilder,
    ) -> Result<GenerationSnapshot, StaleGeneration> {
        let mut active = self.active.write().expect("path index lock poisoned");
        let latest = self.latest_requested.load(Ordering::Acquire);
        if builder.id.0 != latest {
            return Err(StaleGeneration {
                attempted: builder.id,
                latest: GenerationId(latest),
            });
        }

        let generation = Arc::new(Generation {
            id: builder.id,
            store: builder.store,
        });
        *active = Some(generation.clone());
        Ok(GenerationSnapshot { inner: generation })
    }

    /// Obtain a stable reader snapshot. No lock is held while the caller browses paths.
    pub fn snapshot(&self) -> Option<GenerationSnapshot> {
        self.active
            .read()
            .expect("path index lock poisoned")
            .as_ref()
            .cloned()
            .map(|inner| GenerationSnapshot { inner })
    }

    pub fn latest_requested(&self) -> Option<GenerationId> {
        match self.latest_requested.load(Ordering::Acquire) {
            0 => None,
            id => Some(GenerationId(id)),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StaleGeneration {
    pub attempted: GenerationId,
    pub latest: GenerationId,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add(builder: &mut GenerationBuilder, path: &str) {
        builder.insert(Path::new(path));
    }

    #[test]
    fn publishes_complete_generation_and_scoped_handles() {
        let manager = PathIndexManager::new();
        assert!(manager.snapshot().is_none());

        let mut build = manager.begin_generation(2, 64, 1);
        add(&mut build, r"C:\photos\a.jpg");
        add(&mut build, r"C:\photos\b.jpg");
        let snapshot = manager.publish(build).unwrap();

        assert_eq!(snapshot.len(), 2);
        assert_eq!(snapshot.path_at(0), Some(PathBuf::from(r"C:\photos\a.jpg")));
        let handle = snapshot.handle_at(1).unwrap();
        assert_eq!(snapshot.get(handle), Some(PathBuf::from(r"C:\photos\b.jpg")));
        assert_eq!(manager.snapshot().unwrap().id(), snapshot.id());
    }

    #[test]
    fn borrowed_parts_and_reused_buffer_match_materialized_path() {
        let manager = PathIndexManager::new();
        let mut build = manager.begin_generation(1, 32, 1);
        add(&mut build, r"C:\photos\sample.jpg");
        let snapshot = manager.publish(build).unwrap();
        let handle = snapshot.handle_at(0).unwrap();

        let parts = snapshot.parts(handle).unwrap();
        assert_eq!(parts.directory, Path::new(r"C:\photos"));
        assert_eq!(parts.file_name, std::ffi::OsStr::new("sample.jpg"));

        let mut buffer = PathBuf::from(r"X:\long\buffer\placeholder.bin");
        assert!(snapshot.get_into(handle, &mut buffer));
        assert_eq!(buffer, PathBuf::from(r"C:\\photos\\sample.jpg"));
    }

    #[test]
    fn rejects_handle_from_another_generation() {
        let manager = PathIndexManager::new();
        let mut first = manager.begin_generation(1, 32, 1);
        add(&mut first, r"C:\old\a.jpg");
        let old = manager.publish(first).unwrap();
        let old_handle = old.handle_at(0).unwrap();

        let mut second = manager.begin_generation(1, 32, 1);
        add(&mut second, r"D:\new\b.jpg");
        let new = manager.publish(second).unwrap();

        assert_ne!(old.id(), new.id());
        assert!(new.get(old_handle).is_none());
        assert_eq!(old.get(old_handle), Some(PathBuf::from(r"C:\old\a.jpg")));
    }

    #[test]
    fn slower_older_scan_cannot_overwrite_newer_scan() {
        let manager = PathIndexManager::new();
        let mut slow = manager.begin_generation(1, 32, 1);
        add(&mut slow, r"C:\slow\a.jpg");
        let mut fast = manager.begin_generation(1, 32, 1);
        add(&mut fast, r"C:\fast\b.jpg");

        let error = match manager.publish(slow) {
            Err(error) => error,
            Ok(_) => panic!("older generation must not publish"),
        };
        assert_eq!(error.attempted, GenerationId(1));
        assert_eq!(error.latest, GenerationId(2));

        let active = manager.publish(fast).unwrap();
        assert_eq!(active.id(), GenerationId(2));
        assert_eq!(active.path_at(0), Some(PathBuf::from(r"C:\fast\b.jpg")));
    }

    #[test]
    fn old_generation_is_reclaimed_after_last_snapshot_drops() {
        let manager = PathIndexManager::new();
        let mut first = manager.begin_generation(1, 32, 1);
        add(&mut first, r"C:\old\a.jpg");
        let old_snapshot = manager.publish(first).unwrap();
        let weak = Arc::downgrade(&old_snapshot.inner);

        let mut second = manager.begin_generation(1, 32, 1);
        add(&mut second, r"C:\new\b.jpg");
        let _new_snapshot = manager.publish(second).unwrap();

        // The caller's snapshot intentionally keeps the old immutable index alive.
        assert!(weak.upgrade().is_some());
        drop(old_snapshot);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn empty_generation_can_be_published() {
        let manager = PathIndexManager::new();
        let build = manager.begin_generation(0, 0, 0);
        let snapshot = manager.publish(build).unwrap();
        assert!(snapshot.is_empty());
        assert!(snapshot.handle_at(0).is_none());
    }
}
