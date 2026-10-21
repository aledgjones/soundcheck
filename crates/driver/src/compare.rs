//! preview-compare: is what a drawing app saves where its preview showed it?

use crate::device::{DriverError, Result};
use std::path::Path;

/// A box in screenshot pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelBox {
    pub x: i64,
    pub y: i64,
    pub w: i64,
    pub h: i64,
}

pub struct Image {
    pub width: usize,
    pub height: usize,
    /// RGB, row by row.
    rgb: Vec<u8>,
}

impl Image {
    pub fn load(path: &Path) -> Result<Image> {
        let file = std::fs::File::open(path)
            .map_err(|e| DriverError::Device(format!("{}: {e}", path.display())))?;
        let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
        decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
        let mut reader = decoder
            .read_info()
            .map_err(|e| DriverError::Device(e.to_string()))?;
        let mut buf = vec![0; reader.output_buffer_size().unwrap_or(0)];
        let info = reader
            .next_frame(&mut buf)
            .map_err(|e| DriverError::Device(e.to_string()))?;
        let channels = info.color_type.samples();
        let (width, height) = (info.width as usize, info.height as usize);
        let mut rgb = Vec::with_capacity(width * height * 3);
        for px in buf[..info.buffer_size()].chunks(channels) {
            match channels {
                1 | 2 => rgb.extend([px[0], px[0], px[0]]),
                _ => rgb.extend(&px[..3]),
            }
        }
        Ok(Image { width, height, rgb })
    }

    fn pixel(&self, x: usize, y: usize) -> [u8; 3] {
        let i = (y * self.width + x) * 3;
        [self.rgb[i], self.rgb[i + 1], self.rgb[i + 2]]
    }
}

fn clip(region: PixelBox, img: &Image) -> (usize, usize, usize, usize) {
    let x0 = region.x.clamp(0, img.width as i64) as usize;
    let y0 = region.y.clamp(0, img.height as i64) as usize;
    let x1 = (region.x + region.w).clamp(0, img.width as i64) as usize;
    let y1 = (region.y + region.h).clamp(0, img.height as i64) as usize;
    (x0, y0, x1, y1)
}

fn bounding(points: impl Iterator<Item = (usize, usize)>) -> Option<PixelBox> {
    let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
    let mut any = false;
    for (x, y) in points {
        any = true;
        x0 = x0.min(x);
        y0 = y0.min(y);
        x1 = x1.max(x);
        y1 = y1.max(y);
    }
    any.then(|| PixelBox {
        x: x0 as i64,
        y: y0 as i64,
        w: (x1 - x0 + 1) as i64,
        h: (y1 - y0 + 1) as i64,
    })
}

/// Pixels that changed from `before` by more than a fifth of full brightness, inside `region`.
pub fn changed_box(before: &Image, after: &Image, region: PixelBox) -> Option<PixelBox> {
    let (x0, y0, x1, y1) = clip(region, after);
    let changed = (y0..y1)
        .flat_map(|y| (x0..x1).map(move |x| (x, y)))
        .filter(|&(x, y)| {
            if x >= before.width || y >= before.height {
                return true;
            }
            let (a, b) = (before.pixel(x, y), after.pixel(x, y));
            let luma = |p: [u8; 3]| {
                0.299 * f64::from(p[0]) + 0.587 * f64::from(p[1]) + 0.114 * f64::from(p[2])
            };
            (luma(a) - luma(b)).abs() > 51.0
        });
    bounding(changed)
}

/// Pixels near `colour` (within a quarter of the colour cube's diagonal), inside `region`.
pub fn colour_box(img: &Image, region: PixelBox, colour: [u8; 3]) -> Option<PixelBox> {
    let (x0, y0, x1, y1) = clip(region, img);
    let near = (y0..y1)
        .flat_map(|y| (x0..x1).map(move |x| (x, y)))
        .filter(|&(x, y)| {
            let p = img.pixel(x, y);
            let d: f64 = (0..3)
                .map(|i| (f64::from(p[i]) - f64::from(colour[i])).powi(2))
                .sum::<f64>()
                .sqrt();
            d < 0.25 * 441.7
        });
    bounding(near)
}

pub fn parse_colour(hex: &str) -> Result<[u8; 3]> {
    let h = hex.trim_start_matches('#');
    let byte = |i: usize| u8::from_str_radix(h.get(i..i + 2).unwrap_or("x"), 16);
    match (h.len() >= 6, byte(0), byte(2), byte(4)) {
        (true, Ok(r), Ok(g), Ok(b)) => Ok([r, g, b]),
        _ => Err(DriverError::Invalid(format!(
            "{hex} isn't a colour like #FF3B30"
        ))),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Comparison {
    pub preview: Option<PixelBox>,
    pub saved: Option<PixelBox>,
    /// Saved minus preview, per edge: left, top, right, bottom.
    pub offsets: Option<[i64; 4]>,
    pub tolerance: i64,
    pub passed: bool,
}

impl Comparison {
    pub fn new(preview: Option<PixelBox>, saved: Option<PixelBox>, tolerance: i64) -> Comparison {
        let offsets = match (preview, saved) {
            (Some(p), Some(s)) => Some([
                s.x - p.x,
                s.y - p.y,
                (s.x + s.w) - (p.x + p.w),
                (s.y + s.h) - (p.y + p.h),
            ]),
            _ => None,
        };
        let passed = offsets.is_some_and(|o| o.iter().all(|d| d.abs() <= tolerance));
        Comparison {
            preview,
            saved,
            offsets,
            tolerance,
            passed,
        }
    }

    pub fn summary(&self) -> String {
        let show = |b: Option<PixelBox>| {
            b.map_or("nothing".into(), |b| {
                format!("{}x{} at {},{}", b.w, b.h, b.x, b.y)
            })
        };
        match self.offsets {
            None => format!(
                "FAILED: {} (preview {}, saved {})",
                if self.preview.is_none() {
                    "no preview found"
                } else {
                    "nothing saved"
                },
                show(self.preview),
                show(self.saved)
            ),
            Some([l, t, r, b]) => format!(
                "{}: preview {}, saved {}; offset left {l}, top {t}, right {r}, bottom {b} (tolerance {})",
                if self.passed { "PASSED" } else { "FAILED" },
                show(self.preview),
                show(self.saved),
                self.tolerance
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(w: usize, h: usize, draw: &[(usize, usize, [u8; 3])]) -> Image {
        let mut rgb = vec![255; w * h * 3];
        for &(x, y, c) in draw {
            let i = (y * w + x) * 3;
            rgb[i..i + 3].copy_from_slice(&c);
        }
        Image {
            width: w,
            height: h,
            rgb,
        }
    }

    #[test]
    fn a_shifted_save_fails_and_a_matching_one_passes() {
        let blank = image(20, 20, &[]);
        let line = |y| (4..12).map(move |x| (x, y, [0, 0, 0])).collect::<Vec<_>>();
        let preview = image(20, 20, &line(5));
        let region = PixelBox {
            x: 0,
            y: 0,
            w: 20,
            h: 20,
        };
        let same = Comparison::new(
            changed_box(&blank, &preview, region),
            changed_box(&blank, &image(20, 20, &line(6)), region),
            2,
        );
        assert!(same.passed, "{}", same.summary());
        let shifted = Comparison::new(
            changed_box(&blank, &preview, region),
            changed_box(&blank, &image(20, 20, &line(9)), region),
            2,
        );
        assert!(!shifted.passed);
        assert_eq!(shifted.offsets, Some([0, 4, 0, 4]));
    }

    #[test]
    fn colour_mode_ignores_other_changes() {
        let img = image(10, 10, &[(2, 2, [255, 59, 48]), (8, 8, [0, 0, 0])]);
        let b = colour_box(
            &img,
            PixelBox {
                x: 0,
                y: 0,
                w: 10,
                h: 10,
            },
            parse_colour("#FF3B30").unwrap(),
        );
        assert_eq!(
            b,
            Some(PixelBox {
                x: 2,
                y: 2,
                w: 1,
                h: 1
            })
        );
    }
}
