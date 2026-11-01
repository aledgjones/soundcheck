//! match: does a region of the screen render like the suite's reference image?
//!
//! A capture is cropped to the region, scaled to the reference's size and scored by how much of the ink (anything
//! not white) differs. Screens differ in density and antialiasing, so both are blurred slightly and the capture may sit
//! a few pixels off before a pixel counts as different.

use crate::compare::PixelBox;
use crate::device::{DriverError, Result};
use image::{Rgb, RgbImage, imageops};

/// A capture with no reference yet is kept this wide.
pub const WIDTH: u32 = 1000;
/// How far, in compared pixels, a capture may sit from the reference.
const SHIFT: i32 = 3;
/// A pixel is ink when one of its channels is at least this far from white.
const INK: u8 = 24;
/// Two pixels differ when one channel is at least this far apart.
const DIFFER: u8 = 64;
/// How far the region's shape may be from the reference's before the capture can't be compared.
const SHAPE: f64 = 0.03;

#[derive(Debug, Clone, PartialEq)]
pub struct Score {
    /// The share of ink pixels, in either image, that differ: 0 is identical.
    pub differs: f64,
    /// Where the capture lined up best, in compared pixels.
    pub shift: (i32, i32),
}

/// Crops `shot` to `region` and scales it to `size`, or to [`WIDTH`] wide at its own shape.
pub fn normalise(shot: &RgbImage, region: PixelBox, size: Option<(u32, u32)>) -> Result<RgbImage> {
    let x = region.x.clamp(0, i64::from(shot.width()));
    let y = region.y.clamp(0, i64::from(shot.height()));
    let w = (region.x + region.w).min(i64::from(shot.width())) - x;
    let h = (region.y + region.h).min(i64::from(shot.height())) - y;
    if w < region.w - 2 || h < region.h - 2 || w <= 0 || h <= 0 {
        return Err(DriverError::Invalid(
            "the region runs off the screen: the whole of it must show".into(),
        ));
    }

    let (w, h) = (w as u32, h as u32);
    let (tw, th) = match size {
        Some((tw, th)) => {
            let shape = (f64::from(w) / f64::from(h)) / (f64::from(tw) / f64::from(th));
            if (shape - 1.0).abs() > SHAPE {
                return Err(DriverError::Invalid(format!(
                    "the region is {w}x{h} pixels on screen, a different shape from the reference's {tw}x{th}: \
                     is all of it showing, and only it?"
                )));
            }

            (tw, th)
        }
        None => (
            WIDTH,
            ((f64::from(WIDTH) * f64::from(h) / f64::from(w)).round() as u32).max(1),
        ),
    };
    let crop = imageops::crop_imm(shot, x as u32, y as u32, w, h).to_image();
    Ok(imageops::resize(
        &crop,
        tw,
        th,
        imageops::FilterType::Triangle,
    ))
}

/// Checks each area to leave out is `[x, y, width, height]` within the compared area, as fractions of it.
pub fn check_exclude(exclude: &[[f64; 4]]) -> Result<()> {
    for &[x, y, w, h] in exclude {
        let inside = x >= 0.0 && y >= 0.0 && w > 0.0 && h > 0.0 && x + w <= 1.0 && y + h <= 1.0;
        if !inside {
            return Err(DriverError::Invalid(format!(
                "exclude [{x}, {y}, {w}, {h}]: each area is [x, y, width, height] as fractions of the compared area, \
                 inside 0 to 1"
            )));
        }
    }
    Ok(())
}

fn excluded(exclude: &[[f64; 4]], x: u32, y: u32, w: u32, h: u32) -> bool {
    let (fx, fy) = (
        (f64::from(x) + 0.5) / f64::from(w),
        (f64::from(y) + 0.5) / f64::from(h),
    );
    exclude
        .iter()
        .any(|&[ex, ey, ew, eh]| fx >= ex && fx < ex + ew && fy >= ey && fy < ey + eh)
}

fn ink(p: &Rgb<u8>) -> bool {
    p.0.iter().any(|c| 255 - c >= INK)
}

fn apart(a: &Rgb<u8>, b: &Rgb<u8>) -> bool {
    a.0.iter().zip(b.0).any(|(x, y)| x.abs_diff(y) >= DIFFER)
}

/// Scores `capture` against `reference` (the same size), and draws where they differ over a faint copy of the
/// reference: red where the reference has ink the capture doesn't match, blue where the capture has ink of its own.
/// `exclude` areas (see [`check_exclude`]) are blanked in both before scoring, and shaded grey in the diff.
pub fn score(reference: &RgbImage, capture: &RgbImage, exclude: &[[f64; 4]]) -> (Score, RgbImage) {
    let (w, h) = (reference.width() as i32, reference.height() as i32);
    let blank = |img: &RgbImage| {
        let mut img = img.clone();
        for (x, y, p) in img.enumerate_pixels_mut() {
            if excluded(exclude, x, y, w as u32, h as u32) {
                *p = Rgb([255, 255, 255]);
            }
        }
        img
    };
    let (r, c) = (
        imageops::blur(&blank(reference), 1.0),
        imageops::blur(&blank(capture), 1.0),
    );
    let count = |dx: i32, dy: i32| {
        let (mut inked, mut differ) = (0u64, 0u64);
        for y in SHIFT..h - SHIFT {
            for x in SHIFT..w - SHIFT {
                let a = r.get_pixel(x as u32, y as u32);
                let b = c.get_pixel((x + dx) as u32, (y + dy) as u32);
                if ink(a) || ink(b) {
                    inked += 1;
                    if apart(a, b) {
                        differ += 1;
                    }
                }
            }
        }
        (inked, differ)
    };
    let mut best = ((0, 0), count(0, 0));
    for dy in -SHIFT..=SHIFT {
        for dx in -SHIFT..=SHIFT {
            let counted = count(dx, dy);
            if counted.1 < best.1.1 {
                best = ((dx, dy), counted);
            }
        }
    }

    let ((dx, dy), (inked, differ)) = best;
    let mut diff = RgbImage::from_fn(w as u32, h as u32, |x, y| {
        if excluded(exclude, x, y, w as u32, h as u32) {
            return Rgb([200, 200, 200]);
        }

        Rgb(reference.get_pixel(x, y).0.map(|v| 255 - (255 - v) / 4))
    });
    for y in SHIFT..h - SHIFT {
        for x in SHIFT..w - SHIFT {
            let a = r.get_pixel(x as u32, y as u32);
            let b = c.get_pixel((x + dx) as u32, (y + dy) as u32);
            if (ink(a) || ink(b)) && apart(a, b) {
                let mark = if ink(a) { [220, 30, 30] } else { [30, 80, 230] };
                diff.put_pixel(x as u32, y as u32, Rgb(mark));
            }
        }
    }

    let differs = if inked == 0 {
        0.0
    } else {
        100.0 * differ as f64 / inked as f64
    };
    (
        Score {
            differs,
            shift: (dx, dy),
        },
        diff,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(marks: &[(u32, u32, u32, u32, [u8; 3])]) -> RgbImage {
        let mut img = RgbImage::from_pixel(200, 280, Rgb([255, 255, 255]));
        for &(x, y, w, h, colour) in marks {
            for py in y..y + h {
                for px in x..x + w {
                    img.put_pixel(px, py, Rgb(colour));
                }
            }
        }
        img
    }

    const RED: [u8; 3] = [220, 20, 20];
    const BLACK: [u8; 3] = [0, 0, 0];

    #[test]
    fn the_same_marks_score_nothing_even_a_pixel_or_two_off() {
        let reference = page(&[(20, 30, 120, 4, BLACK), (50, 100, 20, 20, RED)]);
        let nudged = page(&[(22, 31, 120, 4, BLACK), (52, 101, 20, 20, RED)]);
        let (s, _) = score(&reference, &nudged, &[]);
        assert!(s.differs < 1.0, "{s:?}");
        assert_eq!(s.shift, (2, 1));
    }

    #[test]
    fn a_missing_or_recoloured_mark_scores_its_share_of_the_ink() {
        let reference = page(&[(20, 30, 120, 4, BLACK), (50, 100, 20, 20, RED)]);
        let missing = page(&[(20, 30, 120, 4, BLACK)]);
        let (s, diff) = score(&reference, &missing, &[]);
        assert!(s.differs > 30.0, "{s:?}");
        assert_eq!(
            diff.get_pixel(60, 110).0,
            [220, 30, 30],
            "missing ink is red"
        );

        let recoloured = page(&[(20, 30, 120, 4, BLACK), (50, 100, 20, 20, [20, 20, 220])]);
        assert!(score(&reference, &recoloured, &[]).0.differs > 30.0);
    }

    #[test]
    fn an_excluded_area_is_left_out_of_the_score_and_shaded_in_the_diff() {
        let reference = page(&[(20, 100, 120, 4, BLACK), (160, 4, 30, 20, BLACK)]);
        let capture = page(&[(20, 100, 120, 4, BLACK)]);
        assert!(score(&reference, &capture, &[]).0.differs > 30.0);

        let top = [[0.0, 0.0, 1.0, 0.1]];
        let (s, diff) = score(&reference, &capture, &top);
        assert!(s.differs < 1.0, "{s:?}");
        assert_eq!(diff.get_pixel(170, 10).0, [200, 200, 200]);

        assert!(check_exclude(&top).is_ok());
        assert!(check_exclude(&[[0.0, 0.95, 1.0, 0.1]]).is_err(), "runs off");
        assert!(check_exclude(&[[0.0, 0.0, 0.0, 0.1]]).is_err(), "empty");
    }

    #[test]
    fn a_region_is_cropped_and_scaled_to_the_reference_and_must_share_its_shape() {
        let shot = page(&[(40, 40, 100, 140, BLACK)]);
        let region = PixelBox {
            x: 40,
            y: 40,
            w: 100,
            h: 140,
        };
        let scaled = normalise(&shot, region, Some((50, 70))).unwrap();
        assert_eq!(scaled.dimensions(), (50, 70));
        assert_eq!(scaled.get_pixel(25, 35).0, BLACK);
        assert!(normalise(&shot, region, Some((70, 70))).is_err());
        assert_eq!(normalise(&shot, region, None).unwrap().width(), WIDTH);

        let off = PixelBox {
            x: 150,
            y: 40,
            w: 100,
            h: 140,
        };
        assert!(normalise(&shot, off, None).is_err(), "half off the screen");
    }
}
