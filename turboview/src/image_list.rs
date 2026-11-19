//! ImageViewer 的“图片列表”抽象层。
//!
//! 目标：把散落在 ImageViewer 里对 `Vec<Arc<Path>>` 的 25 处直接访问收敛到
//! 一个模块，让导航/刷新/插入逻辑只依赖这里定义的 API。
//!
//! 当前内部后端仍是 `Vec<Arc<Path>>`，语义与旧代码逐条一致，因此这一步是
//! 零风险的重构：不物化、不分片、不重排，任何调用的行为都不变。
//!
//! 将来若要用紧凑存储（PathStore）替换后端，只需在本模块内改写实现与
//! 那几个“物化/借用”方法的策略，ImageViewer 的导航代码一行都不用动。
//! 这也是为什么这里刻意把“需要借用 `&Path`”和“需要拥有 `Arc<Path>`/物化”
//! 的操作拆成不同方法——它们对应紧凑存储下不同的代价。

use std::path::{Path, PathBuf};
use std::sync::Arc;

/// 有序的图片路径列表。恒按 `Path` 顺序排列（调用方保证插入有序）。
#[derive(Clone, Default)]
pub struct ImageList {
    items: Vec<Arc<Path>>,
}

impl ImageList {
    pub fn new() -> Self {
        Self { items: Vec::new() }
    }

    /// 从已排序的路径批次建表（启动/打开压缩包/拖入单文件时用）。
    pub fn from_paths(paths: Vec<PathBuf>) -> Self {
        Self {
            items: paths.into_iter().map(Arc::<Path>::from).collect(),
        }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 借用访问当前项（current_path / on_remote 用），零分配。
    pub fn get(&self, index: usize) -> Option<&Path> {
        self.items.get(index).map(|p| p.as_ref())
    }

    /// 拿到拥有所有权的 `Arc<Path>`（goto_dir 路过压缩包判断时用）。
    pub fn get_owned(&self, index: usize) -> Option<Arc<Path>> {
        self.items.get(index).cloned()
    }

    /// 物化成 `PathBuf`（展开压缩包 / 预取加载时用）。
    pub fn path_at(&self, index: usize) -> Option<PathBuf> {
        self.items.get(index).map(|p| p.to_path_buf())
    }

    pub fn first(&self) -> Option<&Path> {
        self.items.first().map(|p| p.as_ref())
    }

    pub fn iter(&self) -> impl Iterator<Item = &Path> {
        self.items.iter().map(|p| p.as_ref())
    }

    /// 给目录树 `recompute_first_from` 的连续切片视图。
    pub fn as_slice(&self) -> &[Arc<Path>] {
        &self.items
    }

    /// 对全表排序（紧凑存储接入后，这里换成排序键比较，零物化）。
    pub fn sort(&mut self) {
        self.items.sort_unstable();
    }

    /// 二分定位路径；列表必须有序。
    pub fn find_index(&self, path: &Path) -> Option<usize> {
        let i = self.items.partition_point(|p| p.as_ref() < path);
        (i < self.items.len() && self.items[i].as_ref() == path).then_some(i)
    }

    /// 二分返回第一个满足谓词的下标（前缀区间起点）。
    pub fn partition_point(&self, mut pred: impl FnMut(&Path) -> bool) -> usize {
        self.items.partition_point(|p| pred(p.as_ref()))
    }

    /// 取区间 [lo, hi) 的路径副本（保留展开的虚拟条目时用）。
    pub fn slice_paths(&self, lo: usize, hi: usize) -> impl Iterator<Item = &Path> {
        self.items[lo..hi].iter().map(|p| p.as_ref())
    }

    /// 在 index 处插入一批（展开压缩包，紧随其后）。
    pub fn splice(&mut self, index: usize, paths: Vec<PathBuf>) {
        self.items
            .splice(index..index, paths.into_iter().map(Arc::<Path>::from));
    }

    /// 用有序批次替换 [lo, hi) 区间（增量合并一层扫描结果）。
    pub fn splice_range(&mut self, lo: usize, hi: usize, paths: Vec<PathBuf>) {
        self.items
            .splice(lo..hi, paths.into_iter().map(Arc::<Path>::from));
    }

    /// 按谓词过滤（目录刷新移除文件）。
    pub fn retain(&mut self, mut keep: impl FnMut(&Path) -> bool) {
        self.items.retain(|p| keep(p.as_ref()));
    }

    pub fn remove(&mut self, index: usize) {
        self.items.remove(index);
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// 整体替换（启动 / 打开压缩包 / 拖入单文件）。
    pub fn replace(&mut self, paths: Vec<PathBuf>) {
        self.items = paths.into_iter().map(Arc::<Path>::from).collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(paths: &[&str]) -> Vec<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn empty_list_has_zero_len() {
        let list = ImageList::new();
        assert!(list.is_empty());
        assert_eq!(list.len(), 0);
        assert!(list.get(0).is_none());
        assert!(list.first().is_none());
    }

    #[test]
    fn from_paths_preserves_order_and_borrow() {
        let list = ImageList::from_paths(sample(&["a/1.jpg", "a/2.jpg", "b/3.jpg"]));
        assert_eq!(list.len(), 3);
        assert_eq!(list.get(0), Some(Path::new("a/1.jpg")));
        assert_eq!(list.get(2), Some(Path::new("b/3.jpg")));
        assert_eq!(list.first(), Some(Path::new("a/1.jpg")));
        assert!(list.get(3).is_none());
    }

    #[test]
    fn owned_and_materialized_access_match_borrow() {
        let list = ImageList::from_paths(sample(&["a/1.jpg", "a/2.jpg"]));
        assert_eq!(list.get_owned(1).unwrap().as_ref(), Path::new("a/2.jpg"));
        assert_eq!(list.path_at(1).unwrap(), PathBuf::from("a/2.jpg"));
        assert_eq!(list.get_owned(5), None);
        assert_eq!(list.path_at(5), None);
    }

    #[test]
    fn iter_and_as_slice_are_consistent() {
        let paths = sample(&["a.jpg", "b.jpg", "c.jpg"]);
        let list = ImageList::from_paths(paths.clone());
        let via_iter: Vec<&Path> = list.iter().collect();
        let via_slice: Vec<&Path> = list.as_slice().iter().map(|p| p.as_ref()).collect();
        assert_eq!(via_iter, via_slice);
        assert_eq!(via_iter, paths.iter().map(|p| p.as_path()).collect::<Vec<_>>());
    }

    #[test]
    fn sort_matches_vec_pathbuf_sort_unstable() {
        let mut list = ImageList::from_paths(sample(&["c.jpg", "a.jpg", "b.jpg"]));
        let mut expected = sample(&["c.jpg", "a.jpg", "b.jpg"]);
        list.sort();
        expected.sort_unstable();
        let actual: Vec<PathBuf> = list.iter().map(Path::to_path_buf).collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn find_index_matches_partition_point() {
        let paths = sample(&["a/1.jpg", "a/2.jpg", "b/3.jpg", "c/4.jpg"]);
        let list = ImageList::from_paths(paths.clone());
        for (i, p) in paths.iter().enumerate() {
            assert_eq!(list.find_index(p), Some(i));
        }
        assert_eq!(list.find_index(Path::new("a/0.jpg")), None);
        assert_eq!(list.find_index(Path::new("aa/9.jpg")), None);
        assert_eq!(list.find_index(Path::new("d/9.jpg")), None);
    }

    #[test]
    fn partition_point_finds_prefix_boundary() {
        let list = ImageList::from_paths(sample(&["a/1.jpg", "a/2.jpg", "b/3.jpg"]));
        let prefix = Path::new("a");
        let lo = list.partition_point(|p| p < prefix);
        let hi = list.partition_point(|p| p <= prefix);
        // "a" 本身小于 "a/1.jpg"，所以 lo=0；所有项都大于 "a"，hi=0。
        assert_eq!((lo, hi), (0, 0));
        let lo2 = list.partition_point(|p| p < Path::new("b"));
        assert_eq!(lo2, 2);
    }

    #[test]
    fn splice_inserts_without_reordering_existing() {
        let mut list = ImageList::from_paths(sample(&["a.jpg", "c.jpg"]));
        list.splice(1, sample(&["b.jpg", "b2.jpg"]));
        let actual: Vec<PathBuf> = list.iter().map(Path::to_path_buf).collect();
        assert_eq!(actual, sample(&["a.jpg", "b.jpg", "b2.jpg", "c.jpg"]));
    }

    #[test]
    fn splice_range_replaces_interval() {
        let mut list = ImageList::from_paths(sample(&["a.jpg", "b.jpg", "c.jpg"]));
        list.splice_range(1, 2, sample(&["x.jpg", "y.jpg"]));
        let actual: Vec<PathBuf> = list.iter().map(Path::to_path_buf).collect();
        assert_eq!(actual, sample(&["a.jpg", "x.jpg", "y.jpg", "c.jpg"]));
    }

    #[test]
    fn retain_remove_clear_replace() {
        let mut list = ImageList::from_paths(sample(&["a.jpg", "b.png", "c.jpg"]));
        list.retain(|p| p.extension().is_some_and(|e| e == "jpg"));
        let actual: Vec<PathBuf> = list.iter().map(Path::to_path_buf).collect();
        assert_eq!(actual, sample(&["a.jpg", "c.jpg"]));
        list.remove(0);
        assert_eq!(list.get(0), Some(Path::new("c.jpg")));
        list.clear();
        assert!(list.is_empty());
        list.replace(sample(&["z.jpg"]));
        assert_eq!(list.get(0), Some(Path::new("z.jpg")));
    }

    #[test]
    fn slice_paths_returns_requested_range() {
        let list = ImageList::from_paths(sample(&["a.jpg", "b.jpg", "c.jpg", "d.jpg"]));
        let mid: Vec<&Path> = list.slice_paths(1, 3).collect();
        assert_eq!(mid, vec![Path::new("b.jpg"), Path::new("c.jpg")]);
    }
}
