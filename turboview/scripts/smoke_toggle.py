#!/usr/bin/env python
"""验证状态栏「随机 / 幻灯」常驻文字开关可点击切换。

流程：启动 → 截状态栏 → 在状态栏里按列空白切分出各个文字块 →
点击「随机」块中心 → 截图（应变「随机 开」）→ 点击「幻灯」块中心 →
截图（应变「幻灯 开 5s」）→ 再点一次全部关闭。
"""
import ctypes
import os
import subprocess
import sys
import time
from ctypes import wintypes

from PIL import Image

user32 = ctypes.WinDLL("user32", use_last_error=True)
gdi32 = ctypes.WinDLL("gdi32", use_last_error=True)
user32.SetProcessDpiAwarenessContext(ctypes.c_void_p(-5))
user32.SetProcessDPIAware()
user32.GetDpiForWindow.restype = ctypes.c_uint


class BITMAPINFOHEADER(ctypes.Structure):
    _fields_ = [
        ("biSize", wintypes.DWORD),
        ("biWidth", wintypes.LONG),
        ("biHeight", wintypes.LONG),
        ("biPlanes", wintypes.WORD),
        ("biBitCount", wintypes.WORD),
        ("biCompression", wintypes.DWORD),
        ("biSizeImage", wintypes.DWORD),
        ("biXPelsPerMeter", wintypes.LONG),
        ("biYPelsPerMeter", wintypes.LONG),
        ("biClrUsed", wintypes.DWORD),
        ("biClrImportant", wintypes.DWORD),
    ]


class BITMAPINFO(ctypes.Structure):
    _fields_ = [("bmiHeader", BITMAPINFOHEADER), ("bmiColors", wintypes.DWORD * 3)]


def grab(hwnd):
    rect = wintypes.RECT()
    user32.GetWindowRect(hwnd, ctypes.byref(rect))
    scale = user32.GetDpiForWindow(hwnd) / 96.0
    w = int(round((rect.right - rect.left) * scale))
    h = int(round((rect.bottom - rect.top) * scale))
    hdc = user32.GetWindowDC(hwnd)
    memdc = gdi32.CreateCompatibleDC(hdc)
    bmp = gdi32.CreateCompatibleBitmap(hdc, w, h)
    gdi32.SelectObject(memdc, bmp)
    user32.PrintWindow(hwnd, memdc, 2)
    info = BITMAPINFO()
    info.bmiHeader.biSize = ctypes.sizeof(BITMAPINFOHEADER)
    info.bmiHeader.biWidth = w
    info.bmiHeader.biHeight = -h
    info.bmiHeader.biPlanes = 1
    info.bmiHeader.biBitCount = 32
    buf = ctypes.create_string_buffer(w * h * 4)
    gdi32.GetDIBits(memdc, bmp, 0, h, buf, ctypes.byref(info), 0)
    img = Image.frombuffer("RGBA", (w, h), buf, "raw", "BGRA", 0, 1).convert("RGB")
    gdi32.DeleteObject(bmp)
    gdi32.DeleteDC(memdc)
    user32.ReleaseDC(hwnd, hdc)
    return img, scale


def status_row_runs(img, scale):
    """在状态栏文字行按列空白切分出文字块，返回 ([(x0,x1)...], y_click)

    y_click 用客户区坐标（物理像素）算：状态栏高 26 逻辑，文字行约在底部
    上方 22 px；x 直接取截图坐标（左边框约 0）。
    """
    w, h = img.size
    px = img.load()
    # 内容底部：从下往上找第一行主色不是纯黑的（再往下是窗口缩放边框）
    from collections import Counter

    content_bottom = h - 1
    while content_bottom > 0:
        c = Counter(px[x, content_bottom] for x in range(0, w, 7))
        if c.most_common(1)[0][0] != (0, 0, 0):
            break
        content_bottom -= 1
    y0 = content_bottom - int(24 * scale)
    y1 = content_bottom - int(8 * scale)
    counter = Counter(px[x, y] for y in range(y0, y1, 2) for x in range(0, w, 5))
    bg = counter.most_common(1)[0][0]
    occ = []
    for x in range(w):
        hit = 0
        for y in range(y0, y1):
            c = px[x, y]
            if abs(c[0] - bg[0]) + abs(c[1] - bg[1]) + abs(c[2] - bg[2]) > 90:
                hit += 1
        occ.append(hit >= 2)
    runs, start, blank = [], None, 0
    min_gap = int(24 * scale)
    for x, o in enumerate(occ):
        if o:
            if start is None:
                start = x
            blank = 0
        elif start is not None:
            blank += 1
            if blank >= min_gap:
                runs.append((start, x - blank))
                start = None
    if start is not None:
        runs.append((start, w - 1))
    return runs, content_bottom


def click(hwnd, x, y, scale=1.0):
    """真实鼠标点击。x/y 是客户区物理像素：先按 scale 折回本进程的
    虚拟化坐标，再由 ClientToScreen 换成屏幕坐标，避免自己算标题栏偏移。"""
    pt = wintypes.POINT(int(round(x / scale)), int(round(y / scale)))
    user32.ClientToScreen(hwnd, ctypes.byref(pt))
    user32.SetForegroundWindow(hwnd)
    time.sleep(0.15)
    user32.SetCursorPos(pt.x, pt.y)
    time.sleep(0.1)
    user32.mouse_event(0x0002, 0, 0, 0, 0)  # LEFTDOWN
    time.sleep(0.08)
    user32.mouse_event(0x0004, 0, 0, 0, 0)  # LEFTUP
    time.sleep(0.8)


def main():
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    exe = os.path.join(root, "target", "debug", "turboview.exe")
    watch_dir = os.path.join(root, "test-watch")
    out = os.path.join(root, "smoke_toggle")
    proc = subprocess.Popen([exe, watch_dir])
    try:
        hwnd = 0
        for _ in range(75):
            hwnd = user32.FindWindowW(None, "test-watch - TurboView")
            if hwnd:
                break
            time.sleep(0.2)
        if not hwnd:
            print("FAIL: 窗口未出现")
            return 1
        time.sleep(4.0)
        img, scale = grab(hwnd)
        img.crop((0, img.size[1] - int(26 * scale), img.size[0], img.size[1])).save(
            out + "_0_init.png"
        )
        runs, _cb = status_row_runs(img, scale)
        c = wintypes.RECT()
        user32.GetClientRect(hwnd, ctypes.byref(c))
        click_y = int(round((c.bottom - c.top) * scale)) - int(round(14 * scale))
        print("scale", scale, "文字块数", len(runs), runs[:8], "click_y", click_y)
        # 两个开关块宽度相近（图标+两三个汉字 ≈ 60~80 物理像素），
        # 且都排在状态栏左半区；按宽度筛出来比按序号稳。
        toggles = [r for r in runs if 45 <= (r[1] - r[0]) <= 95 and r[0] < img.size[0] * 0.6]
        print("开关块:", toggles)
        if len(toggles) < 2:
            print("FAIL: 未找到两个开关文字块")
            return 1
        rx = (toggles[0][0] + toggles[0][1]) // 2
        sx = (toggles[1][0] + toggles[1][1]) // 2
        print("点击随机 @", rx, click_y)
        click(hwnd, rx, click_y, scale)
        img, _ = grab(hwnd)
        img.crop((0, img.size[1] - int(26 * scale), img.size[0], img.size[1])).save(
            out + "_1_random_on.png"
        )
        print("点击幻灯 @", sx, click_y)
        click(hwnd, sx, click_y, scale)
        img, _ = grab(hwnd)
        img.crop((0, img.size[1] - int(26 * scale), img.size[0], img.size[1])).save(
            out + "_2_slideshow_on.png"
        )
        print("再点一次关闭两者")
        click(hwnd, rx, click_y, scale)
        click(hwnd, sx, click_y, scale)
        img, _ = grab(hwnd)
        img.crop((0, img.size[1] - int(26 * scale), img.size[0], img.size[1])).save(
            out + "_3_both_off.png"
        )
    finally:
        proc.terminate()
        try:
            proc.wait(5)
        except subprocess.TimeoutExpired:
            proc.kill()
    print("done")
    return 0


if __name__ == "__main__":
    sys.exit(main())
