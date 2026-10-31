//! Outlines to test on: rectangles, and random contours of lines and
//! quadratics that overlap each other and themselves.

use crate::glyph::{P, Quad};

/// A rectangle as lines, counter-clockwise in y-down cell space or not.
pub fn rect(x0: f32, y0: f32, x1: f32, y1: f32, ccw: bool) -> Vec<Quad> {
    let mut p = vec![[x0, y0], [x1, y0], [x1, y1], [x0, y1]];
    if ccw {
        p.reverse();
    }
    (0..4).map(|i| line(p[i], p[(i + 1) % 4])).collect()
}

fn line(a: P, b: P) -> Quad {
    [a, [(a[0] + b[0]) * 0.5, (a[1] + b[1]) * 0.5], b]
}

/// A 64-bit LCG (Knuth's MMIX constants), top bits out.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }

    /// Uniform in [lo, hi).
    pub fn f(&mut self, lo: f32, hi: f32) -> f32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        lo + (hi - lo) * ((self.0 >> 40) as f32 / (1u64 << 24) as f32)
    }
}

/// One to three closed contours inside [margin, size - margin]^2: points
/// round a centre at random radii, joined by lines or by quadratics with
/// a random control point, either way round.
pub fn random(seed: u64, size: f32, margin: f32) -> Vec<Vec<Quad>> {
    let mut r = Rng::new(seed);
    let lo = margin + 1.0;
    let hi = size - margin - 1.0;
    let n = 1 + (r.f(0.0, 3.0) as usize);
    (0..n)
        .map(|_| {
            let c = [r.f(lo + 3.0, hi - 3.0), r.f(lo + 3.0, hi - 3.0)];
            let reach = (c[0] - lo).min(hi - c[0]).min(c[1] - lo).min(hi - c[1]);
            let k = 3 + (r.f(0.0, 6.0) as usize);
            let dir = if r.f(0.0, 1.0) < 0.5 { 1.0 } else { -1.0 };
            let pts: Vec<P> = (0..k)
                .map(|i| {
                    let a = dir * (i as f32 + r.f(0.0, 0.8)) / k as f32 * std::f32::consts::TAU;
                    let rad = r.f(0.3, 1.0) * reach;
                    [c[0] + rad * a.cos(), c[1] + rad * a.sin()]
                })
                .collect();
            (0..k)
                .map(|i| {
                    let (a, b) = (pts[i], pts[(i + 1) % k]);
                    if r.f(0.0, 1.0) < 0.5 {
                        line(a, b)
                    } else {
                        [a, [r.f(lo, hi), r.f(lo, hi)], b]
                    }
                })
                .collect()
        })
        .collect()
}
