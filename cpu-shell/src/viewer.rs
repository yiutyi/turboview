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
    prev: Option<PrevFrame>,
}

/// Last rendered frame, used to turn a pan into a copy plus thin strips.
struct PrevFrame {
    rect: Rect,
    dst: (i32, i32),
    zoom: f32,
    buf: Vec<u32>,
    w: u32,
    h: u32,
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
            prev: None,
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
        self.prev = None;
        self.fit(area);
    }

    pub fn clear_image(&mut self) {
        self.img = None;
        self.prev = None;
        self.view.clear();
        self.view_w = 0;
        self.view_h = 0;
        self.dirty = true;
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
    ///
    /// When the previous frame is known and only the offset moved (the pan
    /// case), the overlapping area is copied and just the exposed strips are
    /// resampled.
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

        // Pan fast path: same zoom, overlapping viewport, shift + fill strips.
        // The copy is aligned by image content, not by screen position: the new
        // local pixel u shows the old local pixel u + (vis shift) - (content shift).
        let pan_path = self
            .prev
            .as_ref()
            .filter(|p| (p.zoom - self.zoom).abs() < 1e-4)
            .and_then(|p| {
                let dx = dst_x - p.dst.0;
                let dy = dst_y - p.dst.1;
                if dx == 0 && dy == 0 {
                    return None;
                }
                let u0 = 0.max(p.rect.x - vis.x + dx);
                let u1 = vis.w.min(p.rect.x + p.w as i32 - vis.x + dx);
                let v0 = 0.max(p.rect.y - vis.y + dy);
                let v1 = vis.h.min(p.rect.y + p.h as i32 - vis.y + dy);
                if u1 <= u0 || v1 <= v0 {
                    return None;
                }

                let mut buf = vec![0u32; (vis.w * vis.h) as usize];
                let n = (u1 - u0) as usize;
                for v in v0..v1 {
                    let sy = (vis.y + v - dy - p.rect.y) as usize;
                    let sx = (vis.x + u0 - dx - p.rect.x) as usize;
                    let si = sy * p.w as usize + sx;
                    let di = v as usize * vis.w as usize + u0 as usize;
                    buf[di..di + n].copy_from_slice(&p.buf[si..si + n]);
                }

                // Bands not covered by the shifted copy, in new-local coordinates.
                let bands = [
                    Rect::new(vis.x, vis.y, u0, vis.h),
                    Rect::new(vis.x + u1, vis.y, vis.w - u1, vis.h),
                    Rect::new(vis.x + u0, vis.y, u1 - u0, v0),
                    Rect::new(vis.x + u0, vis.y + v1, u1 - u0, vis.h - v1),
                ];
                for band in bands {
                    if band.is_empty() {
                        continue;
                    }
                    sample_into(
                        img,
                        self.zoom,
                        dst_x,
                        dst_y,
                        band,
                        &mut buf,
                        vis.w as usize,
                        (band.x - vis.x, band.y - vis.y),
                    );
                }
                Some(buf)
            });

        let buf = match pan_path {
            Some(b) => b,
            None => {
                let mut buf = vec![0u32; (vis.w * vis.h) as usize];
                sample_into(
                    img,
                    self.zoom,
                    dst_x,
                    dst_y,
                    vis,
                    &mut buf,
                    vis.w as usize,
                    (0, 0),
                );
                buf
            }
        };

        self.view = buf;
        self.view_w = vis.w as u32;
        self.view_h = vis.h as u32;
        self.view_rect = vis;
        // Kept for the next pan; a viewport-sized copy (~3 MB at 1080p).
        self.prev = Some(PrevFrame {
            rect: vis,
            dst: (dst_x, dst_y),
            zoom: self.zoom,
            buf: self.view.clone(),
            w: self.view_w,
            h: self.view_h,
        });
    }

    /// Swaps in a sharper decode while keeping the user's zoom and pan.
    pub fn replace_image(&mut self, bmp: Bitmap) {
        let Some(img) = RgbaImage::from_raw(bmp.width, bmp.height, bmp.rgba) else {
            return;
        };
        self.img = Some(img);
        self.prev = None;
        self.dirty = true;
    }
}

/// Sample a screen-space region of the image into a u32 (0x00RRGGBB) buffer.
///
/// The mapping is evaluated per *screen* pixel, `image = (screen - dst_origin) / zoom`,
/// never relative to the region. A viewport that only moved (same zoom, integer pan)
/// therefore maps the same screen pixel to the same image pixel, which is exactly what
/// makes the pan fast path bit-identical to a full rebuild.
///
/// Nearest when 1:1, box average when shrinking, bilinear when enlarging.
fn sample_into(
    img: &RgbaImage,
    zoom: f32,
    dst_x: i32,
    dst_y: i32,
    region: Rect,
    out: &mut [u32],
    out_w: usize,
    origin: (i32, i32),
) {
    if region.w <= 0 || region.h <= 0 {
        return;
    }
    let raw = img.as_raw();
    let iw = img.width() as usize;
    let ih = img.height() as usize;
    let ow = region.w as usize;
    let oh = region.h as usize;
    let step = 1.0 / zoom;

    if (step - 1.0).abs() <= 0.02 {
        // 1:1 — straight nearest copy, no filtering at all.
        for oy in 0..oh {
            let fy = (region.y + oy as i32 - dst_y) as f32 * step;
            let sy = (fy.round() as i32).clamp(0, ih as i32 - 1) as usize;
            let row = sy * iw;
            let drow = (origin.1 + oy as i32) as usize * out_w;
            for ox in 0..ow {
                let fx = (region.x + ox as i32 - dst_x) as f32 * step;
                let sx = (fx.round() as i32).clamp(0, iw as i32 - 1) as usize;
                let i = (row + sx) * 4;
                out[drow + (origin.0 + ox as i32) as usize] =
                    ((raw[i] as u32) << 16) | ((raw[i + 1] as u32) << 8) | raw[i + 2] as u32;
            }
        }
        return;
    }

    if step >= 1.5 {
        // Downscale: average the source footprint of each output pixel.
        // Column ranges are computed once, not per row.
        let mut cols: Vec<(usize, usize)> = Vec::with_capacity(ow);
        for ox in 0..ow {
            let fx = (region.x + ox as i32 - dst_x) as f32 * step;
            let s0 = fx.floor().max(0.0) as usize;
            let s1 = ((fx + step).ceil() as usize).min(iw).max(s0 + 1);
            cols.push((s0, s1));
        }
        for oy in 0..oh {
            let fy = (region.y + oy as i32 - dst_y) as f32 * step;
            let sy0 = fy.floor().max(0.0) as usize;
            let sy1 = ((fy + step).ceil() as usize).min(ih).max(sy0 + 1);
            let drow = (origin.1 + oy as i32) as usize * out_w;
            for (ox, &(sx0, sx1)) in cols.iter().enumerate() {
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
                out[drow + (origin.0 + ox as i32) as usize] =
                    ((r / n) << 16) | ((g / n) << 8) | (b / n);
            }
        }
    } else {
        // Magnify: integer bilinear with precomputed column weights.
        let mut cols: Vec<(usize, usize, u32)> = Vec::with_capacity(ow);
        for ox in 0..ow {
            let fx = ((region.x + ox as i32 - dst_x) as f32 * step).clamp(0.0, iw as f32 - 1.0);
            let x = fx.floor() as usize;
            let x2 = (x + 1).min(iw - 1);
            let w = ((fx - x as f32) * 256.0) as u32;
            cols.push((x, x2, w.min(256)));
        }
        for oy in 0..oh {
            let fy = ((region.y + oy as i32 - dst_y) as f32 * step).clamp(0.0, ih as f32 - 1.0);
            let y = fy.floor() as usize;
            let y2 = (y + 1).min(ih - 1);
            let wy = ((fy - y as f32) * 256.0) as u32;
            let wy1 = wy.min(256);
            let wy0 = 256 - wy1;
            let row0 = y * iw * 4;
            let row1 = y2 * iw * 4;
            let drow = (origin.1 + oy as i32) as usize * out_w;
            for (ox, &(x, x2, wx1)) in cols.iter().enumerate() {
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
                out[drow + (origin.0 + ox as i32) as usize] = px;
            }
        }
    }
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
        let mut out = vec![0u32; 4];
        sample_into(&img, 1.0, 0, 0, Rect::new(0, 0, 2, 2), &mut out, 2, (0, 0));
        assert_eq!(out, vec![0xFF0000, 0x00FF00, 0x0000FF, 0xFFFF00]);
    }

    #[test]
    fn pan_fast_path_matches_full_rebuild() {
        let mut rgba = Vec::with_capacity(400 * 300 * 4);
        for y in 0..300u32 {
            for x in 0..400u32 {
                rgba.extend_from_slice(&[(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8, 255]);
            }
        }
        let mut v = Viewer::new();
        v.set_image(
            Bitmap {
                width: 400,
                height: 300,
                rgba,
            },
            Rect::new(0, 0, 800, 600),
        );
        v.rebuild(Rect::new(0, 0, 800, 600));

        v.pan(37.0, 21.0);
        v.rebuild(Rect::new(0, 0, 800, 600));
        let shifted = v.view.clone();

        // Force the generic path from the same state.
        v.prev = None;
        v.dirty = true;
        v.rebuild(Rect::new(0, 0, 800, 600));
        assert_eq!(v.view, shifted, "pan fast path diverged from full rebuild");
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
