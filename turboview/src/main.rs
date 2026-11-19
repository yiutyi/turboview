//! TurboView — Rust + GPUI + libjpeg-turbo SIMD 的极速图片浏览器
//!
//! 功能：
//! - JPEG 经 libjpeg-turbo SIMD 直接解码为 BGRA（零转换、零拷贝上传 GPU）
//! - 其他格式（PNG/BMP/GIF/WebP/TIFF/TGA/PNM/QOI/ICO）经 image crate 解码
//! - 压缩包（zip/7z/rar）内图片直接浏览：只读目录 + 按需解压单条目到内存
//! - 滚轮以光标为锚点缩放、左键拖拽平移、0=适应窗口、1=原始大小
//! - 左右方向键/空格切换同目录图片，后台预取相邻图片（LRU 缓存）
//! - F 切换快速解码（FASTDCT + FASTUPSAMPLE）
//! - 支持把图片文件/压缩包直接拖入窗口
//! - 侧边栏目录树（资源管理器风格）：工具栏（刷新 / 折叠全部 / 定位当前 /
//!   在资源管理器中打开 / 隐藏）、当前路径条、"位置"区、层级连接线与展开箭头；
//!   切换图片时自动展开祖先链并把当前目录滚进视野（B 键 / 工具栏按钮显隐）
//! - 以默认看图器方式打开单张图片时，目录树自动收起，状态栏最左侧保留恢复按钮

// GUI 子系统：不弹控制台窗口（同时消灭"运行时多出一个命令行窗口"问题）
#![windows_subsystem = "windows"]

#[cfg(all(target_os = "windows", feature = "wic-decode"))]
mod wic;

use turboview::{archive, turbo, win32};

use anyhow::{Context as _, Result};
use archive::{ARCHIVE_EXTENSIONS, IMAGE_EXTENSIONS as SUPPORTED_EXTENSIONS};
use std::collections::{HashMap, HashSet, VecDeque};
use gpui::{
    actions, div, img, point, prelude::*, uniform_list, AnyElement, App,
    AsyncApp, Bounds, ClickEvent, Context, DevicePixels, ExternalPaths, FocusHandle, ImageSource,
    KeyBinding, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ObjectFit, Pixels, Point,
    Render, RenderImage, ScrollDelta, ScrollStrategy, ScrollWheelEvent, SvgRenderer, SvgSize,
    UniformListScrollHandle, WeakEntity, Window, WindowAppearance, WindowBounds, WindowOptions,
    px, size, svg,
};
use image::{Frame, GenericImageView, RgbaImage};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use turbo::{DecodedImage, RoiRect};
use turboview::dirtree::{DirTree, fmt_count};
use turboview::scanner::scan_one_level;

actions!(
    turboview,
    [
        NextImage,
        PrevImage,
        ZoomIn,
        ZoomOut,
        ZoomActual,
        ZoomFit,
        ToggleFastMode,
        ToggleSidebar,
        RandomImage,
        ToggleSlideshow,
        ToggleFullscreen,
        DeleteImage,
        CopyImage,
        CopyPath,
        RevealInExplorer,
        ToggleTheme,
        RefreshTree,
        Quit,
    ]
);

/// 侧边栏宽度（像素）
const SIDEBAR_WIDTH: f32 = 280.0;
/// 侧边栏行高
const ROW_HEIGHT: f32 = 25.0;
/// 目录树每级缩进宽度
const TREE_INDENT: f32 = 12.0;

/// 解码输出的最长边上限。实际值按**窗口物理尺寸**自适应（见
/// `ImageViewer::decode_max_dim`）：1366×768 的机器只需要 ~1500，
/// 固定解 4096 等于白算 7 倍像素。这里只兜住上限。
/// 放大到接近 100% 时由 ROI 渐进重解码补上原始分辨率细节。
const MAX_DECODE_DIM_CAP: u32 = 4096;
/// 解码尺寸下限：再小的话"适应窗口"都会糊（有 ROI 兜底，可以偏激进）
const MIN_DECODE_DIM: u32 = 1024;
/// 超出视口多少倍才值得多解一些。
/// 1.2 = 视口 + 20% 余量。1366×768 的视口约 1325 → 上限 1590：
/// 4000px 的图走 1/4 档（1000px），解码量比 1.6 时的 1/2 档（2000px）少 4 倍；
/// 放大到 100% 的细节由 ROI 补。低配机上少解 4 倍像素比多留 60% 余量划算。
const DECODE_DIM_HEADROOM: f32 = 1.2;

/// 首帧预览的最长边目标（DCT 降采样一档：6000px 的图落在 1/8 → 750px）。
/// 用 EXIF 内嵌缩略图（常见 160×120）直接拉伸到满屏会糊成一片，
/// 改成一档 DCT 快解码（~20ms），清晰度提升 4~5 倍，代价仍可忽略。
const PREVIEW_DIM: u32 = 1024;
/// EXIF 缩略图够大才值得直接用（够大 = 不糊且比快解码更省时）
const PREVIEW_MIN_THUMB: u32 = 512;
/// 文件小于这个体积就不做"1/8 快解码"预览：全质量解码本来就快，
/// 预览抢不到时间还白占一个核
const PREVIEW_MIN_BYTES: u64 = 1024 * 1024;

/// 预取缓存的字节预算上限；实际预算按可用内存自适应（见 `ImageViewer::new`）
const CACHE_BUDGET_CAP: usize = 96 * 1024 * 1024;
/// 保留基本的前后翻页缓存，但避免图库元数据之外再常驻数百 MB 像素缓存
const CACHE_BUDGET_FLOOR: usize = 32 * 1024 * 1024;

/// ROI 重解码触发阈值：每个原始像素占 0.5 个屏幕像素时就值得补细节。
/// 解码尺寸按窗口自适应后（可能只有原图 1/4），这个阈值必须跟着降低，
/// 否则"原始大小"时看到的仍是降采样纹理。
const ROI_SCALE_THRESHOLD: f32 = 0.5;
/// ROI 期望区域面积上限（像素）：视口 + 余量通常 < 10MP，超出说明视野太大
const ROI_MAX_PIXELS: u64 = 16_000_000;
/// ROI 请求防抖时间
const ROI_DEBOUNCE: Duration = Duration::from_millis(150);

/// 幻灯片播放间隔（秒）
const SLIDESHOW_INTERVAL: u64 = 3;

/// 点击与拖拽的判定阈值（像素）
const CLICK_THRESHOLD: f32 = 5.0;

const ZOOM_MIN: f32 = 0.02;
const ZOOM_MAX: f32 = 64.0;

// ============================================================================
// 状态栏 SVG 单色图标（24x24 viewBox，stroke 风格）
// gpui 的 SVG 走 alpha mask + MonochromeSprite：颜色自动取 style.text.color，
// 因此亮色主题下呈黑色、暗色主题下呈白色，无需为两套主题各存一份。
// ============================================================================

/// 随机模式（交叉箭头 + 两个箭头头部）
const ICON_RANDOM: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="2.1" stroke-linecap="round" stroke-linejoin="round"><path d="M16 3h5v5"/><path d="M4 20 21 3"/><path d="M21 16v5h-5"/><path d="M15 15l6 6"/><path d="M4 4l5 5"/></svg>"##;
/// 幻灯片（圆角矩形框 + 播放三角）
const ICON_SLIDESHOW: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="2.1" stroke-linecap="round" stroke-linejoin="round"><rect x="2.5" y="4" width="19" height="13" rx="2"/><path d="M10 8.5l5 2.5-5 2.5z" fill="#000"/><path d="M8 21h8"/></svg>"##;
/// 压缩包（徽标：盒子里一张图）
const ICON_ARCHIVE: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="2.1" stroke-linecap="round" stroke-linejoin="round"><path d="M3.5 7.5h17v13h-17z"/><path d="M3.5 7.5 12 3l8.5 4.5"/><path d="M10.5 7.5v2.5h3V7.5"/></svg>"##;
/// 原图 1:1（像素网格 + 角标：每格 = 一个原始像素）
const ICON_ORIGINAL: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="2.1" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="3" width="18" height="18" rx="1.5"/><path d="M9 3v18M15 3v18M3 9h18M3 15h18"/></svg>"##;
/// 全屏（四角向外展开）
const ICON_FULLSCREEN: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M8 3H5a2 2 0 0 0-2 2v3M16 3h3a2 2 0 0 1 2 2v3M3 16v3a2 2 0 0 0 2 2h3M21 16v3a2 2 0 0 1-2 2h-3"/></svg>"##;
/// 文件夹（闭合）：单色矢量轮廓，启动时按主题栅格化
const ICON_FOLDER: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="1.9" stroke-linejoin="round"><path d="M3 6.5h6l2 2.5h10v10.5H3z"/></svg>"##;
/// 文件夹（展开）
const ICON_FOLDER_OPEN: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="1.9" stroke-linejoin="round"><path d="M3 8.5h6l2 2.5h9.5L19 19H3z"/></svg>"##;
/// 主题切换：太阳（当前亮色）/ 月亮（当前暗色）
const ICON_THEME_SUN: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="2" stroke-linecap="round"><circle cx="12" cy="12" r="4"/><path d="M12 2.5v2M12 19.5v2M2.5 12h2M19.5 12h2M5.3 5.3l1.4 1.4M17.3 17.3l1.4 1.4M18.7 5.3l-1.4 1.4M6.7 17.3l-1.4 1.4"/></svg>"##;
const ICON_THEME_MOON: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="2" stroke-linejoin="round"><path d="M20 14.2A8.5 8.5 0 0 1 9.8 4 8.5 8.5 0 1 0 20 14.2z"/></svg>"##;

/// 把内置单色 SVG 栅格化为位图。gpui_windows 不保证 svg() 元素能画出来
/// （工具栏已因此改用字形），这里用 gpui 自带 SvgRenderer 在启动时栅格化
/// 一次，之后每帧只是 Arc 克隆；颜色按明暗主题分别生成，不再依赖 alpha mask。
fn make_mono_icon(svg: &[u8], px_size: i32, rgb: u32) -> Arc<RenderImage> {
    let renderer = SvgRenderer::new(Arc::new(()));
    let source = String::from_utf8_lossy(svg).replace("#000", &format!("#{rgb:06X}"));
    let parsed = renderer
        .parse_svg(source.as_bytes())
        .expect("builtin icon svg must parse");
    renderer
        .render_parsed(
            &parsed,
            SvgSize::ExactSize(size(DevicePixels(px_size), DevicePixels(px_size))),
        )
        .expect("builtin icon svg must rasterize")
}

// ============================================================================
// 压缩包内图片的"虚拟路径"
//
// 压缩包场景下 images 列表仍保留完整路径，但底层使用 Arc 共享路径分配。
// "<压缩包路径>|<包内条目名>" 的虚拟路径。'|' 是 Windows 文件名非法字符，
// 真实路径中绝无可能出现，因此拆分绝无歧义；
// 缓存键、在途集合、当前图判断等全部逻辑无需改动即可复用。
// ============================================================================

const VIRTUAL_SEP: char = '|';

/// 构造虚拟路径：压缩包路径 + '|' + 包内条目名
fn virtual_path(archive: &Path, entry: &str) -> PathBuf {
    PathBuf::from(format!("{}{}{}", archive.display(), VIRTUAL_SEP, entry))
}

/// 拆分虚拟路径：虚拟路径 → Some((压缩包路径, 条目名))；本地文件 → None
fn split_virtual(path: &Path) -> Option<(PathBuf, String)> {
    let s = path.to_string_lossy();
    let (archive, entry) = s.split_once(VIRTUAL_SEP)?;
    if entry.is_empty() {
        return None;
    }
    Some((PathBuf::from(archive), entry.to_string()))
}

/// 状态栏/提示用的显示名：本地文件显示文件名；压缩包条目显示 "包名 › 条目名"
fn display_name(path: &Path) -> String {
    if let Some((archive, entry)) = split_virtual(path) {
        let a = archive
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| archive.display().to_string());
        let e = entry.rsplit(['/', '\\']).next().unwrap_or(&entry);
        format!("{a} › {e}")
    } else {
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string())
    }
}

/// 当前显示的图片
/// 预热通道的三种状态
enum PreloadState {
    /// 已解好并塞进缓存
    Hit,
    /// 还在读/解，别重复发起
    Pending,
    /// 没有可用的预热结果
    None,
}

struct LoadedImage {
    render: Arc<RenderImage>,
    width: u32,
    height: u32,
    /// 原始 JPEG 尺寸（缩放前，存储方向，未应用 EXIF 旋转）
    src_width: u32,
    src_height: u32,
    /// EXIF orientation（1-8）
    orientation: u32,
    decode_ms: f64,
    read_ms: f64,
    fast: bool,
    cached: bool,
    /// 首帧预览（EXIF 缩略图或 1/8 快解码），尚未是全质量解码
    preview: bool,
    /// 冷启动预热解码的结果：解码与 UI 初始化并行，没有占用户的等待时间
    preloaded: bool,
}

/// LRU 缓存条目（只保留 GPU 侧 RenderImage）
#[derive(Clone)]
struct CacheEntry {
    render: Arc<RenderImage>,
    width: u32,
    height: u32,
    src_width: u32,
    src_height: u32,
    orientation: u32,
}

impl CacheEntry {
    fn bytes(&self) -> usize {
        self.width as usize * self.height as usize * 4
    }
}

/// ROI 渐进重解码覆盖层：当前图的一个 1/1 原始分辨率区域
/// （坐标为显示方向源坐标系，绘制时按缩放映射到屏幕）
struct RoiOverlay {
    render: Arc<RenderImage>,
    rect: RoiRect,
}

struct ImageViewer {
    /// 同目录下的图片列表（排序后）
    images: Vec<Arc<Path>>,
    index: usize,
    current: Option<LoadedImage>,
    /// 显示缩放（1.0 = 一图素一屏幕像素）
    zoom: f32,
    /// 是否处于“适应窗口”模式
    fit: bool,
    /// 状态栏「原图」开关：开 = 屏幕像素对齐原图像素（1:1），关 = 适应窗口。
    /// 默认关闭；跨翻页保持（切图后仍按 1:1 显示新图）。
    original_view: bool,
    /// 新图落地后需要按当前模式重新居中（居中要用视口尺寸，只能在 render 里算）
    recenter: bool,
    /// 图片左上角在窗口中的位置（拖拽平移用）
    offset: Point<Pixels>,
    /// 上一帧实际生效的缩放（fit 模式下作为滚轮缩放的起点）
    effective_zoom: f32,
    fast_mode: bool,
    error: Option<String>,
    drag: Option<(Point<Pixels>, Point<Pixels>)>,
    focus_handle: FocusHandle,
    focus_requested: bool,
    initial_load_started: bool,
    cache: Vec<(PathBuf, CacheEntry)>,
    /// 缓存字节预算（按可用内存自适应，低配机不再硬吃 256MB）
    cache_budget: usize,
    /// 缓存条目上限（按预算推导）
    cache_max_entries: usize,
    /// 同时在跑的预取解码数上限（按 CPU 核数；2 核机器上不再 4 张齐发）
    max_prefetch_tasks: usize,
    /// 因并发上限没派发的预取任务（FIFO，有空位就补）
    prefetch_queue: VecDeque<PathBuf>,
    /// 最近一次渲染的视口物理尺寸 (w, h)：解码尺寸按它自适应
    viewport_px: (f32, f32),
    /// 缩略图代际：全图落地后递增，迟到的缩略图结果据此丢弃
    thumb_gen: u64,
    /// 解码代际：切换 fast 模式等操作作废旧结果，避免旧模式结果落地污染新模式缓存
    decode_gen: u64,
    /// 是否机械盘（决定预取跨度：机械盘随机读代价高，只取 +1）
    rotational: bool,
    /// 磁盘类型是否已探测（懒探测，避免启动即做 IOCTL）
    rotational_known: bool,
    /// 图片是否在网络上（SMB/UNC）：带宽是瓶颈，预取要收紧、预览帧要跳过
    remote: Option<bool>,
    /// 冷启动预热通道：main() 在平台初始化之前就开线程解码首图，
    /// 结果在这里被认领（与 DirectWrite 字体库初始化并行，白赚 ~250ms）
    preload: Option<std::sync::mpsc::Receiver<(PathBuf, Result<DecodedImage>)>>,
    /// 认领预热结果时带出来的 (路径, decode_ms, read_ms, fast)，状态栏显示真实耗时。
    /// 必须带路径：否则预热标签会飘到后面翻到的图上。
    preload_stats: Option<(PathBuf, f64, f64, bool)>,
    /// 预热的是哪张图（用于判断"要不要等它"）。
    /// 用共享 Arc 是因为目录模式下"第一张是谁"要等后台线程扫完才知道。
    preload_path: std::sync::Arc<std::sync::Mutex<Option<PathBuf>>>,
    /// 已经在等预热结果（防止重复挂等待任务）
    preload_waiting: bool,
    in_flight: Vec<PathBuf>,
    /// 扫描代际：切换根目录后，旧根的迟到扫描结果会被丢弃
    scan_generation: u64,
    /// 进行中的惰性层级扫描数（状态栏提示用）
    pending_levels: usize,
    /// 惰性扫描完成后要跳转到的目录节点（点击未扫描目录时设置）
    nav_target: Option<usize>,
    /// 增量目录树（侧边栏）
    tree: DirTree,
    /// 侧边栏是否可见
    sidebar_visible: bool,
    /// 目录树滚动句柄：切换图片时把当前目录行滚进视野（资源管理器行为）
    tree_scroll: UniformListScrollHandle,
    /// 待滚动定位到的节点（当前目录变化时置位，渲染时消费一次）
    tree_reveal: Option<usize>,
    /// "位置"区（盘符 / 常用目录）是否展开
    places_open: bool,
    /// 幻灯片播放开关与代际（防旧循环干扰）
    slideshow: bool,
    slideshow_gen: u64,
    /// 随机模式（R 切换）："下一张"变为随机跳转，右键/上滚退回历史
    random_mode: bool,
    /// 随机跳转历史（右键退回用）
    history: Vec<usize>,
    /// 伪全屏状态及进入前的侧边栏可见性（退出恢复）
    fullscreen: bool,
    sidebar_before_fullscreen: bool,
    /// 窗口标题（伪全屏 FindWindowW 用）
    window_title: String,
    /// ROI 渐进重解码覆盖层（当前图 1/1 原始分辨率区域）
    roi_overlay: Option<RoiOverlay>,
    /// ROI 最新期望区域（渲染时更新；加载完成后按需重新调度）
    roi_wanted: Option<RoiRect>,
    /// ROI 请求代际（防抖 + 过期结果丢弃）
    roi_gen: u64,
    /// 是否有 ROI 解码在途（在途时只更新 roi_wanted，完成后重评）
    roi_inflight: bool,
    /// 状态栏操作反馈（复制/删除等），切图时清除
    note: Option<String>,
    /// 压缩包浏览信息（"📦 包名 › N 张"），跟随当前图：在包内才显示
    archive_info: Option<String>,
    /// 已内联展开的压缩包：其条目已插入到 `images` 里紧跟其后
    archive_expanded: HashSet<PathBuf>,
    /// 打开失败的压缩包（列表里再次经过时直接跳过，不反复重试）
    archive_failed: HashSet<PathBuf>,
    /// 各压缩包的图片条目数（状态栏显示用）
    archive_counts: HashMap<PathBuf, usize>,
    /// 目录变化监控器（跟随当前浏览目录），元素被替换即停止上一个
    watch: Option<(PathBuf, win32::DirWatch)>,
    /// 监控事件通道
    watch_tx: flume::Sender<PathBuf>,
    watch_rx: flume::Receiver<PathBuf>,
    /// 正在重扫（刷新）的目录，防重入
    refreshing: HashSet<PathBuf>,
    /// 暗色模式开关（T 键切换；Windows 上 gpui 内置外观切换为 no-op，
    /// 故自建调色板由本字段驱动）
    dark_mode: bool,
    /// 状态栏左侧的 P 字 logo（启动时栅格化一次，RGBA 位图保颜色）
    logo: Arc<RenderImage>,
    /// 目录树文件夹图标：[明暗][是否展开]，启动时按两套主题色各栅格化一次
    folder_icons: [[Arc<RenderImage>; 2]; 2],
    /// 状态栏主题图标：[明暗]，显示当前所处主题
    theme_icons: [Arc<RenderImage>; 2],
    /// 状态栏图标：[明暗][压缩包、随机、幻灯片、原图、全屏]，启动时栅格化
    status_icons: [[Arc<RenderImage>; 5]; 2],
}

impl ImageViewer {
    fn new(
        images: Vec<PathBuf>,
        index: usize,
        fast_mode: bool,
        window_title: String,
        // 启动时是否显示目录树（作为默认看图器打开单张图片时隐藏）
        sidebar_visible: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        // 目录变化事件通道：监控线程 → UI（unbounded，事件量极小）
        let (watch_tx, watch_rx) = flume::unbounded();

        // ---- 低配机自适应：缓存预算跟内存走，预取并发跟核数走 ----
        // 总内存 1/32 与可用内存 1/8 取小，夹在 [64MB, 256MB]。
        // 旧公式 total/8 在 4GB 机上算出 512MB、直接被 256MB 上限钳死，
        // 和固定 256MB 没区别；改成 /32 后 4GB→128MB、8GB→256MB 真正分流。
        let (total_mem, avail_mem) =
            win32::memory_info().unwrap_or((8 << 30, 4 << 30));
        let cache_budget = (total_mem as usize / 32)
            .min(avail_mem as usize / 8)
            .clamp(CACHE_BUDGET_FLOOR, CACHE_BUDGET_CAP);
        let cache_max_entries = (cache_budget / (8 << 20)).clamp(2, 12);
        // 双核机器只留 1 个预取位，避免 4 张齐发把当前图解码挤慢
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2);
        let max_prefetch_tasks = cores.saturating_sub(1).clamp(1, 4);

        // ---- 首图解码尺寸：用主显示器工作区先估一个视口 ----
        // 窗口启动即最大化，工作区减去标题栏/边框就是视口上界；
        // 拿不到就退回 1600×900（比原来的 1920×1080 保守，宁可先糊后清）。
        let viewport_px = estimate_viewport_px();

        let dark_mode = win32::system_prefers_dark_mode();

        let mut viewer = Self {
            images: images.into_iter().map(Arc::<Path>::from).collect(),
            index,
            current: None,
            zoom: 1.0,
            fit: true,
            original_view: false,
            recenter: false,
            offset: point(px(0.), px(0.)),
            effective_zoom: 1.0,
            fast_mode,
            error: None,
            drag: None,
            focus_handle: cx.focus_handle(),
            focus_requested: false,
            initial_load_started: false,
            cache: Vec::new(),
            cache_budget,
            cache_max_entries,
            max_prefetch_tasks,
            prefetch_queue: VecDeque::new(),
            viewport_px,
            thumb_gen: 0,
            decode_gen: 0,
            rotational: false,
            rotational_known: false,
            remote: None,
            preload: None,
            preload_stats: None,
            preload_path: std::sync::Arc::new(std::sync::Mutex::new(None)),
            preload_waiting: false,
            in_flight: Vec::new(),
            scan_generation: 0,
            pending_levels: 0,
            nav_target: None,
            tree: DirTree::new(),
            sidebar_visible,
            tree_scroll: UniformListScrollHandle::new(),
            tree_reveal: None,
            places_open: true,
            slideshow: false,
            slideshow_gen: 0,
            random_mode: false,
            history: Vec::new(),
            fullscreen: false,
            sidebar_before_fullscreen: true,
            window_title,
            roi_overlay: None,
            roi_wanted: None,
            roi_gen: 0,
            roi_inflight: false,
            note: None,
            archive_info: None,
            archive_expanded: HashSet::new(),
            archive_failed: HashSet::new(),
            archive_counts: HashMap::new(),
            watch: None,
            watch_tx: watch_tx.clone(),
            watch_rx,
            refreshing: HashSet::new(),
            dark_mode,
            logo: make_logo_image(),
            // [0]=亮色用深灰，[1]=暗色用浅灰；每帧只克隆 Arc
            folder_icons: [
                [
                    make_mono_icon(ICON_FOLDER, 20, 0x5A5A5A),
                    make_mono_icon(ICON_FOLDER_OPEN, 20, 0x5A5A5A),
                ],
                [
                    make_mono_icon(ICON_FOLDER, 20, 0xC8C8C8),
                    make_mono_icon(ICON_FOLDER_OPEN, 20, 0xC8C8C8),
                ],
            ],
            theme_icons: [
                make_mono_icon(ICON_THEME_SUN, 20, 0x5A5A5A),
                make_mono_icon(ICON_THEME_MOON, 20, 0xC8C8C8),
            ],
            status_icons: [
                [
                    make_mono_icon(ICON_ARCHIVE, 20, 0x64748B),
                    make_mono_icon(ICON_RANDOM, 20, 0x64748B),
                    make_mono_icon(ICON_SLIDESHOW, 20, 0x64748B),
                    make_mono_icon(ICON_ORIGINAL, 20, 0x64748B),
                    make_mono_icon(ICON_FULLSCREEN, 20, 0x64748B),
                ],
                [
                    make_mono_icon(ICON_ARCHIVE, 20, 0xAAB4C3),
                    make_mono_icon(ICON_RANDOM, 20, 0xAAB4C3),
                    make_mono_icon(ICON_SLIDESHOW, 20, 0xAAB4C3),
                    make_mono_icon(ICON_ORIGINAL, 20, 0xAAB4C3),
                    make_mono_icon(ICON_FULLSCREEN, 20, 0xAAB4C3),
                ],
            ],
        };
        viewer.spawn_watch_pump(cx);
        viewer
    }

    /// 常驻任务：接收目录变化事件 → 增量刷新列表。
    /// 走异步通道而非每帧轮询：空闲（无重绘）时的文件变化也能及时刷新。
    fn spawn_watch_pump(&mut self, cx: &mut Context<Self>) {
        let rx = self.watch_rx.clone();
        cx.spawn(async move |view: WeakEntity<Self>, cx: &mut AsyncApp| {
            while let Ok(dir) = rx.recv_async().await {
                let alive = view
                    .update(cx, |v, cx| {
                        v.on_dir_changed(dir, cx);
                        cx.notify();
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }

    fn current_path(&self) -> Option<&Path> {
        self.images.get(self.index).map(|p| p.as_ref())
    }

    /// 切换到指定索引并加载
    fn goto(&mut self, index: usize, cx: &mut Context<Self>) {
        self.goto_dir(index, false, cx);
    }

    /// 切换索引并加载。`back` = 后退方向（←/↑）：经过压缩包时向**前**跳过它，
    /// 而不是展开后掉进包内——否则按"上一张"会莫名跳到包里的第一张。
    fn goto_dir(&mut self, index: usize, back: bool, cx: &mut Context<Self>) {
        if self.images.is_empty() {
            return;
        }
        let mut idx = index % self.images.len();
        self.error = None; // 先清：展开失败留下的错误要能显示出来
        // 压缩包不是图片：路过时内联展开（前进则落进包内首图，后退则越过它）。
        // 上限为初始长度 + 8，展开会加长列表，绕行因此有界。
        let guard = self.images.len() + 8;
        for _ in 0..guard {
            let Some(p) = self.images.get(idx).cloned() else {
                return;
            };
            if !archive::is_archive(&p) || self.archive_failed.contains(p.as_ref()) {
                break;
            }
            if !self.archive_expanded.contains(p.as_ref()) && !self.expand_archive_at(idx, cx) {
                // 打不开/包里没图：记下来，之后直接跳过它
                self.archive_failed.insert(p.to_path_buf());
            }
            let n = self.images.len();
            idx = if back {
                // 后退：越过这个压缩包，落到它前面的条目
                if idx == 0 {
                    n - 1
                } else {
                    idx - 1
                }
            } else {
                // 前进：落到包内首图（紧跟其后）；未展开成功则继续往后
                (idx + 1) % n
            };
        }
        self.index = idx;
        self.drag = None;
        self.sync_archive_info();
        self.update_watch();
        self.reveal_current_dir();
        let path = self.images[self.index].to_path_buf();

        // 冷启动：预热线程可能已经把这张图解好了（与 UI 初始化并行），
        // 认领后直接走下面的缓存命中分支，连一次异步往返都省掉
        if self.preload_waiting {
            if trace_enabled() {
                trace("preload: waiting (skip duplicate decode)");
            }
            return;
        }
        let st = self.take_preload(&path);
        if trace_enabled() {
            trace(match st {
                PreloadState::Hit => "preload: hit",
                PreloadState::Pending => "preload: pending",
                PreloadState::None => "preload: none",
            });
        }
        match st {
            PreloadState::Hit => {}
            PreloadState::Pending => {
                // 预热线程正在读这张图：网络盘上 18 MB 要 1.6 s，
                // 此刻再开一个解码会把同一份字节读第二遍，两路互相拖慢。
                self.wait_preload(cx);
                return;
            }
            PreloadState::None => {}
        }

        // 命中 LRU 缓存：直接显示
        if let Some(pos) = self.cache.iter().position(|(p, _)| *p == path) {
            let (p, entry) = self.cache.remove(pos);
            self.cache.insert(0, (p, entry.clone()));
            // 冷启动预热帧：像素是真的解出来的，状态栏照实报解码耗时（而不是"缓存命中"）
            let (dms, rms, fast, cached, preloaded) = match self.preload_stats.take() {
                Some((sp, d, r, f)) if sp == path => (d, r, f, false, true),
                _ => (0.0, 0.0, self.fast_mode, true, false),
            };
            self.set_image(
                entry.render,
                entry.width,
                entry.height,
                entry.src_width,
                entry.src_height,
                entry.orientation,
                dms,
                rms,
                fast,
                cached,
                false,
            );
            if preloaded {
                if let Some(img) = self.current.as_mut() {
                    img.preloaded = true;
                }
            }
            self.prefetch(cx);
            cx.notify();
            return;
        }

        if self.in_flight.contains(&path) {
            // 这张图可能是预取任务：用户已经导航到它，必须让在途结果
            // 在完成时直接上屏，而不能把它当作纯缓存预取然后静默返回。
            self.thumb_gen += 1;
            cx.notify();
            return;
        }

        // 缓存未命中：不清掉 current —— 上一张保持显示直到新图解码完成，
        // 避免"解码中…"闪屏。过期解码结果由 spawn_load 里的守卫丢弃。
        cx.notify();
        self.spawn_load(path, true, cx);
    }

    /// 把 `idx` 处的压缩包内联展开：包内图片条目插到它后面，成为可导航项。
    /// 展开后翻页会顺序穿过包内所有图，再回到目录里的下一项。
    fn expand_archive_at(&mut self, idx: usize, cx: &mut Context<Self>) -> bool {
        let path = self.images[idx].to_path_buf();
        let _ = cx;
        match archive::list_images(&path) {
            Ok(names) if !names.is_empty() => {
                let entries: Vec<PathBuf> = names.iter().map(|n| virtual_path(&path, n)).collect();
                self.archive_expanded.insert(path.clone());
                self.archive_counts.insert(path.clone(), names.len());
                self.images.splice(idx + 1..idx + 1, entries.into_iter().map(Arc::<Path>::from));
                true
            }
            Ok(_) => {
                self.error = Some(format!("{} 里没有图片", display_name(&path)));
                false
            }
            Err(e) => {
                self.error = Some(format!("打开 {} 失败: {e}", display_name(&path)));
                false
            }
        }
    }

    /// 状态栏压缩包信息跟随当前图：在包内显示"包名 › N 张"，否则清空
    fn sync_archive_info(&mut self) {
        self.archive_info = match self.current_path().and_then(|p| split_virtual(p)) {
            Some((a, _)) => {
                let pkg = a
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| a.display().to_string());
                self.archive_counts
                    .get(&a)
                    .map(|n| format!("📦 {pkg} › {n} 张"))
            }
            None => None,
        };
    }

    /// 让目录监控跟着当前浏览目录走（压缩包内 / 未扫描的目录不监控）。
    /// `DirWatch` 被替换时自动 Drop → 停止上一个监控线程。
    fn update_watch(&mut self) {
        let target = self
            .current_path()
            .filter(|p| split_virtual(p).is_none())
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .filter(|d| self.tree.lookup(d).is_some_and(|id| self.tree.is_scanned(id)));
        if target == self.watch.as_ref().map(|(d, _)| d.clone()) {
            return;
        }
        if let Some((_, w)) = self.watch.take() {
            w.stop();
        } // 旧监控停掉
        if let Some(dir) = target {
            let watch = win32::watch_dir(dir.clone(), self.watch_tx.clone());
            self.watch = Some((dir, watch));
        }
    }

    /// 目录变化事件到达：重扫该层并增量更新列表
    fn on_dir_changed(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        // 初始扫描还在跑时不插一脚（那次扫描本身就会把新文件收进来）
        if self.pending_levels > 0 || self.refreshing.contains(&dir) {
            return;
        }
        self.refresh_dir(dir, cx);
    }

    /// 后台重扫目录 `dir`，完成后增量合并（只动增删项，不动其余）
    fn refresh_dir(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        self.refreshing.insert(dir.clone());
        let generation = self.scan_generation;
        let done = dir.clone();
        cx.spawn(async move |view: WeakEntity<Self>, cx: &mut AsyncApp| {
            let level = cx
                .background_executor()
                .spawn(async move { scan_one_level(&dir, SUPPORTED_EXTENSIONS, ARCHIVE_EXTENSIONS) })
                .await;
            view.update(cx, |v, cx| {
                v.refreshing.remove(&done);
                if v.scan_generation == generation {
                    v.on_dir_refreshed(&done, level, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 一层重扫结果落地：新增项并入列表，消失项（含其内联展开的压缩包内容）剔除
    fn on_dir_refreshed(
        &mut self,
        dir: &Path,
        level: turboview::scanner::DirLevel,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.tree.lookup(dir) else {
            return;
        };
        let turboview::scanner::DirLevel {
            subdirs,
            images,
            archives,
        } = level;
        // 新建的子目录先挂进树（收起状态，用户点开才扫）
        self.tree.add_subdirs(id, &subdirs);

        let new: HashSet<PathBuf> = images.iter().cloned().chain(archives.iter().cloned()).collect();
        let old: HashSet<PathBuf> = self
            .images
            .iter()
            .filter(|p| p.parent() == Some(dir) && split_virtual(p).is_none())
            .map(|p| p.to_path_buf())
            .collect();
        let removed: Vec<PathBuf> = old.iter().filter(|p| !new.contains(*p)).cloned().collect();
        let added: Vec<PathBuf> = new.into_iter().filter(|p| !old.contains(p)).collect();
        if removed.is_empty() && added.is_empty() {
            return;
        }

        let gone: HashSet<PathBuf> = removed.iter().cloned().collect();
        // 被删项连带清理：缓存、在途、展开标记（虚拟条目 = 包内图，一并剔除）
        let belongs = |p: &PathBuf| -> bool {
            gone.contains(p) || split_virtual(p).is_some_and(|(a, _)| gone.contains(&a))
        };
        let cur = self.current_path().map(Path::to_path_buf);
        let cur_gone = cur.as_ref().is_some_and(belongs);
        self.images.retain(|p| !belongs(p));
        self.cache.retain(|(p, _)| !belongs(p));
        self.in_flight.retain(|p| !belongs(p));
        for a in &removed {
            if archive::is_archive(a) {
                self.archive_expanded.remove(a);
                self.archive_failed.remove(a);
                self.archive_counts.remove(a);
            }
        }
        // 删项会让索引错位：先把 index 重新对准当前图
        if let Some(c) = &cur {
            if let Some(i) = self.find_image_index(c) {
                self.index = i;
            }
        }

        let rm_img = removed.iter().filter(|p| !archive::is_archive(p)).count();
        let rm_arc = removed.len() - rm_img;
        let add_img = added.iter().filter(|p| !archive::is_archive(p)).count();
        let add_arc = added.len() - add_img;
        let add_total = added.len();

        self.insert_level_sorted(added);
        self.tree.adjust_counts(
            dir,
            add_img as isize - rm_img as isize,
            add_arc as isize - rm_arc as isize,
        );
        // first_image 可能已指向被删的文件：用全量有序列表重算这一支
        self.tree.recompute_first_from(id, &self.images);

        if cur_gone && !self.images.is_empty() {
            // 当前图被删：落到原位置的图（越界则取末位），保持浏览位置感
            let i = self.index.min(self.images.len() - 1);
            self.goto(i, cx);
        } else if self.images.is_empty() {
            self.current = None;
            self.index = 0;
        }
        self.note = Some(format!("列表已更新：+{} −{}", add_total, removed.len()));
    }

    /// 按当前视口物理尺寸算解码最长边上限。
    /// "适应窗口"只需要视口那么多像素；放大到 100% 由 ROI 补细节。
    fn decode_max_dim(&self) -> u32 {
        decode_dim_for(self.viewport_px)
    }

    /// 当前浏览位置是否在网络上（懒探测一次：GetDriveTypeW 是纯本地调用）。
    ///
    /// 网络盘上带宽就是硬上限，解码已经不重要了：
    /// 18 MB 的扫描图在百兆口上光传输就要 1.6 s，此时
    /// ① 多个预取并发只会互相抢带宽 → 限制成单通道 +1；
    /// ② 1/8 快解码预览毫无意义（它读的字节数和全图一样多）→ 直接跳过。
    fn on_remote(&mut self) -> bool {
        if let Some(v) = self.remote {
            return v;
        }
        let probe = self
            .current_path()
            .or_else(|| self.images.get(self.index))
            .map(|p| win32::is_remote(p))
            .unwrap_or(false);
        self.remote = Some(probe);
        probe
    }

    /// 认领冷启动预热结果：命中就塞进缓存（goto 随后的缓存查询会直接上屏）。
    fn take_preload(&mut self, want: &Path) -> PreloadState {
        let Some(rx) = self.preload.as_ref() else {
            return PreloadState::None;
        };
        match rx.try_recv() {
            Ok((path, result)) => {
                self.preload = None;
                if let Ok(mut g) = self.preload_path.lock() {
                    *g = None;
                }
                if path != want {
                    return PreloadState::None;
                }
                let Ok(decoded) = result else {
                    return PreloadState::None;
                };
                let (w, h) = (decoded.width, decoded.height);
                let (sw, sh) = (decoded.src_width, decoded.src_height);
                let o = decoded.orientation;
                self.preload_stats =
                    Some((path.clone(), decoded.decode_ms, decoded.read_ms, decoded.fast));
                let render = make_render_image(decoded);
                self.cache_insert(
                    path,
                    CacheEntry {
                        render,
                        width: w,
                        height: h,
                        src_width: sw,
                        src_height: sh,
                        orientation: o,
                    },
                );
                PreloadState::Hit
            }
            // 还在读/解（网络盘上一张大图要 1～2 s）：调用方应该等它，
            // 而不是另开一次解码把同一份字节再拉一遍
            Err(std::sync::mpsc::TryRecvError::Empty)
                if self
                    .preload_path
                    .lock()
                    .map(|g| g.as_deref() == Some(want))
                    .unwrap_or(false) =>
            {
                PreloadState::Pending
            }
            // 不是这张图，或通道已断：走常规异步路径
            Err(_) => PreloadState::None,
        }
    }

    /// 预热还在路上：等它落地后直接上屏，避免二次读盘/二次解码。
    fn wait_preload(&mut self, cx: &mut Context<Self>) {
        if self.preload_waiting {
            return;
        }
        let Some(rx) = self.preload.take() else {
            return;
        };
        self.preload_waiting = true;
        let dec_gen = self.decode_gen;
        cx.spawn(async move |view: WeakEntity<Self>, cx: &mut AsyncApp| {
            let msg = cx
                .background_executor()
                .spawn(async move { rx.recv().ok() })
                .await;
            view.update(cx, |view, cx| {
                view.preload_waiting = false;
                if view.decode_gen != dec_gen {
                    cx.notify();
                    return;
                }
                match msg {
                    Some((p, Ok(decoded))) => {
                        let stats = (decoded.decode_ms, decoded.read_ms, decoded.fast);
                        let (w, h) = (decoded.width, decoded.height);
                        let (sw, sh) = (decoded.src_width, decoded.src_height);
                        let o = decoded.orientation;
                        let render = make_render_image(decoded);
                        view.cache_insert(
                            p.clone(),
                            CacheEntry {
                                render,
                                width: w,
                                height: h,
                                src_width: sw,
                                src_height: sh,
                                orientation: o,
                            },
                        );
                        view.preload_stats = Some((p.clone(), stats.0, stats.1, stats.2));
                        if let Ok(mut g) = view.preload_path.lock() {
                            *g = None;
                        }
                        // 预热期间用户可能翻到别的图：无条件按当前 index 重入 goto。
                        // 路径相同则命中刚插入的缓存，不同则走正常异步加载。
                        let idx = view.index;
                        view.goto(idx, cx);
                    }
                    // 预热失败（文件损坏/还没下载完）：回到常规异步路径重试
                    _ => {
                        if let Ok(mut g) = view.preload_path.lock() {
                            *g = None;
                        }
                        if let Some(p) = view.images.get(view.index) {
                            view.spawn_load_now(p.to_path_buf(), true, cx);
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 后台解码；`set_current` 为 false 时是纯预取（只进缓存）。
    /// 预取受并发上限约束：满了排队，解码完成回调里再补位。
    fn spawn_load(&mut self, path: PathBuf, set_current: bool, cx: &mut Context<Self>) {
        if !set_current && self.in_flight.len() >= self.max_prefetch_tasks {
            if !self.prefetch_queue.contains(&path) && !self.in_flight.contains(&path) {
                self.prefetch_queue.push_back(path);
            }
            return;
        }
        self.spawn_load_now(path, set_current, cx);
    }

    /// 并发有空位时补发排队中的预取任务
    fn pump_prefetch_queue(&mut self, cx: &mut Context<Self>) {
        while self.in_flight.len() < self.max_prefetch_tasks {
            let Some(next) = self.prefetch_queue.pop_front() else {
                break;
            };
            if self.cache.iter().any(|(p, _)| *p == next) || self.in_flight.contains(&next) {
                continue;
            }
            self.spawn_load_now(next, false, cx);
        }
    }

    /// 第一级渲染（首帧预览）：全质量解码还没回来时，先上屏一个"够清楚"的版本。
    ///
    /// 取图顺序：
    /// 1. EXIF 内嵌缩略图，且最长边 ≥ PREVIEW_MIN_THUMB（相机常见 160×120 会被跳过：
    ///    拉伸到满屏就是一团马赛克，正是之前"初始缩略图太糊"的原因）；
    /// 2. JPEG 走一档 DCT 降采样快解码（最长边 ~PREVIEW_DIM，6000px 图落在 1/8 = 750px，
    ///    约 20ms，比缩略图清晰 4 倍以上）；
    /// 3. 都不行就退回缩略图（哪怕小，聊胜于无）。
    /// 非 JPEG（PNG/BMP 等）不做第 2 步：那条路是全量解码，做预览等于白解一遍。
    fn spawn_preview(&mut self, path: PathBuf, preview_id: u64, cx: &mut Context<Self>) {
        let fast = self.fast_mode;
        cx.spawn(async move |view: WeakEntity<Self>, cx: &mut AsyncApp| {
            let p = path.clone();
            let decoded = cx
                .background_executor()
                .spawn(async move {
                    let thumb = turbo::jpeg_thumbnail_file(&p);
                    let big_enough = thumb.as_ref().is_some_and(|t| {
                        t.width.max(t.height) >= PREVIEW_MIN_THUMB
                    });
                    if big_enough {
                        return thumb;
                    }
                    // 小文件（<1MB）全质量解码本来就 20~30ms，快解码抢不到时间，
                    // 反而白占一个核；这种就直接等全质量帧，不闪一下糊图。
                    let worth_it = std::fs::metadata(&p)
                        .map(|m| m.len() >= PREVIEW_MIN_BYTES)
                        .unwrap_or(true);
                    if !worth_it {
                        return None;
                    }
                    if looks_like_jpeg(&p) {
                        if let Ok(d) = decode_any(&p, fast, PREVIEW_DIM) {
                            return Some(d);
                        }
                    }
                    thumb
                })
                .await;
            let Some(decoded) = decoded else {
                return;
            };
            view.update(cx, |view, cx| {
                // 全图已落地（代际前进）或已翻走 → 丢弃
                if view.thumb_gen != preview_id || view.current_path() != Some(&path) {
                    return;
                }
                let (w, h) = (decoded.width, decoded.height);
                let (sw, sh) = (decoded.src_width, decoded.src_height);
                let o = decoded.orientation;
                let ms = decoded.decode_ms;
                view.set_image(make_render_image(decoded), w, h, sw, sh, o, ms, 0.0, true, false, true);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn spawn_load_now(&mut self, path: PathBuf, set_current: bool, cx: &mut Context<Self>) {
        self.in_flight.push(path.clone());
        let fast = self.fast_mode;
        let dec_gen = self.decode_gen;
        let max_dim = self.decode_max_dim();

        if set_current {
            trace("decode spawn (current)");
            self.thumb_gen += 1;
            // 网络盘上不做"快解码预览"：JPEG 的熵编码数据必须从头走到尾，
            // 1/8 缩放解出来的字节数和全图一样多。同一份 18 MB 读两遍，
            // 只会让真正的全质量帧更晚到 —— 直接跳过。
            if !self.on_remote() {
                self.spawn_preview(path.clone(), self.thumb_gen, cx);
            }
        }

        cx.spawn(async move |view: WeakEntity<Self>, cx: &mut AsyncApp| {
            let decode_path = path.clone();
            let mut result = cx
                .background_executor()
                .spawn(async move {
                    // 预取降优先级：低配机上 4 个同级线程会把当前图的解码挤慢
                    if !set_current {
                        win32::thread_priority_low();
                    }
                    let r = decode_any(&decode_path, fast, max_dim);
                    if !set_current {
                        win32::thread_priority_normal();
                    }
                    r
                })
                .await;

            // 文件可能还在下载/拷贝中（NAS 上的扫描本很常见）：读到半个 JPEG
            // 时等一下重试。用异步 timer 而不是 thread::sleep —— 后者会占死
            // 一个 executor worker 800ms，低配机核少时把预取/ROI 全堵在后面。
            if let Err(e) = &result {
                if is_truncated_jpeg_error(&format!("{e:#}")) {
                    if trace_enabled() {
                        trace("decode truncated → retry in 800ms");
                    }
                    cx.background_executor()
                        .timer(Duration::from_millis(800))
                        .await;
                    let retry_path = path.clone();
                    result = cx
                        .background_executor()
                        .spawn(async move {
                            if !set_current {
                                win32::thread_priority_low();
                            }
                            let r = decode_any(&retry_path, fast, max_dim);
                            if !set_current {
                                win32::thread_priority_normal();
                            }
                            r
                        })
                        .await;
                }
            }

            view.update(cx, |view, cx| {
                view.in_flight.retain(|p| p != &path);
                // 让位后立刻补发排队中的预取
                view.pump_prefetch_queue(cx);
                // decode_gen 变化：模式切换已作废旧解码，连缓存都不写入
                if view.decode_gen != dec_gen {
                    cx.notify();
                    return;
                }
                // 过期守卫：用户已翻走（当前索引不再是这张图）时丢弃结果，
                // 防止快速连翻时旧图覆盖新图
                let still_current = view.current_path() == Some(&path);
                match result {
                    Ok(decoded) => {
                        let width = decoded.width;
                        let height = decoded.height;
                        let src_width = decoded.src_width;
                        let src_height = decoded.src_height;
                        let orientation = decoded.orientation;
                        let decode_ms = decoded.decode_ms;
                        let read_ms = decoded.read_ms;
                        let fast = decoded.fast;
                        let render = make_render_image(decoded);
                        view.cache_insert(
                            path.clone(),
                            CacheEntry {
                                render: render.clone(),
                                width,
                                height,
                                src_width,
                                src_height,
                                orientation,
                            },
                        );
                        if still_current {
                            if trace_enabled() {
                                trace(&format!("decode landed: {}x{} {}ms", width, height, decode_ms));
                            }
                            view.set_image(
                                render, width, height, src_width, src_height, orientation,
                                decode_ms, read_ms, fast, false, false,
                            );
                            // 全图已落地：作废还没回来的缩略图
                            view.thumb_gen += 1;
                            // 首次从磁盘解出来的图也要预取下一张，
                            // 否则"打开目录→第 2 张"永远是冷解码
                            view.prefetch(cx);
                        }
                    }
                    Err(e) => {
                        if still_current {
                            let msg = format!("{e:#}");
                            view.error = Some(if is_truncated_jpeg_error(&msg) {
                                // 数据没走完：多半是文件还在下载/拷贝，或传输中断
                                format!("文件不完整（可能仍在下载/传输中断）：{msg}")
                            } else {
                                msg
                            });
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn cache_insert(&mut self, path: PathBuf, entry: CacheEntry) {
        if let Some(pos) = self.cache.iter().position(|(p, _)| *p == path) {
            self.cache.remove(pos);
        }
        self.cache.insert(0, (path, entry));
        // 字节预算淘汰：从最旧的尾部弹出，直到总占用回到预算内
        let mut total: usize = self.cache.iter().map(|(_, e)| e.bytes()).sum();
        while (total > self.cache_budget || self.cache.len() > self.cache_max_entries)
            && self.cache.len() > 1
        {
            if let Some((_, e)) = self.cache.pop() {
                total -= e.bytes();
            }
        }
    }

    /// 后台预取相邻图。
    ///
    /// 机械盘上只预取 +1：4 个随机读会让磁头来回跳，反而把当前图的
    /// 解码拖慢（低配机常配机械盘，这是最典型的一类）。SSD 才前后各取两张。
    fn prefetch(&mut self, cx: &mut Context<Self>) {
        if self.images.is_empty() {
            return;
        }
        // 懒探测一次磁盘类型（IOCTL 约 0.1ms，只做一次）
        if !self.rotational_known {
            self.rotational_known = true;
            self.rotational = self
                .images
                .first()
                .and_then(|p| win32::is_rotational(p))
                .unwrap_or(false);
        }
        // 网络盘：只有一条 11 MB/s 的通道，四个预取并发就是四个 2.7 MB/s
        // 的流，用户一按"下一张"就得排在它们后面等好几秒。
        // 收紧成"单线程 + 只取下一张"，把带宽留给用户真正要看的那张。
        let slow_source = self.on_remote() || self.rotational;
        if slow_source {
            self.max_prefetch_tasks = 1;
            self.prefetch_queue.truncate(1);
        }
        // 快速连翻时队列会积压，丢掉离当前位置超过 3 张的过期项
        let idx_now = self.index;
        let imgs = &self.images;
        // images 始终按路径有序（insert_level_sorted / find_image_index 都依赖这一点）。
        // 用二分替代逐项线性 position：几十万张图时每次翻页少做 O(N) 全表比较。
        self.prefetch_queue.retain(|p| {
            let i = imgs.partition_point(|x| x.as_ref() < p.as_path());
            i < imgs.len() && imgs[i] == *p && i.abs_diff(idx_now) <= 3
        });
        let deltas: &[isize] = if slow_source {
            &[1]
        } else {
            &[1, -1, 2, -2]
        };
        for &delta in deltas {
            let idx = (self.index as isize + delta).rem_euclid(self.images.len() as isize) as usize;
            let path = self.images[idx].to_path_buf();
            // 压缩包不是图片：翻到它时才展开，不预取
            if archive::is_archive(&path) {
                continue;
            }
            let cached = self.cache.iter().any(|(p, _)| p.as_path() == path.as_path());
            if !cached && !self.in_flight.contains(&path) {
                self.spawn_load(path, false, cx);
            }
        }
    }

    /// 打开新的浏览根目录（位置区点击 / 拖入目录）：
    /// 重置状态 → 建根节点 → 只扫根的第一层
    fn open_root(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.reset_state();
        self.seed_root(path, cx);
        cx.notify();
    }

    /// 打开压缩包（zip/7z/rar）：包内图片成为浏览列表（虚拟路径）。
    /// 侧边栏仍显示压缩包所在目录的目录树，保留原有浏览能力。
    fn open_archive(&mut self, archive_path: PathBuf, cx: &mut Context<Self>) {
        self.reset_state();
        match archive::list_images(&archive_path) {
            Ok(names) if !names.is_empty() => {
                self.images = names
                    .iter()
                    .map(|n| Arc::<Path>::from(virtual_path(&archive_path, n)))
                    .collect();
                self.goto(0, cx);
                if let Some(root) = archive_path.parent().map(Path::to_path_buf) {
                    self.seed_root(root, cx);
                }
                let pkg = archive_path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| archive_path.display().to_string());
                self.archive_counts.insert(archive_path.clone(), names.len());
                self.archive_info = Some(format!("📦 {pkg} › {} 张", names.len()));
            }
            Ok(_) => {
                self.error = Some("压缩包内没有图片".into());
            }
            Err(e) => {
                self.error = Some(format!("打开压缩包失败: {e}"));
            }
        }
        cx.notify();
    }

    /// 清空浏览状态（图片列表、缓存、幻灯片），保留窗口设置
    fn reset_state(&mut self) {
        self.images.clear();
        self.index = 0;
        self.current = None;
        self.error = None;
        self.cache.clear();
        self.in_flight.clear();
        self.prefetch_queue.clear();
        self.thumb_gen += 1;
        self.slideshow = false;
        self.slideshow_gen += 1;
        self.nav_target = None;
        self.tree_reveal = None;
        self.history.clear();
        self.note = None;
        self.archive_info = None;
        self.archive_expanded.clear();
        self.archive_failed.clear();
        self.archive_counts.clear();
        if let Some((_, w)) = self.watch.take() {
            w.stop();
        }
        self.refreshing.clear();
        self.clear_roi();
    }

    /// 建立根节点并扫描其第一层（目录树惰性展开的起点）
    fn seed_root(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        self.scan_generation += 1;
        self.tree = DirTree::new();
        let root_id = self.tree.ensure_dir(&root);
        // 展开"盘符根 → 浏览根"整条链：只 set_expanded 浏览根的话，
        // 祖先全是收起态，侧边栏就只剩一行 "C://"，看不到任何子目录。
        self.tree.expand_to(root_id);
        self.scan_level(root_id, cx);
    }

    /// 惰性扫描目录 `id` 的直接一层（子目录 + 图片），后台执行不阻塞 UI
    fn scan_level(&mut self, id: usize, cx: &mut Context<Self>) {
        if self.tree.is_scanned(id) {
            return; // 已扫描/已派发
        }
        self.tree.set_scanned(id); // 立即置位防重复派发
        self.tree.set_loading(id, true); // 箭头保留但淡显，扫完再恢复
        let path = self.tree.path_of(id).to_path_buf();
        self.pending_levels += 1;
        let generation = self.scan_generation;

        cx.spawn(async move |view: WeakEntity<Self>, cx: &mut AsyncApp| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    scan_one_level(&path, SUPPORTED_EXTENSIONS, ARCHIVE_EXTENSIONS)
                })
                .await;
            view.update(cx, |v, cx| {
                v.pending_levels = v.pending_levels.saturating_sub(1);
                if v.scan_generation == generation {
                    v.on_level_scanned(id, result, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 一层扫描完成：子目录进树（未扫描态）、图片进全局有序列表
    fn on_level_scanned(
        &mut self,
        id: usize,
        level: turboview::scanner::DirLevel,
        cx: &mut Context<Self>,
    ) {
        let turboview::scanner::DirLevel {
            subdirs,
            images,
            archives,
        } = level;
        self.tree.set_loading(id, false); // 扫描结束：箭头恢复常态
        self.tree.add_subdirs(id, &subdirs);
        let was_empty = self.images.is_empty();
        self.tree.add_images(&images);
        self.tree.add_archives(&archives);
        // 图片与压缩包合并成一个有序批次：压缩包在列表里占位，
        // 翻到它时内联展开，包内图紧随其后
        let mut batch = images;
        batch.extend(archives);
        self.insert_level_sorted(batch);

        // 点击未扫描目录后的跳转：该层扫完自动落到它的第一张图
        if self.nav_target == Some(id) {
            self.nav_target = None;
            if let Some(p) = self.tree.first_image(id).cloned() {
                if let Some(i) = self.find_image_index(&p) {
                    self.goto(i, cx);
                }
            }
        } else if was_empty && self.current.is_none() && !self.images.is_empty() {
            // 打开根目录后的首图
            self.goto(0, cx);
        }
        self.prefetch(cx);
    }

    /// 把一层扫描结果（图片 + 压缩包）按序合并进全局列表。
    /// 列表恒有序、重复扫描自动去重；落在替换区间内的**虚拟条目**
    /// （已内联展开的压缩包内容）会被保留，避免重扫后包内图丢失。
    fn insert_level_sorted(&mut self, batch: Vec<PathBuf>) {
        if batch.is_empty() {
            return;
        }
        let mut batch = batch;
        batch.sort_unstable();
        let cur = self.current_path().map(Path::to_path_buf); // 合并会移动元素，先记下当前图
        let lo = self.images.partition_point(|p| p.as_ref() < batch[0].as_path());
        let hi = self.images.partition_point(|p| p.as_ref() <= batch[batch.len() - 1].as_path());
        let keep: HashSet<&PathBuf> = batch.iter().collect();
        let preserved: Vec<PathBuf> = self.images[lo..hi]
            .iter()
            .filter(|p| split_virtual(p).is_some_and(|(a, _)| keep.contains(&a)))
            .map(|p| p.to_path_buf())
            .collect();
        batch.extend(preserved);
        batch.sort_unstable();
        self.images.splice(lo..hi, batch.into_iter().map(Arc::<Path>::from));
        if let Some(c) = cur {
            if let Some(i) = self.find_image_index(&c) {
                self.index = i;
            }
        }
        if !self.images.is_empty() && self.index >= self.images.len() {
            self.index = 0;
        }
    }

    fn set_image(
        &mut self,
        render: Arc<RenderImage>,
        width: u32,
        height: u32,
        src_width: u32,
        src_height: u32,
        orientation: u32,
        decode_ms: f64,
        read_ms: f64,
        fast: bool,
        cached: bool,
        preview: bool,
    ) {
        self.current = Some(LoadedImage {
            render,
            width,
            height,
            src_width,
            src_height,
            orientation,
            decode_ms,
            read_ms,
            fast,
            cached,
            preview,
            preloaded: false,
        });
        // 「原图」开关打开时保持 1:1：按这一帧的纹理尺寸重算缩放，
        // 等最终质量帧落地后会自动再算一次（归一化到原始像素）。
        if self.original_view {
            let (sw, _) = turbo::oriented_dims(orientation, src_width, src_height);
            self.zoom = if width == 0 {
                1.0
            } else {
                (sw as f32 / width as f32).clamp(ZOOM_MIN, ZOOM_MAX)
            };
            self.fit = false;
            self.recenter = true;
        } else {
            self.fit = true;
            self.zoom = 1.0;
        }
        self.offset = point(px(0.), px(0.));
        self.note = None;
        self.clear_roi();
    }

    /// 清空 ROI 状态（切图时调用）；在途结果由代际守卫丢弃
    fn clear_roi(&mut self) {
        self.roi_overlay = None;
        self.roi_wanted = None;
        self.roi_gen += 1;
        // 旧解码可能还要几百 ms；不复位的话新图的 ROI 会被它一直挡在门外
        self.roi_inflight = false;
    }

    /// 以 `cursor` 为锚点缩放
    fn zoom_at(&mut self, cursor: Point<Pixels>, factor: f32) {
        let old = if self.fit {
            self.effective_zoom
        } else {
            self.zoom
        };
        let new = (old * factor).clamp(ZOOM_MIN, ZOOM_MAX);
        if (new - old).abs() < 1e-6 {
            return;
        }
        let k = new / old;
        self.zoom = new;
        self.fit = false;
        // 手动缩放后就不再是"原图 1:1"了，状态栏开关跟着关掉
        self.original_view = false;
        self.recenter = false;
        self.offset = point(
            cursor.x - (cursor.x - self.offset.x) * k,
            cursor.y - (cursor.y - self.offset.y) * k,
        );
    }

    // ---------- 事件处理 ----------

    /// "下一张"：随机模式下随机跳转（记入历史），否则线性前进
    fn nav_next(&mut self, cx: &mut Context<Self>) {
        if self.images.is_empty() {
            return;
        }
        if self.random_mode {
            self.next_random(cx);
        } else {
            let next = (self.index + 1) % self.images.len();
            self.goto(next, cx);
        }
    }

    /// "上一张"：随机模式下优先退回历史记录，否则线性后退
    fn nav_prev(&mut self, cx: &mut Context<Self>) {
        if self.images.is_empty() {
            return;
        }
        if self.random_mode {
            if let Some(i) = self.history.pop() {
                self.goto(i, cx);
                return;
            }
        }
        let prev = if self.index == 0 {
            self.images.len() - 1
        } else {
            self.index - 1
        };
        // back = true：经过压缩包时向前越过，而不是掉进包里
        self.goto_dir(prev, true, cx);
    }

    /// 随机跳一张（当前图压栈，供右键退回）
    fn next_random(&mut self, cx: &mut Context<Self>) {
        if self.images.len() < 2 {
            return;
        }
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as usize ^ (d.as_secs() as usize) << 13)
            .unwrap_or(0);
        let mut idx = (n ^ self.index.wrapping_mul(2654435761)) % self.images.len();
        if idx == self.index {
            idx = (idx + 1) % self.images.len();
        }
        self.history.push(self.index);
        self.goto(idx, cx);
    }

    /// 伪全屏切换：全屏时隐藏目录树（图片自动占满居中），退出时恢复
    fn toggle_fullscreen(&mut self, cx: &mut Context<Self>) {
        self.fullscreen = !self.fullscreen;
        if self.fullscreen {
            self.sidebar_before_fullscreen = self.sidebar_visible;
            self.sidebar_visible = false;
        } else {
            self.sidebar_visible = self.sidebar_before_fullscreen;
        }
        win32::toggle_maximize(&self.window_title);
        cx.notify();
    }

    fn on_next_image(&mut self, _: &NextImage, _: &mut Window, cx: &mut Context<Self>) {
        self.nav_next(cx);
    }

    fn on_prev_image(&mut self, _: &PrevImage, _: &mut Window, cx: &mut Context<Self>) {
        self.nav_prev(cx);
    }

    fn on_zoom_in(&mut self, _: &ZoomIn, window: &mut Window, cx: &mut Context<Self>) {
        let center = self.image_center(window);
        self.zoom_at(center, 1.25);
        cx.notify();
    }

    fn on_zoom_out(&mut self, _: &ZoomOut, window: &mut Window, cx: &mut Context<Self>) {
        let center = self.image_center(window);
        self.zoom_at(center, 0.8);
        cx.notify();
    }

    fn on_zoom_actual(&mut self, _: &ZoomActual, window: &mut Window, cx: &mut Context<Self>) {
        self.set_original_view(true, window, cx);
    }

    /// 「原图」开关：开 = 1:1 原始像素（居中），关 = 适应窗口。
    /// 状态栏按钮与 `1` / `0` 键共用这一处逻辑。
    fn set_original_view(&mut self, on: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.original_view = on;
        if !on {
            self.fit = true;
            self.zoom = 1.0;
            self.recenter = false;
            cx.notify();
            return;
        }
        // zoom=1 是"一解码像素一屏幕像素"。解码时按窗口降采样过，所以
        // "原始大小"要按原始像素算：目标缩放 = 显示方向原始宽 / 解码宽。
        // 这同时让 ROI 触发条件（s = zoom*解码宽/原始宽）达到 1.0，补上细节。
        let (dw, dh, sw) = match self.current.as_ref() {
            Some(c) => {
                let (sw, _) = turbo::oriented_dims(c.orientation, c.src_width, c.src_height);
                (c.width, c.height, sw)
            }
            // 还没有图（首帧未落地）：记下开关，等 set_image 时应用
            None => {
                cx.notify();
                return;
            }
        };
        if dw == 0 || dh == 0 || sw == 0 {
            cx.notify();
            return;
        }
        let target = (sw as f32 / dw as f32).clamp(ZOOM_MIN, ZOOM_MAX);
        // 直接居中，而不是"保持光标处不动"：从 fit 跳到 1:1 时缩放比很大，
        // 沿用旧 offset 会把图推到一边、露出大片空白（旧实现的既有问题）。
        let vp = window.viewport_size();
        let sb = if self.sidebar_visible {
            px(SIDEBAR_WIDTH)
        } else {
            px(0.0)
        };
        let aw = vp.width - sb;
        let ah = vp.height - px(26.0);
        self.zoom = target;
        self.fit = false;
        self.recenter = false;
        self.offset = point(
            (aw - px(dw as f32 * target)) / 2.0,
            (ah - px(dh as f32 * target)) / 2.0,
        );
        cx.notify();
    }

    fn on_zoom_fit(&mut self, _: &ZoomFit, window: &mut Window, cx: &mut Context<Self>) {
        self.set_original_view(false, window, cx);
    }

    fn on_toggle_fast(&mut self, _: &ToggleFastMode, _: &mut Window, cx: &mut Context<Self>) {
        self.fast_mode = !self.fast_mode;
        // 清缓存 + 换代：在途任务的结果会被丢弃，不会把旧 fast 模式的图写回缓存
        self.decode_gen += 1;
        self.thumb_gen += 1; // 同时作废旧 fast 模式的预览帧
        self.cache.clear();
        self.in_flight.clear();
        self.prefetch_queue.clear();
        let index = self.index;
        self.goto(index, cx);
    }

    /// 切换目录树显隐（点汉堡按钮 / B 键共用；双击图片区不触发）
    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.sidebar_visible = !self.sidebar_visible;
        cx.notify();
    }

    fn on_toggle_sidebar(&mut self, _: &ToggleSidebar, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle_sidebar(cx);
    }

    /// 切换明暗主题：翻转 dark_mode 标记，并用本标记驱动自建调色板；
    /// 同时调用平台外观设置（macOS 上让原生窗口边框同步，Windows 为 no-op）。
    fn toggle_theme(&mut self, cx: &mut Context<Self>) {
        self.dark_mode = !self.dark_mode;
        let appearance = if self.dark_mode {
            WindowAppearance::Dark
        } else {
            WindowAppearance::Light
        };
        cx.set_window_appearance(Some(appearance));
        cx.notify();
    }

    fn on_toggle_theme(&mut self, _: &ToggleTheme, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle_theme(cx);
    }

    /// F5：重扫当前目录这一层（与资源管理器一致）
    fn on_refresh_tree(&mut self, _: &RefreshTree, _: &mut Window, cx: &mut Context<Self>) {
        self.refresh_current_dir(cx);
    }

    fn on_random(&mut self, _: &RandomImage, _: &mut Window, cx: &mut Context<Self>) {
        // R = 切换随机模式；进入时清空历史，"下一张"变为随机跳转
        self.random_mode = !self.random_mode;
        self.history.clear();
        cx.notify();
    }

    fn on_toggle_slideshow(&mut self, _: &ToggleSlideshow, _: &mut Window, cx: &mut Context<Self>) {
        self.set_slideshow(!self.slideshow, cx);
    }

    /// 幻灯片开关（S 键 / 状态栏徽标点击共用）。开启时启动定时循环，
    /// 代际守卫保证旧循环不会干扰新一轮。
    fn set_slideshow(&mut self, on: bool, cx: &mut Context<Self>) {
        if on == self.slideshow {
            return;
        }
        self.slideshow = on;
        if on {
            self.slideshow_gen += 1;
            let generation = self.slideshow_gen;
            cx.spawn(async move |view: WeakEntity<Self>, cx: &mut AsyncApp| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_secs(SLIDESHOW_INTERVAL))
                        .await;
                    let mut stop = false;
                    view.update(cx, |v, cx| {
                        if !v.slideshow || v.slideshow_gen != generation || v.images.is_empty() {
                            stop = true;
                            return;
                        }
                        let next = (v.index + 1) % v.images.len();
                        if v.random_mode {
                            v.next_random(cx);
                        } else {
                            v.goto(next, cx);
                        }
                    })
                    .ok();
                    if stop {
                        break;
                    }
                }
            })
            .detach();
        }
        cx.notify();
    }

    fn on_toggle_fullscreen(&mut self, _: &ToggleFullscreen, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle_fullscreen(cx);
    }

    // ---------- ROI 渐进重解码 ----------

    /// 根据当前视口计算期望的 ROI（显示方向源坐标系、原始分辨率）；
    /// 不需要时返回 None。触发条件：纹理是降采样版、源→屏幕比例接近 100%、
    /// 可见区域面积在 ROI_MAX_PIXELS 以内。
    fn compute_roi_rect(
        &self,
        zoom_now: f32,
        img_offset: Point<Pixels>,
        area_w: f32,
        vp_h: f32,
    ) -> Option<RoiRect> {
        let cur = self.current.as_ref()?;
        let (sw, sh) = turbo::oriented_dims(cur.orientation, cur.src_width, cur.src_height);
        if sw == 0 || sh == 0 || cur.width == 0 || cur.height == 0 {
            return None;
        }
        // 纹理已是原始分辨率（小图）→ 基础纹理即完整细节
        if cur.width >= sw && cur.height >= sh {
            return None;
        }
        let s = zoom_now * cur.width as f32 / sw as f32; // 源像素 → 屏幕像素
        if s < ROI_SCALE_THRESHOLD {
            return None; // 离 100% 还远，降采样纹理足够
        }
        // 可见的源区域（屏幕视口 → 纹理坐标 → 源坐标）
        let off_x = f32::from(img_offset.x);
        let off_y = f32::from(img_offset.y);
        let fx = |v: f32| v.max(0.0).min(cur.width as f32);
        let fy = |v: f32| v.max(0.0).min(cur.height as f32);
        let sx0 = fx(-off_x / zoom_now) * sw as f32 / cur.width as f32;
        let sx1 = fx((area_w - off_x) / zoom_now) * sw as f32 / cur.width as f32;
        let sy0 = fy(-off_y / zoom_now) * sh as f32 / cur.height as f32;
        let sy1 = fy((vp_h - off_y) / zoom_now) * sh as f32 / cur.height as f32;
        // 四周加 30% 余量（平移时不必频繁重解码）并夹取到图内
        let mx = (sx1 - sx0).abs() * 0.3;
        let my = (sy1 - sy0).abs() * 0.3;
        let sx0 = (sx0 - mx).max(0.0);
        let sy0 = (sy0 - my).max(0.0);
        let sx1 = (sx1 + mx).min(sw as f32);
        let sy1 = (sy1 + my).min(sh as f32);
        let w = (sx1 - sx0).ceil() as u64;
        let h = (sy1 - sy0).ceil() as u64;
        if w == 0 || h == 0 || w * h > ROI_MAX_PIXELS {
            return None; // 视野太大时基础纹理更划算
        }
        Some(RoiRect {
            x: sx0 as u32,
            y: sy0 as u32,
            w: w as u32,
            h: h as u32,
        })
    }

    /// 已有覆盖层是否盖住期望区域（显示坐标系）。
    /// iMCU 最大边长 32px（见 turbo.rs 的 TJ_MCU_WIDTH/HEIGHT），裁剪区向下对齐，
    /// 返回矩形恒为 want 的子集，最多少一个 iMCU。若要求严格包含则永远为 false：
    /// 解码完成 → cx.notify() → 下一帧再次 roi_kick → 静止画面反复重解码，
    /// 在低配机上把翻页任务挤到队尾。四边界统一留 32px 容差即可打断该循环，
    /// 缺口落在 want 自带 30% 余量内，不影响可见区域清晰度。
    fn roi_covers(&self, want: &RoiRect) -> bool {
        const MCU_TOLERANCE: u32 = 32;
        self.roi_overlay.as_ref().is_some_and(|o| {
            o.rect.x.saturating_add(MCU_TOLERANCE) >= want.x
                && o.rect.y.saturating_add(MCU_TOLERANCE) >= want.y
                && o.rect
                    .x
                    .saturating_add(o.rect.w)
                    .saturating_add(MCU_TOLERANCE)
                    >= want.x.saturating_add(want.w)
                && o.rect
                    .y
                    .saturating_add(o.rect.h)
                    .saturating_add(MCU_TOLERANCE)
                    >= want.y.saturating_add(want.h)
        })
    }

    /// 渲染时调用：更新期望区域，需要时派发（防抖 + 代际守卫）
    fn maybe_request_roi(
        &mut self,
        zoom_now: f32,
        img_offset: Point<Pixels>,
        area_w: f32,
        vp_h: f32,
        cx: &mut Context<Self>,
    ) {
        if self.current.is_none() || self.current_path().is_none() {
            return;
        }
        // 压缩包条目没有本地文件可做 ROI 重解码（基础纹理已完整可用）
        if self.current_path().is_some_and(|p| split_virtual(p).is_some()) {
            return;
        }
        let want = self.compute_roi_rect(zoom_now, img_offset, area_w, vp_h);
        self.roi_wanted = want.clone();
        if let Some(r) = want {
            if !self.roi_inflight && !self.roi_covers(&r) {
                self.roi_kick(cx);
            }
        }
    }

    /// 派发一次 ROI 解码（背景线程 → 150ms 防抖 → 解码 → 代际校验 → 上屏）
    fn roi_kick(&mut self, cx: &mut Context<Self>) {
        if self.roi_inflight {
            return;
        }
        let Some(rect) = self.roi_wanted.clone() else { return };
        let Some(path) = self.current_path().map(Path::to_path_buf) else { return };
        // 压缩包条目：decode_jpeg_roi 需要本地文件，直接跳过
        if split_virtual(&path).is_some() {
            return;
        }
        let Some(cur) = self.current.as_ref() else { return };
        self.roi_inflight = true;
        self.roi_gen += 1;
        let gen_id = self.roi_gen;
        let orientation = cur.orientation;
        let raw_w = cur.src_width;
        let raw_h = cur.src_height;
        let fast = self.fast_mode;
        let path_for_check = path.clone();

        cx.spawn(async move |view: WeakEntity<Self>, cx: &mut AsyncApp| {
            cx.background_executor().timer(ROI_DEBOUNCE).await;
            let result = cx
                .background_executor()
                .spawn(async move {
                    turbo::decode_jpeg_roi(&path, orientation, raw_w, raw_h, rect, fast)
                })
                .await;
            view.update(cx, |view, cx| {
                // 代际已变：这次结果属于上一张图。既不落地，也不能复位
                // roi_inflight —— 那会把新图已在途的解码标志清掉。
                if view.roi_gen != gen_id {
                    return;
                }
                view.roi_inflight = false;
                let still_current =
                    view.current_path().is_some_and(|p| *p == path_for_check);
                if still_current {
                    match result {
                        Ok(decoded) => {
                            let rect = RoiRect {
                                x: decoded.roi_x,
                                y: decoded.roi_y,
                                w: decoded.width,
                                h: decoded.height,
                            };
                            let render = make_render_image(decoded);
                            view.roi_overlay = Some(RoiOverlay { render, rect });
                        }
                        Err(_) => {} // 静默失败：基础降采样图仍完整可用
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // ---------- 文件操作 ----------

    /// Del：删除当前图片到回收站，然后自动跳到同位置的下一张
    fn on_delete_image(&mut self, _: &DeleteImage, _: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.current_path().cloned() else { return };
        // 压缩包内条目没有独立磁盘文件，无法删除
        if let Some((archive, _)) = split_virtual(&path) {
            self.note = Some(format!(
                "压缩包内条目不可删除（包: {}）",
                archive.display()
            ));
            cx.notify();
            return;
        }
        match win32::delete_to_recycle_bin(&path) {
            Ok(()) => {
                self.cache.retain(|(p, _)| p != &path);
                self.in_flight.retain(|p| p != &path);
                if let Some(i) = self.find_image_index(&path) {
                    self.images.remove(i);
                }
                if self.images.is_empty() {
                    self.current = None;
                    self.index = 0;
                    self.clear_roi();
                } else {
                    self.index = self.index.min(self.images.len() - 1);
                    self.goto(self.index, cx);
                }
                // note 在 goto 之后设置（set_image 会清空旧反馈）
                self.note = Some("🗑 已移入回收站".into());
            }
            Err(e) => {
                self.error = Some(format!("删除失败: {e}"));
            }
        }
        cx.notify();
    }

    /// Ctrl+C：把当前显示内容复制为 CF_DIB 到剪贴板
    fn on_copy_image(&mut self, _: &CopyImage, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(cur) = self.current.as_ref() {
            if let Some(bytes) = cur.render.as_bytes(0) {
                match win32::copy_dib_to_clipboard(bytes, cur.width, cur.height) {
                    Ok(()) => self.note = Some(format!("📋 已复制图片 ({}x{})", cur.width, cur.height)),
                    Err(e) => self.note = Some(format!("复制失败: {e}")),
                }
            }
        }
        cx.notify();
    }

    /// Ctrl+Shift+C：复制当前文件完整路径（压缩包条目复制 "包路径\条目名"）
    fn on_copy_path(&mut self, _: &CopyPath, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.current_path() {
            let text = match split_virtual(path) {
                Some((archive, entry)) => {
                    format!("{}\\{}", archive.display(), entry.replace('/', "\\"))
                }
                None => path.to_string_lossy().into_owned(),
            };
            match win32::copy_text_to_clipboard(&text) {
                Ok(()) => self.note = Some("📋 已复制路径".into()),
                Err(e) => self.note = Some(format!("复制失败: {e}")),
            }
        }
        cx.notify();
    }

    /// Ctrl+E：在资源管理器中定位当前文件（压缩包条目定位到压缩包本身）
    fn on_reveal_in_explorer(&mut self, _: &RevealInExplorer, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.current_path() {
            let target = match split_virtual(path) {
                Some((archive, _)) => archive,
                None => path.clone(),
            };
            if let Err(e) = win32::reveal_in_explorer(&target) {
                self.note = Some(format!("定位失败: {e}"));
            }
        }
        cx.notify();
    }

    /// 在 images 里定位路径：列表恒有序（insert_images_sorted 保证），二分 O(log N)
    fn find_image_index(&self, path: &Path) -> Option<usize> {
        let i = self.images.partition_point(|p| p.as_ref() < path);
        if i < self.images.len() && self.images[i].as_ref() == path {
            Some(i)
        } else {
            None
        }
    }

    /// 侧边栏行：箭头点击 = 展开/收起（未扫描的目录先扫该层）。
    /// 收起时保留后代的展开状态，与资源管理器一致：再展开原样恢复。
    fn dir_toggle(&mut self, id: usize, cx: &mut Context<Self>) {
        if !self.tree.is_scanned(id) {
            self.tree.set_expanded(id, true);
            self.scan_level(id, cx);
        } else {
            self.tree.toggle(id);
        }
        cx.notify();
    }

    /// 侧边栏行：名字点击 = 跳到该目录（子树）第一张图并展开；
    /// 该层未扫描时先派发扫描，扫完自动跳转（nav_target）
    fn dir_navigate(&mut self, id: usize, cx: &mut Context<Self>) {
        self.tree.expand_to(id);
        if let Some(p) = self.tree.first_image(id).cloned() {
            if let Some(i) = self.find_image_index(&p) {
                self.goto(i, cx);
                cx.notify();
                return;
            }
        }
        if self.tree.is_scanned(id) {
            // 已扫描过、子树里确实没有图：不再派发扫描，给个明确反馈
            // （否则 nav_target 会一直挂着，等一个永远不会到来的扫描结果）
            self.nav_target = None;
            self.note = Some("该目录下还没有找到图片".into());
            cx.notify();
            return;
        }
        self.nav_target = Some(id);
        self.scan_level(id, cx);
        cx.notify();
    }

    /// 当前图片所在目录 → 展开祖先链并标记"待滚动定位"。
    ///
    /// 资源管理器里选中节点会自动滚进视野；这里在每次切换图片后置位，
    /// 由 `render_sidebar` 消费一次（避免每帧都强制滚动，用户手动拖动不被打断）。
    fn reveal_current_dir(&mut self) {
        let Some(dir) = self
            .current_path()
            .and_then(|p| p.parent())
            .map(Path::to_path_buf)
        else {
            return;
        };
        // 压缩包条目是虚拟路径（"包|条目"），其 parent 不是真实目录，lookup 自然落空
        if let Some(id) = self.tree.lookup(&dir) {
            self.tree.expand_to(id);
            self.tree_reveal = Some(id);
        }
    }

    /// 工具栏"定位"：把当前目录展开并滚到可见处（不改变浏览位置）
    fn reveal_now(&mut self, cx: &mut Context<Self>) {
        match self
            .current_path()
            .and_then(|p| p.parent())
            .and_then(|d| self.tree.lookup(d))
        {
            Some(id) => {
                self.tree.expand_to(id);
                self.tree_reveal = Some(id);
                self.note = Some("已定位到当前目录".into());
            }
            None => self.note = Some("当前目录不在树中".into()),
        }
        cx.notify();
    }

    /// 工具栏"全部折叠"：保留根展开，其余收起
    fn collapse_tree(&mut self, cx: &mut Context<Self>) {
        self.tree.collapse_all();
        self.tree_reveal = None;
        cx.notify();
    }

    /// 工具栏"刷新"：重扫当前浏览目录这一层（增删项增量合并）
    fn refresh_current_dir(&mut self, cx: &mut Context<Self>) {
        // 还没打开图片时退回树的根，保证按钮任何时候都有意义
        let Some(dir) = self
            .current_path()
            .and_then(|p| p.parent())
            .map(Path::to_path_buf)
            .or_else(|| self.tree.root_path())
        else {
            return;
        };
        let Some(id) = self.tree.lookup(&dir) else {
            return;
        };
        if !self.tree.is_scanned(id) {
            return; // 该层还没扫过：首次扫描本身就是最新结果
        }
        self.refresh_dir(dir, cx);
        cx.notify();
    }

    /// 工具栏"在资源管理器中打开"：当前图（压缩包内则定位压缩包）；
    /// 还没有图时退到目录树的根，保证按钮任何时候都有意义。
    fn open_in_explorer(&mut self, cx: &mut Context<Self>) {
        let target = match self.current_path() {
            Some(path) => match split_virtual(path) {
                Some((archive, _)) => archive,
                None => path.clone(),
            },
            None => match self.tree.root_path() {
                Some(root) => root,
                None => return,
            },
        };
        if let Err(e) = win32::reveal_in_explorer(&target) {
            self.note = Some(format!("打开失败: {e}"));
        }
        cx.notify();
    }

    /// 图片区中心（缩放快捷键的锚点；图片区坐标系）
    fn image_center(&self, window: &Window) -> Point<Pixels> {
        let vp = window.viewport_size();
        let sb = if self.sidebar_visible {
            px(SIDEBAR_WIDTH)
        } else {
            px(0.)
        };
        point((vp.width - sb) / 2.0, vp.height / 2.0)
    }

    fn on_quit(&mut self, _: &Quit, _: &mut Window, cx: &mut Context<Self>) {
        cx.quit();
    }

    /// 左键按下：记录拖拽起点；右键按下：上一张（随机模式下退回历史）
    fn on_mouse_down(&mut self, event: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        match event.button {
            MouseButton::Left => self.drag = Some((event.position, self.offset)),
            MouseButton::Right => self.nav_prev(cx),
            _ => {}
        }
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if let Some((start_mouse, start_offset)) = self.drag {
            self.offset = point(
                start_offset.x + (event.position.x - start_mouse.x),
                start_offset.y + (event.position.y - start_mouse.y),
            );
            cx.notify();
        }
    }

    /// 左键抬起：位移小于阈值视为点击 —— 单击下一张；双击不触发任何动作
    fn on_mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if event.button == MouseButton::Left {
            if let Some((start, _)) = self.drag {
                let dx = f32::from(event.position.x - start.x);
                let dy = f32::from(event.position.y - start.y);
                let is_click = dx.abs() < CLICK_THRESHOLD && dy.abs() < CLICK_THRESHOLD;
                // 只认单击。双击的第二次抬起必须忽略，否则一次双击会翻两张
                // （第一次抬起已 nav_next）；更不再用它切换目录树。
                if is_click && event.click_count < 2 && !self.images.is_empty() {
                    self.nav_next(cx);
                }
            }
            self.drag = None;
            cx.notify();
        }
    }

    /// 滚轮 = 翻页（向下滚下一张，向上滚上一张）；Ctrl+滚轮 = 以光标为锚点缩放
    fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.images.is_empty() {
            return;
        }
        if event.modifiers.control {
            // 平滑缩放：像素增量过指数映射，每次滚动限制在 0.6x-1.6x
            let dy = match event.delta {
                ScrollDelta::Pixels(p) => f32::from(p.y),
                ScrollDelta::Lines(l) => l.y * 40.0,
            };
            let factor = (dy / 240.0f32).exp().clamp(0.6, 1.6);
            // event.position 是窗口坐标；offset 以图片区（侧边栏右侧）为原点
            let sb = if self.sidebar_visible { SIDEBAR_WIDTH } else { 0.0 };
            self.zoom_at(point(event.position.x - px(sb), event.position.y), factor);
            cx.notify();
            return;
        }
        let down = match event.delta {
            ScrollDelta::Pixels(p) => f32::from(p.y) < 0.0,
            ScrollDelta::Lines(l) => l.y < 0.0,
        };
        if down {
            self.nav_next(cx);
        } else {
            self.nav_prev(cx);
        }
        cx.notify();
    }

    fn on_file_drop(
        &mut self,
        paths: &ExternalPaths,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = paths.paths().first().cloned() else {
            return;
        };
        self.cache.clear();
        if path.is_dir() {
            // 拖入目录：切换浏览根（惰性扫第一层）
            self.open_root(path, cx);
        } else if archive::is_archive(&path) {
            // 拖入压缩包：包内图片成为浏览列表
            self.open_archive(path, cx);
        } else {
            // 拖入文件：立即显示，同时惰性扫其所在目录
            let root = path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| path.clone());
            self.reset_state();
            self.images = vec![Arc::<Path>::from(path)];
            self.goto(0, cx);
            self.seed_root(root, cx);
        }
        cx.notify();
    }

    /// 文件侧边栏：资源管理器（Explorer）风格目录树
    ///
    /// 自上而下：工具栏 → 当前路径条 → "位置"（常用目录 / 盘符）→ 树。
    /// 树行走 uniform_list 虚拟化，百万目录也只渲染可见行：
    /// 每行 = 缩进参考线 + 展开箭头 + 文件夹图标 + 名称（省略号截断）+ 图片数。
    fn render_sidebar(
        &mut self,
        colors: &gpui::colors::Colors,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let row_count = self.tree.rows().len();
        // 当前图片所在目录（选中行高亮）
        let selected = self
            .current_path()
            .and_then(|p| p.parent())
            .and_then(|d| self.tree.lookup(d));
        let current_dir = self
            .current_path()
            .and_then(|p| p.parent())
            .map(|d| d.display().to_string())
            .or_else(|| self.tree.root_path().map(|d| d.display().to_string()));
        let total_images = self.images.len();
        let scanning = self.pending_levels > 0;

        // 消费一次"定位"请求（祖先链已展开）：把目标行滚进视野。
        // Nearest 策略：已可见则不动，不打断用户的手动滚动。
        if let Some(id) = self.tree_reveal.take() {
            if let Some(ix) = self.tree.row_index(id) {
                self.tree_scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
            }
        }

        // 侧边栏专用派生色：gpui 基础色板只有 8 个色，
        // 缩进参考线与悬停底需要额外的中间色
        let (guide, hover_bg) = if self.dark_mode {
            (gpui::rgba(0xFFFFFF1E), gpui::rgba(0xFFFFFF12))
        } else {
            (gpui::rgba(0x00000018), gpui::rgba(0x0000000D))
        };

        // 工具栏小图标按钮（20x20 点按区）。
        // 图标用文字字形而非 SVG：gpui_windows 未实现 SvgRenderer，
        // svg() 元素在这套后端上画不出任何东西（状态栏那组图标同理）。
        let tool = |glyph: &'static str, id: (&'static str, usize)| {
            div()
                .id(id)
                .w(px(20.0))
                .h(px(20.0))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded_sm()
                .cursor_pointer()
                .text_sm()
                .text_color(colors.text)
                .opacity(0.62)
                .hover(|s| s.bg(hover_bg).opacity(1.0))
                .child(glyph)
        };

        let header = div()
            .h(px(32.0))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(1.0))
            .pl_2()
            .pr_1()
            .border_b_1()
            .border_color(colors.border)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_xs()
                    .text_color(colors.disabled)
                    .child("目录树"),
            )
            .child(
                tool("⟳", ("tv-tool-refresh", 0)).on_click(cx.listener(
                    |this, _: &ClickEvent, _w, cx| this.refresh_current_dir(cx),
                )),
            )
            .child(
                tool("⊟", ("tv-tool-collapse", 0)).on_click(cx.listener(
                    |this, _: &ClickEvent, _w, cx| this.collapse_tree(cx),
                )),
            )
            .child(
                tool("◎", ("tv-tool-locate", 0))
                    .on_click(cx.listener(|this, _: &ClickEvent, _w, cx| this.reveal_now(cx))),
            )
            .child(
                tool("↗", ("tv-tool-reveal", 0))
                    .on_click(cx.listener(|this, _: &ClickEvent, _w, cx| this.open_in_explorer(cx))),
            )
            .child(
                tool("☰", ("tv-tool-hide", 0))
                    .on_click(cx.listener(|this, _: &ClickEvent, _w, cx| this.toggle_sidebar(cx))),
            );

        // 当前浏览目录（中段省略，保留盘符与末尾目录名）+ 图片总数
        let path_bar = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(6.0))
            .px_2()
            .py(px(3.0))
            .border_b_1()
            .border_color(colors.border)
            .text_xs()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis_middle()
                    .text_color(colors.text)
                    .opacity(0.7)
                    .child(current_dir.unwrap_or_else(|| "未打开目录".into())),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(colors.disabled)
                    .child(if scanning {
                        format!("{}+ 张", fmt_count(total_images))
                    } else {
                        format!("{} 张", fmt_count(total_images))
                    }),
            );

        let list: AnyElement = if row_count == 0 {
            div()
                .flex_1()
                .p_3()
                .text_xs()
                .text_color(colors.disabled)
                .child("目录树为空 —— 拖入目录，或用命令行参数打开")
                .into_any_element()
        } else {
            let colors_in = colors.clone();
            let dark = (self.dark_mode as usize);
            div()
                .flex_1()
                .min_h_0()
                .child(
                    uniform_list("dir-list", row_count, {
                        cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                            let mut items = Vec::with_capacity(range.end - range.start);
                            // 只读可见区间的行：原来每帧 to_vec() 复制整棵目录树（几十万 String），
                            // 现在按 range 借用，翻页/刷新不再做全量分配。
                            for ix in range {
                                let Some(row) = this.tree.rows().get(ix).cloned() else {
                                    continue;
                                };
                                let is_selected = selected == Some(row.id);
                                // 有子目录、或子目录未知 / 正在扫描时保留展开箭头
                                let has_toggle =
                                    row.has_children || !row.scanned || row.loading;

                                // 层级连接线（资源管理器/VS Code 风格）：
                                // 非最后子节点的竖线贯穿整行；最后子节点的止于行中点，
                                // 并在本层补一条横线，形成 ├ / └ 的层级关系。
                                let mut indent = div().h_full().flex_shrink_0().flex().flex_row();
                                let depth = row.depth.min(63) as usize;
                                for level in 0..depth {
                                    let ancestor_last = (row.last_flags >> level) & 1 == 1;
                                    let cell = div()
                                        .relative()
                                        .w(px(TREE_INDENT))
                                        .h_full()
                                        .flex_shrink_0()
                                        .flex()
                                        .justify_center()
                                        .child(
                                            div()
                                                .w(px(1.0))
                                                .h(if ancestor_last {
                                                    px(ROW_HEIGHT / 2.0)
                                                } else {
                                                    px(ROW_HEIGHT)
                                                })
                                                .bg(guide),
                                        );
                                    let cell = if level + 1 == depth {
                                        cell.child(
                                            div()
                                                .absolute()
                                                .left(px(TREE_INDENT / 2.0))
                                                .top(px(ROW_HEIGHT / 2.0))
                                                .w(px(TREE_INDENT / 2.0))
                                                .h(px(1.0))
                                                .bg(guide),
                                        )
                                    } else {
                                        cell
                                    };
                                    indent = indent.child(cell);
                                }

                                // 计数/次要文字用淡色；箭头用正文色 + 透明度，更接近资源管理器
                                let fg = if is_selected {
                                    colors_in.selected_text
                                } else {
                                    colors_in.disabled
                                };
                                let arrow_fg = if is_selected {
                                    colors_in.selected_text
                                } else {
                                    colors_in.text
                                };

                                // 展开箭头：点击只切换展开态（stop_propagation 防止触发行点击跳转）
                                let chevron: AnyElement = if has_toggle {
                                    div()
                                        .id(("tv-arrow", row.id))
                                        .w(px(16.0))
                                        .h_full()
                                        .flex_shrink_0()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .cursor_pointer()
                                        .text_color(arrow_fg)
                                        .text_xs()
                                        // 子目录未知 / 扫描中：箭头淡显
                                        .opacity(if row.scanned && !row.loading {
                                            0.55
                                        } else {
                                            0.3
                                        })
                                        .on_click(cx.listener(
                                            move |this, _: &ClickEvent, _w, cx| {
                                                cx.stop_propagation();
                                                this.dir_toggle(row.id, cx);
                                            },
                                        ))
                                        .child(if row.expanded { "▾" } else { "▸" })
                                        .into_any_element()
                                } else {
                                    // 无子目录：留同宽空位，保证同层名称左对齐
                                    div().w(px(16.0)).h_full().flex_shrink_0().into_any_element()
                                };

                                // 文件夹图标：矢量轮廓在启动时按明暗各栅格化一套，
                                // 不再用 emoji（emoji 固定黄色，暗色主题下刺眼且不统一）
                                let folder = div()
                                    .w(px(16.0))
                                    .h_full()
                                    .flex_shrink_0()
                                    .mr(px(4.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        img(ImageSource::Render(
                                            this.folder_icons[dark][usize::from(row.expanded)]
                                                .clone(),
                                        ))
                                        .w(px(14.0))
                                        .h(px(14.0)),
                                    );

                                // 名称：超宽走省略号；整行可点（单击跳转 / 双击展开）
                                let name = div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(row.name.to_string());

                                // 计数：0 张的目录不显示（避免整屏 "0" 噪声）
                                let count_text = if row.count == 0 && row.archives == 0 {
                                    String::new()
                                } else if row.archives > 0 {
                                    format!("{} · {} 包", fmt_count(row.count), row.archives)
                                } else {
                                    fmt_count(row.count)
                                };
                                let count = div()
                                    .flex_shrink_0()
                                    .pl(px(6.0))
                                    .text_xs()
                                    .text_color(fg)
                                    .opacity(if is_selected { 0.85 } else { 1.0 })
                                    .child(count_text);

                                let mut row_el = div()
                                    .id(("tv-row", row.id))
                                    .h(px(ROW_HEIGHT))
                                    .w_full()
                                    .flex()
                                    .items_center()
                                    .pl(px(4.0))
                                    .pr(px(6.0))
                                    .mx(px(3.0))
                                    .rounded_md()
                                    .text_sm()
                                    .cursor_pointer()
                                    .on_click(cx.listener(
                                        move |this, e: &ClickEvent, _w, cx| {
                                            if e.click_count() >= 2 {
                                                this.dir_toggle(row.id, cx);
                                            } else {
                                                this.dir_navigate(row.id, cx);
                                            }
                                        },
                                    ));
                                row_el = if is_selected {
                                    row_el
                                        .bg(colors_in.selected)
                                        .text_color(colors_in.selected_text)
                                } else {
                                    row_el.hover(move |s| s.bg(hover_bg))
                                };

                                items.push(
                                    row_el
                                        .child(indent)
                                        .child(chevron)
                                        .child(folder)
                                        .child(name)
                                        .child(count)
                                        .into_any_element(),
                                );
                            }
                            items
                        })
                    })
                    .track_scroll(&self.tree_scroll)
                    .h_full()
                    .w_full(),
                )
                .into_any_element()
        };

        div()
            .id("sidebar")
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(colors.container)
            .border_r_1()
            .border_color(colors.border)
            // 滚轮在侧边栏上只滚列表，不缩放图片
            .on_scroll_wheel(cx.listener(|_, _, _, cx| {
                cx.stop_propagation();
            }))
            .child(header)
            .child(path_bar)
            .children(self.render_places(colors, cx))
            .child(list)
            .into_any_element()
    }

    /// 侧边栏"位置"区：常用用户目录 + 本机盘符，点击切换浏览根。
    /// 可折叠（标题行点击），盘符多时不会把目录树挤没。
    fn render_places(
        &mut self,
        colors: &gpui::colors::Colors,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        // 0 = 常用目录（文件夹图标），1 = 驱动器
        let mut entries: Vec<(u8, String, PathBuf)> = Vec::new();
        for (label, path) in win32::user_places() {
            entries.push((0, label.to_string(), path));
        }
        for d in win32::drives() {
            entries.push((1, format!("{d}\\"), PathBuf::from(format!("{d}\\"))));
        }
        if entries.is_empty() {
            return None;
        }

        let hover_bg = if self.dark_mode {
            gpui::rgba(0xFFFFFF12)
        } else {
            gpui::rgba(0x0000000D)
        };
        let open = self.places_open;

        let mut section = div()
            .w_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .py(px(2.0))
            .border_b_1()
            .border_color(colors.border)
            .child(
                div()
                    .id("tv-places-head")
                    .h(px(22.0))
                    .w_full()
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .px_2()
                    .text_xs()
                    .text_color(colors.disabled)
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover_bg))
                    .on_click(cx.listener(|this, _: &ClickEvent, _w, cx| {
                        this.places_open = !this.places_open;
                        cx.notify();
                    }))
                    .child(if open { "▾" } else { "▸" })
                    .child("位置"),
            );

        if !open {
            return Some(section.into_any_element());
        }

        for (ix, (kind, label, path)) in entries.into_iter().enumerate() {
            let row = div()
                .id(("tv-place", ix))
                .h(px(20.0))
                .w_full()
                .flex()
                .items_center()
                .gap(px(5.0))
                .px_2()
                .text_sm()
                .cursor_pointer()
                .hover(move |s| s.bg(hover_bg))
                .on_click(cx.listener(move |this, _: &ClickEvent, _w, cx| {
                    this.open_root(path.clone(), cx);
                }))
                .child(
                    div()
                        .w(px(16.0))
                        .h_full()
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .child(if kind == 1 { "💾" } else { "📁" }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(label),
                );
            section = section.child(row);
        }
        Some(section.into_any_element())
    }
}

impl Render for ImageViewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 首次渲染：抢占焦点（键盘事件才能派发到根节点）
        if !self.focus_requested {
            self.focus_requested = true;
            window.focus(&self.focus_handle, cx);
        }
        // 首次渲染：加载初始图片
        if !self.initial_load_started {
            trace("first render");
            self.initial_load_started = true;
            // 冷启动预热帧已经上屏时不要再 goto 一次：重复走缓存命中会把
            // 状态栏的"解码 xx ms"覆盖成"缓存命中"，看着像没解码一样
            if !self.images.is_empty() && self.current.is_none() {
                let index = self.index;
                self.goto(index, cx);
            }
        }

        let viewport = window.viewport_size();
        // 记录视口物理尺寸：解码尺寸按它自适应（逻辑点 × DPI 缩放）
        let sf = window.scale_factor();
        self.viewport_px = (
            viewport.width.as_f32() * sf,
            viewport.height.as_f32() * sf,
        );
        // 自建调色板：Windows 上 gpui 内置外观切换为 no-op，故由 dark_mode 驱动
        let colors = if self.dark_mode {
            gpui::colors::Colors::dark()
        } else {
            gpui::colors::Colors::light()
        };
        let sb_w = if self.sidebar_visible {
            px(SIDEBAR_WIDTH)
        } else {
            px(0.0)
        };
        let area_w = viewport.width - sb_w;
        // 状态栏走普通文档流（根 = 纵向 flex：内容行 flex_1 + 状态栏 30px），
        // 图片区高度 = 视口高 − 状态栏。以前用 absolute + bottom_0 定位状态栏，
        // 在部分环境下整条不渲染，改为文档流后不再依赖视口数学。
        let area_h = viewport.height - px(30.0);

        // 计算图片显示几何（先算好再赋值，避免借用冲突）
        // 注意：offset 以图片区（侧边栏右侧）为坐标系
        let (img_w, img_h, img_offset, zoom_now) = match &self.current {
            Some(image) => {
                let zoom = if self.fit {
                    let z = ((area_w.as_f32() - 16.0) / image.width as f32)
                        .min(area_h.as_f32() / image.height as f32);
                    z.max(0.01)
                } else {
                    self.zoom
                };
                let w = px(image.width as f32 * zoom);
                let h = px(image.height as f32 * zoom);
                // 适应窗口时始终在图片区内水平、垂直居中。
                // 宽幅图、竖幅图和隐藏侧边栏的单图模式使用同一规则，
                // 避免宽幅图片贴顶或因侧边栏状态不同而产生视觉偏移。
                let offset = if self.fit {
                    point(
                        ((area_w - w) / 2.0).max(px(0.0)),
                        ((area_h - h) / 2.0).max(px(0.0)),
                    )
                } else {
                    self.offset
                };
                (w, h, offset, zoom)
            }
            None => (px(0.0), px(0.0), point(px(0.0), px(0.0)), self.zoom),
        };
        // 新图刚落地（原图模式跨翻页）：按 1:1 重新居中。居中要用视口尺寸，
        // set_image 拿不到，所以推迟到这一帧做。
        let img_offset = if self.recenter && !self.fit {
            self.recenter = false;
            let o = point(
                (area_w - img_w) / 2.0,
                ((area_h - img_h) / 2.0).max(px(0.0)),
            );
            self.offset = o;
            o
        } else {
            img_offset
        };
        // fit 模式下 offset 由上面按视口算出，但此前没有回写 self.offset，
        // 导致第一次滚轮缩放（以 self.offset 为锚点）会跳一下。这里统一回写。
        if self.fit {
            self.offset = img_offset;
        }
        self.effective_zoom = zoom_now;

        // ROI 渐进重解码：接近 100% 且纹理是降采样版时，请求原始分辨率可见区域
        self.maybe_request_roi(zoom_now, img_offset, area_w.as_f32(), area_h.as_f32(), cx);

        // ---------- 图片区 ----------
        // 普通 flex 子项（横向 flex 行内）：不再 absolute + 视口高度，
        // 高度由内容行决定（= 视口高 − 状态栏），侧边栏隐藏时自动铺满
        let mut image_area = div()
            .id("image-area")
            .flex_1()
            .h_full()
            .min_w_0()
            .relative()
            .overflow_hidden()
            .flex()
            .items_center()
            .justify_center()
            // 鼠标交互只挂图片区：侧边栏点击/滚轮不会干扰缩放平移
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel));

        if let Some(image) = &self.current {
            image_area = image_area.child(
                img(ImageSource::Render(image.render.clone()))
                    .absolute()
                    .left(img_offset.x)
                    .top(img_offset.y)
                    .w(img_w)
                    .h(img_h)
                    .object_fit(ObjectFit::Fill),
            );
            // ROI 覆盖层：1/1 原始分辨率区域，按源坐标系映射到屏幕，
            // 与基础图恒几何对齐（平移缩放中也不会错位）
            if let Some(roi) = &self.roi_overlay {
                let (sw, _) =
                    turbo::oriented_dims(image.orientation, image.src_width, image.src_height);
                if sw > 0 {
                    let s = zoom_now * image.width as f32 / sw as f32;
                    image_area = image_area.child(
                        img(ImageSource::Render(roi.render.clone()))
                            .absolute()
                            .left(img_offset.x + px(roi.rect.x as f32 * s))
                            .top(img_offset.y + px(roi.rect.y as f32 * s))
                            .w(px(roi.rect.w as f32 * s))
                            .h(px(roi.rect.h as f32 * s))
                            .object_fit(ObjectFit::Fill),
                    );
                }
            }
        } else if let Some(err) = &self.error {
            image_area = image_area.child(
                div()
                    .text_base()
                    .text_color(gpui::rgb(0xff6b6b))
                    .child(format!("⚠ {}", err)),
            );
        } else {
            image_area = image_area.child(
                div()
                    .text_base()
                    .text_color(colors.text)
                    .child(if self.images.is_empty() {
                        "把图片文件拖进窗口，或用命令行参数打开：turboview <图片路径>".to_string()
                    } else {
                        "解码中…".to_string()
                    }),
            );
        }

        // ---------- 状态栏 ----------
        let scan_note = if self.pending_levels > 0 {
            " · ⏳ 扫描中".to_string()
        } else {
            String::new()
        };
        let (left_text, right_text) = match (&self.current, &self.error) {
            (Some(image), _) => {
                // 压缩包条目显示 "包名 › 条目名"，本地文件显示文件名
                let name = self
                    .current_path()
                    .map(|p| display_name(p))
                    .unwrap_or_default();
                // 原图尺寸按显示方向报告（EXIF 旋转后宽高互换是正常的）
                let (sw, sh) =
                    turbo::oriented_dims(image.orientation, image.src_width, image.src_height);
                let scaled = image.width != sw || image.height != sh;
                let dims = if scaled {
                    format!("尺寸 {} × {}（原图 {} × {}）", image.width, image.height, sw, sh)
                } else {
                    format!("尺寸 {} × {}", image.width, image.height)
                };
                let left = format!(
                    "[{}/{}]  {}  {}",
                    self.index + 1,
                    self.images.len(),
                    name,
                    dims
                );
                let zoom_pct = (self.effective_zoom * 100.0).round() as i32;
                let perf = if image.preloaded {
                    // 冷启动预热：解码发生在 UI 初始化期间，用户没等它。
                    // 网络盘上这一坨里大半是缺页读取，标出来免得以为是解码慢
                    format!(
                        "解码 {}{} · 启动预热",
                        fmt_ms(image.decode_ms),
                        if image.decode_ms >= 500.0 { "（含读取）" } else { "" }
                    )
                } else if image.preview {
                    // 首帧预览：说明当前是快解码结果，全质量帧马上覆盖
                    format!("预览 {}", fmt_ms(image.decode_ms))
                } else if image.cached {
                    "缓存命中".to_string()
                } else if image.read_ms < 80.0 && image.decode_ms >= 500.0 {
                    // mmap 路径：磁盘/网络的读取摊在解码的缺页里，read_ms 近似 0。
                    // 这种"解码"其实大半是在等字节，标清楚免得误会解码器
                    format!(
                        "解码 {} · 含读取{}",
                        fmt_ms(image.decode_ms),
                        if image.fast { " · 快速" } else { "" }
                    )
                } else if image.read_ms >= 80.0 {
                    // 网络盘/机械盘上读字节比解码慢一个量级：分开报，
                    // 否则一个 1.3 s 的"解码"看着像解码器太慢（其实是网速）
                    format!(
                        "读取 {} + 解码 {}{}",
                        fmt_ms(image.read_ms),
                        fmt_ms(image.decode_ms),
                        if image.fast { " · 快速" } else { "" }
                    )
                } else {
                    format!(
                        "解码 {}{}",
                        fmt_ms(image.decode_ms),
                        if image.fast { " · 快速" } else { "" }
                    )
                };
                let right = format!("缩放 {}% · {}", zoom_pct, perf);
                (left, right)
            }
            (None, Some(err)) => ("解码失败".to_string(), err.clone()),
            (None, None) => (
                format!("{} 张图片", self.images.len()),
                "左键下一张 · 右键上一张 · 滚轮翻页 · Ctrl+滚轮缩放 · ESC 全屏切换 · Del 删除 · Ctrl+C 复制 · Ctrl+Shift+C 路径 · Ctrl+E 定位 · R 随机 · S 幻灯 · B 侧边栏 · F5 刷新 · T 主题 · 0 适应 · 1 原图 · Shift+F 快速 · Q 退出 · 支持 zip/7z/rar 压缩包"
                    .to_string(),
            ),
        };
        let right_text = format!("{}{}", right_text, scan_note);

        // 状态栏徽标：全部无背景色 —— 文字与单色图标统一取主题文字色
        // （亮色=黑 / 暗色=白），不引入任何色块，保持状态栏干净。
        let mut badges: Vec<AnyElement> = Vec::new();

        // 压缩包浏览信息
        if let Some(info) = &self.archive_info {
            badges.push(
                div()
                    .h(px(18.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.0))
                    .px_1()
                    .text_xs()
                    .text_color(colors.text)
                    .child(
                        img(ImageSource::Render(self.status_icons[(self.dark_mode as usize)][0].clone()))
                            .w(px(15.0))
                            .h(px(15.0))
                            .flex_shrink_0(),
                    )
                    .child(info.clone())
                    .into_any_element(),
            );
        }
        // 随机模式开关：常驻显示（开/关都可见），点击即切换。
        // 开启时用正常文字色，关闭时降到 disabled 色；无任何背景色块。
        badges.push(
            div()
                .h(px(18.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.0))
                .px_1()
                .text_xs()
                .text_color(if self.random_mode {
                    colors.text
                } else {
                    colors.disabled
                })
                .cursor_pointer()
                .hover(|s| s.opacity(0.6))
                .child(
                    img(ImageSource::Render(self.status_icons[(self.dark_mode as usize)][1].clone()))
                        .w(px(15.0))
                        .h(px(15.0))
                        .flex_shrink_0(),
                )
                .child(if self.random_mode {
                    "随机 开"
                } else {
                    "随机 关"
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, _w, cx| {
                        this.random_mode = !this.random_mode;
                        this.history.clear();
                        cx.notify();
                    }),
                )
                .into_any_element(),
        );
        // 幻灯片开关：常驻显示（开/关都可见），点击即开/关播放
        badges.push(
            div()
                .h(px(18.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.0))
                .px_1()
                .text_xs()
                .text_color(if self.slideshow {
                    colors.text
                } else {
                    colors.disabled
                })
                .cursor_pointer()
                .hover(|s| s.opacity(0.6))
                .child(
                    img(ImageSource::Render(self.status_icons[(self.dark_mode as usize)][2].clone()))
                        .w(px(15.0))
                        .h(px(15.0))
                        .flex_shrink_0(),
                )
                .child(if self.slideshow {
                    format!("幻灯 开 {}s", SLIDESHOW_INTERVAL)
                } else {
                    "幻灯 关".to_string()
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, _w, cx| {
                        let on = !this.slideshow;
                        this.set_slideshow(on, cx);
                    }),
                )
                .into_any_element(),
        );
        // 原图开关：开 = 1:1 原始像素，关 = 适应窗口（默认关）。
        // 与 `1` / `0` 键等价，给鼠标用户一个可点的入口。
        badges.push(
            div()
                .id("toggle-original")
                .h(px(18.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.0))
                .px_1()
                .text_xs()
                .text_color(if self.original_view {
                    colors.text
                } else {
                    colors.disabled
                })
                .cursor_pointer()
                .hover(|s| s.opacity(0.6))
                .child(
                    img(ImageSource::Render(self.status_icons[(self.dark_mode as usize)][3].clone()))
                        .w(px(15.0))
                        .h(px(15.0))
                        .flex_shrink_0(),
                )
                .child(if self.original_view {
                    "原图 开"
                } else {
                    "原图 关"
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, w, cx| {
                        let on = !this.original_view;
                        this.set_original_view(on, w, cx);
                    }),
                )
                .into_any_element(),
        );
        // 主题开关：与 T 键共用 toggle_theme。图标走启动时栅格化的位图，
        // 不依赖这套 Windows 后端不保证支持的 svg() 元素。
        badges.push(
            div()
                .id("toggle-theme")
                .h(px(18.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.0))
                .px_1()
                .text_xs()
                .text_color(colors.text)
                .cursor_pointer()
                .hover(|s| s.opacity(0.6))
                .child(
                    img(ImageSource::Render(
                        self.theme_icons[(self.dark_mode as usize)].clone(),
                    ))
                    .w(px(12.0))
                    .h(px(12.0))
                    .flex_shrink_0(),
                )
                .child(if self.dark_mode { "暗色" } else { "亮色" })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, _w, cx| {
                        this.toggle_theme(cx);
                    }),
                )
                .into_any_element(),
        );
        // 全屏按钮：与 F 快捷键共用同一状态，ESC 也可退出。
        badges.push(
            div()
                .id("toggle-fullscreen")
                .h(px(22.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.0))
                .px_1()
                .text_xs()
                .text_color(if self.fullscreen { gpui::rgb(0x3B82F6) } else { colors.disabled })
                .cursor_pointer()
                .hover(|s| s.opacity(0.72))
                .child(
                    img(ImageSource::Render(
                        self.status_icons[(self.dark_mode as usize)][4].clone(),
                    ))
                    .w(px(15.0))
                    .h(px(15.0))
                    .flex_shrink_0(),
                )
                .child(if self.fullscreen { "退出全屏" } else { "全屏" })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, _w, cx| {
                        this.toggle_fullscreen(cx);
                    }),
                )
                .into_any_element(),
        );

        // 最近一次操作反馈
        if let Some(note) = &self.note {
            badges.push(
                div()
                    .h(px(18.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.0))
                    .px_1()
                    .text_xs()
                    .text_color(colors.text)
                    .child(note.clone())
                    .into_any_element(),
            );
        }

        // 目录树开/关按钮始终留在状态栏最左侧，避免按钮随状态消失。
        // B 键与按钮共用 toggle_sidebar，初始隐藏目录树时也能立即恢复。
        let status_leading: Vec<AnyElement> = vec![
            div()
                .id("sidebar-toggle")
                .h(px(22.0))
                .px_1()
                .flex_shrink_0()
                .flex()
                .flex_row()
                .items_center()
                .justify_center()
                .gap(px(4.0))
                .rounded_sm()
                .text_color(if self.sidebar_visible { colors.text } else { colors.disabled })
                .cursor_pointer()
                .hover(|s| s.bg(colors.container).text_color(colors.text))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, _w, cx| {
                        this.toggle_sidebar(cx);
                    }),
                )
                .child("◧")
                .child(if self.sidebar_visible { "目录树 开 · B" } else { "目录树 关 · B" })
                .into_any_element(),
        ];

        let status_bar = div()
            .id("status-bar")
            .w_full()
            .flex_shrink_0()
            .h(px(30.0))
            .bg(colors.container)
            .border_t_1()
            .border_color(colors.border)
            .flex()
            .flex_row()
            .items_center()
            .px_3()
            .gap(px(10.0))
            .text_xs()
            .text_color(colors.text)
            .children(status_leading.into_iter())
            // 左侧：P 字 logo（品牌位图，彩色）
            .child(
                img(ImageSource::Render(self.logo.clone()))
                    .w(px(17.0))
                    .h(px(17.0))
                    .flex_shrink_0(),
            )
            .child(left_text)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .children(badges.into_iter()),
            )
            .child(right_text);

        // ---------- 侧边栏 ----------
        let sidebar = if self.sidebar_visible {
            Some(self.render_sidebar(&colors, cx))
        } else {
            None
        };

        // 根布局：纵向 flex —— 内容行（flex_1）+ 状态栏（固定 30px）。
        // 状态栏走普通文档流：以前 absolute + bottom_0 时在部分环境下整条不渲染。
        // 注意：必须给显式宽高。size_full()=100% 在此解析不到视口（父容器高度 auto），
        // flex 会整体塌缩；显式 viewport 尺寸后 flex_1 内容行才能拿到剩余高度。
        div()
            .id("root")
            .relative()
            .w(viewport.width)
            .h(viewport.height)
            .flex()
            .flex_col()
            .bg(colors.background)
            .text_color(colors.text)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_next_image))
            .on_action(cx.listener(Self::on_prev_image))
            .on_action(cx.listener(Self::on_zoom_in))
            .on_action(cx.listener(Self::on_zoom_out))
            .on_action(cx.listener(Self::on_zoom_actual))
            .on_action(cx.listener(Self::on_zoom_fit))
            .on_action(cx.listener(Self::on_toggle_fast))
            .on_action(cx.listener(Self::on_toggle_sidebar))
            .on_action(cx.listener(Self::on_toggle_theme))
            .on_action(cx.listener(Self::on_refresh_tree))
            .on_action(cx.listener(Self::on_toggle_fullscreen))
            .on_action(cx.listener(Self::on_random))
            .on_action(cx.listener(Self::on_toggle_slideshow))
            .on_action(cx.listener(Self::on_quit))
            .on_action(cx.listener(Self::on_delete_image))
            .on_action(cx.listener(Self::on_copy_image))
            .on_action(cx.listener(Self::on_copy_path))
            .on_action(cx.listener(Self::on_reveal_in_explorer))
            .on_drop(cx.listener(Self::on_file_drop))
            // 内容行：侧边栏 + 图片区，吃掉状态栏以外的全部高度
            .child(
                div()
                    .id("content")
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_row()
                    .relative()
                    .children(sidebar)
                    .child(image_area),
            )
            .child(status_bar)
    }
}

/// 统一解码入口：本地文件走 SIMD / image crate；压缩包条目先解压到内存再走同一套解码
/// 是否 JPEG（决定能不能走"一档 DCT 快解码"做首帧预览）。
/// 压缩包虚拟路径 "<包>|<条目>" 的 extension() 拿到的正是条目扩展名。
fn looks_like_jpeg(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("jpg") | Some("jpeg")
    )
}

/// 毫秒格式化：超过 1 秒就换单位，状态栏宽度有限
fn fmt_ms(ms: f64) -> String {
    if ms >= 1000.0 {
        format!("{:.2} s", ms / 1000.0)
    } else {
        format!("{:.1} ms", ms)
    }
}

/// libjpeg 的"数据没走完"类错误：文件被截断，或还在下载/拷贝中。
/// 这类失败值得等一下重试，而不是当成"这不是张图"。
fn is_truncated_jpeg_error(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("premature end") || m.contains("corrupt jpeg data")
}

/// 视口物理尺寸的下限估算：窗口启动即最大化，主显示器工作区减去
/// 标题栏/状态栏就是可视区的上界。首次 render 之前用它算首图解码尺寸。
fn estimate_viewport_px() -> (f32, f32) {
    match win32::screen_work_area() {
        Some((w, h)) => (w as f32 * 0.97, h as f32 * 0.93),
        None => (1600.0, 900.0),
    }
}

/// 由视口物理尺寸推解码最长边上限（自适应降采样的核心公式）
fn decode_dim_for(viewport_px: (f32, f32)) -> u32 {
    let longest = viewport_px.0.max(viewport_px.1);
    ((longest * DECODE_DIM_HEADROOM).ceil() as u32).clamp(MIN_DECODE_DIM, MAX_DECODE_DIM_CAP)
}

fn decode_any(path: &Path, fast: bool, max_dim: u32) -> Result<DecodedImage> {
    if let Some((archive, entry)) = split_virtual(path) {
        return decode_archive_entry(&archive, &entry, fast, max_dim);
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("jpg") | Some("jpeg") => match turbo::decode_jpeg(path, fast, max_dim) {
            Ok(decoded) => Ok(decoded),
            Err(jpeg_error) => {
                // 扩展名不可信：例如 WebP 被错误改名为 .jpg。
                // image crate 的 ImageReader 按文件头识别真实格式；正常 JPEG 仍走
                // libjpeg-turbo SIMD 快路径，只有 JPEG 解码失败才付出回退成本。
                decode_with_image_crate(path, max_dim).or(Err(jpeg_error))
            }
        },
        Some("png") => match turbo::decode_png_fast(path, max_dim) {
            Some(d) => Ok(d),
            None => decode_with_image_crate(path, max_dim), // 16 位/调色板/隔行等回退
        },
        _ => decode_with_image_crate(path, max_dim),
    }
}

/// 压缩包条目解码：只解压这一个条目到内存（耗时计入 read_ms），
/// 再按扩展名走与本地文件完全相同的 SIMD 解码路径，零临时文件。
fn decode_archive_entry(
    archive: &Path,
    entry: &str,
    fast: bool,
    max_dim: u32,
) -> Result<DecodedImage> {
    let start = Instant::now();
    let bytes = archive::extract_entry(archive, entry)?;
    let extract_ms = start.elapsed().as_secs_f64() * 1000.0;
    let ext = entry.rsplit('.').next().map(|e| e.to_ascii_lowercase());
    decode_bytes(&bytes, ext.as_deref(), fast, max_dim, extract_ms)
}

/// 内存字节统一解码（decode_any 的内存版，策略一致）
fn decode_bytes(
    data: &[u8],
    ext: Option<&str>,
    fast: bool,
    max_dim: u32,
    read_ms: f64,
) -> Result<DecodedImage> {
    match ext {
        Some("jpg") | Some("jpeg") => {
            match turbo::decode_jpeg_bytes(data, fast, max_dim, read_ms) {
                Ok(d) => Ok(d),
                Err(e) => {
                    // 12-bit 精度等当前 libjpeg-turbo 构建不支持的 JPEG，回退 image crate
                    decode_bytes_with_image_crate(data, read_ms, max_dim).or(Err(e))
                }
            }
        }
        Some("png") => match turbo::decode_png_fast_bytes(data, read_ms, max_dim) {
            Some(d) => Ok(d),
            None => decode_bytes_with_image_crate(data, read_ms, max_dim), // 16 位/调色板/隔行等回退
        },
        _ => decode_bytes_with_image_crate(data, read_ms, max_dim),
    }
}

fn decode_with_image_crate(path: &Path, max_dim: u32) -> Result<DecodedImage> {
    let start = Instant::now();
    let reader = image::ImageReader::open(path)
        .with_context(|| format!("无法读取图片 {}", path.display()))?
        .with_guessed_format()
        .context("无法识别图片真实格式")?;
    let img = reader
        .decode()
        .with_context(|| format!("无法解码 {}", path.display()))?;
    let (src_width, src_height) = img.dimensions();
    // 通用解码器目前不提供与 TurboJPEG 等价的原生降采样接口；
    // 至少在交给 GPU/缓存前限制输出尺寸，避免超大纹理长期占用内存。
    let limit = max_dim.max(1);
    let img = if src_width.max(src_height) > limit {
        img.thumbnail(limit, limit)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    let (width, height) = rgba.dimensions();
    let bgra = turbo::bgra_from_rgba(rgba.as_raw());
    Ok(DecodedImage {
        width,
        height,
        pixels: bgra,
        src_width,
        src_height,
        orientation: 1,
        roi_x: 0,
        roi_y: 0,
        read_ms: 0.0,
        decode_ms: start.elapsed().as_secs_f64() * 1000.0,
        fast: false,
    })
}

/// 把 BGRA 像素缓冲包装成 GPUI 的 RenderImage（零拷贝）
fn make_render_image(decoded: DecodedImage) -> Arc<RenderImage> {
    let buffer = RgbaImage::from_raw(decoded.width, decoded.height, decoded.pixels)
        .expect("像素缓冲长度与尺寸不匹配");
    // 注意：Frame 的缓冲区实际是 BGRA（GPUI 约定），类型参数仅为字节容器
    Arc::new(RenderImage::new(vec![Frame::new(buffer)]))
}

fn decode_bytes_with_image_crate(
    data: &[u8],
    read_ms: f64,
    max_dim: u32,
) -> Result<DecodedImage> {
    let start = Instant::now();
    let img = image::load_from_memory(data).context("无法解码图片数据")?;
    let (src_width, src_height) = img.dimensions();
    let limit = max_dim.max(1);
    let img = if src_width.max(src_height) > limit {
        img.thumbnail(limit, limit)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    let (width, height) = rgba.dimensions();
    let bgra = turbo::bgra_from_rgba(rgba.as_raw());
    Ok(DecodedImage {
        width,
        height,
        pixels: bgra,
        src_width,
        src_height,
        orientation: 1,
        roi_x: 0,
        roi_y: 0,
        read_ms,
        decode_ms: start.elapsed().as_secs_f64() * 1000.0,
        fast: false,
    })
}

// ============================================================================
// P 字 logo（品牌位图）
//
// gpui 的 svg() 走 alpha mask 单色化，无法呈现彩色 logo，
// 因此这里用纯 Rust 手绘栅格化：圆角方牌（蓝紫→青对角渐变）+ 白色 P 字形，
// 4x 超采样 + SDF 抗锯齿，输出 64x64 BGRA（GPUI 帧缓冲字节序）。
// 启动时生成一次，之后每帧只是 Arc 克隆。
// ============================================================================

fn make_logo_image() -> Arc<RenderImage> {
    const S: f32 = 256.0; // 超采样画布边长
    const OUT: usize = 64; // 输出边长
    let mut pixels = vec![0u8; OUT * OUT * 4];

    // 徽标圆角矩形（中心居中、半边长 92、圆角半径 54，256 坐标系）
    let half = 92.0f32;
    let radius = 54.0f32;
    let center = S / 2.0;
    // P 字形几何：竖条 + 圆环（碗），碗顶与条顶对齐（256 坐标系）
    // 状态栏里只显示 17px，笔画要够粗才不糊：竖条 40 宽、碗环厚 36。
    let stem = (82.0f32, 54.0f32, 122.0f32, 202.0f32); // x0,y0,x1,y1
    let bowl_c = (158.0f32, 108.0f32);
    let bowl_r_out = 54.0f32;
    let bowl_r_in = 18.0f32;

    // 圆角矩形 SDF：<0 在内
    let sdf_round_rect = |x: f32, y: f32| -> f32 {
        let qx = (x - center).abs() - (half - radius);
        let qy = (y - center).abs() - (half - radius);
        let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
        outside + qx.max(qy).min(0.0) - radius
    };
    // 矩形 SDF（竖条）
    let sdf_rect = |x: f32, y: f32, r: (f32, f32, f32, f32)| -> f32 {
        let dx = (x - (r.0 + r.2) / 2.0).abs() - (r.2 - r.0) / 2.0;
        let dy = (y - (r.1 + r.3) / 2.0).abs() - (r.3 - r.1) / 2.0;
        let ax = dx.max(0.0);
        let ay = dy.max(0.0);
        (ax * ax + ay * ay).sqrt() + dx.max(dy).min(0.0)
    };
    // 圆环 SDF（碗）：返回到环形材料的距离，>0 在材料外
    let sdf_ring = |x: f32, y: f32| -> f32 {
        let d = ((x - bowl_c.0).powi(2) + (y - bowl_c.1).powi(2)).sqrt();
        if d > bowl_r_out {
            d - bowl_r_out
        } else if d < bowl_r_in {
            bowl_r_in - d
        } else {
            0.0
        }
    };
    // 覆盖率：SDF 距离（像素）→ 0..1，边缘 1px 平滑
    let cov = |d: f32| (0.5 - d).clamp(0.0, 1.0);

    let top = [0x5Bu8, 0x5C, 0xE2]; // 蓝紫
    let bottom = [0x22u8, 0xC1, 0xE8]; // 青

    for oy in 0..OUT {
        for ox in 0..OUT {
            let mut acc = [0f32; 4];
            for sy in 0..4 {
                for sx in 0..4 {
                    let x = ox as f32 * 4.0 + sx as f32 + 0.5;
                    let y = oy as f32 * 4.0 + sy as f32 + 0.5;
                    let badge = cov(sdf_round_rect(x, y));
                    if badge <= 0.0 {
                        continue;
                    }
                    // 对角渐变
                    let t = ((x / S) + (y / S)) / 2.0;
                    let r = top[0] as f32 + (bottom[0] as f32 - top[0] as f32) * t;
                    let g = top[1] as f32 + (bottom[1] as f32 - top[1] as f32) * t;
                    let b = top[2] as f32 + (bottom[2] as f32 - top[2] as f32) * t;
                    // P 字形（白色）覆盖在徽标上
                    let glyph = cov(sdf_rect(x, y, stem).min(sdf_ring(x, y)));
                    let (pr, pg, pb) = if glyph > 0.0 {
                        (
                            r + (255.0 - r) * glyph,
                            g + (255.0 - g) * glyph,
                            b + (255.0 - b) * glyph,
                        )
                    } else {
                        (r, g, b)
                    };
                    // 预乘到累加器（按 badge 覆盖率加权）
                    acc[0] += pb * badge;
                    acc[1] += pg * badge;
                    acc[2] += pr * badge;
                    acc[3] += 255.0 * badge;
                }
            }
            let n = 16.0f32; // 4x4 子采样
            let a = acc[3]; // Σ 255*badge（预乘 alpha 之和）
            let alpha = a / n;
            // 累加器是预乘的：直通颜色 = Σ(color*badge) / Σ(255*badge) * 255
            // （漏乘 255 会让整块 logo 变成近黑色 —— 之前就是这个 bug）
            let inv = if a > 0.0 { 255.0 / a } else { 0.0 };
            let i = (oy * OUT + ox) * 4;
            pixels[i] = (acc[0] * inv).round().clamp(0.0, 255.0) as u8; // B
            pixels[i + 1] = (acc[1] * inv).round().clamp(0.0, 255.0) as u8; // G
            pixels[i + 2] = (acc[2] * inv).round().clamp(0.0, 255.0) as u8; // R
            pixels[i + 3] = alpha.round().clamp(0.0, 255.0) as u8; // A
        }
    }

    let buffer = RgbaImage::from_raw(OUT as u32, OUT as u32, pixels)
        .expect("logo 像素缓冲长度与尺寸不匹配");
    // 注意：缓冲区实际是 BGRA（GPUI 约定），类型参数仅为字节容器
    Arc::new(RenderImage::new(vec![Frame::new(buffer)]))
}

fn main() {
    trace("entry");
    // 必须第一个调用：PerMonitorV2 DPI 感知。否则 125%+ 缩放的屏幕上
    // 窗口按 1:1 物理像素渲染，“最大化”只占屏幕左上角一小块。
    win32::set_per_monitor_dpi_aware();
    trace("dpi aware");

    let args: Vec<String> = std::env::args().collect();
    // 目录参数 → 打开该根（惰性扫第一层）；压缩包参数 → 包内图片列表；
    // 文件参数 → 立即显示 + 扫其所在目录
    let initial_path = args.get(1).map(PathBuf::from);

    // 机械盘：JPEG 读取改走 FILE_FLAG_SEQUENTIAL_SCAN。
    // mmap 的缺页是同步的、无法预读，冷缓存下等于一串 4KB 随机读。
    //
    // 注意：网络盘**不用**顺序读。实测（E: → SMB → 百兆网卡）顺序读只快 4%
    // （1.87 s vs 1.95 s），但一个 18 MB 的 ReadFile 长期占着 SMB 会话，
    // UI 线程同时要做的目录枚举/变更通知会被压在后面，首帧反而晚 1.3 s。
    // mmap 的按需缺页是"细水长流"，和元数据请求能交错。
    if let Some(p) = initial_path.as_deref() {
        if win32::is_rotational(p).unwrap_or(false) {
            turbo::set_sequential_io(true);
        }
    }
    trace("rotational probe done");

    // 是否"打开单张图片"：双击图片 / 文件关联（本程序已注册为默认看图器）
    // 传进来的都是一个图片文件路径 —— 这种场景用户只想看图，
    // 目录树自动收起，状态栏最左侧保留恢复按钮（点一下或按 B 随时叫回来）。
    // 传目录或压缩包时是"浏览"意图，目录树保持展开。
    let open_single_image = initial_path
        .as_ref()
        .is_some_and(|p| !p.is_dir() && !archive::is_archive(p));

    // 注册为 jpg/png 的默认图片查看器（HKCU 作用域，幂等，毫秒级，无需管理员）。
    // 失败（如注册表被策略锁定）静默忽略，不影响正常使用。
    if let Ok(exe) = std::env::current_exe() {
        let _ = win32::register_default_viewer(&exe);
    }
    trace("default viewer registered");

    // ---- 冷启动重叠：首图解码提前到平台初始化之前 ----
    // WindowsPlatform::new 里建 DirectWrite 字体库要 ~250ms，open_window 再 ~100ms，
    // 这段时间 CPU 和磁盘完全闲着。先把首图丢给后台线程解码，等窗口建好时
    // 像素已经在手里 —— 用户等的时间从"初始化 + 解码"变成 max(初始化, 解码)。
    let (preload_tx, preload_rx) = std::sync::mpsc::channel();
    let mut preload: Option<std::sync::mpsc::Receiver<(PathBuf, Result<DecodedImage>)>> = None;
    // 预热线程正在处理哪张（供 viewer 判断"要不要等它"，避免重复读一遍）
    let preload_shared: std::sync::Arc<std::sync::Mutex<Option<PathBuf>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    if open_single_image {
        if let Some(p) = initial_path.clone() {
            let dim = decode_dim_for(estimate_viewport_px());
            if let Ok(mut g) = preload_shared.lock() {
                *g = Some(p.clone());
            }
            let shared = preload_shared.clone();
            std::thread::Builder::new()
                .name("tv-predecode".into())
                .stack_size(1024 * 1024)
                .spawn(move || {
                    let decoded = decode_any(&p, false, dim);
                    let _ = shared;
                    let _ = preload_tx.send((p, decoded));
                })
                .ok();
            preload = Some(preload_rx);
        }
    } else if let Some(dir) = initial_path.clone().filter(|p| p.is_dir()) {
        // 目录模式：第一张图要等目录扫描完才知道是谁。扫描本身是
        // FindFirstFileEx 一趟的事（几百项也就几毫秒），先在后台扫一层
        // 挑出排序最靠前的图片预热解码，缩略图窗口建好时像素已经在手里。
        let dim = decode_dim_for(estimate_viewport_px());
        let shared = preload_shared.clone();
        std::thread::Builder::new()
            .name("tv-predecode-dir".into())
            .stack_size(1024 * 1024)
            .spawn(move || {
                let level = scan_one_level(&dir, SUPPORTED_EXTENSIONS, ARCHIVE_EXTENSIONS);
                let mut first = level.images;
                // 列表里图片和压缩包统一按 PathBuf 排序，压缩包要展开才知道
                // 包内首图，这里只预热"排序最靠前的图片"——不是它也无妨，
                // 认领失败就走常规异步路径，只是白解一张。
                first.append(&mut level.archives.clone());
                first.sort_unstable();
                let mut picked = None;
                for p in first {
                    if archive::is_archive(&p) {
                        continue;
                    }
                    picked = Some(p);
                    break;
                }
                if let Some(p) = picked {
                    if let Ok(mut g) = shared.lock() {
                        *g = Some(p.clone());
                    }
                    let decoded = decode_any(&p, false, dim);
                    let _ = preload_tx.send((p, decoded));
                }
            })
            .ok();
        preload = Some(preload_rx);
    }
    trace("predecode thread spawned");

    // 标题带 " - TurboView" 后缀：伪全屏 FindWindowW 靠标题找窗口，
    // 后缀降低与资源管理器等同名窗口撞车的概率
    let window_title = match &initial_path {
        Some(p) => format!(
            "{} - TurboView",
            p.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| p.display().to_string())
        ),
        None => "TurboView".to_string(),
    };

    let platform = Rc::new(
        gpui_windows::WindowsPlatform::new(false).expect("初始化 Windows 平台失败"),
    );
    trace("platform ready");
    let app = gpui::Application::with_platform(platform);
    app.run(move |cx: &mut App| {
        trace("app.run enter");
        cx.init_colors();

        cx.bind_keys([
            KeyBinding::new("right", NextImage, None),
            KeyBinding::new("left", PrevImage, None),
            KeyBinding::new("down", NextImage, None),
            KeyBinding::new("up", PrevImage, None),
            KeyBinding::new("n", NextImage, None),
            KeyBinding::new("p", PrevImage, None),
            KeyBinding::new("space", NextImage, None),
            KeyBinding::new("pageup", PrevImage, None),
            KeyBinding::new("pagedown", NextImage, None),
            KeyBinding::new("+", ZoomIn, None),
            KeyBinding::new("=", ZoomIn, None),
            KeyBinding::new("-", ZoomOut, None),
            KeyBinding::new("_", ZoomOut, None),
            KeyBinding::new("0", ZoomFit, None),
            KeyBinding::new("1", ZoomActual, None),
            KeyBinding::new("f", ToggleFullscreen, None),
            KeyBinding::new("shift-f", ToggleFastMode, None),
            KeyBinding::new("b", ToggleSidebar, None),
            KeyBinding::new("t", ToggleTheme, None),
            KeyBinding::new("f5", RefreshTree, None),
            KeyBinding::new("r", RandomImage, None),
            KeyBinding::new("s", ToggleSlideshow, None),
            KeyBinding::new("q", Quit, None),
            KeyBinding::new("ctrl-q", Quit, None),
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("escape", ToggleFullscreen, None),
            KeyBinding::new("delete", DeleteImage, None),
            KeyBinding::new("ctrl-c", CopyImage, None),
            KeyBinding::new("ctrl-shift-c", CopyPath, None),
            KeyBinding::new("ctrl-e", RevealInExplorer, None),
        ]);

        // 还原尺寸按主显示器可视区域（逻辑像素）的 85% 取，
        // 高 DPI 缩放下也不会超过屏幕；启动时永远以最大化呈现。
        let restore = cx
            .primary_display()
            .map(|d| {
                let vb = d.visible_bounds();
                size(vb.size.width * 0.85, vb.size.height * 0.85)
            })
            .unwrap_or_else(|| size(px(900.), px(600.)));
        let bounds = Bounds::centered(None, restore, cx);

        cx.open_window(
            WindowOptions {
                // 启动即最大化（bounds 作为还原尺寸记忆）
                window_bounds: Some(WindowBounds::Maximized(bounds)),
                window_min_size: Some(size(px(480.), px(360.))),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some(window_title.clone().into()),
                    appears_transparent: false,
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_, cx| {
                cx.new(|cx| {
                    // 打开单张图片（默认看图器场景）时目录树默认收起，只留展开按钮
                    let mut viewer = ImageViewer::new(
                        Vec::new(),
                        0,
                        true,
                        window_title.clone(),
                        !open_single_image,
                        cx,
                    );
                    // 认领通道必须在第一次 goto 之前挂上，否则预热结果赶不上首帧
                    viewer.preload = preload;
                    viewer.preload_path = preload_shared.clone();
                    match &initial_path {
                        Some(p) if p.is_dir() => viewer.open_root(p.clone(), cx),
                        Some(p) if archive::is_archive(p) => viewer.open_archive(p.clone(), cx),
                        Some(p) => {
                            let root = p
                                .parent()
                                .map(Path::to_path_buf)
                                .unwrap_or_else(|| p.clone());
                            viewer.images = vec![Arc::<Path>::from(p.clone())];
                            viewer.goto(0, cx);
                            viewer.seed_root(root, cx);
                        }
                        None => {}
                    }
                    viewer
                })
            },
        )
        .unwrap();
        trace("window created");

        cx.activate(true);
    });
}

// ---------------------------------------------------------------------------
// 启动耗时探针：仅在 TV_TRACE 环境变量存在时写 %TEMP%\tvtrace.log。
// 正式运行时零开销（一次环境变量查询），用于定位冷启动各阶段耗时。
// ---------------------------------------------------------------------------
fn trace_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("TV_TRACE").is_some())
}

fn trace(msg: &str) {
    use std::io::Write;
    static T0: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let t0 = T0.get_or_init(Instant::now);
    if !trace_enabled() {
        return;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::env::temp_dir().join("tvtrace.log"))
    {
        let _ = writeln!(f, "{:>8.2} ms  {}", t0.elapsed().as_secs_f64() * 1000.0, msg);
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_webp_content_even_when_extension_is_jpg() {
        let mut encoded = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut encoded)
            .encode(
                &[255, 0, 0, 255],
                1,
                1,
                image::ExtendedColorType::Rgba8,
            )
            .expect("encode WebP test fixture");

        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "turboview-webp-mislabeled-{}-{nonce}.jpg",
            std::process::id()
        ));
        std::fs::write(&path, encoded).expect("write WebP fixture");

        let decoded = decode_any(&path, false, 1024);
        let _ = std::fs::remove_file(&path);
        let decoded = decoded.expect("WebP content should decode despite .jpg suffix");

        assert_eq!((decoded.width, decoded.height), (1, 1));
    }

    #[test]
    fn decodes_mislabeled_webp_from_archive_bytes() {
        let mut encoded = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut encoded)
            .encode(
                &[0, 255, 0, 255],
                1,
                1,
                image::ExtendedColorType::Rgba8,
            )
            .expect("encode WebP test fixture");

        let decoded = decode_bytes(&encoded, Some("jpg"), false, 1024, 0.0)
            .expect("WebP bytes should decode despite .jpg entry suffix");

        assert_eq!((decoded.width, decoded.height), (1, 1));
    }


    #[test]
    fn generic_webp_fallback_respects_decode_dimension_limit() {
        let (source_width, source_height, limit) = (2048u32, 1024u32, 512u32);
        let pixels = vec![127u8; (source_width * source_height * 4) as usize];
        let mut encoded = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut encoded)
            .encode(
                &pixels,
                source_width,
                source_height,
                image::ExtendedColorType::Rgba8,
            )
            .expect("encode large WebP fixture");

        let decoded = decode_bytes(&encoded, Some("webp"), false, limit, 0.0)
            .expect("decode WebP through generic fallback");

        assert_eq!((decoded.src_width, decoded.src_height), (source_width, source_height));
        assert_eq!((decoded.width, decoded.height), (limit, limit / 2));

        // 同一尺寸限制也必须覆盖本地文件路径，而不只是压缩包内存路径。
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "turboview-webp-dimension-limit-{}-{nonce}.webp",
            std::process::id()
        ));
        std::fs::write(&path, &encoded).expect("write WebP fixture");
        let local = decode_any(&path, false, limit);
        let _ = std::fs::remove_file(&path);
        let local = local.expect("decode local WebP with dimension limit");
        assert_eq!((local.src_width, local.src_height), (source_width, source_height));
        assert_eq!((local.width, local.height), (limit, limit / 2));
    }

    #[test]
    fn decodes_mislabeled_webp_from_real_zip_entry() {
        use std::io::Write;

        let mut encoded = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut encoded)
            .encode(
                &[0, 0, 255, 255],
                1,
                1,
                image::ExtendedColorType::Rgba8,
            )
            .expect("encode WebP test fixture");

        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "turboview-webp-archive-{}-{nonce}.zip",
            std::process::id()
        ));

        let file = std::fs::File::create(&path).expect("create ZIP fixture");
        let mut writer = zip::ZipWriter::new(file);
        writer
            .start_file("mislabelled.jpg", zip::write::SimpleFileOptions::default())
            .expect("start ZIP entry");
        writer.write_all(&encoded).expect("write WebP ZIP entry");
        writer.finish().expect("finish ZIP fixture");

        let result = archive::extract_entry(&path, "mislabelled.jpg")
            .and_then(|bytes| decode_bytes(&bytes, Some("jpg"), false, 1024, 0.0));
        let _ = std::fs::remove_file(&path);
        let decoded = result.expect("WebP inside a real ZIP entry should decode");

        assert_eq!((decoded.width, decoded.height), (1, 1));
    }
}
