//! Text rendering with a bundled CJK subset.
//!
//! One font, one rasteriser, three platforms — no DirectWrite, no CoreText,
//! no fontconfig. Glyphs are cached per (char, pixel size), which is what
//! keeps a scrolling sidebar under a millisecond after the first pass.

use fontdue::{Font, FontSettings};
use std::collections::HashMap;

pub struct Glyph {
    pub xmin: i32,
    pub ymin: i32,
    pub width: usize,
    pub height: usize,
    pub advance: f32,
    pub bitmap: Vec<u8>,
}

pub struct TextRenderer {
    font: Font,
    cache: HashMap<(char, u32), Glyph>,
}

impl TextRenderer {
    pub fn new(bytes: &'static [u8]) -> anyhow::Result<Self> {
        let font = Font::from_bytes(bytes, FontSettings::default())
            .map_err(|e| anyhow::anyhow!("font parse failed: {e}"))?;
        Ok(Self {
            font,
            cache: HashMap::new(),
        })
    }

    pub fn ascent(&self, px: f32) -> f32 {
        self.font
            .horizontal_line_metrics(px)
            .map(|m| m.ascent)
            .unwrap_or(px * 0.8)
    }

    pub fn line_height(&self, px: f32) -> f32 {
        self.font
            .horizontal_line_metrics(px)
            .map(|m| m.ascent - m.descent + m.line_gap)
            .unwrap_or(px * 1.3)
    }

    fn glyph(&mut self, ch: char, px: f32) -> &Glyph {
        let key = (ch, px.round() as u32);
        if !self.cache.contains_key(&key) {
            let (metrics, bitmap) = self.font.rasterize(ch, px);
            let g = Glyph {
                xmin: metrics.xmin,
                ymin: metrics.ymin,
                width: metrics.width,
                height: metrics.height,
                advance: metrics.advance_width,
                bitmap,
            };
            self.cache.insert(key, g);
        }
        &self.cache[&key]
    }

    pub fn measure(&mut self, text: &str, px: f32) -> f32 {
        let mut w = 0.0;
        for ch in text.chars() {
            w += self.glyph(ch, px).advance;
        }
        w
    }

    /// Draws `text` with the baseline at `baseline`, clipped to `clip`.
    pub fn draw(
        &mut self,
        canvas: &mut super::canvas::Canvas,
        x: i32,
        baseline: i32,
        text: &str,
        px: f32,
        color: u32,
        clip: super::canvas::Rect,
    ) {
        let mut pen = x;
        for ch in text.chars() {
            let g = self.glyph(ch, px);
            if g.width > 0 && g.height > 0 {
                let gx = pen + g.xmin;
                let gy = baseline - g.ymin - g.height as i32;
                canvas.blend_mask(gx, gy, &g.bitmap, g.width, clip, color);
            }
            pen += g.advance as i32;
        }
    }

    /// Draws text horizontally clipped: returns the x position after the last
    /// fully drawn glyph.
    pub fn draw_ellipsized(
        &mut self,
        canvas: &mut super::canvas::Canvas,
        x: i32,
        baseline: i32,
        max_width: i32,
        text: &str,
        px: f32,
        color: u32,
        clip: super::canvas::Rect,
    ) {
        if self.measure(text, px) <= max_width as f32 {
            self.draw(canvas, x, baseline, text, px, color, clip);
            return;
        }
        let ellipsis = "…";
        let ew = self.measure(ellipsis, px);
        let mut w = 0.0f32;
        let mut pen = x;
        for ch in text.chars() {
            let adv = self.glyph(ch, px).advance;
            if w + adv + ew > max_width as f32 {
                break;
            }
            w += adv;
            let g = self.glyph(ch, px);
            if g.width > 0 && g.height > 0 {
                let gx = pen + g.xmin;
                let gy = baseline - g.ymin - g.height as i32;
                canvas.blend_mask(gx, gy, &g.bitmap, g.width, clip, color);
            }
            pen += adv as i32;
        }
        self.draw(canvas, pen, baseline, ellipsis, px, color, clip);
    }
}
