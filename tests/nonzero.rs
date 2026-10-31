//! coverage::coverage: each texel's area under the nonzero rule.

use crate::coverage::{coverage, polylines};
use crate::shapes::{random, rect};

/// The area of [x0, x1] x [y0, y1] in texel (tx, ty).
fn overlap(r: [f32; 4], tx: u32, ty: u32) -> f32 {
    let w = (r[2].min(tx as f32 + 1.0) - r[0].max(tx as f32)).max(0.0);
    let h = (r[3].min(ty as f32 + 1.0) - r[1].max(ty as f32)).max(0.0);
    w * h
}

/// Two rectangles overlapping: the same way round the overlap has winding
/// 2 and counts once (a sum of signed areas would count it twice);
/// opposite ways it has winding 0 and is a hole.
#[test]
fn overlaps_count_once_and_holes_not_at_all() {
    let (a, b) = ([1.3, 2.25, 9.6, 7.5], [4.75, 0.6, 12.2, 10.1]);
    let (w, h) = (14, 12);
    for same in [true, false] {
        let lines = polylines(&[rect(a[0], a[1], a[2], a[3], false), rect(b[0], b[1], b[2], b[3], !same)]);
        let cov = coverage(&lines, w, h);
        let both = [a[0].max(b[0]), a[1].max(b[1]), a[2].min(b[2]), a[3].min(b[3])];
        for ty in 0..h {
            for tx in 0..w {
                let (oa, ob, o) = (overlap(a, tx, ty), overlap(b, tx, ty), overlap(both, tx, ty));
                let want = if same { oa + ob - o } else { oa + ob - 2.0 * o };
                let got = cov[(ty * w + tx) as usize];
                assert!((got - want).abs() < 1e-5, "same {} texel ({}, {}): {} want {}", same, tx, ty, got, want);
            }
        }
    }
}

/// Random contours against the nonzero rule sampled 32 x 32 per texel.
#[test]
fn matches_sampled_nonzero() {
    const N: u32 = 32;
    let (w, h) = (16, 16);
    let (mut worst, mut sum, mut bias, mut texels) = (0f64, 0f64, 0f64, 0u32);
    for seed in 0..24 {
        let lines = polylines(&random(seed, 16.0, 0.0));
        let segs: Vec<_> = lines
            .iter()
            .flat_map(|l| (0..l.len()).map(move |i| (l[i], l[(i + 1) % l.len()])))
            .collect();
        let cov = coverage(&lines, w, h);
        for i in 0..(w * h) as usize {
            let (tx, ty) = ((i as u32 % w) as f64, (i as u32 / w) as f64);
            let mut inside = 0;
            for sy in 0..N {
                let y = ty + (sy as f64 + 0.5) / N as f64;
                for sx in 0..N {
                    let x = tx + (sx as f64 + 0.5) / N as f64;
                    let mut wn = 0;
                    for &(a, b) in &segs {
                        let (ay, by) = (a[1] as f64, b[1] as f64);
                        if (ay <= y) != (by <= y) && a[0] as f64 + (y - ay) / (by - ay) * (b[0] - a[0]) as f64 > x {
                            wn += if by > ay { 1 } else { -1 };
                        }
                    }
                    inside += (wn != 0) as u32;
                }
            }
            let e = cov[i] as f64 - inside as f64 / (N * N) as f64;
            worst = worst.max(e.abs());
            sum += e.abs();
            bias += e;
            texels += 1;
        }
    }
    let (mean, bias) = (sum / texels as f64, bias / texels as f64);
    // Sampling alone is off by up to about 2 / N where an edge crosses.
    assert!(worst < 2.0 / N as f64, "worst {}", worst);
    assert!(mean < 0.002, "mean {}", mean);
    assert!(bias.abs() < 2e-4, "bias {}", bias);
}
