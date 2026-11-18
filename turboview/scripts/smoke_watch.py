#!/usr/bin/env python
"""TurboView 冒烟测试：启动 → 截图 → 制造目录变化 → 再截图。

用 PrintWindow(PW_RENDERFULLCONTENT=2) 抓 DirectComposition 窗口内容
（普通 BitBlt 对 gpui 窗口只能拿到全黑）。
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

# 关键：本进程必须 DPI 感知，否则窗口矩形被虚拟化，
# PrintWindow 的 DC 尺寸与真实物理尺寸不符 → 截图只截到左上角一块
if not user32.SetProcessDpiAwarenessContext(ctypes.c_void_p(-5)):
    user32.SetProcessDPIAware()

SRCCOPY = 0x00CC0020
DIB_RGB_COLORS = 0


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


def wait_window(title, timeout=15.0):
    end = time.time() + timeout
    while time.time() < end:
        hwnd = user32.FindWindowW(None, title)
        if hwnd:
            return hwnd
        time.sleep(0.2)
    return 0


user32.GetDpiForWindow.restype = ctypes.c_uint


def capture(hwnd, path, band=None):
    """band: (height_px) 只保留底部这么多物理像素（状态栏）

    注意：本进程即使调用了 SetProcessDpiAwarenessContext，在这台机器上
    GetWindowRect 仍返回虚拟化尺寸（物理 / scale）。而 PrintWindow 是按
    物理像素 1:1 绘制的，所以位图必须按 scale 放大，否则只截到左上角。
    """
    rect = wintypes.RECT()
    user32.GetWindowRect(hwnd, ctypes.byref(rect))
    scale = user32.GetDpiForWindow(hwnd) / 96.0
    w = int(round((rect.right - rect.left) * scale))
    h = int(round((rect.bottom - rect.top) * scale))
    hdc = user32.GetWindowDC(hwnd)
    memdc = gdi32.CreateCompatibleDC(hdc)
    bmp = gdi32.CreateCompatibleBitmap(hdc, w, h)
    gdi32.SelectObject(memdc, bmp)
    ok = user32.PrintWindow(hwnd, memdc, 2)
    info = BITMAPINFO()
    info.bmiHeader.biSize = ctypes.sizeof(BITMAPINFOHEADER)
    info.bmiHeader.biWidth = w
    info.bmiHeader.biHeight = -h  # top-down
    info.bmiHeader.biPlanes = 1
    info.bmiHeader.biBitCount = 32
    info.bmiHeader.biCompression = 0
    size = w * h * 4
    buf = ctypes.create_string_buffer(size)
    gdi32.GetDIBits(memdc, bmp, 0, h, buf, ctypes.byref(info), DIB_RGB_COLORS)
    img = Image.frombuffer("RGBA", (w, h), buf, "raw", "BGRA", 0, 1).convert("RGB")
    gdi32.DeleteObject(bmp)
    gdi32.DeleteDC(memdc)
    user32.ReleaseDC(hwnd, hdc)
    print(f"  PrintWindow ok={ok} size={w}x{h}")
    if band:
        img = img.crop((0, h - band, w, h))
        img = img.resize((img.width * 2, img.height * 2), Image.LANCZOS)
    img.save(path)
    return img


def main():
    root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    exe = os.path.join(root, "target", "debug", "turboview.exe")
    watch_dir = os.path.join(root, "test-watch")
    title = "test-watch - TurboView"
    out = os.path.join(root, "smoke")

    for f in os.listdir(watch_dir):
        if f.startswith("new_"):
            os.remove(os.path.join(watch_dir, f))

    proc = subprocess.Popen([exe, watch_dir])
    try:
        hwnd = wait_window(title)
        print("hwnd =", hwnd)
        if not hwnd:
            print("FAIL: 窗口未出现")
            return 1
        time.sleep(4.0)
        capture(hwnd, out + "_1_full.png")
        capture(hwnd, out + "_1_bar.png", band=64)

        # 新增一个文件 → 监控应触发刷新，列表 +1
        with open(os.path.join(watch_dir, "new_001.jpg"), "wb") as f:
            f.write(open(os.path.join(watch_dir, "img_000.jpg"), "rb").read())
        print("已新增 new_001.jpg，等待刷新…")
        time.sleep(3.0)
        capture(hwnd, out + "_2_bar.png", band=64)

        # 删除一个文件 → 列表 -1
        os.remove(os.path.join(watch_dir, "img_001.jpg"))
        print("已删除 img_001.jpg，等待刷新…")
        time.sleep(3.0)
        capture(hwnd, out + "_3_bar.png", band=64)
        capture(hwnd, out + "_3_full.png")
    finally:
        proc.terminate()
        try:
            proc.wait(5)
        except subprocess.TimeoutExpired:
            proc.kill()
        # 还原测试目录
        src = os.path.join(watch_dir, "new_001.jpg")
        if os.path.exists(src):
            os.remove(src)
        back = os.path.join(watch_dir, "img_001.jpg")
        if not os.path.exists(back):
            import shutil

            shutil.copy(os.path.join(root, "test-tree", "album_0000", "sub_0", "img_001.jpg"), back)
    print("done")
    return 0


if __name__ == "__main__":
    sys.exit(main())
