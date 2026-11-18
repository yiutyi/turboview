//! Win32 辅助：伪全屏（最大化/还原）、盘符与常用目录枚举、文件操作
//!
//! gpui 未暴露运行时窗口控制，这里用 FindWindowW 按标题拿 HWND
//! 调 ShowWindow 实现伪全屏（保留标题栏的最大化）。

#![allow(non_snake_case)]

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

type HWND = *mut core::ffi::c_void;

const SW_MAXIMIZE: i32 = 3;
const SW_RESTORE: i32 = 9;

#[link(name = "user32")]
unsafe extern "system" {
    fn FindWindowW(lpClassName: *const u16, lpWindowName: *const u16) -> HWND;
    fn ShowWindow(hWnd: HWND, nCmdShow: i32) -> i32;
    fn IsZoomed(hWnd: HWND) -> i32;
    fn OpenClipboard(hWndNewOwner: HWND) -> i32;
    fn CloseClipboard() -> i32;
    fn EmptyClipboard() -> i32;
    fn SetClipboardData(uFormat: u32, hMem: *mut core::ffi::c_void) -> *mut core::ffi::c_void;
    fn GetSystemMetrics(nIndex: i32) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetLogicalDrives() -> u32;
    fn GlobalAlloc(uFlags: u32, dwBytes: usize) -> *mut core::ffi::c_void;
    fn GlobalLock(hMem: *mut core::ffi::c_void) -> *mut core::ffi::c_void;
    fn GlobalUnlock(hMem: *mut core::ffi::c_void) -> i32;
    fn GlobalFree(hMem: *mut core::ffi::c_void) -> *mut core::ffi::c_void;
    fn GetModuleHandleW(lpModuleName: *const u16) -> *mut core::ffi::c_void;
    fn GetProcAddress(hModule: *mut core::ffi::c_void, lpProcName: *const u8) -> *mut core::ffi::c_void;
}

#[link(name = "user32")]
unsafe extern "system" {
    fn SetProcessDPIAware() -> i32;
}

/// 设置 PerMonitorV2 DPI 感知。
///
/// 必须在创建任何窗口之前调用。不设置的话进程是 DPI 不感知的：
/// 125%/150%/175% 缩放的显示器上窗口只按 1:1 物理像素渲染，
/// “最大化”也只占屏幕左上角一小块（gpui 自己不设 DPI 感知）。
///
/// `SetProcessDpiAwarenessContext` 经 GetProcAddress 动态获取
/// （Win10 1703+ 才有），老系统上不会因为导入表缺失而无法启动；
/// 失败时回退到 system DPI aware。
pub fn set_per_monitor_dpi_aware() {
    unsafe {
        let module = wide("user32.dll");
        let handle = GetModuleHandleW(module.as_ptr());
        let proc = GetProcAddress(handle, b"SetProcessDpiAwarenessContext\0".as_ptr());
        if !proc.is_null() {
            let set_ctx: unsafe extern "system" fn(isize) -> i32 = std::mem::transmute(proc);
            // 常量：-5 = PER_MONITOR_AWARE_V2，-4 = PER_MONITOR_AWARE（V1）
            if set_ctx(-5) != 0 {
                return;
            }
            if set_ctx(-4) != 0 {
                return;
            }
        }
        // 回退：系统级 DPI 感知（老系统 / V2 不可用）
        SetProcessDPIAware();
    }
}

#[link(name = "shell32")]
unsafe extern "system" {
    fn SHFileOperationW(lpFileOp: *mut SHFILEOPSTRUCTW) -> i32;
    fn ShellExecuteW(
        hwnd: HWND,
        lpVerb: *const u16,
        lpFile: *const u16,
        lpParameters: *const u16,
        lpDirectory: *const u16,
        nShowCmd: i32,
    ) -> usize;
}

fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

/// 伪全屏切换：最大化 ⇄ 还原（保留标题栏）
pub fn toggle_maximize(title: &str) {
    unsafe {
        let t = wide(title);
        let hwnd = FindWindowW(std::ptr::null(), t.as_ptr());
        if hwnd.is_null() {
            return;
        }
        ShowWindow(
            hwnd,
            if IsZoomed(hwnd) != 0 {
                SW_RESTORE
            } else {
                SW_MAXIMIZE
            },
        );
    }
}

/// 枚举本机所有盘符（"C:"、"D:" …）
pub fn drives() -> Vec<String> {
    let mask = unsafe { GetLogicalDrives() };
    (0..26)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| format!("{}:", (b'A' + i) as char))
        .collect()
}

/// 当前用户的常用目录（桌面/文档/下载/图片），只返回真实存在的
pub fn user_places() -> Vec<(&'static str, PathBuf)> {
    let Ok(home) = std::env::var("USERPROFILE") else {
        return Vec::new();
    };
    let home = PathBuf::from(home);
    [
        ("桌面", "Desktop"),
        ("文档", "Documents"),
        ("下载", "Downloads"),
        ("图片", "Pictures"),
    ]
    .into_iter()
    .map(|(label, dir)| (label, home.join(dir)))
    .filter(|(_, p)| p.is_dir())
    .collect()
}

// ============================================================================
// 目录变化监控（ReadDirectoryChangesW）
//
// 用重叠 I/O：一次 ReadDirectoryChangesW 挂起在内核里，线程只等两个事件
// （变化事件 / 停止事件）。停止时 CancelIoEx 取消挂起操作，线程干净退出、
// 句柄全部释放——切换目录不会泄漏线程。
// ============================================================================

const FILE_LIST_DIRECTORY: u32 = 0x0001;
const FILE_SHARE_READ: u32 = 0x0000_0001;
const FILE_SHARE_WRITE: u32 = 0x0000_0002;
const FILE_SHARE_DELETE: u32 = 0x0000_0004;
const OPEN_EXISTING: u32 = 3;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;

const FILE_NOTIFY_CHANGE_FILE_NAME: u32 = 0x001;
const FILE_NOTIFY_CHANGE_DIR_NAME: u32 = 0x002;
const FILE_NOTIFY_CHANGE_SIZE: u32 = 0x008;
const FILE_NOTIFY_CHANGE_LAST_WRITE: u32 = 0x010;

const WAIT_OBJECT_0: u32 = 0;
const INFINITE: u32 = 0xFFFF_FFFF;
/// 事件防抖窗口：批量拷贝/删除会连发几十个通知，合并成一次刷新
const WATCH_DEBOUNCE_MS: u32 = 300;

type HANDLE = *mut core::ffi::c_void;

#[repr(C)]
struct OVERLAPPED {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    h_event: HANDLE,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateFileW(
        lpFileName: *const u16,
        dwDesiredAccess: u32,
        dwShareMode: u32,
        lpSecurityAttributes: *const core::ffi::c_void,
        dwCreationDisposition: u32,
        dwFlagsAndAttributes: u32,
        hTemplateFile: HANDLE,
    ) -> HANDLE;
    fn ReadDirectoryChangesW(
        hDirectory: HANDLE,
        lpBuffer: *mut core::ffi::c_void,
        nBufferLength: u32,
        bWatchSubtree: i32,
        dwNotifyFilter: u32,
        lpBytesReturned: *mut u32,
        lpOverlapped: *mut OVERLAPPED,
        lpCompletionRoutine: *const core::ffi::c_void,
    ) -> i32;
    fn CreateEventW(
        lpEventAttributes: *const core::ffi::c_void,
        bManualReset: i32,
        bInitialState: i32,
        lpName: *const u16,
    ) -> HANDLE;
    fn WaitForMultipleObjects(
        nCount: u32,
        lpHandles: *const HANDLE,
        bWaitAll: i32,
        dwMilliseconds: u32,
    ) -> u32;
    fn WaitForSingleObject(hHandle: HANDLE, dwMilliseconds: u32) -> u32;
    fn ResetEvent(hEvent: HANDLE) -> i32;
    fn SetEvent(hEvent: HANDLE) -> i32;
    fn CloseHandle(hObject: HANDLE) -> i32;
    fn CancelIoEx(hFile: HANDLE, lpOverlapped: *const OVERLAPPED) -> i32;
    fn GetOverlappedResult(
        hFile: HANDLE,
        lpOverlapped: *mut OVERLAPPED,
        lpNumberOfBytesTransferred: *mut u32,
        bWait: i32,
    ) -> i32;
    fn GetLastError() -> u32;
    fn GetDriveTypeW(lpRootPathName: *const u16) -> u32;
}

/// 一个正在运行的目录监控器。Drop 即停止（发出停止事件并关闭句柄）。
pub struct DirWatch {
    /// 停止事件句柄（manual-reset）；线程里以 usize 传递，因为裸指针不是 Send
    stop: usize,
    /// 工作线程：关闭 stop 句柄前必须 join，否则线程仍在等待的句柄被关闭是 UB
    thread: Option<std::thread::JoinHandle<()>>,
}

impl DirWatch {
    /// 请求停止：置位事件 → 线程从等待中醒来并退出
    pub fn stop(&self) {
        if self.stop != 0 {
            unsafe { SetEvent(self.stop as HANDLE) };
        }
    }
}

impl Drop for DirWatch {
    fn drop(&mut self) {
        self.stop();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if self.stop != 0 {
            unsafe { CloseHandle(self.stop as HANDLE) };
        }
    }
}

/// 取消挂起的 ReadDirectoryChangesW 并等到内核真正完成，之后才能关闭句柄/回收 OVERLAPPED。
/// CancelIoEx 是异步的：不等待就在另一线程 CloseHandle 会构成 use-after-close。
unsafe fn cancel_and_wait(hdir: HANDLE, ov: *mut OVERLAPPED) {
    const ERROR_NOT_FOUND: u32 = 1168; // 已无挂起操作
    unsafe {
        if CancelIoEx(hdir, ov) == 0 && GetLastError() == ERROR_NOT_FOUND {
            return;
        }
        let mut bytes = 0u32;
        // bWait=TRUE 阻塞到操作结束（取消后以 ERROR_OPERATION_ABORTED 完成）
        let _ = GetOverlappedResult(hdir, ov, &mut bytes, 1);
    }
}

/// 监控 `dir` 自身（不递归）的文件变化，变化经 `tx` 送出（已防抖）。
/// 返回的 `DirWatch` 被丢弃时监控线程自动退出。
pub fn watch_dir(dir: PathBuf, tx: flume::Sender<PathBuf>) -> DirWatch {
    let stop_evt = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if stop_evt.is_null() {
        return DirWatch {
            stop: 0,
            thread: None,
        };
    }
    let raw = stop_evt as usize;
    let thread = std::thread::spawn(move || unsafe {
        let stop_evt = raw as HANDLE;
        // 打开目录句柄：目录必须有 FILE_FLAG_BACKUP_SEMANTICS 才能 CreateFile
        let p: Vec<u16> = dir
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let hdir = CreateFileW(
            p.as_ptr(),
            FILE_LIST_DIRECTORY,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
            std::ptr::null_mut(),
        );
        if hdir as isize == -1 || hdir.is_null() {
            return;
        }
        let changed = CreateEventW(std::ptr::null(), 1, 0, std::ptr::null());
        if changed.is_null() {
            CloseHandle(hdir);
            return;
        }

        let mut buf = vec![0u8; 64 * 1024];
        let mut ov = OVERLAPPED {
            internal: 0,
            internal_high: 0,
            offset: 0,
            offset_high: 0,
            h_event: changed,
        };
        loop {
            ResetEvent(changed);
            let ok = ReadDirectoryChangesW(
                hdir,
                buf.as_mut_ptr() as *mut core::ffi::c_void,
                buf.len() as u32,
                0, // 不递归：只监控当前浏览的这一层
                FILE_NOTIFY_CHANGE_FILE_NAME
                    | FILE_NOTIFY_CHANGE_DIR_NAME
                    | FILE_NOTIFY_CHANGE_SIZE
                    | FILE_NOTIFY_CHANGE_LAST_WRITE,
                std::ptr::null_mut(),
                &mut ov,
                std::ptr::null(),
            );
            if ok == 0 {
                break; // 目录被删/权限变化：静默退出
            }
            let handles = [changed, stop_evt];
            let r = WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE);
            if r != WAIT_OBJECT_0 {
                // 停止事件（或异常）：取消挂起的操作后退出
                cancel_and_wait(hdir, &mut ov);
                break;
            }
            // 变化已到：再等一个防抖窗口，把连发的通知合并成一次刷新；
            // 期间若收到停止信号则直接退出
            if WaitForSingleObject(stop_evt, WATCH_DEBOUNCE_MS) == WAIT_OBJECT_0 {
                cancel_and_wait(hdir, &mut ov);
                break;
            }
            if tx.send(dir.clone()).is_err() {
                cancel_and_wait(hdir, &mut ov);
                break; // 接收端已关闭（窗口关闭）
            }
        }
        CloseHandle(changed);
        CloseHandle(hdir);
    });
    DirWatch {
        stop: raw,
        thread: Some(thread),
    }
}

// ============================================================================
// 文件操作
// ============================================================================

const CF_DIB: u32 = 8;
const CF_UNICODETEXT: u32 = 13;
const GMEM_MOVEABLE: u32 = 0x0002;

/// FO_DELETE / FOF_* 标志
const FO_DELETE: u32 = 3;
const FOF_ALLOWUNDO: u16 = 0x40; // 进回收站而非直接删除
const FOF_NOCONFIRMATION: u16 = 0x10;
const FOF_SILENT: u16 = 0x4;
const FOF_NOERRORUI: u16 = 0x400;

const SW_SHOWNORMAL: i32 = 1;

fn put32(buf: &mut [u8], o: usize, v: u32) {
    buf[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
fn put16(buf: &mut [u8], o: usize, v: u16) {
    buf[o..o + 2].copy_from_slice(&v.to_le_bytes());
}

/// x64 上的 SHFILEOPSTRUCTW（含对齐填充）
#[repr(C)]
struct SHFILEOPSTRUCTW {
    hwnd: HWND,
    wFunc: u32,
    _pad0: u32,
    pFrom: *const u16,
    pTo: *const u16,
    fFlags: u16,
    _pad1: u16,
    fAnyOperationsAborted: i32,
    hNameMappings: *mut core::ffi::c_void,
    lpszProgressTitle: *const u16,
}

/// 删除文件到回收站（SHFileOperationW + FOF_ALLOWUNDO，可撤销）
pub fn delete_to_recycle_bin(path: &Path) -> io::Result<()> {
    if !path.is_file() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "文件不存在"));
    }
    // pFrom 要求双 null 结尾的路径列表
    let mut from: Vec<u16> = path.as_os_str().encode_wide().collect();
    from.push(0);
    from.push(0);
    let mut op = SHFILEOPSTRUCTW {
        hwnd: std::ptr::null_mut(),
        wFunc: FO_DELETE,
        _pad0: 0,
        pFrom: from.as_ptr(),
        pTo: std::ptr::null(),
        fFlags: FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI,
        _pad1: 0,
        fAnyOperationsAborted: 0,
        hNameMappings: std::ptr::null_mut(),
        lpszProgressTitle: std::ptr::null(),
    };
    let rc = unsafe { SHFileOperationW(&mut op) };
    if rc != 0 {
        return Err(io::Error::other(format!("SHFileOperation 错误码 {rc}")));
    }
    if op.fAnyOperationsAborted != 0 {
        return Err(io::Error::other("操作被取消"));
    }
    Ok(())
}

/// 把 BGRA 像素缓冲（GPUI RenderImage 帧格式）作为 CF_DIB 放入剪贴板。
/// 32bpp BI_RGB、负 biHeight（top-down），与 BGRA 行序一致，零转换。
pub fn copy_dib_to_clipboard(bgra: &[u8], width: u32, height: u32) -> io::Result<()> {
    let header_size = 40usize;
    let pixel_bytes = width as usize * height as usize * 4;
    if bgra.len() < pixel_bytes {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "像素缓冲不足"));
    }
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut ok = false;
        EmptyClipboard(); // 必须先清空并绑定剪贴板所有者，SetClipboardData 才可靠
        let hglob = GlobalAlloc(GMEM_MOVEABLE, header_size + pixel_bytes);
        if !hglob.is_null() {
            let p = GlobalLock(hglob) as *mut u8;
            if !p.is_null() {
                let buf = std::slice::from_raw_parts_mut(p, header_size + pixel_bytes);
                // BITMAPINFOHEADER（40 字节，小端逐字段写入）
                put32(buf, 0, 40); // biSize
                put32(buf, 4, width); // biWidth
                put32(buf, 8, (height as i32).wrapping_neg() as u32); // biHeight：负值 = top-down
                put16(buf, 12, 1); // biPlanes
                put16(buf, 14, 32); // biBitCount
                put32(buf, 16, 0); // biCompression = BI_RGB
                put32(buf, 20, pixel_bytes as u32); // biSizeImage
                put32(buf, 24, 0); // biXPelsPerMeter
                put32(buf, 28, 0); // biYPelsPerMeter
                put32(buf, 32, 0); // biClrUsed
                put32(buf, 36, 0); // biClrImportant
                buf[header_size..].copy_from_slice(&bgra[..pixel_bytes]);
                GlobalUnlock(hglob);
                if !SetClipboardData(CF_DIB, hglob).is_null() {
                    ok = true; // 剪贴板接管 hglob，不能 GlobalFree
                }
            }
            if !ok {
                GlobalFree(hglob);
            }
        }
        CloseClipboard();
        if ok {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

/// 把 UTF-8 文本作为 CF_UNICODETEXT 放入剪贴板
pub fn copy_text_to_clipboard(text: &str) -> io::Result<()> {
    let mut wide_text: Vec<u16> = OsStr::new(text).encode_wide().collect();
    wide_text.push(0);
    let bytes = wide_text.len() * 2;
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut ok = false;
        EmptyClipboard();
        let hglob = GlobalAlloc(GMEM_MOVEABLE, bytes);
        if !hglob.is_null() {
            let p = GlobalLock(hglob) as *mut u16;
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(wide_text.as_ptr(), p, wide_text.len());
                GlobalUnlock(hglob);
                if !SetClipboardData(CF_UNICODETEXT, hglob).is_null() {
                    ok = true;
                }
            }
            if !ok {
                GlobalFree(hglob);
            }
        }
        CloseClipboard();
        if ok {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

/// 在资源管理器中定位文件
pub fn reveal_in_explorer(path: &Path) -> io::Result<()> {
    let verb = wide("open");
    let exe = wide("explorer.exe");
    // 统一成反斜杠本地路径：explorer 的 /select 不认正斜杠，
    // 也不接受 canonicalize 带来的 \\?\ 长路径前缀（带上会退化成"此电脑"）。
    let mut norm = path.display().to_string().replace('/', "\\");
    if let Some(rest) = norm.strip_prefix(r"\\?\") {
        norm = rest.to_string();
    }
    // 目录：直接打开它；文件：在父目录里选中它
    let target = if path.is_dir() {
        norm
    } else {
        format!("/select,\"{norm}\"")
    };
    let params = wide(&target);
    let rc = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            exe.as_ptr(),
            params.as_ptr(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW 返回值 > 32 表示成功
    if rc > 32 {
        Ok(())
    } else {
        Err(io::Error::other(format!("ShellExecute 失败 ({rc})")))
    }
}

// ============================================================================
// 注册表：注册为 Windows 默认图片查看器（HKCU 作用域，无需管理员）
// ============================================================================

type HKEY = *mut core::ffi::c_void;

const HKEY_CURRENT_USER: HKEY = 0x8000_0001 as HKEY;
const REG_SZ: u32 = 1;
/// KEY_SET_VALUE | KEY_CREATE_SUB_KEY
const KEY_WRITE: u32 = 0x0002_0006;
const ERROR_SUCCESS: i32 = 0;
const REG_OPTION_NON_VOLATILE: u32 = 0;
const SHCNE_ASSOCCHANGED: u32 = 0x0800_0000;
const SHCNF_IDLIST: u32 = 0x0000;

#[link(name = "advapi32")]
unsafe extern "system" {
    fn RegCreateKeyExW(
        hKey: HKEY,
        lpSubKey: *const u16,
        Reserved: u32,
        lpClass: *mut u16,
        dwOptions: u32,
        samDesired: u32,
        lpSecurityAttributes: *mut core::ffi::c_void,
        phkResult: *mut HKEY,
        lpdwDisposition: *mut u32,
    ) -> i32;
    fn RegSetValueExW(
        hKey: HKEY,
        lpValueName: *const u16,
        Reserved: u32,
        dwType: u32,
        lpData: *const u8,
        cbData: u32,
    ) -> i32;
    fn RegCloseKey(hKey: HKEY) -> i32;
}

#[link(name = "shell32")]
unsafe extern "system" {
    fn SHChangeNotify(
        wEventId: u32,
        uFlags: u32,
        dwItem1: *const core::ffi::c_void,
        dwItem2: *const core::ffi::c_void,
    );
}

/// 在 HKCU 下写一个 REG_SZ（value_name 为 None 时写键的默认值）
fn reg_write_string(subkey: &str, value_name: Option<&str>, data: &str) -> io::Result<()> {
    unsafe {
        let sub = wide(subkey);
        let mut hkey: HKEY = std::ptr::null_mut();
        let rc = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            sub.as_ptr(),
            0,
            std::ptr::null_mut(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            std::ptr::null_mut(),
            &mut hkey,
            std::ptr::null_mut(),
        );
        if rc != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(rc));
        }
        let name = value_name.map(wide);
        let name_ptr = name.as_ref().map_or(std::ptr::null(), |n| n.as_ptr());
        let mut text: Vec<u16> = data.encode_utf16().collect();
        text.push(0); // REG_SZ 要求 null 结尾
        let rc = RegSetValueExW(
            hkey,
            name_ptr,
            0,
            REG_SZ,
            text.as_ptr() as *const u8,
            (text.len() * 2) as u32,
        );
        RegCloseKey(hkey);
        if rc != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(rc));
        }
    }
    Ok(())
}

/// 把 TurboView 注册为 jpg/png 的默认图片查看器。
/// HKCU 作用域（每用户），不需要管理员权限；重复调用幂等。
/// 说明：若用户此前通过"打开方式"锁定过其他程序（UserChoice），
/// 该哈希受系统保护，本注册不会强行覆盖；未锁定时本注册即为默认。
pub fn register_default_viewer(exe: &Path) -> io::Result<()> {
    let exe_str = exe.display().to_string();
    let prog_id = "TurboView.Image";
    let command = format!("\"{}\" \"%1\"", exe_str);

    // ProgID 本体：显示名 + 打开命令
    reg_write_string(
        &format!("Software\\Classes\\{prog_id}"),
        None,
        "TurboView 图片",
    )?;
    reg_write_string(
        &format!("Software\\Classes\\{prog_id}\\shell\\open\\command"),
        None,
        &command,
    )?;

    // 扩展名默认 ProgID + OpenWithProgids（经典关联路径）
    for ext in [".jpg", ".png"] {
        reg_write_string(&format!("Software\\Classes\\{ext}"), None, prog_id)?;
        reg_write_string(
            &format!("Software\\Classes\\{ext}\\OpenWithProgids"),
            Some(prog_id),
            "",
        )?;
    }

    // Applications 注册：资源管理器"打开方式"列表 / 默认程序设置页
    let app_base = "Software\\Classes\\Applications\\turboview.exe";
    reg_write_string(&format!("{app_base}\\shell\\open\\command"), None, &command)?;
    reg_write_string(&format!("{app_base}\\SupportedTypes"), Some(".jpg"), "")?;
    reg_write_string(&format!("{app_base}\\SupportedTypes"), Some(".png"), "")?;

    // 通知资源管理器关联关系已变更（刷新图标/默认程序缓存）
    unsafe {
        SHChangeNotify(
            SHCNE_ASSOCCHANGED,
            SHCNF_IDLIST,
            std::ptr::null(),
            std::ptr::null(),
        );
    }
    Ok(())
}

// ============================================================================
// 机器能力探测：内存 / 磁盘类型 / 顺序读
//
// 低配机适配全靠这几个数：缓存预算跟内存走，预取策略跟磁盘走。
// ============================================================================

#[repr(C)]
struct MemoryStatusEx {
    dwLength: u32,
    dwMemoryLoad: u32,
    ullTotalPhys: u64,
    ullAvailPhys: u64,
    ullTotalPageFile: u64,
    ullAvailPageFile: u64,
    ullTotalVirtual: u64,
    ullAvailVirtual: u64,
    ullAvailExtendedVirtual: u64,
}

#[repr(C)]
struct StoragePropertyQuery {
    PropertyId: u32,
    QueryType: u32,
    AdditionalParameters: [u8; 1],
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GlobalMemoryStatusEx(lpBuffer: *mut MemoryStatusEx) -> i32;
    fn DeviceIoControl(
        hDevice: HANDLE,
        dwIoControlCode: u32,
        lpInBuffer: *mut core::ffi::c_void,
        nInBufferSize: u32,
        lpOutBuffer: *mut core::ffi::c_void,
        nOutBufferSize: u32,
        lpBytesReturned: *mut u32,
        lpOverlapped: *mut core::ffi::c_void,
    ) -> i32;
    fn ReadFile(
        hFile: HANDLE,
        lpBuffer: *mut u8,
        nNumberOfBytesToRead: u32,
        lpNumberOfBytesRead: *mut u32,
        lpOverlapped: *mut core::ffi::c_void,
    ) -> i32;
    fn GetCurrentThread() -> HANDLE;
    fn SetThreadPriority(hThread: HANDLE, nPriority: i32) -> i32;
}

const THREAD_PRIORITY_BELOW_NORMAL: i32 = -1;
const THREAD_PRIORITY_NORMAL: i32 = 0;

/// SM_CXFULLSCREEN：主显示器工作区宽度（不含任务栏），进程 DPI 感知下即物理像素
const SM_CXFULLSCREEN: i32 = 16;
/// SM_CYFULLSCREEN：主显示器工作区高度
const SM_CYFULLSCREEN: i32 = 17;

/// 主显示器工作区（不含任务栏）的物理像素尺寸。
///
/// 窗口启动即最大化，所以这个值就是"用户能看到的画面"的上界；
/// 第一次 render 之前没有真实视口，用它先估一个，避免首张图按
/// 4096 上限白解一遍（1366×768 的机器上能省掉一半以上的首图耗时）。
pub fn screen_work_area() -> Option<(u32, u32)> {
    unsafe {
        let w = GetSystemMetrics(SM_CXFULLSCREEN);
        let h = GetSystemMetrics(SM_CYFULLSCREEN);
        if w > 0 && h > 0 {
            Some((w as u32, h as u32))
        } else {
            None
        }
    }
}

/// 降到"低于正常"：预取解码用，避免和用户正等着的当前图抢 CPU。
pub fn thread_priority_low() {
    unsafe {
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
}

/// 恢复"正常"：线程池线程会被复用，降完必须还原，否则当前图也跟着变慢。
pub fn thread_priority_normal() {
    unsafe {
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_NORMAL);
    }
}

const GENERIC_READ: u32 = 0x8000_0000;
/// 读文件时共享全部访问，避免别人占着文件就看不到图
const FILE_SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;
const INVALID_HANDLE_VALUE: HANDLE = -1isize as HANDLE;

/// IOCTL_STORAGE_QUERY_PROPERTY
const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x002D_1400;
/// StorageDeviceSeekPenaltyProperty（机械盘 = 有寻道代价）
const STORAGE_DEVICE_SEEK_PENALTY_PROPERTY: u32 = 7;
/// PropertyStandardQuery
const PROPERTY_STANDARD_QUERY: u32 = 0;

fn wide_os(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

/// 物理内存：(总量, 可用量)，字节。失败返回 None。
pub fn memory_info() -> Option<(u64, u64)> {
    let mut st = MemoryStatusEx {
        dwLength: std::mem::size_of::<MemoryStatusEx>() as u32,
        dwMemoryLoad: 0,
        ullTotalPhys: 0,
        ullAvailPhys: 0,
        ullTotalPageFile: 0,
        ullAvailPageFile: 0,
        ullTotalVirtual: 0,
        ullAvailVirtual: 0,
        ullAvailExtendedVirtual: 0,
    };
    let ok = unsafe { GlobalMemoryStatusEx(&mut st) };
    if ok == 0 {
        None
    } else {
        Some((st.ullTotalPhys, st.ullAvailPhys))
    }
}

/// 判断路径所在卷是不是机械盘（有寻道代价）。
///
/// `None` 表示无法判定（网络盘 / 可移动盘 / 权限不足），调用方按"未知"处理。
/// 对 `\\.\C:` 发 IOCTL_STORAGE_QUERY_PROPERTY 通常不需要管理员权限；
/// 失败时不猜，直接返回 None（宁可少优化，不要错误地把 SSD 当 HDD）。
pub fn is_rotational(path: &Path) -> Option<bool> {
    // 只处理 "C:" 这类本地盘符根；UNC / 相对路径直接跳过
    let root = path.components().next()?;
    let root_str = root.as_os_str().to_string_lossy().into_owned();
    if !root_str.ends_with(':') {
        return None;
    }
    let device = wide(&format!("\\\\.\\{root_str}"));
    unsafe {
        let handle = CreateFileW(
            device.as_ptr(),
            0, // 只查询属性，不读数据
            FILE_SHARE_ALL,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut query = StoragePropertyQuery {
            PropertyId: STORAGE_DEVICE_SEEK_PENALTY_PROPERTY,
            QueryType: PROPERTY_STANDARD_QUERY,
            AdditionalParameters: [0],
        };
        // STORAGE_DEVICE_SEEK_PENALTY_DESCRIPTOR：Version(4) + Size(4) + IncursSeekPenalty(1)
        let mut out = [0u8; 16];
        let mut returned: u32 = 0;
        let ok = DeviceIoControl(
            handle,
            IOCTL_STORAGE_QUERY_PROPERTY,
            &mut query as *mut _ as *mut core::ffi::c_void,
            std::mem::size_of::<StoragePropertyQuery>() as u32,
            out.as_mut_ptr() as *mut core::ffi::c_void,
            out.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        );
        CloseHandle(handle);
        if ok == 0 || returned < 9 {
            return None;
        }
        let size = u32::from_ne_bytes([out[4], out[5], out[6], out[7]]);
        if size < 9 {
            return None;
        }
        Some(out[8] != 0)
    }
}

/// 路径是否位于网络驱动器（映射盘 `E:\` 或 UNC `\\server\share`）。
///
/// 网络盘的读取代价和本地盘完全不是一个量级：千兆网满速也就 ~110 MB/s，
/// 而百兆 USB 网卡只有 ~11 MB/s —— 一张 18 MB 的扫描图光传过来就要 1.6 s。
/// 判定之后调用方会切到"顺序读 + 只预取下一张"的保守策略，
/// 避免多个预取线程把这条本来就不宽的通道挤爆。
pub fn is_remote(path: &Path) -> bool {
    /// DRIVE_REMOTE
    const DRIVE_REMOTE: u32 = 4;
    let s = path.to_string_lossy();
    let root = if let Some(rest) = s.strip_prefix("\\\\") {
        // UNC：取 \\server\share 这一段
        let mut it = rest.splitn(3, '\\');
        let server = it.next().unwrap_or("");
        let share = it.next().unwrap_or("");
        if server.is_empty() || share.is_empty() {
            return false;
        }
        format!("\\\\{server}\\{share}")
    } else {
        let bytes = s.as_bytes();
        if bytes.len() < 2 || bytes[1] != b':' {
            return false;
        }
        format!("{}:\\", &s[..1])
    };
    let w = wide(&root);
    unsafe { GetDriveTypeW(w.as_ptr()) == DRIVE_REMOTE }
}

/// 顺序读整个文件（`FILE_FLAG_SEQUENTIAL_SCAN`）。
///
/// 机械盘上比 mmap 更划算：mmap 的缺页是同步的，系统无法为它做前向预读，
/// 冷缓存下变成一连串 4KB 随机读；顺序读提示能让内核一次预取更多扇区。
pub fn read_file_sequential(path: &Path) -> io::Result<Vec<u8>> {
    let p = wide_os(path);
    unsafe {
        let handle = CreateFileW(
            p.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_ALL,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            FILE_FLAG_SEQUENTIAL_SCAN,
            std::ptr::null_mut(),
        );
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let guard = HandleGuard(handle);
        let len = std::fs::metadata(path)?.len() as usize;
        let mut buf = vec![0u8; len];
        let mut filled = 0usize;
        while filled < len {
            let want = (len - filled).min(u32::MAX as usize) as u32;
            let mut read: u32 = 0;
            let ok = ReadFile(
                guard.0,
                buf[filled..].as_mut_ptr(),
                want,
                &mut read,
                std::ptr::null_mut(),
            );
            if ok == 0 || read == 0 {
                return Err(io::Error::last_os_error());
            }
            filled += read as usize;
        }
        Ok(buf)
    }
}

struct HandleGuard(*mut core::ffi::c_void);
impl Drop for HandleGuard {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
