//! Draws the whole row of covers as one picture, in pixels: the cover in the
//! centre flat, its neighbours turned towards it (the Cover Flow "fold") and
//! stacked outwards, nearer ones over farther ones. Positions, sizes and the
//! amount each cover is turned are continuous, so a row that glides between
//! albums moves smoothly rather than in terminal cells.

use image::{DynamicImage, Rgba, RgbaImage, imageops::FilterType};
use std::collections::HashMap;

/// Height of a fully turned side cover compared with the centre one
const SIDE_HEIGHT: f32 = 0.86;
/// Width of a fully turned cover compared with a flat one
const SQUEEZE: f32 = 0.42;
/// How much each side cover hides of the one behind it
const OVERLAP: f32 = 0.45;
/// Height of a turned cover's far edge compared with its near edge
const FAR_EDGE: f32 = 0.72;
/// Brightness of a turned cover's far edge compared with its near edge
const FAR_SHADE: f32 = 0.55;
/// Space between the centre cover and the first side one, of the cover size
const GAP: f32 = 0.04;
/// Most side covers drawn on each side
pub const MAX_SIDE: usize = 10;

/// A cover scaled to the canvas, plus a half-size copy for small side covers
struct Scaled {
    full: RgbaImage,
    half: RgbaImage,
}

#[derive(Default)]
pub struct Scene {
    size: (u32, u32),
    scaled: HashMap<usize, Scaled>,
}

/// Where a cover was drawn, horizontally in canvas pixels, for clicks
pub struct Hit {
    pub left: f32,
    pub right: f32,
    pub index: usize,
}

impl Scene {
    /// Sets the canvas size; covers scaled for the old size are dropped.
    pub fn resize(&mut self, width: u32, height: u32) {
        if self.size != (width, height) {
            self.size = (width, height);
            self.scaled.clear();
        }
    }

    /// Drops a cover's scaled copies (it changed or went out of reach)
    pub fn forget(&mut self, index: usize) {
        self.scaled.remove(&index);
    }

    /// Draws the row with `pos` (fractional: the row can be between albums)
    /// in the centre. `covers` are the loaded covers by album index.
    pub fn compose(&mut self, covers: &HashMap<usize, Option<DynamicImage>>, pos: f32, count: usize) -> (RgbaImage, Vec<Hit>) {
        let (width, height) = self.size;
        let mut canvas = RgbaImage::new(width, height);
        let mut hits = Vec::new();
        if width == 0 || height == 0 || count == 0 {
            return (canvas, hits);
        }
        let size = height as f32;
        let side_w = size * SIDE_HEIGHT * SQUEEZE;
        let first = size / 2.0 + size * GAP + side_w / 2.0;
        let spacing = side_w * (1.0 - OVERLAP);
        let mid = width as f32 / 2.0;

        let lo = (pos - MAX_SIDE as f32 - 1.0).floor().max(0.0) as usize;
        let hi = ((pos + MAX_SIDE as f32 + 1.0).ceil() as usize).min(count - 1);
        // far ones first, so nearer covers are drawn over them
        let mut order: Vec<usize> = (lo..=hi).collect();
        order.sort_by(|a, b| (*b as f32 - pos).abs().total_cmp(&(*a as f32 - pos).abs()));

        for index in order {
            let d = index as f32 - pos;
            let far = d.abs();
            let turn = smoothstep(far.min(1.0));
            let h = size * (1.0 - (1.0 - SIDE_HEIGHT) * turn);
            let w = h * (1.0 - (1.0 - SQUEEZE) * turn);
            let offset = if far <= 1.0 { far * first } else { first + (far - 1.0) * spacing };
            let left = mid + offset.copysign(d) - w / 2.0;
            if left + w < 0.0 || left > width as f32 {
                continue;
            }
            hits.push(Hit { left, right: left + w, index });
            let Some(Some(cover)) = covers.get(&index) else { continue };
            let scaled = self.scaled.entry(index).or_insert_with(|| scale(cover, height));
            let source = if h < size * 0.6 { &scaled.half } else { &scaled.full };
            // dimmer as it turns, and further out
            let light = 1.0 - 0.1 * turn - 0.08 * (far - 1.0).clamp(0.0, 4.0);
            draw(&mut canvas, source, Placement { left, top: (size - h) / 2.0, w, h, left_side: d < 0.0, turn, light });
        }
        (canvas, hits)
    }
}

fn smoothstep(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}

fn scale(cover: &DynamicImage, size: u32) -> Scaled {
    let full = cover.resize_exact(size, size, FilterType::Triangle).to_rgba8();
    let half = image::imageops::resize(&full, size.div_ceil(2), size.div_ceil(2), FilterType::Triangle);
    Scaled { full, half }
}

struct Placement {
    left: f32,
    top: f32,
    w: f32,
    h: f32,
    /// left of the centre: its near edge is on its right
    left_side: bool,
    /// 0 flat … 1 fully turned
    turn: f32,
    light: f32,
}

/// Draws `source` turned by `p.turn`: column by column, each column shorter
/// and darker towards the far edge, the source column sampled to fit.
fn draw(canvas: &mut RgbaImage, source: &RgbaImage, p: Placement) {
    let (cw, ch) = (canvas.width() as i64, canvas.height() as i64);
    let (sw, sh) = (source.width() as f32, source.height() as f32);
    let far_edge = 1.0 - (1.0 - FAR_EDGE) * p.turn;
    let far_shade = 1.0 - (1.0 - FAR_SHADE) * p.turn;
    let x0 = p.left.floor() as i64;
    let x1 = (p.left + p.w).ceil() as i64;
    for x in x0.max(0)..x1.min(cw) {
        let t = ((x as f32 + 0.5 - p.left) / p.w).clamp(0.0, 1.0);
        let near = if p.left_side { t } else { 1.0 - t };
        let col_h = p.h * (far_edge + (1.0 - far_edge) * near);
        let top = p.top + (p.h - col_h) / 2.0;
        let shade = p.light * (far_shade + (1.0 - far_shade) * near);
        // edge columns only partly covered: blend, for smooth motion
        let coverage = ((x as f32 + 1.0).min(p.left + p.w) - (x as f32).max(p.left)).clamp(0.0, 1.0);
        let sx = ((t * sw) as u32).min(source.width() - 1);
        let y0 = top.floor() as i64;
        let y1 = (top + col_h).ceil() as i64;
        for y in y0.max(0)..y1.min(ch) {
            let v = ((y as f32 + 0.5 - top) / col_h).clamp(0.0, 1.0);
            let sy = ((v * sh) as u32).min(source.height() - 1);
            let vcov = ((y as f32 + 1.0).min(top + col_h) - (y as f32).max(top)).clamp(0.0, 1.0);
            let alpha = coverage * vcov;
            let Rgba([r, g, b, _]) = *source.get_pixel(sx, sy);
            let under = *canvas.get_pixel(x as u32, y as u32);
            let mix = |c: u8, u: u8| (c as f32 * shade * alpha + u as f32 * (1.0 - alpha)).round() as u8;
            let a = (255.0 * alpha + under[3] as f32 * (1.0 - alpha)).round() as u8;
            canvas.put_pixel(x as u32, y as u32, Rgba([mix(r, under[0]), mix(g, under[1]), mix(b, under[2]), a]));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes frames of a glide to $COVERFLOW_PREVIEW_DIR (one PNG each) from
    /// the covers in $COVERFLOW_COVERS (colon-separated), to eyeball the
    /// animation. Does nothing without them.
    #[test]
    fn preview() {
        let (Ok(dir), Ok(list)) = (std::env::var("COVERFLOW_PREVIEW_DIR"), std::env::var("COVERFLOW_COVERS")) else {
            return;
        };
        let images: Vec<DynamicImage> = list.split(':').map(|p| image::open(p).unwrap()).collect();
        let covers: HashMap<usize, Option<DynamicImage>> =
            (0..12).map(|i| (i, Some(images[i % images.len()].clone()))).collect();
        let mut scene = Scene::default();
        scene.resize(1200, 300);
        for (n, pos) in [5.0, 5.25, 5.5, 5.75, 6.0].iter().enumerate() {
            let (canvas, _) = scene.compose(&covers, *pos, covers.len());
            canvas.save(format!("{dir}/frame{n}.png")).unwrap();
        }
    }
}
