//! Pure pixel operations on decoded frames (origin `imaging.py`), and the image codecs. No I/O.

use egui::{Color32, ColorImage};
use image::codecs::jpeg::JpegEncoder;
use image::{ExtendedColorType, ImageResult};

use super::logic::{NEUTRAL_FILL, SIGNATURE_SIZE, Signature};
use crate::sensors::types::{Region, RegionShape};

/// Tightly packed RGB8, what the models and the JPEG encoder take.
#[derive(Clone, Debug, PartialEq)]
pub struct Rgb {
    pub size: [usize; 2],
    pub pixels: Vec<u8>,
}

impl Rgb {
    fn filled(size: [usize; 2], color: [u8; 3]) -> Self {
        Self { size, pixels: color.repeat(size[0] * size[1]) }
    }
}

impl From<&ColorImage> for Rgb {
    /// Video is opaque, so alpha is dropped.
    fn from(frame: &ColorImage) -> Self {
        Self { size: frame.size, pixels: frame.pixels.iter().flat_map(|c| [c.r(), c.g(), c.b()]).collect() }
    }
}

/// A normalised `[x1, y1, x2, y2]` box cut from `frame` without masking: a detector needs to
/// see whole objects (`imaging.crop_box`). At least one pixel each way.
pub fn crop_box(frame: &ColorImage, area: [f32; 4]) -> Rgb {
    let [fw, fh] = frame.size;
    let [x1, y1, x2, y2] = area;
    crop_pixels(frame, [(x1 * fw as f32) as usize, (y1 * fh as f32) as usize, (x2 * fw as f32) as usize, (y2 * fh as f32) as usize])
}

/// The pixels `[x1, y1, x2, y2)` of `frame`, clamped to it, at least one pixel each way.
pub fn crop_pixels(frame: &ColorImage, [x1, y1, x2, y2]: [usize; 4]) -> Rgb {
    let [fw, fh] = frame.size;
    let (x0, y0) = (x1.min(fw - 1), y1.min(fh - 1));
    let (x_end, y_end) = (x2.clamp(x0 + 1, fw), y2.clamp(y0 + 1, fh));
    let size = [x_end - x0, y_end - y0];
    let pixels = (y0..y_end).flat_map(|y| frame.pixels[y * fw + x0..y * fw + x_end].iter().flat_map(|c| [c.r(), c.g(), c.b()])).collect();
    Rgb { size, pixels }
}

/// Resized to exactly `size`, stretched if the aspect differs.
pub fn scaled(img: &Rgb, size: [usize; 2]) -> Rgb {
    Rgb { size, pixels: resize(&img.pixels, 3, img.size, size) }
}

/// A `side` square sampled from `frame` through the affine map `to` (frame → square), bilinear,
/// black outside the frame (PIL's `transform(AFFINE, BILINEAR)`, pixel centres at `+ 0.5`).
pub fn warp(frame: &ColorImage, to: [[f32; 3]; 2], side: usize) -> Rgb {
    let [[a, b, tx], [c, d, ty]] = to;
    let det = a * d - b * c;
    let inv = [[d / det, -b / det], [-c / det, a / det]];
    let [fw, fh] = frame.size;
    let mut pixels = Vec::with_capacity(side * side * 3);
    for y in 0..side {
        for x in 0..side {
            let (ox, oy) = (x as f32 + 0.5 - tx, y as f32 + 0.5 - ty);
            let (sx, sy) = (inv[0][0] * ox + inv[0][1] * oy, inv[1][0] * ox + inv[1][1] * oy);
            if !(0.0..fw as f32).contains(&sx) || !(0.0..fh as f32).contains(&sy) {
                pixels.extend([0, 0, 0]);
                continue;
            }
            let (fx, fy) = (sx - 0.5, sy - 0.5);
            let (x0, y0) = (fx.floor(), fy.floor());
            let (dx, dy) = (fx - x0, fy - y0);
            let clamp = |v: f32, n: usize| (v.max(0.0) as usize).min(n - 1);
            let (xa, xb, ya, yb) = (clamp(x0, fw), clamp(x0 + 1.0, fw), clamp(y0, fh), clamp(y0 + 1.0, fh));
            let at = |px: usize, py: usize| frame.pixels[py * fw + px];
            let [p00, p10, p01, p11] = [at(xa, ya), at(xb, ya), at(xa, yb), at(xb, yb)];
            for channel in 0..3 {
                let v = |p: Color32| f32::from(p.to_array()[channel]);
                let top = v(p00) * (1.0 - dx) + v(p10) * dx;
                let bottom = v(p01) * (1.0 - dx) + v(p11) * dx;
                pixels.push((top * (1.0 - dy) + bottom * dy).round().clamp(0.0, 255.0) as u8);
            }
        }
    }
    Rgb { size: [side, side], pixels }
}

/// ITU-R 601 luma, as PIL's "L".
pub fn grey(img: &Rgb) -> Vec<u8> {
    img.pixels
        .as_chunks::<3>()
        .0
        .iter()
        .map(|&[r, g, b]| ((r as u32 * 19595 + g as u32 * 38470 + b as u32 * 7471 + 0x8000) >> 16) as u8)
        .collect()
}

/// Resized to a `side` square, stretched, not letterboxed: how the detectors were trained
/// (`Detector.preprocess`).
pub fn stretch(img: &Rgb, side: usize) -> Rgb {
    Rgb { size: [side, side], pixels: resize(&img.pixels, 3, img.size, [side, side]) }
}

/// The region's bounding box cut from `frame`, with everything outside a polygon painted
/// `NEUTRAL_FILL` (`imaging.crop`).
pub fn crop(frame: &ColorImage, region: &Region) -> Rgb {
    let [fw, fh] = frame.size;
    let (min, max) = region.bounds();
    let x0 = ((min.x() * fw as f32) as usize).min(fw - 1);
    let y0 = ((min.y() * fh as f32) as usize).min(fh - 1);
    let x1 = ((max.x() * fw as f32) as usize).clamp(x0 + 1, fw);
    let y1 = ((max.y() * fh as f32) as usize).clamp(y0 + 1, fh);
    let size = [x1 - x0, y1 - y0];
    let mut out = Rgb::filled(size, NEUTRAL_FILL);
    let mut copy = |y: usize, from: usize, to: usize| {
        let src = &frame.pixels[(y0 + y) * fw + x0..][from..to];
        let dst = &mut out.pixels[(y * size[0] + from) * 3..(y * size[0] + to) * 3];
        for (rgb, c) in dst.as_chunks_mut::<3>().0.iter_mut().zip(src) {
            *rgb = [c.r(), c.g(), c.b()];
        }
    };
    match region.shape() {
        RegionShape::Whole | RegionShape::Rect { .. } => (0..size[1]).for_each(|y| copy(y, 0, size[0])),
        RegionShape::Polygon(points) => {
            let corners: Vec<[f32; 2]> = points.iter().map(|p| [p.x() * fw as f32, p.y() * fh as f32]).collect();
            for y in 0..size[1] {
                for (from, to) in polygon_spans(&corners, (y0 + y) as f32 + 0.5, x0, size[0]) {
                    copy(y, from, to);
                }
            }
        }
    }
    out
}

/// Even-odd scanline fill: the pixel columns, relative to `x0`, of the row centred at `cy` whose
/// centres lie inside the polygon.
fn polygon_spans(corners: &[[f32; 2]], cy: f32, x0: usize, width: usize) -> Vec<(usize, usize)> {
    let mut xs: Vec<f32> = corners
        .iter()
        .zip(corners.iter().cycle().skip(1))
        .filter(|([_, ay], [_, by])| (*ay > cy) != (*by > cy))
        .map(|([ax, ay], [bx, by])| ax + (cy - ay) * (bx - ax) / (by - ay))
        .collect();
    xs.sort_by(f32::total_cmp);
    let column = |x: f32| ((x - x0 as f32 - 0.5).ceil().max(0.0) as usize).min(width);
    xs.as_chunks::<2>().0.iter().map(|&[a, b]| (column(a), column(b))).filter(|(a, b)| a < b).collect()
}

/// The region in grey (ITU-R 601 luma, as PIL's "L"), resized to `SIGNATURE_SIZE` square, in
/// `0.0..=1.0` (`imaging.region_signature`).
pub fn signature(frame: &ColorImage, region: &Region) -> Signature {
    let rgb = crop(frame, region);
    let small = resize(&grey(&rgb), 1, rgb.size, [SIGNATURE_SIZE, SIGNATURE_SIZE]);
    Signature(Box::new(std::array::from_fn(|i| small[i] as f32 / 255.0)))
}

/// Fit inside a `side` square keeping the aspect, never enlarging, centred on `NEUTRAL_FILL`
/// (`imaging.letterbox`).
pub fn letterbox(img: &Rgb, side: usize) -> Rgb {
    let [w, h] = img.size;
    let scale = (side as f32 / w.max(h) as f32).min(1.0);
    let fit = |n: usize| ((n as f32 * scale).round() as usize).clamp(1, side);
    let size = [fit(w), fit(h)];
    let small = if size == img.size { img.pixels.clone() } else { resize(&img.pixels, 3, img.size, size) };
    let mut out = Rgb::filled([side, side], NEUTRAL_FILL);
    let (left, top) = ((side - size[0]) / 2, (side - size[1]) / 2);
    for (y, row) in small.chunks_exact(size[0] * 3).enumerate() {
        let start = ((top + y) * side + left) * 3;
        out.pixels[start..start + row.len()].copy_from_slice(row);
    }
    out
}

/// Downscaled to fit `max_edge` on its longer side, never enlarged.
pub fn fit_rgba(frame: &ColorImage, max_edge: usize) -> ColorImage {
    let [w, h] = frame.size;
    if w.max(h) <= max_edge {
        return frame.clone();
    }
    let scale = max_edge as f32 / w.max(h) as f32;
    let size = [w, h].map(|n| ((n as f32 * scale).round() as usize).clamp(1, max_edge));
    let rgba = resize(frame.as_raw(), 4, frame.size, size);
    let pixels = rgba.as_chunks::<4>().0.iter().map(|&[r, g, b, a]| Color32::from_rgba_premultiplied(r, g, b, a));
    ColorImage::new(size, pixels.collect())
}

pub fn encode_jpeg(img: &Rgb, quality: u8) -> ImageResult<Vec<u8>> {
    let [w, h] = img.size.map(|n| n as u32);
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, quality).encode(&img.pixels, w, h, ExtendedColorType::Rgb8)?;
    Ok(out)
}

/// A JPEG or PNG file's pixels.
pub fn decode(bytes: &[u8]) -> ImageResult<ColorImage> {
    let rgba = image::load_from_memory(bytes)?.into_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Ok(ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()))
}

/// Separable resampling with a triangle filter widened by the scale, as PIL's BILINEAR: a large
/// reduction averages every source pixel instead of sampling a few.
pub(super) fn resize(src: &[u8], channels: usize, [sw, sh]: [usize; 2], [dw, dh]: [usize; 2]) -> Vec<u8> {
    let columns = taps(sw, dw);
    let mut mid = vec![0.0f32; sh * dw * channels];
    for y in 0..sh {
        for (x, (first, weights)) in columns.iter().enumerate() {
            for c in 0..channels {
                mid[(y * dw + x) * channels + c] =
                    weights.iter().enumerate().map(|(k, w)| w * src[(y * sw + first + k) * channels + c] as f32).sum();
            }
        }
    }
    let mut out = vec![0u8; dw * dh * channels];
    for (y, (first, weights)) in taps(sh, dh).iter().enumerate() {
        for x in 0..dw {
            for c in 0..channels {
                let v: f32 =
                    weights.iter().enumerate().map(|(k, w)| w * mid[((first + k) * dw + x) * channels + c]).sum();
                out[(y * dw + x) * channels + c] = v.round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}

/// For each output index: the first source index it reads, and the normalised weights from there.
fn taps(src: usize, dst: usize) -> Vec<(usize, Vec<f32>)> {
    let scale = src as f32 / dst as f32;
    let support = scale.max(1.0);
    (0..dst)
        .map(|i| {
            let center = (i as f32 + 0.5) * scale;
            let first = (center - support).floor().max(0.0) as usize;
            let end = ((center + support).ceil() as usize).min(src);
            let weights: Vec<f32> =
                (first..end).map(|j| (1.0 - ((j as f32 + 0.5 - center) / support).abs()).max(0.0)).collect();
            let total: f32 = weights.iter().sum();
            (first, weights.into_iter().map(|w| w / total).collect())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sensors::types::NormPoint;
    use crate::sensors::vision::logic::changed;

    fn p(x: f32, y: f32) -> NormPoint {
        NormPoint::new(x, y).unwrap()
    }

    fn rect(a: [f32; 2], b: [f32; 2]) -> Region {
        Region::rect(p(a[0], a[1]), p(b[0], b[1])).unwrap()
    }

    fn pixel(img: &Rgb, x: usize, y: usize) -> [u8; 3] {
        let i = (y * img.size[0] + x) * 3;
        img.pixels[i..i + 3].try_into().unwrap()
    }

    fn count(img: &Rgb, color: [u8; 3]) -> usize {
        img.pixels.as_chunks::<3>().0.iter().filter(|c| **c == color).count()
    }

    /// Black, with the top `white_rows` rows white.
    fn top_white(size: [usize; 2], white_rows: usize) -> ColorImage {
        let mut img = ColorImage::filled(size, Color32::BLACK);
        img.pixels[..size[0] * white_rows].fill(Color32::WHITE);
        img
    }

    #[test]
    fn crop_takes_the_bounding_box() {
        let frame = ColorImage::filled([200, 100], Color32::from_rgb(10, 20, 30));
        let out = crop(&frame, &rect([0.25, 0.5], [0.75, 1.0]));
        assert_eq!(out.size, [100, 50]);
        assert_eq!(count(&out, [10, 20, 30]), 100 * 50);
        assert_eq!(crop(&frame, &Region::WHOLE).size, [200, 100]);
    }

    #[test]
    fn polygon_outside_is_neutral() {
        let frame = ColorImage::filled([100, 100], Color32::WHITE);
        let triangle = Region::polygon(vec![p(0.1, 0.1), p(0.9, 0.1), p(0.1, 0.9)]).unwrap();
        let out = crop(&frame, &triangle);
        assert_eq!(out.size, [80, 80]);
        assert_eq!(pixel(&out, 5, 5), [255, 255, 255], "inside the triangle");
        assert_eq!(pixel(&out, 75, 75), NEUTRAL_FILL, "outside");
        assert_eq!(pixel(&out, 78, 0), [255, 255, 255], "along the top edge");
        assert_eq!(pixel(&out, 0, 78), [255, 255, 255], "along the left edge");
        let inside = count(&out, [255, 255, 255]);
        assert!((3150..=3250).contains(&inside), "half of 80×80, got {inside}");
    }

    #[test]
    fn concave_polygon_keeps_its_notch_neutral() {
        let frame = ColorImage::filled([100, 100], Color32::WHITE);
        let u_shape = [(0.0, 0.0), (0.3, 0.0), (0.3, 0.7), (0.7, 0.7), (0.7, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)];
        let region = Region::polygon(u_shape.iter().map(|&(x, y)| p(x, y)).collect()).unwrap();
        let out = crop(&frame, &region);
        assert_eq!(pixel(&out, 50, 30), NEUTRAL_FILL, "in the notch");
        assert_eq!(pixel(&out, 10, 30), [255, 255, 255]);
        assert_eq!(pixel(&out, 90, 30), [255, 255, 255]);
        assert_eq!(pixel(&out, 50, 90), [255, 255, 255]);
    }

    #[test]
    fn signature_sees_only_the_region() {
        let black = ColorImage::filled([100, 100], Color32::BLACK);
        let half = top_white([100, 100], 50);
        let whole_black = signature(&black, &Region::WHOLE);
        let whole_half = signature(&half, &Region::WHOLE);
        assert!(whole_half.0.iter().all(|v| (0.0..=1.0).contains(v)));
        let mean_diff = whole_half.0.iter().zip(whole_black.0.iter()).map(|(a, b)| (a - b).abs()).sum::<f32>() / 2304.0;
        assert!((mean_diff - 0.5).abs() <= 0.02, "{mean_diff}");
        assert!(changed(&whole_black, &whole_half));
        let bottom = rect([0.0, 0.6], [1.0, 1.0]);
        assert_eq!(signature(&half, &bottom), signature(&black, &bottom), "a change outside the region scores 0");
    }

    #[test]
    fn signature_of_a_large_frame_averages() {
        let stripes = ColorImage::new(
            [1920, 1080],
            (0..1920 * 1080).map(|i| if i % 2 == 0 { Color32::WHITE } else { Color32::BLACK }).collect(),
        );
        let sig = signature(&stripes, &Region::WHOLE);
        assert!(sig.0.iter().all(|v| (v - 0.5).abs() < 0.02), "one-pixel stripes average to grey");
    }

    #[test]
    fn letterbox_pads_and_never_enlarges() {
        let wide = Rgb::filled([448, 224], [200, 0, 0]);
        let out = letterbox(&wide, 224);
        assert_eq!(out.size, [224, 224]);
        assert_eq!(pixel(&out, 112, 0), NEUTRAL_FILL);
        assert_eq!(pixel(&out, 112, 112), [200, 0, 0]);
        assert_eq!(pixel(&out, 112, 223), NEUTRAL_FILL);
        let small = letterbox(&Rgb::filled([50, 30], [0, 0, 200]), 224);
        let blue = count(&small, [0, 0, 200]);
        assert_eq!(blue, 50 * 30);
        assert_eq!(pixel(&small, 87, 97), [0, 0, 200], "centred");
    }

    #[test]
    fn fit_rgba_downscales_to_the_edge_only() {
        let main = ColorImage::filled([1920, 1080], Color32::from_rgb(1, 2, 3));
        let fitted = fit_rgba(&main, 960);
        assert_eq!(fitted.size, [960, 540]);
        assert!(fitted.pixels.iter().all(|c| *c == Color32::from_rgb(1, 2, 3)));
        let sub = ColorImage::filled([640, 360], Color32::WHITE);
        assert_eq!(fit_rgba(&sub, 960), sub);
    }

    #[test]
    fn a_box_is_cut_unmasked_and_at_least_one_pixel() {
        let frame = top_white([200, 100], 50);
        let out = crop_box(&frame, [0.25, 0.25, 0.75, 1.0]);
        assert_eq!(out.size, [100, 75]);
        assert_eq!((pixel(&out, 0, 0), pixel(&out, 0, 74)), ([255, 255, 255], [0, 0, 0]), "the white top, then black");
        assert_eq!(crop_box(&frame, [0.5, 0.5, 0.5, 0.5]).size, [1, 1]);
        assert_eq!(stretch(&out, 64).size, [64, 64]);
    }
}
