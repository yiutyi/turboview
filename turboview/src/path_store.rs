//! Compact, interned path storage prototype.
//!
//! This is deliberately isolated from ImageViewer until correctness and benchmarks
//! justify integration. Each image record stores a directory ID and a range in a
//! shared filename arena. Full PathBuf values are materialized only on demand.
//!
//! Ordering uses precomputed byte sort keys that reproduce `std::path::Path`
//! ordering exactly, without materializing any `PathBuf` during comparison.
//! See `encode_component` for why the byte layout matches `Path::cmp`.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct PathId(u32);

impl PathId {
    /// Returns the dense storage index without allocating or materializing a path.
    #[inline]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Clone, Copy, Debug)]
struct ImageRecord {
    directory: u32,
    /// Range into `name_keys`: `[TAG_NORMAL][WTF-8/raw bytes][0x00]`.
    /// This doubles as the comparison key and the lossless file-name store.
    name_key: RangeU32,
}

#[derive(Clone, Copy, Debug)]
struct RangeU32 {
    start: u32,
    len: u32,
}

impl RangeU32 {
    #[inline]
    fn as_range(self) -> std::ops::Range<usize> {
        let start = self.start as usize;
        start..start + self.len as usize
    }
}

/// Interns directory prefixes and stores file names contiguously.
#[derive(Default)]
pub struct PathStore {
    directories: Vec<Arc<Path>>,
    directory_ids: HashMap<Arc<Path>, u32>,
    images: Vec<ImageRecord>,
    /// One encoded component stream per directory (shared across all its images).
    directory_keys: Vec<Vec<u8>>,
    /// Concatenated sort keys for the trailing file-name component. Each is
    /// `[TAG_NORMAL][as_encoded_bytes][0x00]`, which is also the lossless
    /// on-disk byte form used by `get()` — no separate UTF-16 arena is needed.
    name_keys: Vec<u8>,
}

impl PathStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Preallocates the image/name arenas when a scan already knows its entry count.
    /// `name_bytes` is an estimate of total encoded file-name bytes.
    pub fn with_capacity(images: usize, name_bytes: usize, directories: usize) -> Self {
        Self {
            directories: Vec::with_capacity(directories),
            directory_ids: HashMap::with_capacity(directories),
            images: Vec::with_capacity(images),
            directory_keys: Vec::with_capacity(directories),
            name_keys: Vec::with_capacity(name_bytes),
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
            let mut key = Vec::new();
            encode_directory(&mut key, parent);
            self.directory_keys.push(key);
            self.directories.push(shared.clone());
            self.directory_ids.insert(shared, id);
            id
        };

        let name = path.file_name().unwrap_or(path.as_os_str());
        // Encode once into name_keys: this byte form serves both comparison and
        // lossless get(). No separate UTF-16 arena allocation.
        let key_start = self.name_keys.len() as u32;
        encode_name(&mut self.name_keys, name);
        let name_key = RangeU32 {
            start: key_start,
            len: self.name_keys.len() as u32 - key_start,
        };

        let id = PathId(self.images.len() as u32);
        self.images.push(ImageRecord { directory, name_key });
        id
    }

    /// Materializes a path into a caller-owned buffer, reusing its allocation.
    ///
    /// This is intended for sequential consumers (for example, a single image
    /// decoder/navigation worker) that need an owned `PathBuf` only temporarily.
    /// The buffer is unchanged when `id` is invalid.
    #[inline]
    pub fn get_into(&self, id: PathId, target: &mut PathBuf) -> bool {
        let Some(record) = self.images.get(id.0 as usize) else {
            return false;
        };
        let Some(directory) = self.directories.get(record.directory as usize) else {
            return false;
        };
        let Some(key) = self.name_keys.get(record.name_key.as_range()) else {
            return false;
        };

        // key layout: [TAG_NORMAL][payload][0x00]
        debug_assert_eq!(key.first(), Some(&TAG_NORMAL));
        debug_assert_eq!(key.last(), Some(&0));
        let name = if key.len() >= 2 {
            // SAFETY: these bytes were produced by `OsStr::as_encoded_bytes()`
            // in `encode_name`, so they are a valid encoded form on this platform.
            unsafe { OsStr::from_encoded_bytes_unchecked(&key[1..key.len() - 1]) }
        } else {
            OsStr::new("")
        };

        // Clear and append both components into the existing PathBuf allocation.
        target.clear();
        target.push(directory.as_ref());
        target.push(name);
        true
    }

    /// Returns an independently owned path. Use `get_into` for repeated reads
    /// where one caller-owned buffer can be reused.
    #[inline]
    pub fn get(&self, id: PathId) -> Option<PathBuf> {
        let mut path = PathBuf::new();
        self.get_into(id, &mut path).then_some(path)
    }

    /// Exact `std::path::Path` ordering, computed from precomputed byte sort keys.
    /// No `PathBuf`/`OsString` is allocated and no UTF-16 transcoding happens here.
    #[inline]
    pub fn cmp(&self, a: PathId, b: PathId) -> Ordering {
        let ra = &self.images[a.0 as usize];
        let rb = &self.images[b.0 as usize];
        let da = self.directory_keys[ra.directory as usize].as_slice();
        let db = self.directory_keys[rb.directory as usize].as_slice();
        let na = &self.name_keys[ra.name_key.as_range()];
        let nb = &self.name_keys[rb.name_key.as_range()];
        compare_parts(da, na, db, nb)
    }

    pub fn sort_ids(&self, ids: &mut [PathId]) {
        ids.sort_unstable_by(|a, b| self.cmp(*a, *b));
    }

    /// Approximate heap capacity, including the directory hash table and directory strings.
    /// Allocator metadata/alignment are implementation-specific and are not included.
    pub fn storage_bytes(&self) -> usize {
        let directory_strings: usize = self.directories.iter().map(|p| p.as_os_str().len()).sum();
        let directory_key_bytes: usize = self.directory_keys.iter().map(Vec::capacity).sum();
        self.name_keys.capacity()
            + directory_key_bytes
            + self.images.capacity() * std::mem::size_of::<ImageRecord>()
            + self.directories.capacity() * std::mem::size_of::<Arc<Path>>()
            + self.directory_ids.capacity() * std::mem::size_of::<(Arc<Path>, u32)>() * 2
            + directory_strings
    }
}

// --- Sort key encoding -------------------------------------------------------
//
// `Path::cmp` delegates to `compare_components`, which compares the path's
// `Component`s one by one (`Iterator::cmp`). A `Component`'s `Ord` is derived and
// therefore ordered first by enum discriminant, then by its payload:
//
//   Prefix(0) < RootDir(1) < CurDir(2) < ParentDir(3) < Normal(4)
//
// `Normal` payload is `&OsStr`, whose `Ord` is `as_encoded_bytes().cmp()`.
// `Prefix` payload is `PrefixComponent`, whose `Ord` compares its `Prefix` enum
// (Verbatim < VerbatimUNC < VerbatimDisk < DeviceNS < UNC < Disk), then fields.
//
// We encode each component as `[tag][payload][0x00]`. Because a path can never
// contain a NUL byte, 0x00 is a safe terminator and is strictly smaller than any
// valid payload byte, so a shorter payload sorts before a longer one that has it
// as a prefix — exactly matching `OsStr`/slice ordering. Byte-comparing the
// concatenated stream is therefore identical to comparing the component list.

const TAG_PREFIX: u8 = 0;
const TAG_ROOT: u8 = 1;
const TAG_CURDIR: u8 = 2;
const TAG_PARENTDIR: u8 = 3;
const TAG_NORMAL: u8 = 4;

#[inline]
fn push_component(dst: &mut Vec<u8>, tag: u8, payload: &[u8]) {
    dst.push(tag);
    dst.extend_from_slice(payload);
    dst.push(0);
}

fn encode_prefix(dst: &mut Vec<u8>, p: Prefix<'_>) {
    match p {
        Prefix::Verbatim(s) => {
            dst.push(0);
            dst.extend_from_slice(s.as_encoded_bytes());
        }
        Prefix::VerbatimUNC(a, b) => {
            dst.push(1);
            dst.extend_from_slice(a.as_encoded_bytes());
            dst.push(0);
            dst.extend_from_slice(b.as_encoded_bytes());
        }
        Prefix::VerbatimDisk(d) => {
            dst.push(2);
            dst.push(d);
        }
        Prefix::DeviceNS(s) => {
            dst.push(3);
            dst.extend_from_slice(s.as_encoded_bytes());
        }
        Prefix::UNC(a, b) => {
            dst.push(4);
            dst.extend_from_slice(a.as_encoded_bytes());
            dst.push(0);
            dst.extend_from_slice(b.as_encoded_bytes());
        }
        Prefix::Disk(d) => {
            dst.push(5);
            dst.push(d);
        }
    }
}

fn encode_directory(dst: &mut Vec<u8>, path: &Path) {
    for c in path.components() {
        match c {
            Component::Prefix(p) => {
                let mut payload = Vec::new();
                encode_prefix(&mut payload, p.kind());
                push_component(dst, TAG_PREFIX, &payload);
            }
            Component::RootDir => push_component(dst, TAG_ROOT, &[]),
            Component::CurDir => push_component(dst, TAG_CURDIR, &[]),
            Component::ParentDir => push_component(dst, TAG_PARENTDIR, &[]),
            Component::Normal(s) => push_component(dst, TAG_NORMAL, s.as_encoded_bytes()),
        }
    }
}

#[inline]
fn encode_name(dst: &mut Vec<u8>, name: &OsStr) {
    push_component(dst, TAG_NORMAL, name.as_encoded_bytes());
}

/// Byte-compare the virtual concatenation `a_first ++ a_second` with
/// `b_first ++ b_second` without allocating. This is used to compare a full path
/// key (directory component stream + trailing name component).
#[inline]
fn compare_parts(a_first: &[u8], a_second: &[u8], b_first: &[u8], b_second: &[u8]) -> Ordering {
    let a_len = a_first.len() + a_second.len();
    let b_len = b_first.len() + b_second.len();
    let common = a_len.min(b_len);
    for i in 0..common {
        let ab = if i < a_first.len() {
            a_first[i]
        } else {
            a_second[i - a_first.len()]
        };
        let bb = if i < b_first.len() {
            b_first[i]
        } else {
            b_second[i - b_first.len()]
        };
        match ab.cmp(&bb) {
            Ordering::Equal => continue,
            ord => return ord,
        }
    }
    a_len.cmp(&b_len)
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

    /// Insert `paths`, sort ids via PathStore, and compare with PathBuf ordering.
    fn assert_same_order(paths: &[PathBuf]) {
        let mut store = PathStore::with_capacity(paths.len(), paths.len() * 20, 4096);
        let ids: Vec<_> = paths.iter().map(|p| store.insert(p)).collect();
        let mut actual = ids.clone();
        store.sort_ids(&mut actual);
        let mut expected = ids;
        expected.sort_unstable_by(|a, b| paths[a.index()].cmp(&paths[b.index()]));
        let actual_paths: Vec<_> = actual.iter().map(|id| store.get(*id).unwrap()).collect();
        let expected_paths: Vec<_> = expected.iter().map(|id| paths[id.index()].clone()).collect();
        assert_eq!(actual_paths, expected_paths);
    }

    #[test]
    fn ordering_matches_path_buf_cmp_for_prefix_edges() {
        let paths: Vec<PathBuf> = [
            r"C:\foo\bar.jpg",
            r"C:\foo.txt",
            r"C:\foo\a.jpg",
            r"C:\foo",
            r"C:\foo\bar",
            r"C:\Foo\z.jpg",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        assert_same_order(&paths);
    }

    #[cfg(windows)]
    #[test]
    fn ordering_matches_path_buf_across_prefix_variants() {
        // Disk, VerbatimDisk and UNC prefixes exercise the `Prefix` enum branch
        // of the sort key, whose order is NOT the as_os_str() byte order.
        let source: Vec<PathBuf> = [
            r"C:\a.jpg",
            r"D:\z.jpg",
            r"\\?\C:\m.jpg",
            r"\\?\D:\m.jpg",
            r"\\server\share\a.jpg",
            r"\\server\share\b.jpg",
            r"\\localserver\share\a.jpg",
            r"C:\A.jpg",
            r"C:\a.b.jpg",
            r"C:\a\b.jpg",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        assert_same_order(&source);
    }

    #[cfg(windows)]
    #[test]
    fn preserves_unpaired_utf16_filename_units() {
        use std::ffi::OsString;
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
        let duplicate = source[17].clone();
        source.extend([
            PathBuf::from(r"D:\图库"),
            PathBuf::from(r"D:\图库\000"),
            PathBuf::from(r"D:\图库\000\a.jpg"),
            duplicate.clone(),
            duplicate,
        ]);
        assert_same_order(&source);
    }

    /// Component-boundary vs payload: "foo" then "bar" must not be confused with
    /// "foo.bar". This catches encodings that ignore the component tag/terminator.
    #[test]
    fn ordering_respects_component_boundaries() {
        let mut source = Vec::new();
        for a in ["a", "a.b", "a b", "a-b", "ab", "a0"] {
            for b in ["b", "b.jpg", "c"] {
                source.push(PathBuf::from(format!("R:\\{a}\\{b}")));
                source.push(PathBuf::from(format!("R:\\{a}.{b}")));
            }
        }
        source.push(PathBuf::from("R:\\a"));
        source.push(PathBuf::from("R:\\a\\"));
        assert_same_order(&source);
    }

    /// Nested-directory prefix relationship: a directory that is itself a path
    /// prefix of another directory must still order by the next component (the
    /// file name), not merely by directory length.
    #[test]
    fn ordering_handles_nested_directory_prefixes() {
        let source: Vec<PathBuf> = [
            r"R:\foo\z.jpg",
            r"R:\foo\bar\a.jpg",
            r"R:\foo.txt",
            r"R:\foo",
            r"R:\foo\a.jpg",
            r"R:\foo\bar",
            r"R:\foo\bar\a.b.jpg",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        assert_same_order(&source);
    }

    /// Deterministic pseudo-random fuzz over path-shaped byte strings, using the
    /// platform's own separator so both Unix and Windows component parsing apply.
    #[test]
    fn ordering_matches_path_buf_for_pseudo_random_paths() {
        let sep = std::path::MAIN_SEPARATOR;
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let alphabet = b"abcXYZ019._-";
        let mut source = Vec::new();
        for _ in 0..6000 {
            let depth = (next() % 4) as usize;
            let mut s = String::new();
            if cfg!(windows) {
                s.push('R');
                s.push(':');
                s.push(sep);
            } else {
                s.push(sep);
            }
            for _ in 0..depth {
                let len = 1 + (next() % 5) as usize;
                for _ in 0..len {
                    s.push(alphabet[(next() % alphabet.len() as u64) as usize] as char);
                }
                s.push(sep);
            }
            let len = 1 + (next() % 8) as usize;
            for _ in 0..len {
                s.push(alphabet[(next() % alphabet.len() as u64) as usize] as char);
            }
            s.push_str(".jpg");
            source.push(PathBuf::from(s));
        }
        assert_same_order(&source);
    }

    #[test]
    fn get_into_reuses_buffer_and_preserves_contents_on_invalid_id() {
        let mut store = PathStore::new();
        let first = store.insert(Path::new(r"C:\photos\one.jpg"));
        let second = store.insert(Path::new(r"D:\archive\nested\two.png"));
        let mut buffer = PathBuf::from(r"X:\an-existing-long-buffer\placeholder.bin");
        let initial_capacity = buffer.capacity();

        assert!(store.get_into(first, &mut buffer));
        assert!(buffer.capacity() >= initial_capacity, "reusable buffer should retain its capacity");
        assert_eq!(buffer, PathBuf::from(r"C:\photos\one.jpg"));
        assert!(store.get_into(second, &mut buffer));
        assert_eq!(buffer, PathBuf::from(r"D:\archive\nested\two.png"));

        let before = buffer.clone();
        assert!(!store.get_into(PathId(u32::MAX), &mut buffer));
        assert_eq!(buffer, before);
    }

    #[test]
    fn dense_index_returns_stable_ids_without_path_materialization() {
        let mut store = PathStore::new();
        let first = store.insert(Path::new(r"D:\a\one.jpg"));
        let second = store.insert(Path::new(r"D:\a\two.jpg"));
        assert_eq!(store.id_at(0), Some(first));
        assert_eq!(store.id_at(1), Some(second));
        assert_eq!(store.id_at(2), None);
        assert_eq!(first.index(), 0);
        assert_eq!(second.index(), 1);
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
        // 11-byte names + 2 tag/terminator bytes each = ~13 MB of sort keys.
        let mut store = PathStore::with_capacity(1_000_000, 14_000_000, 1);
        for i in 0..1_000_000u32 {
            let name = format!(r"X:\images\{i:07}.jpg");
            store.insert(Path::new(&name));
        }
        assert_eq!(store.len(), 1_000_000);
        assert_eq!(store.directory_count(), 1);
        assert!(store.storage_bytes() < 48 * 1024 * 1024);
    }
}
