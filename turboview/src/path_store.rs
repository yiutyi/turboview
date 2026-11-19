//! Compact, interned path storage prototype.
//!
//! This is deliberately isolated from ImageViewer until correctness and benchmarks
//! justify integration. Each image record stores a directory ID and a range in a
//! shared UTF-16 filename arena. Full PathBuf values are materialized only on demand.
//!
//! Ordering currently materializes paths before comparing. This is a correctness-first
//! baseline; replacing it with a component-aware comparator requires parity benchmarks.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct PathId(u32);

#[derive(Clone, Copy, Debug)]
struct ImageRecord {
    directory: u32,
    name: RangeU32,
}

#[derive(Clone, Copy, Debug)]
struct RangeU32 {
    start: u32,
    len: u32,
}

/// Interns directory prefixes and stores file names contiguously.
#[derive(Default)]
pub struct PathStore {
    directories: Vec<Arc<Path>>,
    directory_ids: HashMap<Arc<Path>, u32>,
    names: Vec<u16>,
    images: Vec<ImageRecord>,
}

impl PathStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Preallocates the image/name arenas when a scan already knows its entry count.
    pub fn with_capacity(images: usize, name_units: usize, directories: usize) -> Self {
        Self {
            directories: Vec::with_capacity(directories),
            directory_ids: HashMap::with_capacity(directories),
            names: Vec::with_capacity(name_units),
            images: Vec::with_capacity(images),
        }
    }

    pub fn len(&self) -> usize {
        self.images.len()
    }

    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }

    pub fn directory_count(&self) -> usize {
        self.directories.len()
    }

    /// Returns the stable ID at a dense list position without materializing a path.
    /// This is the primitive used by an ImageViewer list backed by PathStore.
    #[inline]
    pub fn id_at(&self, index: usize) -> Option<PathId> {
        (index < self.images.len()).then_some(PathId(index as u32))
    }

    /// Adds a path and returns its stable ID. Duplicate paths are allowed as
    /// separate image entries; identical parent directories share one allocation.
    pub fn insert(&mut self, path: &Path) -> PathId {
        let parent = path.parent().unwrap_or_else(|| Path::new(""));
        let directory = if let Some(&id) = self.directory_ids.get(parent) {
            id
        } else {
            let id = self.directories.len() as u32;
            let shared: Arc<Path> = Arc::from(parent.to_path_buf());
            self.directories.push(shared.clone());
            self.directory_ids.insert(shared, id);
            id
        };

        let name = path.file_name().unwrap_or(path.as_os_str());
        let range = append_os(&mut self.names, name);
        let id = PathId(self.images.len() as u32);
        self.images.push(ImageRecord {
            directory,
            name: range,
        });
        id
    }

    pub fn get(&self, id: PathId) -> Option<PathBuf> {
        let record = *self.images.get(id.0 as usize)?;
        let directory = self.directories.get(record.directory as usize)?;
        let start = record.name.start as usize;
        let end = start.checked_add(record.name.len as usize)?;
        let name = decode_os(self.names.get(start..end)?);
        let mut path = directory.to_path_buf();
        path.push(name);
        Some(path)
    }

    /// Exact std::path::Path ordering parity, intentionally using materialization
    /// until a faster component-aware comparator is proven equivalent.
    pub fn cmp(&self, a: PathId, b: PathId) -> std::cmp::Ordering {
        match (self.get(a), self.get(b)) {
            (Some(a), Some(b)) => a.cmp(&b),
            (None, None) => std::cmp::Ordering::Equal,
            (None, Some(_)) => std::cmp::Ordering::Less,
            (Some(_), None) => std::cmp::Ordering::Greater,
        }
    }

    pub fn sort_ids(&self, ids: &mut [PathId]) {
        ids.sort_unstable_by(|a, b| self.cmp(*a, *b));
    }

    /// Approximate heap capacity, including the directory hash table and directory strings.
    /// Allocator metadata/alignment are implementation-specific and are not included.
    pub fn storage_bytes(&self) -> usize {
        let directory_strings: usize = self.directories.iter().map(|p| p.as_os_str().len()).sum();
        self.names.capacity() * std::mem::size_of::<u16>()
            + self.images.capacity() * std::mem::size_of::<ImageRecord>()
            + self.directories.capacity() * std::mem::size_of::<Arc<Path>>()
            + self.directory_ids.capacity() * std::mem::size_of::<(Arc<Path>, u32)>() * 2
            + directory_strings
    }
}

/// Append directly into the arena: the Windows hot path performs no per-name Vec allocation.
#[cfg(windows)]
fn append_os(dst: &mut Vec<u16>, value: &std::ffi::OsStr) -> RangeU32 {
    use std::os::windows::ffi::OsStrExt;
    let start = dst.len() as u32;
    dst.extend(value.encode_wide());
    RangeU32 { start, len: (dst.len() as u32).wrapping_sub(start) }
}

#[cfg(not(windows))]
fn append_os(dst: &mut Vec<u16>, value: &std::ffi::OsStr) -> RangeU32 {
    let start = dst.len() as u32;
    dst.extend(value.to_string_lossy().encode_utf16());
    RangeU32 { start, len: (dst.len() as u32).wrapping_sub(start) }
}

#[cfg(windows)]
fn decode_os(value: &[u16]) -> OsString {
    use std::os::windows::ffi::OsStringExt;
    OsString::from_wide(value)
}

#[cfg(not(windows))]
fn decode_os(value: &[u16]) -> OsString {
    OsString::from(String::from_utf16_lossy(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_shared_directories() {
        let mut store = PathStore::new();
        let a = store.insert(Path::new(r"C:\photos\猫.jpg"));
        let b = store.insert(Path::new(r"C:\photos\旅行.png"));
        assert_eq!(store.get(a).unwrap(), PathBuf::from(r"C:\photos\猫.jpg"));
        assert_eq!(store.get(b).unwrap(), PathBuf::from(r"C:\photos\旅行.png"));
        assert_eq!(store.directory_count(), 1);
    }

    #[test]
    fn ordering_matches_path_buf_cmp_for_prefix_edges() {
        let paths = [
            r"C:\foo\bar.jpg",
            r"C:\foo.txt",
            r"C:\foo\a.jpg",
            r"C:\foo",
            r"C:\foo\bar",
            r"C:\Foo\z.jpg",
        ];
        let mut store = PathStore::new();
        let ids: Vec<_> = paths.iter().map(|p| store.insert(Path::new(p))).collect();
        let mut actual = ids.clone();
        store.sort_ids(&mut actual);
        let mut expected = paths.iter().map(PathBuf::from).collect::<Vec<_>>();
        expected.sort_unstable();
        let actual_paths = actual.iter().map(|id| store.get(*id).unwrap()).collect::<Vec<_>>();
        assert_eq!(actual_paths, expected);
    }

    #[cfg(windows)]
    #[test]
    fn preserves_unpaired_utf16_filename_units() {
        use std::os::windows::ffi::OsStringExt;
        let odd_name = OsString::from_wide(&[b'x' as u16, 0xD800, b'.' as u16, b'j' as u16]);
        let path = PathBuf::from(r"C:\photos").join(odd_name);
        let mut store = PathStore::new();
        let id = store.insert(&path);
        assert_eq!(store.get(id).unwrap(), path);
    }

    #[test]
    fn ordering_matches_path_buf_for_deterministic_large_mixed_set() {
        let mut source = Vec::new();
        for i in 0..4096usize {
            let dir = (i.wrapping_mul(73)) % 257;
            let name = match i % 7 {
                0 => format!("IMG_{i:08}.jpg"),
                1 => format!("猫_{i:06}.png"),
                2 => format!("a.b.{i:05}.webp"),
                3 => format!("_{i:07}.tif"),
                4 => format!("Z-{i:06}.bmp"),
                5 => format!("旅行-{i:05}.gif"),
                _ => format!("x{i:08}.jpeg"),
            };
            source.push(PathBuf::from(format!(r"D:\图库\{dir:03}\{name}")));
        }
        // Include duplicates and component-prefix edge cases.
        let duplicate = source[17].clone();
        source.extend([
            PathBuf::from(r"D:\图库"),
            PathBuf::from(r"D:\图库\000"),
            PathBuf::from(r"D:\图库\000\a.jpg"),
            duplicate.clone(),
            duplicate,
        ]);

        let mut store = PathStore::with_capacity(source.len(), source.len() * 20, 258);
        let ids: Vec<_> = source.iter().map(|p| store.insert(p)).collect();
        let mut actual = ids;
        store.sort_ids(&mut actual);
        let mut expected = source;
        expected.sort_unstable();

        let actual_paths: Vec<_> = actual.iter().map(|id| store.get(*id).unwrap()).collect();
        assert_eq!(actual_paths, expected);
    }

    #[test]
    fn dense_index_returns_stable_ids_without_path_materialization() {
        let mut store = PathStore::new();
        let first = store.insert(Path::new(r"D:\a\one.jpg"));
        let second = store.insert(Path::new(r"D:\a\two.jpg"));
        assert_eq!(store.id_at(0), Some(first));
        assert_eq!(store.id_at(1), Some(second));
        assert_eq!(store.id_at(2), None);
    }

    #[test]
    fn ids_remain_stable_after_more_inserts() {
        let mut store = PathStore::new();
        let first = store.insert(Path::new(r"D:\a\one.jpg"));
        for i in 0..100 {
            store.insert(Path::new(format!(r"D:\b\{i}.jpg").as_str()));
        }
        assert_eq!(store.get(first).unwrap(), PathBuf::from(r"D:\a\one.jpg"));
    }

    #[test]
    #[ignore = "large synthetic memory smoke test"]
    fn million_entry_synthetic_storage_smoke() {
        // Kept ignored in normal CI; run explicitly with
        // cargo test million_entry_synthetic_storage_smoke -- --ignored.
        let mut store = PathStore::with_capacity(1_000_000, 11_000_000, 1);
        for i in 0..1_000_000u32 {
            let name = format!(r"X:\images\{i:07}.jpg");
            store.insert(Path::new(&name));
        }
        assert_eq!(store.len(), 1_000_000);
        assert_eq!(store.directory_count(), 1);
        assert!(store.storage_bytes() < 40 * 1024 * 1024);
    }
}
