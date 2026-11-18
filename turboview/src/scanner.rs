//! 超快并行目录树扫描器（Windows 专精）
//!
//! 为什么快：
//! - `FindFirstFileExW` + `FindExInfoBasic`：跳过 8.3 短名生成；一次系统调用
//!   由内核批量返回一串条目，而不是逐条往返。
//! - `FIND_FIRST_EX_LARGE_FETCH`：内核用更大的内部缓冲，海量目录下系统调用
//!   次数再降一个量级（不支持的旧系统自动回退普通模式）。
//! - 多线程工作窃取：共享目录队列，每个线程完整枚举一个目录后把新发现的
//!   子目录丢回队列。对网络盘（如 Z:）这种高延迟介质，并发线程直接把
//!   RTT 藏起来——这是扫描速度的决定性因素。
//! - 不跟随 reparse point（junction/符号链接），百万目录树不会死循环。
//! - 结果按批经 channel 流出：UI 边扫边看，无需等全树完成。

#![allow(non_snake_case)]

use std::collections::VecDeque;
use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const FIND_FIRST_EX_LARGE_FETCH: u32 = 0x2;
const FIND_EX_INFO_BASIC: i32 = 1;
const FIND_EX_SEARCH_NAME_MATCH: i32 = 0;
const INVALID_HANDLE_VALUE: usize = usize::MAX; // -1

/// 每批流出多少个图片路径（4096 个路径 ≈ 半 MB，UI 更新频率与拷贝量的平衡点）
const BATCH_SIZE: usize = 4096;

#[repr(C)]
#[derive(Clone, Copy)]
struct WIN32_FIND_DATAW {
    dwFileAttributes: u32,
    ftCreationTime: [u32; 2],
    ftLastAccessTime: [u32; 2],
    ftLastWriteTime: [u32; 2],
    nFileSizeHigh: u32,
    nFileSizeLow: u32,
    dwReserved0: u32,
    dwReserved1: u32,
    cFileName: [u16; 260],
    cAlternateFileName: [u16; 14],
}

unsafe extern "system" {
    fn FindFirstFileExW(
        lpFileName: *const u16,
        fInfoLevelId: i32,
        lpFindFileData: *mut WIN32_FIND_DATAW,
        fSearchOp: i32,
        lpSearchFilter: *const core::ffi::c_void,
        dwAdditionalFlags: u32,
    ) -> *mut core::ffi::c_void;
    fn FindNextFileW(hFindFile: *mut core::ffi::c_void, lpFindFileData: *mut WIN32_FIND_DATAW) -> i32;
    fn FindClose(hFindFile: *mut core::ffi::c_void) -> i32;
    fn GetLastError() -> u32;
}

/// 扫描事件（经 channel 发给 UI）
pub enum ScanEvent {
    /// 一批新发现的图片 + 累计已扫描目录数
    Batch { images: Vec<PathBuf>, dirs: usize },
    /// 全树扫描完成（目录总数、图片总数）
    Done { dirs: usize, images: usize },
}

struct ScanState {
    queue: Mutex<VecDeque<PathBuf>>,
    cv: Condvar,
    /// 已发现但尚未枚举完的目录数（>0 时队列可能还会进新目录）
    pending: AtomicUsize,
    dirs_scanned: AtomicUsize,
    images_found: AtomicUsize,
    cancelled: Arc<AtomicBool>,
}

/// 并行扫描 `root` 整棵目录树，把匹配 `exts` 的图片路径按批发到 `tx`。
/// 阻塞直到扫完/取消。`cancel` 置 true 后各线程在 ~50ms 内退出。
pub fn scan_tree(
    root: PathBuf,
    exts: &'static [&'static str],
    threads: usize,
    cancel: Arc<AtomicBool>,
    tx: flume::Sender<ScanEvent>,
) {
    let state = Arc::new(ScanState {
        queue: Mutex::new(VecDeque::from([root])),
        cv: Condvar::new(),
        pending: AtomicUsize::new(1),
        dirs_scanned: AtomicUsize::new(0),
        images_found: AtomicUsize::new(0),
        cancelled: cancel,
    });

    let mut handles = Vec::with_capacity(threads.max(1));
    for _ in 0..threads.max(1) {
        let st = state.clone();
        let tx = tx.clone();
        handles.push(thread::spawn(move || worker(st, exts, tx)));
    }
    for h in handles {
        let _ = h.join();
    }
    if !state.cancelled.load(Ordering::Relaxed) {
        let _ = tx.send(ScanEvent::Done {
            dirs: state.dirs_scanned.load(Ordering::Relaxed),
            images: state.images_found.load(Ordering::Relaxed),
        });
    }
}

fn worker(st: Arc<ScanState>, exts: &'static [&'static str], tx: flume::Sender<ScanEvent>) {
    let mut batch: Vec<PathBuf> = Vec::with_capacity(BATCH_SIZE);
    loop {
        if st.cancelled.load(Ordering::Relaxed) {
            break;
        }
        // 取一个待扫目录；队列空但还有线程在干活时等待
        let dir = {
            let mut q = st.queue.lock().unwrap();
            loop {
                if st.cancelled.load(Ordering::Relaxed) {
                    break None;
                }
                if let Some(d) = q.pop_front() {
                    break Some(d);
                }
                if st.pending.load(Ordering::Acquire) == 0 {
                    break None; // 全树扫完
                }
                // 短超时轮询：取消时无需专门 notify
                let (nq, _timeout) = st
                    .cv
                    .wait_timeout(q, Duration::from_millis(50))
                    .unwrap();
                q = nq;
            }
        };
        let Some(dir) = dir else { break };

        scan_dir(&dir, exts, &st, &tx, &mut batch);

        st.dirs_scanned.fetch_add(1, Ordering::Relaxed);
        if st.pending.fetch_sub(1, Ordering::AcqRel) == 1 {
            st.cv.notify_all(); // 可能是最后一个目录
        }
    }
    // 退出前 flush 尾批：不满 BATCH_SIZE 的图片也要计数和发出
    if !batch.is_empty() {
        st.images_found
            .fetch_add(batch.len(), Ordering::Relaxed);
        let _ = tx.send(ScanEvent::Batch {
            images: batch,
            dirs: st.dirs_scanned.load(Ordering::Relaxed),
        });
    }
}

fn scan_dir(
    dir: &Path,
    exts: &'static [&'static str],
    st: &ScanState,
    tx: &flume::Sender<ScanEvent>,
    batch: &mut Vec<PathBuf>,
) {
    // 搜索模式："dir\*"
    let mut pattern: Vec<u16> = dir.as_os_str().encode_wide().collect();
    pattern.push('\\' as u16);
    pattern.push('*' as u16);
    pattern.push(0);

    let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    let mut handle = unsafe {
        FindFirstFileExW(
            pattern.as_ptr(),
            FIND_EX_INFO_BASIC,
            &mut data,
            FIND_EX_SEARCH_NAME_MATCH,
            std::ptr::null(),
            FIND_FIRST_EX_LARGE_FETCH,
        )
    };
    if handle as usize == INVALID_HANDLE_VALUE {
        // 旧系统不支持 LARGE_FETCH → 退化普通模式重试一次
        handle = unsafe {
            FindFirstFileExW(
                pattern.as_ptr(),
                FIND_EX_INFO_BASIC,
                &mut data,
                FIND_EX_SEARCH_NAME_MATCH,
                std::ptr::null(),
                0,
            )
        };
        if handle as usize == INVALID_HANDLE_VALUE {
            let _ = unsafe { GetLastError() }; // 权限不足等：直接跳过该目录
            return;
        }
    }

    loop {
        let attrs = data.dwFileAttributes;
        if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
            // 跳过 "." / ".."（不跳会无限重新入队，死循环）；
            // 注意不能用 continue —— 会跳过循环末尾的 FindNextFileW
            let is_dot = data.cFileName[0] == '.' as u16
                && (data.cFileName[1] == 0 || data.cFileName[1] == '.' as u16);
            // junction/符号链接目录不跟随（防环、防重复）
            if !is_dot && attrs & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
                let name_len = wide_len(&data.cFileName);
                let name = OsString::from_wide(&data.cFileName[..name_len]);
                let mut sub = PathBuf::from(dir);
                sub.push(name);
                st.pending.fetch_add(1, Ordering::Release);
                st.queue.lock().unwrap().push_back(sub);
                st.cv.notify_one();
            }
        } else {
            let name_len = wide_len(&data.cFileName);
            // 先用文件名判断扩展名（零分配），命中才构建完整 PathBuf
            let name = OsString::from_wide(&data.cFileName[..name_len]);
            let matched = Path::new(&name)
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| exts.iter().any(|x| x.eq_ignore_ascii_case(e)))
                .unwrap_or(false);
            if matched {
                let mut p = PathBuf::from(dir);
                p.push(name);
                batch.push(p);
                if batch.len() >= BATCH_SIZE {
                    st.images_found
                        .fetch_add(batch.len(), Ordering::Relaxed);
                    let _ = tx.send(ScanEvent::Batch {
                        images: std::mem::take(batch),
                        dirs: st.dirs_scanned.load(Ordering::Relaxed),
                    });
                    batch.reserve(BATCH_SIZE);
                }
            }
        }
        if unsafe { FindNextFileW(handle, &mut data) } == 0 {
            break;
        }
    }
    unsafe { FindClose(handle) };
}

#[inline]
fn wide_len(buf: &[u16; 260]) -> usize {
    buf.iter().position(|&c| c == 0).unwrap_or(260)
}

/// 单层扫描结果：直接子目录 + 本层图片 + 本层压缩包
pub struct DirLevel {
    pub subdirs: Vec<PathBuf>,
    pub images: Vec<PathBuf>,
    /// 本层的 zip/7z/rar（可内联展开成可导航条目）
    pub archives: Vec<PathBuf>,
}

/// 只枚举 `dir` 的直接一层（子目录 + 匹配扩展名的图片/压缩包），同步、无递归。
/// 用于侧边栏目录树的惰性展开：点开哪个目录才扫哪个目录。
pub fn scan_one_level(dir: &Path, exts: &[&str], arc_exts: &[&str]) -> DirLevel {
    let mut out = DirLevel {
        subdirs: Vec::new(),
        images: Vec::new(),
        archives: Vec::new(),
    };

    // 搜索模式："dir\*"；根路径（如 "Z:\"）自带反斜杠，先去掉避免出现 "\\\*"
    let mut pattern: Vec<u16> = dir.as_os_str().encode_wide().collect();
    while pattern.last() == Some(&('\\' as u16)) {
        pattern.pop();
    }
    pattern.push('\\' as u16);
    pattern.push('*' as u16);
    pattern.push(0);

    let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    let mut handle = unsafe {
        FindFirstFileExW(
            pattern.as_ptr(),
            FIND_EX_INFO_BASIC,
            &mut data,
            FIND_EX_SEARCH_NAME_MATCH,
            std::ptr::null(),
            FIND_FIRST_EX_LARGE_FETCH,
        )
    };
    if handle as usize == INVALID_HANDLE_VALUE {
        // 旧系统不支持 LARGE_FETCH → 退化普通模式重试一次
        handle = unsafe {
            FindFirstFileExW(
                pattern.as_ptr(),
                FIND_EX_INFO_BASIC,
                &mut data,
                FIND_EX_SEARCH_NAME_MATCH,
                std::ptr::null(),
                0,
            )
        };
        if handle as usize == INVALID_HANDLE_VALUE {
            let _ = unsafe { GetLastError() }; // 权限不足等：返回空结果
            return out;
        }
    }

    loop {
        let attrs = data.dwFileAttributes;
        if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
            // 跳过 "." / ".."；不能用 continue —— 会跳过循环末尾的 FindNextFileW
            let is_dot = data.cFileName[0] == '.' as u16
                && (data.cFileName[1] == 0 || data.cFileName[1] == '.' as u16);
            if !is_dot && attrs & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
                let name_len = wide_len(&data.cFileName);
                let name = OsString::from_wide(&data.cFileName[..name_len]);
                let mut sub = PathBuf::from(dir);
                sub.push(name);
                out.subdirs.push(sub);
            }
        } else {
            let name_len = wide_len(&data.cFileName);
            let name = OsString::from_wide(&data.cFileName[..name_len]);
            let matched = Path::new(&name)
                .extension()
                .and_then(|e| e.to_str())
                .map(|ext| {
                    if exts.iter().any(|x| x.eq_ignore_ascii_case(ext)) {
                        Some(false)
                    } else if arc_exts.iter().any(|x| x.eq_ignore_ascii_case(ext)) {
                        Some(true)
                    } else {
                        None
                    }
                })
                .unwrap_or(None);
            if let Some(is_archive) = matched {
                let mut p = PathBuf::from(dir);
                p.push(name);
                if is_archive {
                    out.archives.push(p);
                } else {
                    out.images.push(p);
                }
            }
        }
        if unsafe { FindNextFileW(handle, &mut data) } == 0 {
            break;
        }
    }
    unsafe { FindClose(handle) };

    out.subdirs.sort_by(|a, b| {
        a.file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .cmp(&b.file_name().map(|n| n.to_string_lossy().to_lowercase()))
    });
    out.images.sort_unstable();
    out.archives.sort_unstable();
    out
}
