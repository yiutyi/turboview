//! Minimal BGRA canvas: the whole window is one u32 buffer, exactly what
//! softbuffer expects (`0x00RRGGBB`). No GPU, no compositor, no tiny-skia.
//!
//! Two design rules keep the CPU path cheap:
//!   1. everything draws through a clip rect, so a dirty-rect redraw only
//!      touches the pixels it must;
//!   2. the decoded image layer is pre-scaled once (the decoder already
//!      produces display-sized pixels), so presenting it is a plain copy.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    pub fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }

    pub fn right(&self) -> i32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }

    pub fn intersect(&self, other: Rect) -> Rect {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let r = self.right().min(other.right());
        let b = self.bottom().min(other.bottom());
        Rect::new(x, y, (r - x).max(0), (b - y).max(0))
    }

    pub fn union(&self, other: Rect) -> Rect {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return *self;
        }
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let r = self.right().max(other.right());
        let b = self.bottom().max(other.bottom());
        Rect::new(x, y, r - x, b - y)
    }
}

#[inline]
pub const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

pub struct Canvas {
    pub width: usize,
    pub height: usize,
    pub buf: Vec<u32>,
}

impl Canvas {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            buf: vec![0u32; width * height],
        }
    }

    pub fn resize(&mut self, width: usize, height: usize) {
        if self.width == width && self.height == height {
            return;
        }
        self.width = width;
        self.height = height;
        self.buf.clear();
        self.buf.resize(width * height, 0);
    }

    #[inline]
    fn idx(&self, x: i32, y: i32) -> Option<usize> {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            None
        } else {
            Some(y as usize * self.width + x as usize)
        }
    }

    pub fn clear(&mut self, color: u32) {
        self.buf.fill(color);
    }

    /// Fills the intersection of `rect` and `clip` with an opaque colour.
    pub fn fill(&mut self, rect: Rect, clip: Rect, color: u32) {
        let r = rect.intersect(clip).intersect(Rect::new(0, 0, self.width as i32, self.height as i32));
        if r.is_empty() {
            return;
        }
        let row_len = r.w as usize;
        for y in r.y..r.bottom() {
            let start = y as usize * self.width + r.x as usize;
            self.buf[start..start + row_len].fill(color);
        }
    }

    /// Blends a flat colour over the existing pixels (used for hover/selection).
    pub fn fill_alpha(&mut self, rect: Rect, clip: Rect, color: u32, alpha: u8) {
        let r = rect.intersect(clip).intersect(Rect::new(0, 0, self.width as i32, self.height as i32));
        if r.is_empty() || alpha == 0 {
            return;
        }
        let a = alpha as u32;
        for y in r.y..r.bottom() {
            let start = y as usize * self.width + r.x as usize;
            for px in &mut self.buf[start..start + r.w as usize] {
                *px = blend(*px, color, a);
            }
        }
    }

    /// Copies a pre-scaled image into `dst`, honouring the clip rect.
    /// `src` is already at `dst`'s size, rows are `dst.w` wide.
    pub fn blit(&mut self, dst: Rect, clip: Rect, src: &[u32], src_stride: usize) {
        let r = dst.intersect(clip).intersect(Rect::new(0, 0, self.width as i32, self.height as i32));
        if r.is_empty() {
            return;
        }
        let dx = (r.x - dst.x) as usize;
        let dy = (r.y - dst.y) as usize;
        let row_len = r.w as usize;
        for row in 0..r.h as usize {
            let sy = dy + row;
            let sx = sy * src_stride + dx;
            let dstart = (r.y as usize + row) * self.width + r.x as usize;
            self.buf[dstart..dstart + row_len].copy_from_slice(&src[sx..sx + row_len]);
        }
    }

    /// Draws an 8-bit coverage mask (fontdue glyph) in `color`.
    pub fn blend_mask(
        &mut self,
        x: i32,
        y: i32,
        mask: &[u8],
        mask_w: usize,
        clip: Rect,
        color: u32,
    ) {
        let h = if mask_w == 0 { 0 } else { mask.len() / mask_w };
        let rect = Rect::new(x, y, mask_w as i32, h as i32);
        let r = rect.intersect(clip).intersect(Rect::new(0, 0, self.width as i32, self.height as i32));
        if r.is_empty() {
            return;
        }
        for row in r.y..r.bottom() {
            let my = (row - y) as usize;
            for col in r.x..r.right() {
                let mx = (col - x) as usize;
                let a = mask[my * mask_w + mx] as u32;
                if a == 0 {
                    continue;
                }
                let idx = row as usize * self.width + col as usize;
                self.buf[idx] = blend(self.buf[idx], color, a);
            }
        }
    }
}

/// Alpha-blends `src` (opaque RGB) onto `dst`.
#[inline]
pub fn blend(dst: u32, src: u32, alpha: u32) -> u32 {
    if alpha >= 255 {
        return src;
    }
    let ia = 255 - alpha;
    let r = (((src >> 16) & 0xff) * alpha + ((dst >> 16) & 0xff) * ia) / 255;
    let g = (((src >> 8) & 0xff) * alpha + ((dst >> 8) & 0xff) * ia) / 255;
    let b = ((src & 0xff) * alpha + (dst & 0xff) * ia) / 255;
    (r << 16) | (g << 8) | b
}
