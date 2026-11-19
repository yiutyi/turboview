//! View state: zoom, pan, fit/actual, and the ROI rebuild that keeps CPU
//! work proportional to the viewport, not to the image.

use crate::canvas::Rect;
use crate::decode::Bitmap;
use image::RgbaImage;

pub struct Viewer {
    pub img: Option<RgbaImage>,
    /// Screen pixels per image pixel.
    pub zoom: f32,
    /// Image top-left corner, relative to the image area origin.
    pub offset: (f32, f32),
    pub view: Vec<u32>,
    pub view_w: u32,
    pub view_h: u32,
    pub view_rect: Rect,
    dirty: bool,
    last_area: Rect,
}

impl Viewer {
    pub fn new() -> Self {
        Self {
            img: None,
            zoom: 1.0,
            offset: (0.0, 0.0),
            view: Vec::new(),
            view_w: 0,
            view_h: 0,
            view_rect: Rect::new(0, 0, 0, 0),
            dirty: true,
            last_area: Rect::new(0, 0, 0, 0),
        }
    }

    pub fn image_size(&self) -> (f32, f32) {
        self.img
            .as_ref()
            .map(|i| (i.width() as f32, i.height() as f32))
            .unwrap_or((0.0, 0.0))
    }

    pub fn set_image(&mut self, bmp: Bitmap, area: Rect) {
        let img = RgbaImage::from_raw(bmp.width, bmp.height, bmp.rgba)
            .expect("decoded buffer size mismatch");
        self.img = Some(img);
        self.fit(area);
    }

    pub fn fit(&mut self, area: Rect) {
        let (iw, ih) = self.image_size();
        if iw <= 0.0 || ih <= 0.0 || area.w <= 0 || area.h <= 0 {
            return;
        }
        self.zoom = (area.w as f32 / iw).min(area.h as f32 / ih).min(8.0);
        self.offset = (
            (area.w as f32 - iw * self.zoom) / 2.0,
            (area.h as f32 - ih * self.zoom) / 2.0,
        );
        self.dirty = true;
    }

    pub fn actual(&mut self, area: Rect) {
        let (iw, ih) = self.image_size();
        if iw <= 0.0 {
            return;
        }
        self.zoom = 1.0;
        self.offset = (
            (area.w as f32 - iw) / 2.0,
            (area.h as f32 - ih) / 2.0,
        );
        self.dirty = true;
    }

    /// Zooms by `factor` keeping the image point under `cursor` fixed.
    pub fn zoom_at(&mut self, factor: f32, cursor: (f32, f32), area: Rect) {
        let (iw, ih) = self.image_size();
        if iw <= 0.0 {
            return;
        }
        let old = self.zoom;
        let new = (old * factor).clamp(0.02, 16.0);
        if (new - old).abs() < f32::EPSILON {
            return;
        }
        let local = (cursor.0 - self.offset.0, cursor.1 - self.offset.1);
        let image_pt = (local.0 / old, local.1 / old);
        self.offset = (
            cursor.0 - image_pt.0 * new,
            cursor.1 - image_pt.1 * new,
        );
        self.zoom = new;
        let _ = area;
        self.dirty = true;
    }

    pub fn pan(&mut self, dx: f32, dy: f32) {
        self.offset = (self.offset.0 + dx, self.offset.1 + dy);
        self.dirty = true;
    }

    pub fn invalidate(&mut self, area: Rect) {
        if area != self.last_area {
            self.last_area = area;
            self.dirty = true;
        }
    }

    pub fn zoom_pct(&self) -> i32 {
        (self.zoom * 100.0).round() as i32
    }

    /// Rebuilds the visible slice. Only the crop that lands on screen is
    /// resampled, so cost tracks the viewport size, never the image size.
    pub fn rebuild(&mut self, area: Rect) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        let Some(img) = &self.img else {
            self.view.clear();
            self.view_w = 0;
            self.view_h = 0;
            return;
        };
        let (iw, ih) = (img.width() as f32, img.height() as f32);
        let dst_w = (iw * self.zoom).round() as i32;
        let dst_h = (ih * self.zoom).round() as i32;
        let dst_x = area.x + self.offset.0.round() as i32;
        let dst_y = area.y + self.offset.1.round() as i32;
        let dst = Rect::new(dst_x, dst_y, dst_w, dst_h);
        let vis = dst.intersect(area);
        if vis.is_empty() {
            self.view.clear();
            self.view_w = 0;
            self.view_h = 0;
            return;
        }

        let sx_f = (vis.x - dst_x) as f32 / self.zoom;
        let sy_f = (vis.y - dst_y) as f32 / self.zoom;
        let sw_f = vis.w as f32 / self.zoom;
        let sh_f = vis.h as f32 / self.zoom;
        let x0 = sx_f.max(0.0);
        let y0 = sy_f.max(0.0);
        let x1 = (sx_f + sw_f).min(iw);
        let y1 = (sy_f + sh_f).min(ih);
        if x1 <= x0 || y1 <= y0 {
            self.view.clear();
            self.view_w = 0;
            self.view_h = 0;
            return;
        }

        self.view = resample(img, x0, y0, x1, y1, vis.w as u32, vis.h as u32);
        self.view_w = vis.w as u32;
        self.view_h = vis.h as u32;
        self.view_rect = vis;
    }
}

/// Nearest when 1:1, box average when shrinking, bilinear when enlarging.
/// Writes 0x00RRGGBB directly, so the blitter never touches channel maths.
fn resample(img: &RgbaImage, x0: f32, y0: f32, x1: f32, y1: f32, ow: u32, oh: u32) -> Vec<u32> {
    let raw = img.as_raw();
    let iw = img.width() as usize;
    let ih = img.height() as usize;
    let ow = ow as usize;
    let oh = oh as usize;
    let sx_step = (x1 - x0) / ow as f32;
    let sy_step = (y1 - y0) / oh as f32;
    let mut out = Vec::with_capacity(ow * oh);

    // 1:1 — straight nearest copy, no filtering at all.
    if (sx_step - 1.0).abs() <= 0.02 && (sy_step - 1.0).abs() <= 0.02 {
        for oy in 0..oh {
            let sy = ((y0 + oy as f32 * sy_step).round() as usize).min(ih - 1);
            let row = sy * iw;
            for ox in 0..ow {
                let sx = ((x0 + ox as f32 * sx_step).round() as usize).min(iw - 1);
                let i = (row + sx) * 4;
                out.push(((raw[i] as u32) << 16) | ((raw[i + 1] as u32) << 8) | raw[i + 2] as u32);
            }
        }
        return out;
    }

    if sx_step >= 1.5 || sy_step >= 1.5 {
        // Downscale: average the source footprint of each output pixel.
        // Column ranges are computed once, not per row.
        let mut cols: Vec<(usize, usize)> = Vec::with_capacity(ow);
        for ox in 0..ow {
            let s0 = (x0 + ox as f32 * sx_step).floor().max(0.0) as usize;
            let s1 = ((x0 + (ox + 1) as f32 * sx_step).ceil() as usize)
                .min(iw)
                .max(s0 + 1);
            cols.push((s0, s1));
        }
        for oy in 0..oh {
            let sy0 = (y0 + oy as f32 * sy_step).floor().max(0.0) as usize;
            let sy1 = ((y0 + (oy + 1) as f32 * sy_step).ceil() as usize)
                .min(ih)
                .max(sy0 + 1);
            for &(sx0, sx1) in &cols {
                let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
                for y in sy0..sy1 {
                    let row = y * iw;
                    for x in sx0..sx1 {
                        let i = (row + x) * 4;
                        r += raw[i] as u32;
                        g += raw[i + 1] as u32;
                        b += raw[i + 2] as u32;
                        n += 1;
                    }
                }
                let n = n.max(1);
                out.push(((r / n) << 16) | ((g / n) << 8) | (b / n));
            }
        }
    } else {
        // Magnify: integer bilinear with precomputed column weights.
        let mut cols: Vec<(usize, usize, u32)> = Vec::with_capacity(ow);
        for ox in 0..ow {
            let fx = (x0 + (ox as f32 + 0.5) * sx_step - 0.5).clamp(0.0, iw as f32 - 1.0);
            let x = fx.floor() as usize;
            let x2 = (x + 1).min(iw - 1);
            let w = ((fx - x as f32) * 256.0) as u32;
            cols.push((x, x2, w.min(256)));
        }
        for oy in 0..oh {
            let fy = (y0 + (oy as f32 + 0.5) * sy_step - 0.5).clamp(0.0, ih as f32 - 1.0);
            let y = fy.floor() as usize;
            let y2 = (y + 1).min(ih - 1);
            let wy = ((fy - y as f32) * 256.0) as u32;
            let wy1 = wy.min(256);
            let wy0 = 256 - wy1;
            let row0 = y * iw * 4;
            let row1 = y2 * iw * 4;
            for &(x, x2, wx1) in &cols {
                let wx0 = 256 - wx1;
                let mut px = 0u32;
                for c in 0..3 {
                    let top = raw[row0 + x * 4 + c] as u32 * wx0
                        + raw[row0 + x2 * 4 + c] as u32 * wx1;
                    let bot = raw[row1 + x * 4 + c] as u32 * wx0
                        + raw[row1 + x2 * 4 + c] as u32 * wx1;
                    let v = ((top >> 8) * wy0 + (bot >> 8) * wy1) >> 8;
                    px |= v.min(255) << (16 - c * 8);
                }
                out.push(px);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::Rect;

    fn solid(w: u32, h: u32, c: [u8; 4]) -> Bitmap {
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..w * h {
            rgba.extend_from_slice(&c);
        }
        Bitmap {
            width: w,
            height: h,
            rgba,
        }
    }

    #[test]
    fn fit_centers_image() {
        let mut v = Viewer::new();
        v.set_image(solid(200, 100, [255, 0, 0, 255]), Rect::new(0, 0, 400, 300));
        assert!((v.zoom - 2.0).abs() < 1e-6, "zoom {}", v.zoom);
        assert!((v.offset.1 - 50.0).abs() < 1e-6, "offset {:?}", v.offset);
    }

    #[test]
    fn zoom_keeps_cursor_anchor() {
        let mut v = Viewer::new();
        v.set_image(solid(200, 100, [1, 2, 3, 255]), Rect::new(0, 0, 400, 300));
        let before = (
            (200.0 - v.offset.0) / v.zoom,
            (150.0 - v.offset.1) / v.zoom,
        );
        v.zoom_at(2.0, (200.0, 150.0), Rect::new(0, 0, 400, 300));
        let after = (
            (200.0 - v.offset.0) / v.zoom,
            (150.0 - v.offset.1) / v.zoom,
        );
        assert!((before.0 - after.0).abs() < 0.01, "{before:?} vs {after:?}");
        assert!((before.1 - after.1).abs() < 0.01, "{before:?} vs {after:?}");
    }

    #[test]
    fn zoom_percentages_are_stable() {
        let mut v = Viewer::new();
        v.set_image(solid(100, 100, [0, 0, 0, 255]), Rect::new(0, 0, 100, 100));
        assert_eq!(v.zoom_pct(), 100);
        v.zoom_at(2.0, (0.0, 0.0), Rect::new(0, 0, 100, 100));
        assert_eq!(v.zoom_pct(), 200);
    }

    #[test]
    fn resample_one_to_one_copies_pixels() {
        let img = RgbaImage::from_raw(
            2,
            2,
            vec![
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255,
            ],
        )
        .unwrap();
        let out = resample(&img, 0.0, 0.0, 2.0, 2.0, 2, 2);
        assert_eq!(out, vec![0xFF0000, 0x00FF00, 0x0000FF, 0xFFFF00]);
    }

    #[test]
    fn rebuild_is_bounded_by_viewport() {
        let mut v = Viewer::new();
        v.set_image(solid(4000, 3000, [10, 20, 30, 255]), Rect::new(280, 0, 1000, 774));
        v.rebuild(Rect::new(280, 0, 1000, 774));
        assert!(!v.view.is_empty());
        assert!(v.view_w <= 1000, "view_w {}", v.view_w);
        assert!(v.view_h <= 774, "view_h {}", v.view_h);
        assert_eq!(v.view.len(), (v.view_w * v.view_h) as usize);
    }
}
