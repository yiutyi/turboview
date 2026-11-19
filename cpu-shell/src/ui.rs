//! Chrome drawing: sidebar + status bar, clipped so partial redraws stay cheap.

use crate::canvas::{rgb, Canvas, Rect};
use crate::text::TextRenderer;

pub const BG: u32 = rgb(0x1e, 0x1e, 0x1e);
pub const SIDEBAR_BG: u32 = rgb(0x25, 0x25, 0x26);
pub const HEADER_BG: u32 = rgb(0x2d, 0x2d, 0x30);
pub const STATUS_BG: u32 = rgb(0x1b, 0x1b, 0x1c);
pub const TEXT: u32 = rgb(0xe6, 0xe6, 0xe6);
pub const TEXT_DIM: u32 = rgb(0xb0, 0xb0, 0xb0);
pub const ACCENT: u32 = rgb(0x4a, 0x9e, 0xff);
pub const SEPARATOR: u32 = rgb(0x3a, 0x3a, 0x3c);
pub const HOVER: u32 = rgb(0x2f, 0x2f, 0x33);

pub const HEADER_H: i32 = 38;
pub const ROW_H: i32 = 25;
pub const SIDEBAR_PAD: i32 = 10;
pub const INDENT: i32 = 12;

pub struct Row {
    pub level: i32,
    pub label: String,
    pub count: i32,
    /// None = leaf (no chevron), Some(true) = expanded, Some(false) = collapsed
    pub expanded: Option<bool>,
    pub selected: bool,
}

pub struct Layout {
    pub width: i32,
    pub height: i32,
    pub sidebar_w: i32,
    pub status_h: i32,
}

impl Layout {
    pub fn sidebar(&self) -> Rect {
        Rect::new(0, 0, self.sidebar_w, self.height - self.status_h)
    }

    pub fn image_area(&self) -> Rect {
        Rect::new(
            self.sidebar_w,
            0,
            self.width - self.sidebar_w,
            self.height - self.status_h,
        )
    }

    pub fn status(&self) -> Rect {
        Rect::new(0, self.height - self.status_h, self.width, self.status_h)
    }
}

pub fn draw_sidebar(
    canvas: &mut Canvas,
    text: &mut TextRenderer,
    clip: Rect,
    layout: &Layout,
    rows: &[Row],
    scroll: i32,
    hover: Option<usize>,
    title: &str,
) {
    let area = layout.sidebar().intersect(clip);
    if area.is_empty() {
        return;
    }
    canvas.fill(layout.sidebar(), clip, SIDEBAR_BG);
    canvas.fill(
        Rect::new(0, 0, layout.sidebar_w, HEADER_H),
        clip,
        HEADER_BG,
    );

    // header: current source name + two text hints
    let baseline = HEADER_H / 2 + 6;
    let hint_w = text.measure("刷新", 11.5) as i32 + 12;
    text.draw_ellipsized(
        canvas,
        SIDEBAR_PAD,
        baseline,
        layout.sidebar_w - SIDEBAR_PAD * 2 - hint_w,
        title,
        14.0,
        TEXT,
        clip,
    );
    let buttons = ["刷新"];
    let mut bx = layout.sidebar_w - SIDEBAR_PAD;
    for b in buttons.iter().rev() {
        let w = text.measure(b, 11.5) as i32;
        bx -= w;
        text.draw(canvas, bx, baseline, b, 11.5, TEXT_DIM, clip);
        bx -= 12;
    }

    let list_top = HEADER_H + 8;
    let list_bottom = layout.sidebar().h - 24;
    let visible = (list_bottom - list_top) / ROW_H;
    let first = (scroll / ROW_H).max(0) as usize;
    let offset = scroll % ROW_H;

    let list_clip = Rect::new(0, list_top, layout.sidebar_w, list_bottom - list_top).intersect(clip);

    for i in 0..(visible + 2) as usize {
        let idx = first + i;
        let Some(row) = rows.get(idx) else { break };
        let y = list_top - offset + i as i32 * ROW_H;
        let row_rect = Rect::new(0, y, layout.sidebar_w, ROW_H).intersect(list_clip);
        if row_rect.is_empty() {
            continue;
        }
        if row.selected {
            canvas.fill_alpha(row_rect, clip, ACCENT, 40);
        } else if hover == Some(idx) {
            canvas.fill(row_rect, clip, HOVER);
        }

        let x = SIDEBAR_PAD + row.level * INDENT;
        if let Some(expanded) = row.expanded {
            let sign = if expanded { "▾" } else { "▸" };
            text.draw(canvas, x, y + ROW_H / 2 + 5, sign, 11.0, TEXT_DIM, clip);
        }
        let label_x = x + if row.expanded.is_some() { 14 } else { 0 };
        let count_w = if row.count > 0 {
            text.measure(&row.count.to_string(), 11.5) as i32 + 12
        } else {
            0
        };
        let label_max = layout.sidebar_w - label_x - SIDEBAR_PAD - count_w;
        let color = if row.selected { TEXT } else { TEXT_DIM };
        text.draw_ellipsized(
            canvas,
            label_x,
            y + ROW_H / 2 + 5,
            label_max,
            &row.label,
            13.0,
            color,
            list_clip,
        );
        if row.count > 0 {
            let s = row.count.to_string();
            let w = text.measure(&s, 11.0) as i32;
            text.draw(
                canvas,
                layout.sidebar_w - SIDEBAR_PAD - w,
                y + ROW_H / 2 + 5,
                &s,
                11.5,
                TEXT_DIM,
                list_clip,
            );
        }
    }

    canvas.fill(
        Rect::new(layout.sidebar_w - 1, 0, 1, layout.sidebar().h),
        clip,
        SEPARATOR,
    );

    // separator + bottom hint
    let sep_y = layout.sidebar().h - 22;
    canvas.fill(Rect::new(0, sep_y, layout.sidebar_w, 1), clip, SEPARATOR);
    text.draw(
        canvas,
        SIDEBAR_PAD,
        sep_y + 15,
        "拖入图片文件即可打开",
        11.0,
        TEXT_DIM,
        clip,
    );
}

pub fn draw_status(
    canvas: &mut Canvas,
    text: &mut TextRenderer,
    clip: Rect,
    layout: &Layout,
    left: &str,
    middle: &str,
    right: &str,
) {
    let area = layout.status().intersect(clip);
    if area.is_empty() {
        return;
    }
    canvas.fill(layout.status(), clip, STATUS_BG);
    canvas.fill(
        Rect::new(0, layout.status().y, layout.width, 1),
        clip,
        SEPARATOR,
    );

    let baseline = layout.status().y + layout.status_h - 8;
    text.draw(canvas, SIDEBAR_PAD, baseline, left, 12.0, TEXT, clip);
    text.draw(
        canvas,
        SIDEBAR_PAD + 280,
        baseline,
        middle,
        11.5,
        TEXT_DIM,
        clip,
    );
    let rw = text.measure(right, 12.0) as i32;
    text.draw(
        canvas,
        layout.width - SIDEBAR_PAD - rw,
        baseline,
        right,
        11.5,
        ACCENT,
        clip,
    );
}
