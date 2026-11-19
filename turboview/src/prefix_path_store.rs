//! Prefix-interned path storage prototype (v2).
use std::cmp::Ordering;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf, Prefix};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct PrefixPathId(u32);
impl PrefixPathId { pub fn index(self) -> usize { self.0 as usize } }
#[derive(Clone, Copy)] struct Range { start: u32, len: u32 }
impl Range { fn slice(self) -> std::ops::Range<usize> { self.start as usize..self.start as usize + self.len as usize } }
#[derive(Clone, Copy)] struct Node { parent: u32, bytes: Range, raw: Range, depth: u32 }
#[derive(Clone, Copy)] struct Image { dir: u32, name: Range }

/// Shares each directory component by its parent node, rather than interning whole parents.
#[derive(Default)]
pub struct PrefixPathStore {
    nodes: Vec<Node>, components: Vec<u8>, raw_components: Vec<u8>,
    intern: HashMap<(u32, u64), u32>,
    collisions: HashMap<(u32, u64), Vec<u32>>,
    images: Vec<Image>, names: Vec<u8>,
}
impl PrefixPathStore {
    pub fn with_capacity(images: usize, names: usize, dirs: usize) -> Self {
        let mut nodes = Vec::with_capacity(dirs.saturating_add(1));
        nodes.push(Node { parent: 0, bytes: Range { start: 0, len: 0 }, raw: Range { start: 0, len: 0 }, depth: 0 });
        Self { nodes, components: Vec::with_capacity(dirs.saturating_mul(16)), raw_components: Vec::with_capacity(dirs.saturating_mul(16)),
            intern: HashMap::with_capacity(dirs), collisions: HashMap::new(),
            images: Vec::with_capacity(images), names: Vec::with_capacity(names) }
    }
    pub fn len(&self) -> usize { self.images.len() }
    pub fn directory_count(&self) -> usize { self.nodes.len() - 1 }
    fn bytes(&self, id: u32) -> &[u8] { let n = self.nodes[id as usize]; &self.components[n.bytes.slice()] }
    pub fn insert(&mut self, path: &Path) -> PrefixPathId {
        let mut parent = 0u32;
        let mut key_bytes = Vec::with_capacity(64);
        for c in path.parent().unwrap_or_else(|| Path::new("")).components() {
            key_bytes.clear(); encode_component(&mut key_bytes, c);
            let hash = hash64(&key_bytes); let key = (parent, hash);
            let first = self.intern.get(&key).copied();
            let found = if let Some(id) = first {
                if self.bytes(id) == key_bytes { Some(id) } else {
                    self.collisions.get(&key).and_then(|v| v.iter().copied().find(|&x| self.bytes(x) == key_bytes))
                }
            } else { None };
            parent = if let Some(id) = found { id } else {
                let id = self.nodes.len() as u32; let start = self.components.len() as u32;
                self.components.extend_from_slice(&key_bytes);
                let raw_start = self.raw_components.len() as u32;
                self.raw_components.extend_from_slice(c.as_os_str().as_encoded_bytes());
                let raw = Range { start: raw_start, len: self.raw_components.len() as u32 - raw_start };
                let depth = self.nodes[parent as usize].depth + 1;
                self.nodes.push(Node { parent, bytes: Range { start, len: key_bytes.len() as u32 }, raw, depth });
                if let Some(first_id) = first { self.collisions.entry(key).or_insert_with(|| vec![first_id]).push(id); }
                else { self.intern.insert(key, id); } id
            };
        }
        let name = path.file_name().unwrap_or(path.as_os_str()); let start = self.names.len() as u32;
        self.names.push(4); self.names.extend_from_slice(name.as_encoded_bytes()); self.names.push(0);
        let id = PrefixPathId(self.images.len() as u32);
        self.images.push(Image { dir: parent, name: Range { start, len: self.names.len() as u32 - start } }); id
    }
    fn ancestor(&self, mut id: u32, depth: u32) -> u32 { while self.nodes[id as usize].depth > depth { id = self.nodes[id as usize].parent; } id }
    fn component<'a>(&'a self, image: &'a Image, index: u32) -> &'a [u8] {
        let depth = self.nodes[image.dir as usize].depth;
        if index < depth { self.bytes(self.ancestor(image.dir, index + 1)) } else { &self.names[image.name.slice()] }
    }
    pub fn cmp(&self, a: PrefixPathId, b: PrefixPathId) -> Ordering {
        let a = &self.images[a.0 as usize]; let b = &self.images[b.0 as usize];
        let da = self.nodes[a.dir as usize].depth; let db = self.nodes[b.dir as usize].depth;
        for i in 0..=da.min(db) { match self.component(a, i).cmp(self.component(b, i)) { Ordering::Equal => (), x => return x } }
        (da + 1).cmp(&(db + 1))
    }
    pub fn sort_ids(&self, ids: &mut [PrefixPathId]) { ids.sort_unstable_by(|a,b| self.cmp(*a,*b)); }
    pub fn get(&self, id: PrefixPathId) -> Option<PathBuf> {
        let image = *self.images.get(id.0 as usize)?;
        let mut stack = Vec::with_capacity(self.nodes[image.dir as usize].depth as usize);
        let mut node = image.dir;
        while node != 0 {
            stack.push(node);
            node = self.nodes[node as usize].parent;
        }
        let mut path = PathBuf::new();
        for node in stack.into_iter().rev() {
            let raw = &self.raw_components[self.nodes[node as usize].raw.slice()];
            let component = unsafe { OsStr::from_encoded_bytes_unchecked(raw) };
            path.push(component);
        }
        let name = &self.names[image.name.slice()];
        let payload = &name[1..name.len() - 1];
        let name = unsafe { OsStr::from_encoded_bytes_unchecked(payload) };
        path.push(name);
        Some(path)
    }
    pub fn storage_bytes(&self) -> usize {
        self.components.capacity() + self.raw_components.capacity() + self.names.capacity() + self.nodes.capacity() * std::mem::size_of::<Node>()
        + self.images.capacity() * std::mem::size_of::<Image>()
        + self.intern.capacity() * (std::mem::size_of::<(u32,u64)>() + 4) * 2
        + self.collisions.values().map(|v| v.capacity() * 4).sum::<usize>()
    }
}
fn hash64(bytes: &[u8]) -> u64 { let mut h=0xcbf29ce484222325u64; for &b in bytes { h ^= b as u64; h=h.wrapping_mul(0x100000001b3); } h }
fn encode_component(dst: &mut Vec<u8>, c: Component<'_>) {
    match c {
        Component::Prefix(p) => {
            dst.push(0);
            match p.kind() {
                Prefix::Verbatim(s) => { dst.push(0); dst.extend_from_slice(s.as_encoded_bytes()); }
                Prefix::VerbatimUNC(a,b) => { dst.push(1); dst.extend_from_slice(a.as_encoded_bytes()); dst.push(0); dst.extend_from_slice(b.as_encoded_bytes()); }
                Prefix::VerbatimDisk(d) => { dst.push(2); dst.push(d); }
                Prefix::DeviceNS(s) => { dst.push(3); dst.extend_from_slice(s.as_encoded_bytes()); }
                Prefix::UNC(a,b) => { dst.push(4); dst.extend_from_slice(a.as_encoded_bytes()); dst.push(0); dst.extend_from_slice(b.as_encoded_bytes()); }
                Prefix::Disk(d) => { dst.push(5); dst.push(d); }
            }
            dst.push(0);
        },
        Component::RootDir => dst.extend_from_slice(&[1,0]),
        Component::CurDir => dst.extend_from_slice(&[2,0]),
        Component::ParentDir => dst.extend_from_slice(&[3,0]),
        Component::Normal(s) => { dst.push(4); dst.extend_from_slice(s.as_encoded_bytes()); dst.push(0); },
    }
}
#[cfg(test)] mod tests {
    use super::*;
    #[test] fn shares_nested_prefixes_and_orders_like_path() {
        let paths: Vec<PathBuf> = [r"C:\\a\\b\\x.jpg", r"C:\\a\\b\\y.jpg", r"C:\\a\\c\\z.jpg", r"D:\\x.jpg"].iter().map(PathBuf::from).collect();
        let mut s = PrefixPathStore::with_capacity(4, 128, 16);
        let ids: Vec<_> = paths.iter().map(|p| s.insert(p)).collect();
        assert!(s.directory_count() < 10);
        for (id, path) in ids.iter().zip(paths.iter()) { assert_eq!(s.get(*id).unwrap(), *path); }
        let mut actual = ids.clone(); s.sort_ids(&mut actual);
        let mut expected = ids; expected.sort_unstable_by(|a,b| paths[a.0 as usize].cmp(&paths[b.0 as usize]));
        assert_eq!(actual, expected);
    }
}