//! TurboView CPU shell — no GPU, no third-party UI framework.
//!
//! Decoding is the real thing: libjpeg-turbo (same source + SIMD as the
//! Windows build) for JPEG, `image` for the rest. Chrome is drawn into a
//! RGBA buffer and handed to softbuffer, so any machine that can show a
//! window can run this.
//!
//! Hooks:
//!   TV_OPEN=<path>        open a file / folder / zip at startup
//!   TV_DUMP=<file.png>    write the composed frame on first paint
//!   TV_DUMP_DIR=<dir>     write every TV_DEMO step as step-NN-*.png
//!   TV_DEMO=1             scripted tour: fit → 2x → 4x → pan → 1:1 → fit → next
//!   TV_BENCH=1            decode / rebuild / compose / present timings
//!   TV_EXIT_MS=<n>        exit after n ms (used by the headless tests)

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod canvas;
mod decode;
mod docs;
mod platform;
mod text;
mod ui;
mod viewer;

use anyhow::Result;
use canvas::{Canvas, Rect};
use docs::{FileEntry, ZipBook};
use platform::Platform;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use text::TextRenderer;
use ui::{Layout, Row};
use viewer::Viewer;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalSize};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Fullscreen, Window, WindowId};

const FONT_BYTES: &[u8] = include_bytes!("../assets/NotoSansSC-subset.ttf");
const SIDEBAR_W: i32 = 280;
const STATUS_H: i32 = 26;

#[derive(Debug)]
enum UserEvent {
    Exit,
}

enum Source {
    Dir {
        dir: PathBuf,
        files: Vec<FileEntry>,
        index: usize,
    },
    Zip {
        book: ZipBook,
        index: usize,
    },
    Empty,
}

impl Source {
    fn len(&self) -> usize {
        match self {
            Source::Dir { files, .. } => files.len(),
            Source::Zip { book, .. } => book.names.len(),
            Source::Empty => 0,
        }
    }

    fn index(&self) -> usize {
        match self {
            Source::Dir { index, .. } => *index,
            Source::Zip { index, .. } => *index,
            Source::Empty => 0,
        }
    }

    fn set_index(&mut self, i: usize) {
        match self {
            Source::Dir { index, .. } => *index = i,
            Source::Zip { index, .. } => *index = i,
            Source::Empty => {}
        }
    }

    fn label(&self) -> String {
        match self {
            Source::Dir { dir, .. } => dir
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| dir.display().to_string()),
            Source::Zip { book, .. } => book
                .path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default(),
            Source::Empty => "未打开".into(),
        }
    }

    fn names(&self) -> Vec<String> {
        match self {
            Source::Dir { files, .. } => files.iter().map(|f| f.name.clone()).collect(),
            Source::Zip { book, .. } => book.names.clone(),
            Source::Empty => Vec::new(),
        }
    }

    fn current_name(&self) -> String {
        self.names().get(self.index()).cloned().unwrap_or_default()
    }

    fn read_bytes(&self) -> Result<Vec<u8>> {
        match self {
            Source::Dir { files, index, .. } => {
                let f = files.get(*index).ok_or_else(|| anyhow::anyhow!("no file"))?;
                Ok(std::fs::read(&f.path)?)
            }
            Source::Zip { book, index } => {
                let name = book
                    .names
                    .get(*index)
                    .ok_or_else(|| anyhow::anyhow!("no entry"))?;
                book.read(name)
            }
            Source::Empty => anyhow::bail!("no source"),
        }
    }

    fn ext(&self) -> String {
        Path::new(&self.current_name())
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default()
    }

    fn current_path(&self) -> Option<PathBuf> {
        match self {
            Source::Dir { files, index, .. } => files.get(*index).map(|f| f.path.clone()),
            _ => None,
        }
    }
}

struct App {
    window: Option<Arc<Window>>,
    context: Option<softbuffer::Context<Arc<Window>>>,
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,

    canvas: Canvas,
    text: TextRenderer,
    layout: Layout,
    platform: Box<dyn Platform>,
    viewer: Viewer,
    source: Source,

    rows: Vec<Row>,
    scroll: i32,
    hover: Option<usize>,
    hover_cursor: Option<(f32, f32)>,
    dragging: Option<(f64, f64)>,
    pending_open: Option<String>,

    status_left: String,
    status_mid: String,
    status_right: String,

    dirty: Option<Rect>,
    first_frame: bool,
    demo_ran: bool,
    fullscreen: bool,
    last_decode_ms: f64,
    last_rebuild_ms: f64,
}

impl App {
    fn new() -> Result<Self> {
        let text = TextRenderer::new(FONT_BYTES)?;
        Ok(Self {
            window: None,
            context: None,
            surface: None,
            canvas: Canvas::new(1280, 800),
            text,
            layout: Layout {
                width: 1280,
                height: 800,
                sidebar_w: SIDEBAR_W,
                status_h: STATUS_H,
            },
            platform: platform::current(),
            viewer: Viewer::new(),
            source: Source::Empty,
            rows: Vec::new(),
            scroll: 0,
            hover: None,
            hover_cursor: None,
            dragging: None,
            pending_open: None,
            status_left: "TurboView CPU".into(),
            status_mid: String::new(),
            status_right: "CPU 渲染 · 无 GPU".into(),
            dirty: None,
            first_frame: true,
            demo_ran: false,
            fullscreen: false,
            last_decode_ms: 0.0,
            last_rebuild_ms: 0.0,
        })
    }

    fn area(&self) -> Rect {
        self.layout.image_area()
    }

    fn open_path(&mut self, path: &Path) {
        match docs::classify(path) {
            docs::SourceKind::Dir => {
                let files = docs::list_images(path);
                if files.is_empty() {
                    self.status_right = "目录里没有图片".into();
                    return;
                }
                self.source = Source::Dir {
                    dir: path.to_path_buf(),
                    files,
                    index: 0,
                };
            }
            docs::SourceKind::Zip => match ZipBook::open(path) {
                Ok(book) if !book.names.is_empty() => {
                    self.source = Source::Zip { book, index: 0 };
                }
                Ok(_) => {
                    self.status_right = "压缩包里没有图片".into();
                    return;
                }
                Err(e) => {
                    self.status_right = format!("{e}");
                    return;
                }
            },
            docs::SourceKind::Single => {
                let dir = path
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| PathBuf::from("."));
                let files = docs::list_images(&dir);
                let index = files.iter().position(|f| f.path == *path).unwrap_or(0);
                self.source = Source::Dir { dir, files, index };
            }
        }
        self.load_current();
    }

    fn load_current(&mut self) {
        if self.source.len() == 0 {
            return;
        }
        let t0 = Instant::now();
        let max_dim = self.decode_max_dim();
        let bytes = match self.source.read_bytes() {
            Ok(b) => b,
            Err(e) => {
                self.status_right = format!("{e:#}");
                return;
            }
        };
        let ext = self.source.ext();
        match decode::decode_bytes(&bytes, Some(&ext), max_dim, false) {
            Ok(bmp) => {
                self.last_decode_ms = t0.elapsed().as_secs_f64() * 1000.0;
                let (w, h) = (bmp.width, bmp.height);
                self.viewer.set_image(bmp, self.area());
                self.refresh_meta();
                self.refresh_rows();
                self.status_mid = format!(
                    "{} × {}   ·   {} / {}   ·   {}%",
                    w,
                    h,
                    self.source.index() + 1,
                    self.source.len(),
                    self.viewer.zoom_pct()
                );
                self.dirty = Some(Rect::new(0, 0, self.layout.width, self.layout.height));
                if let Some(w) = &self.window {
                    let name = self.source.current_name();
                    w.set_title(&format!("{name} — TurboView CPU"));
                    w.request_redraw();
                }
            }
            Err(e) => {
                self.status_right = format!("解码失败: {e:#}");
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
        }
    }

    fn decode_max_dim(&self) -> u32 {
        let area = self.area();
        let longest = area.w.max(area.h).max(64) as u32;
        (longest * 2).clamp(1024, 4096)
    }

    fn refresh_meta(&mut self) {
        let (w, h) = self.viewer.image_size();
        self.status_left = self.source.current_name();
        self.status_mid = format!(
            "{} × {}   ·   {} / {}   ·   {}%",
            w as u32,
            h as u32,
            self.source.index() + 1,
            self.source.len(),
            self.viewer.zoom_pct()
        );
        self.status_right = format!("CPU 渲染 · 解码 {:.1} ms", self.last_decode_ms);
        // Status bar text changed, so the chrome has to be redrawn too.
        self.dirty = Some(Rect::new(0, 0, self.layout.width, self.layout.height));
    }

    fn refresh_rows(&mut self) {
        let names = self.source.names();
        let index = self.source.index();
        self.rows = names
            .into_iter()
            .enumerate()
            .map(|(i, name)| Row {
                level: 0,
                label: name,
                count: 0,
                expanded: None,
                selected: i == index,
            })
            .collect();
    }

    fn step(&mut self, delta: i32) {
        let len = self.source.len() as i32;
        if len == 0 {
            return;
        }
        let cur = self.source.index() as i32;
        let next = (cur + delta).rem_euclid(len);
        self.source.set_index(next as usize);
        self.scroll_to_selected();
        self.load_current();
    }

    fn scroll_to_selected(&mut self) {
        let index = self.source.index() as i32;
        let row_h = ui::ROW_H;
        let view_h = self.layout.sidebar().h - ui::HEADER_H - 32;
        let top = index * row_h;
        if top < self.scroll {
            self.scroll = top;
        } else if top + row_h > self.scroll + view_h {
            self.scroll = top + row_h - view_h;
        }
        self.dirty = Some(Rect::new(0, 0, self.layout.sidebar_w, self.layout.sidebar().h));
    }

    fn on_resize(&mut self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 {
            return;
        }
        if let Some(surface) = &mut self.surface {
            let _ = surface.resize(
                NonZeroU32::new(size.width).unwrap(),
                NonZeroU32::new(size.height).unwrap(),
            );
        }
        self.layout.width = size.width as i32;
        self.layout.height = size.height as i32;
        self.canvas.resize(size.width as usize, size.height as usize);
        self.viewer.invalidate(self.area());
        self.viewer.fit(self.area());
        self.dirty = Some(Rect::new(0, 0, self.layout.width, self.layout.height));
    }

    fn compose(&mut self, clip: Rect) {
        let full = Rect::new(0, 0, self.layout.width, self.layout.height);
        self.canvas.fill(full, clip, ui::BG);

        let area = self.area().intersect(clip);
        if !area.is_empty() {
            let t0 = Instant::now();
            self.viewer.rebuild(self.area());
            self.last_rebuild_ms = t0.elapsed().as_secs_f64() * 1000.0;
            if !self.viewer.view.is_empty() {
                self.canvas.blit(
                    self.viewer.view_rect,
                    clip,
                    &self.viewer.view,
                    self.viewer.view_w as usize,
                );
            }
        }

        let title = self.source.label();
        ui::draw_sidebar(
            &mut self.canvas,
            &mut self.text,
            clip,
            &self.layout,
            &self.rows,
            self.scroll,
            self.hover,
            &title,
        );
        ui::draw_status(
            &mut self.canvas,
            &mut self.text,
            clip,
            &self.layout,
            &self.status_left,
            &self.status_mid,
            &self.status_right,
        );
    }

    fn present(&mut self) {
        let Some(surface) = &mut self.surface else { return };
        let Ok(mut buffer) = surface.buffer_mut() else { return };
        let n = buffer.len().min(self.canvas.buf.len());
        buffer[..n].copy_from_slice(&self.canvas.buf[..n]);
        let _ = buffer.present();
    }

    fn render(&mut self) {
        let full = Rect::new(0, 0, self.layout.width, self.layout.height);
        let clip = self.dirty.take().unwrap_or(full);
        self.compose(clip);
        self.present();
        if self.first_frame {
            self.first_frame = false;
            self.dump_png("first");
        }
    }

    fn dump_png(&self, tag: &str) {
        let dir = std::env::var("TV_DUMP_DIR").ok();
        let target = if let Some(dir) = dir {
            let n = std::fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0);
            Path::new(&dir).join(format!("step-{:02}-{}.png", n, tag))
        } else if let Ok(file) = std::env::var("TV_DUMP") {
            PathBuf::from(file)
        } else {
            return;
        };
        let mut bytes = Vec::with_capacity(self.canvas.buf.len() * 4);
        for px in &self.canvas.buf {
            bytes.push(((px >> 16) & 0xff) as u8);
            bytes.push(((px >> 8) & 0xff) as u8);
            bytes.push((px & 0xff) as u8);
            bytes.push(255);
        }
        if let Some(img) = image::RgbaImage::from_raw(
            self.canvas.width as u32,
            self.canvas.height as u32,
            bytes,
        ) {
            if img.save(&target).is_ok() {
                println!("dump {}", target.display());
            }
        }
    }

    fn toggle_fullscreen(&mut self) {
        let Some(window) = &self.window else { return };
        self.fullscreen = !self.fullscreen;
        if self.fullscreen {
            window.set_fullscreen(Some(Fullscreen::Borderless(None)));
        } else {
            window.set_fullscreen(None);
        }
    }

    fn zoom(&mut self, factor: f32, cursor: Option<(f32, f32)>) {
        let area = self.area();
        let (cx, cy) = cursor.unwrap_or((area.w as f32 / 2.0, area.h as f32 / 2.0));
        self.viewer.zoom_at(factor, (cx, cy), area);
        self.refresh_meta();
        self.dirty = Some(area);
    }

    fn run_demo(&mut self) {
        println!("--- demo ---");
        let area = self.area();
        let steps: Vec<(&str, Box<dyn Fn(&mut App)>)> = vec![
            ("fit", Box::new(|a: &mut App| a.viewer.fit(a.area()))),
            (
                "zoom2x",
                Box::new(|a: &mut App| {
                    let c = (a.area().w as f32 * 0.6, a.area().h as f32 * 0.5);
                    a.zoom(2.0, Some(c))
                }),
            ),
            (
                "zoom4x",
                Box::new(|a: &mut App| {
                    let c = (a.area().w as f32 * 0.6, a.area().h as f32 * 0.5);
                    a.zoom(2.0, Some(c))
                }),
            ),
            ("pan", Box::new(|a: &mut App| a.viewer.pan(-160.0, -110.0))),
            ("actual", Box::new(|a: &mut App| a.viewer.actual(a.area()))),
            ("fit2", Box::new(|a: &mut App| a.viewer.fit(a.area()))),
            ("next", Box::new(|a: &mut App| a.step(1))),
        ];
        for (name, f) in steps {
            f(self);
            let full = Rect::new(0, 0, self.layout.width, self.layout.height);
            self.dirty = Some(full);
            self.compose(full);
            println!(
                "{:<8} zoom {:>4}%  decode {:>6.1} ms  rebuild {:>6.2} ms",
                name,
                self.viewer.zoom_pct(),
                self.last_decode_ms,
                self.last_rebuild_ms
            );
            self.dump_png(name);
        }
        println!("--- demo end ---");
    }

    fn run_bench(&mut self) {
        println!(
            "\n=== cost breakdown ({}x{}) ===",
            self.layout.width, self.layout.height
        );
        let area = self.area();
        let t = Instant::now();
        for _ in 0..20 {
            self.viewer.zoom = 1.0;
            self.viewer.pan(0.0, 0.0);
            self.viewer.rebuild(area);
            self.dirty = Some(area);
            self.compose(area);
        }
        println!(
            "rebuild + compose @100%   {:>7.2} ms",
            t.elapsed().as_secs_f64() * 1000.0 / 20.0
        );

        let t = Instant::now();
        for _ in 0..30 {
            self.compose(area);
        }
        println!(
            "compose only              {:>7.2} ms",
            t.elapsed().as_secs_f64() * 1000.0 / 30.0
        );

        let t = Instant::now();
        for _ in 0..30 {
            self.compose(area);
            self.present();
        }
        println!(
            "compose + present         {:>7.2} ms",
            t.elapsed().as_secs_f64() * 1000.0 / 30.0
        );
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = Window::default_attributes()
            .with_title("TurboView · CPU")
            .with_inner_size(LogicalSize::new(1280.0, 800.0))
            .with_min_inner_size(LogicalSize::new(640.0, 400.0));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                eprintln!("window creation failed: {e}");
                event_loop.exit();
                return;
            }
        };
        let context = match softbuffer::Context::new(window.clone()) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("softbuffer context failed: {e}");
                event_loop.exit();
                return;
            }
        };
        let surface = match softbuffer::Surface::new(&context, window.clone()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("softbuffer surface failed: {e}");
                event_loop.exit();
                return;
            }
        };
        self.window = Some(window.clone());
        self.context = Some(context);
        self.surface = Some(surface);

        let size = window.inner_size();
        self.on_resize(size);

        if let Some(path) = self.pending_open.clone() {
            self.open_path(Path::new(&path));
        } else if let Ok(path) = std::env::var("TV_OPEN") {
            self.open_path(Path::new(&path));
        }
        println!(
            "window up {}x{}  platform {}  renderer softbuffer",
            self.layout.width,
            self.layout.height,
            self.platform.name()
        );
        window.request_redraw();
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Exit => event_loop.exit(),
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                self.on_resize(size);
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                self.render();
                if !self.demo_ran && std::env::var_os("TV_DEMO").is_some() {
                    self.demo_ran = true;
                    self.run_demo();
                    if std::env::var_os("TV_BENCH").is_some() {
                        self.run_bench();
                    }
                    event_loop.exit();
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed {
                    return;
                }
                match &event.logical_key {
                    Key::Named(NamedKey::ArrowRight) | Key::Named(NamedKey::Space) => self.step(1),
                    Key::Named(NamedKey::ArrowLeft) => self.step(-1),
                    Key::Named(NamedKey::Escape) => {
                        if self.fullscreen {
                            self.toggle_fullscreen();
                        } else {
                            event_loop.exit();
                        }
                    }
                    Key::Named(NamedKey::F11) => self.toggle_fullscreen(),
                    Key::Character(s) => match s.as_str() {
                        "+" | "=" => self.zoom(1.25, None),
                        "-" => self.zoom(0.8, None),
                        "0" => {
                            self.viewer.fit(self.area());
                            self.refresh_meta();
                            self.dirty = Some(self.area());
                        }
                        "1" => {
                            self.viewer.actual(self.area());
                            self.refresh_meta();
                            self.dirty = Some(self.area());
                        }
                        "e" | "E" => {
                            if let Some(p) = self.source.current_path() {
                                self.platform.reveal(&p);
                            }
                        }
                        "c" | "C" => {
                            if let Some(p) = self.source.current_path() {
                                let ok = self.platform.copy_text(&p.display().to_string());
                                self.status_right =
                                    if ok { "路径已复制".into() } else { "复制失败".into() };
                            }
                        }
                        "r" | "R" => {
                            if let Source::Dir { dir, .. } = &self.source {
                                let d = dir.clone();
                                self.open_path(&d);
                            }
                        }
                        _ => {}
                    },
                    _ => {}
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let factor = match delta {
                    MouseScrollDelta::LineDelta(_, y) => (1.15f32).powf(y),
                    MouseScrollDelta::PixelDelta(p) => (1.15f32).powf(p.y as f32 / 50.0),
                };
                let cursor = self.hover_cursor.unwrap_or_else(|| {
                    let area = self.area();
                    (area.w as f32 / 2.0, area.h as f32 / 2.0)
                });
                self.zoom(factor, Some(cursor));
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let x = position.x as f32 - self.layout.sidebar_w as f32;
                let y = position.y as f32;
                self.hover_cursor = Some((x, y));
                if let Some((lx, ly)) = self.dragging {
                    let dx = position.x - lx;
                    let dy = position.y - ly;
                    self.dragging = Some((position.x, position.y));
                    self.viewer.pan(dx as f32, dy as f32);
                    self.dirty = Some(self.area());
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                    return;
                }
                let py = position.y as i32;
                let list_top = ui::HEADER_H + 8;
                let idx = if py >= list_top && (position.x as i32) < self.layout.sidebar_w {
                    let i = (py - list_top + self.scroll) / ui::ROW_H;
                    if i >= 0 && (i as usize) < self.rows.len() {
                        Some(i as usize)
                    } else {
                        None
                    }
                } else {
                    None
                };
                if idx != self.hover {
                    self.hover = idx;
                    self.dirty = Some(Rect::new(
                        0,
                        ui::HEADER_H,
                        self.layout.sidebar_w,
                        self.layout.sidebar().h - ui::HEADER_H,
                    ));
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if button == MouseButton::Left {
                    match state {
                        ElementState::Pressed => {
                            if let Some(row) = self.hover {
                                if row < self.rows.len() && row != self.source.index() {
                                    self.source.set_index(row);
                                    self.scroll_to_selected();
                                    self.load_current();
                                    return;
                                }
                            }
                            if let Some(c) = self.hover_cursor {
                                self.dragging =
                                    Some((c.0 as f64 + self.layout.sidebar_w as f64, c.1 as f64));
                            }
                        }
                        ElementState::Released => self.dragging = None,
                    }
                }
            }
            WindowEvent::DroppedFile(path) => {
                self.open_path(&path);
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            _ => {}
        }
    }
}

fn main() -> Result<()> {
    println!("TurboView CPU shell — libjpeg-turbo + softbuffer + bundled font");
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Wait);

    if let Some(ms) = std::env::var("TV_EXIT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        let proxy = event_loop.create_proxy();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(ms));
            let _ = proxy.send_event(UserEvent::Exit);
        });
    }

    let mut app = App::new()?;
    if let Some(path) = std::env::args().nth(1) {
        app.pending_open = Some(path);
    }
    event_loop.run_app(&mut app)?;
    Ok(())
}
