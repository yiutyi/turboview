//! 增量式目录树 —— 侧边栏导航的数据引擎
//!
//! 设计目标：配合并行扫描器的分批流出，**从不重扫、从不全量重建**：
//! - arena + 路径哈希索引：目录插入 O(深度)，百万目录也只有一次哈希查找
//! - 图片按批增量汇入：批内先按父目录聚合（同一目录只走一次祖先链），
//!   计数与"最早图片"沿祖先链向上传播
//! - `first_image` 增量维护最小值：点击任意目录 O(1) 拿到该子树第一张图
//! - 可见行列表带脏标记：只在展开/收起/新增目录时重建 DFS 展开

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

struct DirNode {
    /// Retained arena slot keeps outstanding async node IDs stable.
    alive: bool,
    /// 显示名（最后一段路径；盘根为 "Z:\" 这类完整形式）
    name: Arc<str>,
    /// name 的小写缓存：children 插入排序时避免每次比较都分配新 String
    name_lower: Arc<str>,
    /// 完整路径（惰性展开时按需扫描该层用）；与路径索引共享同一分配
    path: Arc<Path>,
    parent: Option<usize>,
    /// 子目录，按名字（忽略大小写）有序
    children: Vec<usize>,
    /// 本层是否已枚举（惰性扫描标记）
    scanned: bool,
    /// 本层正在后台扫描（箭头保留但淡显，避免扫描期间箭头闪没）
    loading: bool,
    /// 子树内图片总数（含所有后代）
    image_count: usize,
    /// 子树内压缩包总数（含所有后代）
    archive_count: usize,
    /// 子树内按路径序最早的一张图（与全量排序后的顺序一致）
    first_image: Option<Arc<Path>>,
    expanded: bool,
    depth: u32,
}

/// 侧边栏一行可见目录
#[derive(Clone)]
pub struct DirRow {
    pub id: usize,
    pub depth: u32,
    pub name: Arc<str>,
    pub count: usize,
    /// 子树内压缩包数（为 0 时不显示）
    pub archives: usize,
    pub expanded: bool,
    pub has_children: bool,
    /// 本层是否已扫描（未扫描的节点保留展开箭头，展开时才扫描）
    pub scanned: bool,
    /// 本层正在后台扫描
    pub loading: bool,
    /// 位 i：路径上深度 i 的节点是否为父节点的最后一个子节点。
    /// 渲染 ├ / └ 连接线时用：非最后节点的竖线贯穿整行，最后节点的止于中点。
    pub last_flags: u64,
}

#[derive(Default)]
pub struct DirTree {
    nodes: Vec<DirNode>,
    by_path: HashMap<Arc<Path>, usize>,
    rows: Vec<DirRow>,
    /// 节点 id → 可见行下标，rows 重建时同步填充；避免每次定位都全表扫描
    row_index_by_id: HashMap<usize, usize>,
    dirty: bool,
}

impl DirTree {
    pub fn new() -> Self {
        Self::default()
    }

    /// 确保目录及其所有祖先存在，返回节点 id
    pub fn ensure_dir(&mut self, path: &Path) -> usize {
        if let Some(&id) = self.by_path.get(path) {
            return id;
        }
        // 递归建祖先链（盘根的 parent() 返回 None，自然终止）
        let parent_id = path.parent().and_then(|p| {
            if p == path {
                None // 防御：某些平台根路径 parent 可能等于自身
            } else {
                Some(self.ensure_dir(p))
            }
        });
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        let name_lower: Arc<str> = Arc::from(name.to_lowercase());
        let name: Arc<str> = Arc::from(name);
        let depth = parent_id.map_or(0, |p| self.nodes[p].depth + 1);
        let id = self.nodes.len();
        self.nodes.push(DirNode {
            alive: true,
            name,
            name_lower,
            path: Arc::from(path.to_path_buf()),
            parent: parent_id,
            children: Vec::new(),
            scanned: false,
            loading: false,
            image_count: 0,
            archive_count: 0,
            first_image: None,
            expanded: false,
            depth,
        });
        self.by_path.insert(self.nodes[id].path.clone(), id);
        if let Some(p) = parent_id {
            let key = &self.nodes[id].name_lower;
            let pos = self.nodes[p]
                .children
                .partition_point(|&c| self.nodes[c].name_lower.as_ref() < key.as_ref());
            self.nodes[p].children.insert(pos, id);
        }
        self.dirty = true;
        id
    }

    /// 汇入一批图片：批内按父目录聚合，每个唯一目录只走一次祖先链
    pub fn add_images(&mut self, batch: &[PathBuf]) {
        self.add_paths(batch, false);
    }

    /// 汇入一批压缩包：与图片分开计数（`is_archive` = true），
    /// 但仍参与 first_image 竞争 —— 这样"只有压缩包、没有散图"的目录点得进去。
    pub fn add_archives(&mut self, batch: &[PathBuf]) {
        self.add_paths(batch, true);
    }

    fn add_paths(&mut self, batch: &[PathBuf], is_archive: bool) {
        let mut groups: HashMap<&Path, (usize, &PathBuf)> = HashMap::new();
        for p in batch {
            if let Some(parent) = p.parent() {
                let e = groups.entry(parent).or_insert((0, p));
                e.0 += 1;
                if p < e.1 {
                    e.1 = p;
                }
            }
        }
        for (parent, (count, first)) in groups {
            // One shared path allocation per batch-group; every ancestor stores an Arc clone.
            let first: Arc<Path> = Arc::from(first.as_path().to_path_buf());
            let id = self.ensure_dir(parent);
            let mut cur = Some(id);
            while let Some(c) = cur {
                let node = &mut self.nodes[c];
                if is_archive {
                    node.archive_count += count;
                } else {
                    node.image_count += count;
                }
                if node.first_image.as_ref().is_none_or(|f| first.as_ref() < f.as_ref()) {
                    node.first_image = Some(first.clone());
                }
                cur = node.parent;
            }
        }
    }

    /// 目录刷新后修正计数：`images` / `archives` 为增量（可为负）。
    /// 目录不在树里（未扫描过）时静默忽略。
    pub fn adjust_counts(&mut self, dir: &Path, images: isize, archives: isize) {
        let Some(&id) = self.by_path.get(dir) else {
            return;
        };
        let mut cur = Some(id);
        while let Some(c) = cur {
            let node = &mut self.nodes[c];
            node.image_count = node.image_count.saturating_add_signed(images);
            node.archive_count = node.archive_count.saturating_add_signed(archives);
            cur = node.parent;
        }
        self.dirty = true;
    }

    /// 用全量有序列表重算 `id` 及其所有祖先的 first_image。
    ///
    /// `images` 必须是**全量且已排序**的列表（即 ImageViewer::images）：
    /// 排序后第一个落在某子树内的路径就是该子树的最小路径，与增量维护的
    /// 语义一致。文件增删后旧的 first_image 可能已失效，用它一次性修正。
    pub fn recompute_first_from(&mut self, id: usize, images: &[Arc<Path>]) {
        let mut cur = Some(id);
        while let Some(c) = cur {
            let path = self.nodes[c].path.clone();
            // images 全量有序：二分定位到前缀起点后顺序扫描，命中即停。
            // 原实现对每个祖先线性 find，几十万张图 × 深度 = 百万级 starts_with。
            let start = images.partition_point(|p| p.as_ref() < path.as_ref());
            // 后代在有序列表里连续：起点不是本目录后代 ⇒ 整支为空，无需线性扫描。
            // 直接克隆调用方的 Arc：每条祖先链零路径分配。
            self.nodes[c].first_image = images
                .get(start)
                .filter(|p| p.starts_with(&path))
                .cloned();
            cur = self.nodes[c].parent;
        }
        self.dirty = true;
    }

    pub fn toggle(&mut self, id: usize) {
        self.nodes[id].expanded = !self.nodes[id].expanded;
        self.dirty = true;
    }

    pub fn set_expanded(&mut self, id: usize, v: bool) {
        self.nodes[id].expanded = v;
        self.dirty = true;
    }

    /// 展开从根到 `id` 的整条链。返回是否真的改变了展开状态
    /// （只在新展开时置脏，翻图时不会白白重建可见行列表）
    pub fn expand_to(&mut self, id: usize) -> bool {
        let mut cur = Some(id);
        let mut changed = false;
        while let Some(c) = cur {
            if !self.nodes[c].expanded {
                self.nodes[c].expanded = true;
                changed = true;
            }
            cur = self.nodes[c].parent;
        }
        if changed {
            self.dirty = true;
        }
        changed
    }

    /// 树的根（未打开目录时为 None）
    pub fn root_path(&self) -> Option<PathBuf> {
        self.nodes
            .iter()
            .find(|n| n.alive && n.parent.is_none())
            .map(|n| n.path.to_path_buf())
    }

    /// 全部折叠：根节点保持展开（否则整棵树只剩一行、看不到任何子目录），
    /// 其余节点一律收起 —— 对应资源管理器"折叠全部"。
    pub fn collapse_all(&mut self) {
        for n in &mut self.nodes {
            if n.parent.is_some() {
                n.expanded = false;
            }
        }
        self.dirty = true;
    }

    pub fn is_expanded(&self, id: usize) -> bool {
        self.nodes[id].expanded
    }

    /// 某节点在可见行里的下标（自动滚动定位用；不可见时为 None）
    pub fn row_index(&mut self, id: usize) -> Option<usize> {
        self.rows(); // 确保索引是最新的
        self.row_index_by_id.get(&id).copied()
    }

    pub fn lookup(&self, path: &Path) -> Option<usize> {
        self.by_path.get(path).copied()
    }

    /// 节点完整路径（惰性扫描该层用）
    pub fn path_of(&self, id: usize) -> &Path {
        &self.nodes[id].path
    }

    /// 本层是否已扫描
    pub fn is_scanned(&self, id: usize) -> bool {
        self.nodes[id].scanned
    }

    /// 标记本层已扫描（派发惰性扫描前立即置位，防重复派发）
    pub fn set_scanned(&mut self, id: usize) {
        self.nodes[id].scanned = true;
        self.dirty = true;
    }

    /// 标记本层正在后台扫描（结果落地后置回 false）
    pub fn set_loading(&mut self, id: usize, v: bool) {
        if !self.nodes.get(id).is_some_and(|n| n.alive) {
            return;
        }
        if self.nodes[id].loading != v {
            self.nodes[id].loading = v;
            self.dirty = true;
        }
    }

    /// Reconcile a scanned directory's immediate children.
    /// Removed subtrees release path/name/child allocations while arena IDs
    /// remain stable for asynchronous callbacks that may still hold them.
    pub fn reconcile_subdirs(&mut self, parent_path: &Path, subdirs: &[PathBuf]) -> Vec<PathBuf> {
        let Some(&parent_id) = self.by_path.get(parent_path) else {
            return Vec::new();
        };
        let desired: std::collections::HashSet<&Path> =
            subdirs.iter().map(PathBuf::as_path).collect();
        let stale: Vec<usize> = self.nodes[parent_id]
            .children
            .iter()
            .copied()
            .filter(|&id| self.nodes[id].alive && !desired.contains(self.nodes[id].path.as_ref()))
            .collect();
        let mut removed_roots = Vec::with_capacity(stale.len());
        for root in stale {
            removed_roots.push(self.nodes[root].path.to_path_buf());
            let mut stack = vec![root];
            while let Some(id) = stack.pop() {
                if !self.nodes[id].alive {
                    continue;
                }
                stack.extend(self.nodes[id].children.iter().copied());
                let path = self.nodes[id].path.clone();
                self.by_path.remove(path.as_ref());
                let node = &mut self.nodes[id];
                node.alive = false;
                node.name = Arc::from("");
                node.name_lower = Arc::from("");
                node.path = Arc::from(PathBuf::new());
                node.parent = None;
                node.children = Vec::new();
                node.scanned = false;
                node.loading = false;
                node.image_count = 0;
                node.archive_count = 0;
                node.first_image = None;
                node.expanded = false;
            }
        }
        if self.by_path.len().saturating_mul(2) < self.by_path.capacity() {
            self.by_path.shrink_to_fit();
        }
        let alive: Vec<bool> = self.nodes.iter().map(|node| node.alive).collect();
        self.nodes[parent_id].children.retain(|&id| alive[id]);
        let node_count_before_add = self.nodes.len();
        for path in subdirs {
            self.ensure_dir(path);
        }
        if !removed_roots.is_empty() || self.nodes.len() != node_count_before_add {
            self.dirty = true;
        }
        removed_roots
    }

    pub fn is_alive(&self, id: usize) -> bool {
        self.nodes.get(id).is_some_and(|node| node.alive)
    }

    /// 把一层扫描发现的子目录挂到 `parent` 下（ensure_dir 自动按名序插入）
    pub fn add_subdirs(&mut self, _parent: usize, subdirs: &[PathBuf]) {
        for p in subdirs {
            self.ensure_dir(p);
        }
    }

    /// 该子树最早一张图的路径（增量维护，O(1) 取出）
    pub fn first_image(&self, id: usize) -> Option<&Arc<Path>> {
        self.nodes[id].first_image.as_ref()
    }

    /// 展开后的可见行（DFS；脏标记缓存，未变时零成本）
    pub fn rows(&mut self) -> &[DirRow] {
        if self.dirty {
            self.rows.clear();
            self.row_index_by_id.clear();
            let roots: Vec<usize> = (0..self.nodes.len())
                .filter(|&i| self.nodes[i].alive && self.nodes[i].parent.is_none())
                .collect();
            let root_count = roots.len();
            for (i, r) in roots.into_iter().enumerate() {
                self.push_visible(r, u64::from(i + 1 == root_count));
            }
            self.dirty = false;
        }
        &self.rows
    }

    fn push_visible(&mut self, id: usize, last_flags: u64) {
        if !self.nodes[id].alive {
            return;
        }
        let (name, depth, expanded, has_children, count, scanned, loading, archives) = {
            let n = &self.nodes[id];
            (
                n.name.clone(),
                n.depth,
                n.expanded,
                !n.children.is_empty(),
                n.image_count,
                n.scanned,
                n.loading,
                n.archive_count,
            )
        };
        self.row_index_by_id.insert(id, self.rows.len());
        self.rows.push(DirRow {
            id,
            depth,
            name,
            count,
            archives,
            expanded,
            has_children,
            scanned,
            loading,
            last_flags,
        });
        if expanded {
            // 只保留 0..=depth 位：更深的位是上一次 DFS 分支遗留的，必须清掉。
            // 深度到 62 已远超 Windows 路径上限，直接不再记连接线状态。
            let (prefix, child_bit) = if depth < 62 {
                (last_flags & ((1u64 << (depth + 1)) - 1), 1u64 << (depth + 1))
            } else {
                (last_flags, 0)
            };
            let child_count = self.nodes[id].children.len();
            // 用索引遍历，避免克隆整棵子树 children（大目录下每帧省一次分配）
            for i in 0..child_count {
                let c = self.nodes[id].children[i];
                if !self.nodes[c].alive {
                    continue;
                }
                let flags = if i + 1 == child_count {
                    prefix | child_bit
                } else {
                    prefix & !child_bit
                };
                self.push_visible(c, flags);
            }
        }
    }
}

/// 1234567 -> "1_234_567"
pub fn fmt_count(n: usize) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push('_');
        }
        out.push(*b as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 跨平台构造绝对路径：Windows 用 C:\，其余用 /
    fn p(parts: &[&str]) -> PathBuf {
        let mut b = PathBuf::from(if cfg!(windows) { "C:\\" } else { "/" });
        for s in parts {
            b.push(s);
        }
        b
    }

    fn imgs(list: &[&[&str]]) -> Vec<Arc<Path>> {
        let mut v: Vec<Arc<Path>> = list.iter().map(|s| Arc::from(p(s))).collect();
        v.sort();
        v
    }

    /// 回归：子树内没有图片、但排序上更靠后的兄弟目录有图片时，
    /// 必须返回 None（旧实现在此处会扫完整段尾部）。
    #[test]
    fn empty_subtree_stops_immediately() {
        let mut t = DirTree::new();
        let empty = t.ensure_dir(&p(&["root", "aaa"]));
        let images = imgs(&[&["root", "zzz", "1.jpg"], &["root", "zzz", "2.jpg"]]);
        t.recompute_first_from(empty, &images);
        assert!(t.first_image(empty).is_none());
        let root = t.lookup(&p(&["root"])).unwrap();
        assert_eq!(t.first_image(root).unwrap().as_ref(), images[0].as_ref());
    }

    /// first_image 必须与源列表共享同一份路径分配（零拷贝）。
    #[test]
    fn recompute_shares_arc_with_source_list() {
        let mut t = DirTree::new();
        let a = t.ensure_dir(&p(&["root", "a"]));
        let images = imgs(&[&["root", "a", "1.jpg"], &["root", "a", "2.jpg"]]);
        t.recompute_first_from(a, &images);
        let root = t.lookup(&p(&["root"])).unwrap();
        assert!(Arc::ptr_eq(t.first_image(a).unwrap(), &images[0]));
        assert!(Arc::ptr_eq(t.first_image(root).unwrap(), &images[0]));
    }

    /// 分批汇入与一次性汇入必须得到同样的计数与 first_image。
    /// 注意：add_paths 用 HashMap 分组，节点创建顺序不确定，只能按路径比对。
    #[test]
    fn batching_matches_one_shot() {
        let batch: Vec<PathBuf> = vec![
            p(&["root", "a", "2.jpg"]),
            p(&["root", "a", "1.jpg"]),
            p(&["root", "b", "9.jpg"]),
        ];
        let mut a = DirTree::new();
        a.add_images(&batch);
        let mut b = DirTree::new();
        for chunk in batch.chunks(1) {
            b.add_images(chunk);
        }
        assert_eq!(a.by_path.len(), b.by_path.len());
        for (path, &id) in a.by_path.iter() {
            let other = b.lookup(path).unwrap();
            assert_eq!(
                (a.nodes[id].image_count, a.nodes[id].first_image.as_deref()),
                (b.nodes[other].image_count, b.nodes[other].first_image.as_deref())
            );
        }
    }

    /// 压缩包单独计数，但参与 first_image 竞争。
    #[test]
    fn archives_count_separately_but_win_first_image() {
        let mut t = DirTree::new();
        t.add_images(&[p(&["root", "a", "5.jpg"])]);
        t.add_archives(&[p(&["root", "a", "0.zip"])]);
        let a = t.lookup(&p(&["root", "a"])).unwrap();
        assert_eq!((t.nodes[a].image_count, t.nodes[a].archive_count), (1, 1));
        assert_eq!(
            t.first_image(a).unwrap().as_ref(),
            p(&["root", "a", "0.zip"]).as_path()
        );
    }

    /// 负增量沿祖先链传播且在 0 处饱和。
    #[test]
    fn adjust_counts_saturates_up_the_chain() {
        let mut t = DirTree::new();
        t.add_images(&[p(&["root", "a", "1.jpg"]), p(&["root", "b", "2.jpg"])]);
        t.adjust_counts(&p(&["root", "a"]), -5, 0);
        let a = t.lookup(&p(&["root", "a"])).unwrap();
        let root = t.lookup(&p(&["root"])).unwrap();
        assert_eq!(t.nodes[a].image_count, 0);
        assert_eq!(t.nodes[root].image_count, 0);
    }

    /// DFS 展开顺序 + ├/└ 连接线位 + 折叠全部后根仍展开。
    #[test]
    fn rows_last_flags_and_collapse_all() {
        let mut t = DirTree::new();
        let top = t.ensure_dir(&p(&[]));
        let root = t.ensure_dir(&p(&["root"]));
        let a = t.ensure_dir(&p(&["root", "a"]));
        t.ensure_dir(&p(&["root", "b"]));
        t.ensure_dir(&p(&["root", "a", "x"]));
        for id in [top, root, a] {
            t.set_expanded(id, true);
        }
        let names: Vec<String> = t.rows().iter().map(|r| r.name.to_string()).collect();
        assert_eq!(names.len(), 5);
        assert_eq!(&names[1..], ["root", "a", "x", "b"]);
        assert_eq!(t.rows()[3].last_flags & (1 << 3), 1 << 3); // x 是末子
        assert_eq!(t.rows()[2].last_flags & (1 << 2), 0); // a 不是末子
        assert_eq!(t.rows()[4].last_flags & (1 << 2), 1 << 2); // b 是末子
        t.collapse_all();
        assert_eq!(t.rows().len(), 2);
        assert!(t.row_index(a).is_none());
    }

    #[test]
    fn reconcile_subdirs_releases_removed_subtree_and_keeps_ids_safe() {
        let mut t = DirTree::new();
        let root_path = p(&["root"]);
        let child_path = p(&["root", "gone"]);
        let grandchild_path = p(&["root", "gone", "nested"]);
        let root = t.ensure_dir(&root_path);
        t.set_expanded(root, true);
        let child = t.ensure_dir(&child_path);
        let grandchild = t.ensure_dir(&grandchild_path);
        t.add_images(&[p(&["root", "gone", "nested", "1.jpg"])]);
        let removed = t.reconcile_subdirs(&root_path, &[]);
        assert_eq!(removed, vec![child_path.clone()]);
        assert!(t.lookup(&child_path).is_none());
        assert!(t.lookup(&grandchild_path).is_none());
        assert!(!t.is_alive(child));
        assert!(!t.is_alive(grandchild));
        assert!(t.nodes[child].children.is_empty());
        assert!(t.nodes[child].path.as_os_str().is_empty());
        assert_eq!(t.nodes[root].children.len(), 0);
        assert_eq!(t.rows().len(), 1);
    }

    #[test]
    fn fmt_count_groups_by_three() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(1000), "1_000");
        assert_eq!(fmt_count(1234567), "1_234_567");
    }
}
