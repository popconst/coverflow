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
    /// Stands in for albums without a cover (or not read yet)
    placeholder: Option<Scaled>,
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
            self.placeholder = None;
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
            let scaled = match covers.get(&index) {
                Some(Some(cover)) => self.scaled.entry(index).or_insert_with(|| scale(cover, height)),
                _ => self.placeholder.get_or_insert_with(|| scale(&placeholder(), height)),
            };
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

/// A dark sleeve with a record on it
fn placeholder() -> DynamicImage {
    const SIZE: u32 = 600;
    let c = SIZE as f32 / 2.0;
    DynamicImage::ImageRgba8(RgbaImage::from_fn(SIZE, SIZE, |x, y| {
        let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
        // sleeve: slate, lighter towards the top left
        let sheen = 1.0 - (fx + fy) / (2.0 * SIZE as f32);
        let sleeve = [30.0 + 14.0 * sheen, 34.0 + 16.0 * sheen, 44.0 + 20.0 * sheen];
        let r = ((fx - c).powi(2) + (fy - c).powi(2)).sqrt() / c;
        let rgb = if r > 0.82 {
            sleeve
        } else if r > 0.32 {
            // vinyl, with faint grooves and a highlight on one side
            let groove = if ((r * 90.0) as u32).is_multiple_of(2) { 4.0 } else { 0.0 };
            let shine = 18.0 * (((fx - c) - (fy - c)) / (c * 1.4)).clamp(0.0, 1.0).powi(2);
            let v = 14.0 + groove + shine;
            [v, v, v + 2.0]
        } else if r > 0.035 {
            // label
            [74.0, 104.0, 152.0]
        } else {
            sleeve
        };
        // soft edge between the record and the sleeve
        let edge = ((0.82 - r) * c).clamp(0.0, 1.0);
        let rgb = if r > 0.79 { [0, 1, 2].map(|i| sleeve[i] + (rgb[i] - sleeve[i]) * edge) } else { rgb };
        Rgba([rgb[0] as u8, rgb[1] as u8, rgb[2] as u8, 255])
    }))
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
/// and darker towards the far edge. Each canvas pixel averages the source
/// pixels it covers across, and blends the two nearest down, so a cover that
/// shrinks while gliding stays smooth instead of breaking into blocks.
fn draw(canvas: &mut RgbaImage, source: &RgbaImage, p: Placement) {
    let (cw, ch) = (canvas.width() as i64, canvas.height() as i64);
    let (sw, sh) = (source.width() as f32, source.height() as f32);
    let far_edge = 1.0 - (1.0 - FAR_EDGE) * p.turn;
    let far_shade = 1.0 - (1.0 - FAR_SHADE) * p.turn;
    let x0 = p.left.floor() as i64;
    let x1 = (p.left + p.w).ceil() as i64;
    let mut column: Vec<[f32; 3]> = Vec::with_capacity(source.height() as usize);
    for x in x0.max(0)..x1.min(cw) {
        let t = ((x as f32 + 0.5 - p.left) / p.w).clamp(0.0, 1.0);
        let near = if p.left_side { t } else { 1.0 - t };
        let col_h = p.h * (far_edge + (1.0 - far_edge) * near);
        let top = p.top + (p.h - col_h) / 2.0;
        let shade = p.light * (far_shade + (1.0 - far_shade) * near);
        // edge columns only partly covered: blend, for smooth motion
        let coverage = ((x as f32 + 1.0).min(p.left + p.w) - (x as f32).max(p.left)).clamp(0.0, 1.0);

        // the source columns this canvas column spans, averaged
        let s0 = ((x as f32 - p.left) / p.w * sw).clamp(0.0, sw - 1.0);
        let s1 = ((x as f32 + 1.0 - p.left) / p.w * sw).clamp(s0 + 1.0, sw);
        let (c0, c1) = (s0 as usize, (s1.ceil() as usize).min(source.width() as usize));
        let n = (c1 - c0).max(1) as f32;
        let stride = source.width() as usize * 4;
        column.clear();
        column.extend(source.as_raw().chunks_exact(stride).map(|row| {
            let mut sum = [0.0f32; 3];
            for px in row[c0 * 4..c1 * 4].chunks_exact(4) {
                sum[0] += px[0] as f32;
                sum[1] += px[1] as f32;
                sum[2] += px[2] as f32;
            }
            sum.map(|v| v / n)
        }));

        let y0 = top.floor() as i64;
        let y1 = (top + col_h).ceil() as i64;
        let canvas_stride = cw as usize * 4;
        let pixels: &mut [u8] = canvas;
        for y in y0.max(0)..y1.min(ch) {
            let vcov = ((y as f32 + 1.0).min(top + col_h) - (y as f32).max(top)).clamp(0.0, 1.0);
            let alpha = coverage * vcov;
            // the source rows this pixel spans: averaged when shrinking,
            // the two nearest blended otherwise
            let r0 = (((y as f32 - top) / col_h) * sh).clamp(0.0, sh - 1.0);
            let r1 = (((y as f32 + 1.0 - top) / col_h) * sh).clamp(r0, sh);
            let rgb = if r1 - r0 > 1.0 {
                let (a, b) = (r0 as usize, (r1.ceil() as usize).min(column.len()));
                let mut sum = [0.0f32; 3];
                for c in &column[a..b] {
                    for i in 0..3 {
                        sum[i] += c[i];
                    }
                }
                sum.map(|v| v / (b - a) as f32)
            } else {
                let m = ((r0 + r1) / 2.0 - 0.5).clamp(0.0, sh - 1.0);
                let (a, f) = (m as usize, m.fract());
                let b = (a + 1).min(column.len() - 1);
                [0, 1, 2].map(|i| column[a][i] * (1.0 - f) + column[b][i] * f)
            };
            let at = y as usize * canvas_stride + x as usize * 4;
            let under = &mut pixels[at..at + 4];
            for i in 0..3 {
                under[i] = (rgb[i] * shade * alpha + under[i] as f32 * (1.0 - alpha)).round() as u8;
            }
            under[3] = (255.0 * alpha + under[3] as f32 * (1.0 - alpha)).round() as u8;
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
        // every third album without a cover
        let covers: HashMap<usize, Option<DynamicImage>> =
            (0..12).map(|i| (i, (i % 3 != 1).then(|| images[i % images.len()].clone()))).collect();
        let mut scene = Scene::default();
        scene.resize(1000, 250);
        for (n, pos) in [5.0, 5.25, 5.5, 5.75, 6.0].iter().enumerate() {
            let (canvas, _) = scene.compose(&covers, *pos, covers.len());
            canvas.save(format!("{dir}/frame{n}.png")).unwrap();
        }
    }
}

#[cfg(test)]
mod bench {
    #[test]
    fn frame_time() {
        let Ok(list) = std::env::var("COVERFLOW_COVERS") else { return };
        let img = image::open(list.split(':').next().unwrap()).unwrap();
        let covers = (0..30).map(|i| (i, Some(img.clone()))).collect();
        let mut scene = super::Scene::default();
        let (w, h): (u32, u32) = std::env::var("BENCH_SIZE").ok().and_then(|s| { let (a, b) = s.split_once("x")?; Some((a.parse().ok()?, b.parse().ok()?)) }).unwrap_or((900, 225));
        scene.resize(w, h);
        scene.compose(&covers, 10.0, 30);
        let t = std::time::Instant::now();
        let mut bytes = 0;
        for k in 0..20 {
            let (c, _) = scene.compose(&covers, 10.0 + k as f32 * 0.05, 30);
            let t2 = std::time::Instant::now();
            let p = crate::kitty::Picture::new(&c, 1, 100, 25, &crate::kitty::Medium::TempFile(std::env::temp_dir()));
            bytes = p.len();
            if k == 0 { eprintln!("encode {:?}", t2.elapsed()); }
        }
        eprintln!("per frame {:?}, {} KB sent", t.elapsed() / 20, bytes / 1024);
    }
}
