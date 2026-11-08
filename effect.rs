//! The tilt-shift "miniature" effect, applied to a packed RGB24 frame.
//!
//! All spatial parameters are fractions of the frame height, so the same
//! settings look identical on the small preview and the full-size export.

use rayon::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Params {
    /// Vertical centre of the sharp band (0 = top, 1 = bottom).
    pub focus_center: f32,
    /// Height of the fully sharp band.
    pub focus_size: f32,
    /// Distance over which blur ramps from none to full.
    pub falloff: f32,
    /// Maximum blur radius.
    pub blur: f32,
    pub saturation: f32,
    pub contrast: f32,
    /// Playback speed multiplier (export only).
    pub speed: f32,
    /// Output frame rate (export only). Lower = more toy-like stutter.
    pub fps: u32,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            focus_center: 0.6,
            focus_size: 0.12,
            falloff: 0.25,
            blur: 0.012,
            saturation: 1.5,
            contrast: 1.15,
            speed: 4.0,
            fps: 15,
        }
    }
}

/// Apply the full effect in place.
pub fn apply(rgb: &mut [u8], w: usize, h: usize, p: &Params) {
    debug_assert_eq!(rgb.len(), w * h * 3);
    grade(rgb, p.saturation, p.contrast);

    let radius = (p.blur * h as f32).round() as usize;
    if radius == 0 {
        return;
    }

    // Two blur levels give a smoother, more lens-like ramp than one.
    let (full, half) = rayon::join(
        || gaussian(rgb, w, h, radius),
        || gaussian(rgb, w, h, (radius / 2).max(1)),
    );

    let stride = w * 3;
    rgb.par_chunks_mut(stride).enumerate().for_each(|(y, row)| {
        let t = blur_weight(y, h, p);
        if t <= 0.0 {
            return;
        }
        let off = y * stride;
        let f_row = &full[off..off + stride];
        let h_row = &half[off..off + stride];
        for i in 0..stride {
            let o = row[i] as f32;
            let hv = h_row[i] as f32;
            let fv = f_row[i] as f32;
            let v = if t < 0.5 {
                o + (hv - o) * (t * 2.0)
            } else {
                hv + (fv - hv) * (t * 2.0 - 1.0)
            };
            row[i] = v.round() as u8;
        }
    });
}

/// 0 inside the sharp band, rising smoothly to 1 at the end of the falloff.
fn blur_weight(y: usize, h: usize, p: &Params) -> f32 {
    let pos = (y as f32 + 0.5) / h as f32;
    let d = (pos - p.focus_center).abs() - p.focus_size * 0.5;
    if d <= 0.0 {
        return 0.0;
    }
    let t = (d / p.falloff.max(1e-4)).min(1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Saturation and contrast boost — the "painted plastic" look.
fn grade(rgb: &mut [u8], sat: f32, con: f32) {
    if (sat - 1.0).abs() < 1e-3 && (con - 1.0).abs() < 1e-3 {
        return;
    }
    rgb.par_chunks_mut(3 * 4096).for_each(|chunk| {
        for px in chunk.chunks_exact_mut(3) {
            let (r, g, b) = (px[0] as f32, px[1] as f32, px[2] as f32);
            let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            for c in px.iter_mut() {
                let v = luma + (*c as f32 - luma) * sat;
                let v = (v - 128.0) * con + 128.0;
                *c = v.clamp(0.0, 255.0) as u8;
            }
        }
    });
}

/// Three box-blur passes per axis ≈ a Gaussian with sigma ≈ radius.
fn gaussian(src: &[u8], w: usize, h: usize, radius: usize) -> Vec<u8> {
    let mut a = src.to_vec();
    let mut b = vec![0u8; src.len()];

    for _ in 0..3 {
        box_blur_rows(&a, &mut b, w, radius);
        std::mem::swap(&mut a, &mut b);
    }
    // Transpose so the vertical pass is also a cache-friendly row pass.
    transpose(&a, &mut b, w, h);
    std::mem::swap(&mut a, &mut b);
    for _ in 0..3 {
        box_blur_rows(&a, &mut b, h, radius);
        std::mem::swap(&mut a, &mut b);
    }
    transpose(&a, &mut b, h, w);
    b
}

fn box_blur_rows(src: &[u8], dst: &mut [u8], w: usize, r: usize) {
    dst.par_chunks_mut(w * 3)
        .zip(src.par_chunks(w * 3))
        .for_each(|(out, row)| blur_line(row, out, w, r));
}

/// Sliding-window box blur with clamped edges.
fn blur_line(row: &[u8], out: &mut [u8], w: usize, r: usize) {
    let r = r as isize;
    let wi = w as isize;
    let norm = (2 * r + 1) as u32;
    for c in 0..3 {
        let px = |x: isize| row[x.clamp(0, wi - 1) as usize * 3 + c] as u32;
        let mut sum: u32 = (-r..=r).map(px).sum();
        for x in 0..wi {
            out[x as usize * 3 + c] = ((sum + norm / 2) / norm) as u8;
            sum += px(x + r + 1);
            sum -= px(x - r);
        }
    }
}

/// `src` is w×h; `dst` becomes h×w.
fn transpose(src: &[u8], dst: &mut [u8], w: usize, h: usize) {
    dst.par_chunks_mut(h * 3).enumerate().for_each(|(x, out_row)| {
        for y in 0..h {
            let s = (y * w + x) * 3;
            out_row[y * 3..y * 3 + 3].copy_from_slice(&src[s..s + 3]);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transpose_roundtrip() {
        let (w, h) = (5, 3);
        let src: Vec<u8> = (0..(w * h * 3) as u8).collect();
        let mut t = vec![0; src.len()];
        let mut back = vec![0; src.len()];
        transpose(&src, &mut t, w, h);
        transpose(&t, &mut back, h, w);
        assert_eq!(src, back);
    }

    #[test]
    fn flat_image_stays_flat() {
        let (w, h) = (64, 48);
        let mut img = vec![100u8; w * h * 3];
        let p = Params { saturation: 1.0, contrast: 1.0, ..Default::default() };
        apply(&mut img, w, h, &p);
        assert!(img.iter().all(|&v| v == 100));
    }

    #[test]
    fn band_stays_sharp() {
        let (w, h) = (64, 100);
        // Vertical stripes: blur would average them toward the middle.
        let mut img: Vec<u8> = (0..w * h)
            .flat_map(|i| if (i % w) % 2 == 0 { [0u8; 3] } else { [255u8; 3] })
            .collect();
        let orig = img.clone();
        let p = Params {
            focus_center: 0.5, focus_size: 0.2, blur: 0.05,
            saturation: 1.0, contrast: 1.0, ..Default::default()
        };
        apply(&mut img, w, h, &p);
        let mid = 50 * w * 3;
        assert_eq!(&img[mid..mid + w * 3], &orig[mid..mid + w * 3]);
        assert_ne!(&img[..w * 3], &orig[..w * 3]); // top row got blurred
    }
}
