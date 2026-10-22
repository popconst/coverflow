//! The Cover Flow "fold": a side cover turned to face the centre, drawn as a
//! trapezoid that is full height at the edge nearest the centre and shorter,
//! narrower and darker towards the far edge. The corners are left
//! transparent so the terminal background shows through.

use image::{DynamicImage, Rgba, RgbaImage};

/// How wide a folded cover is compared with a flat one
pub const SQUEEZE: f32 = 0.42;
/// Height of the far edge compared with the near one
const FAR_EDGE: f32 = 0.72;
/// Brightness at the far edge compared with the near one
const FAR_SHADE: f32 = 0.55;

/// Which way a folded cover faces
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Facing {
    /// a cover left of the centre, its near edge on its right
    Right,
    /// a cover right of the centre, its near edge on its left
    Left,
}

/// Folds `image` towards the centre by `amount` (0 flat … 1 fully turned);
/// `light` (0–1) darkens the whole cover, for covers further out.
pub fn fold(image: &DynamicImage, facing: Facing, amount: f32, light: f32) -> DynamicImage {
    let src = image.to_rgba8();
    let (w, h) = src.dimensions();
    let lerp = |full: f32| 1.0 - (1.0 - full) * amount;
    let (squeeze, far_edge, far_shade) = (lerp(SQUEEZE), lerp(FAR_EDGE), lerp(FAR_SHADE));
    let out_w = ((w as f32 * squeeze).round() as u32).max(2);
    let mut out = RgbaImage::from_pixel(out_w, h, Rgba([0, 0, 0, 0]));

    for x in 0..out_w {
        // 0 at the far edge, 1 at the edge nearest the centre
        let t = x as f32 / (out_w - 1) as f32;
        let near = match facing {
            Facing::Right => t,
            Facing::Left => 1.0 - t,
        };
        let col_h = h as f32 * (far_edge + (1.0 - far_edge) * near);
        let top = (h as f32 - col_h) / 2.0;
        let shade = light * (far_shade + (1.0 - far_shade) * near);
        let sx = ((t * (w - 1) as f32).round() as u32).min(w - 1);
        let (y0, y1) = (top.ceil() as u32, (top + col_h).floor() as u32);
        for y in y0..y1.min(h) {
            let sy = ((((y as f32 - top) / col_h) * (h - 1) as f32).round() as u32).min(h - 1);
            let Rgba([r, g, b, a]) = *src.get_pixel(sx, sy);
            let dim = |c: u8| (c as f32 * shade).round() as u8;
            out.put_pixel(x, y, Rgba([dim(r), dim(g), dim(b), a]));
        }
    }
    DynamicImage::ImageRgba8(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{GenericImage, imageops};

    /// Writes a mock Cover Flow row to $COVERFLOW_PREVIEW (a PNG path) from
    /// the covers listed in $COVERFLOW_COVERS (colon-separated), to eyeball
    /// the fold. Does nothing without them.
    #[test]
    fn preview() {
        let (Ok(out), Ok(list)) = (std::env::var("COVERFLOW_PREVIEW"), std::env::var("COVERFLOW_COVERS")) else {
            return;
        };
        let covers: Vec<DynamicImage> =
            list.split(':').map(|p| image::open(p).unwrap().thumbnail(300, 300)).collect();
        let (size, side_h) = (300u32, 258u32);
        let side_w = (side_h as f32 * SQUEEZE).round() as u32;
        let mut canvas = RgbaImage::from_pixel(size + 6 * side_w + 40, size, Rgba([12, 14, 20, 255]));
        let center_x = 3 * side_w + 20;
        canvas.copy_from(&covers[0].to_rgba8(), center_x, 0).unwrap();
        for depth in 1..=3usize {
            let light = (0.85 - 0.08 * (depth - 1) as f32).max(0.35);
            let y = (size - side_h) / 2;
            for (facing, x) in [
                (Facing::Right, center_x - 10 - depth as u32 * side_w),
                (Facing::Left, center_x + size + 10 + (depth as u32 - 1) * side_w),
            ] {
                let img = covers[depth % covers.len()].resize_exact(side_h, side_h, imageops::FilterType::Triangle);
                let folded = fold(&img, facing, 1.0, light).to_rgba8();
                imageops::overlay(&mut canvas, &folded, x as i64, y as i64);
            }
        }
        canvas.save(out).unwrap();
    }
}
