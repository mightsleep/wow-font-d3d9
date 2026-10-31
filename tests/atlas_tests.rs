//! atlas: the encoder through the reference decoder (reference.rs), its
//! cover bytes and its texel words, against the exact nonzero area
//! (coverage::coverage) of the outline as the record holds it.

use crate::atlas::{cover_cell, encode, texel_words, Record};
use crate::coverage;
use crate::glyph::{Glyph, Quad, P};
use crate::reference::{coverage, distance, word_coverage};
use crate::shapes::{rect, Rng};

/// A circle of radius r as eight quadratic arcs, either way round.
fn circle(c: P, r: f32, ccw: bool) -> Vec<Quad> {
    let n = 8;
    let k = 1.0 / (std::f32::consts::PI / n as f32).cos();
    let at = |i: f32, rad: f32| {
        let a = i / n as f32 * std::f32::consts::TAU * if ccw { 1.0 } else { -1.0 };
        [c[0] + rad * a.cos(), c[1] + rad * a.sin()]
    };
    (0..n)
        .map(|i| {
            [
                at(i as f32, r),
                at(i as f32 + 0.5, r * k),
                at(i as f32 + 1.0, r),
            ]
        })
        .collect()
}

/// A convex polygon round a centre whose sides bulge outwards a little:
/// a simple contour, as a static font's are.
fn blob(r: &mut Rng, c: P, rad: f32) -> Vec<Quad> {
    let k = 3 + (r.f(0.0, 6.0) as usize);
    let pts: Vec<P> = (0..k)
        .map(|i| {
            let a = (i as f32 + r.f(0.0, 0.5)) / k as f32 * std::f32::consts::TAU;
            [c[0] + rad * a.cos(), c[1] + rad * a.sin()]
        })
        .collect();
    (0..k)
        .map(|i| {
            let (a, b) = (pts[i], pts[(i + 1) % k]);
            let m = [(a[0] + b[0]) * 0.5, (a[1] + b[1]) * 0.5];
            let bulge = r.f(0.0, 0.25);
            let out = [m[0] + (m[0] - c[0]) * bulge, m[1] + (m[1] - c[1]) * bulge];
            if r.f(0.0, 1.0) < 0.4 {
                [a, m, b]
            } else {
                [a, out, b]
            }
        })
        .collect()
}

/// The outlines to test: rectangles, a ring (a hole the other way round),
/// and blobs side by side.
fn outlines() -> Vec<Vec<Vec<Quad>>> {
    let mut v = vec![
        vec![rect(1.3, 2.25, 9.6, 7.5, false)],
        vec![
            circle([8.0, 8.0], 6.5, true),
            circle([8.0, 8.0], 3.2, false),
        ],
    ];
    let mut r = Rng::new(7);
    for _ in 0..20 {
        v.push(vec![
            blob(&mut r, [6.0, 8.0], 4.5),
            blob(&mut r, [16.0, 8.0], 4.0),
        ]);
    }
    v
}

/// The record's pieces back in outline units, regrouped into contours
/// (a piece that does not start where the last ended starts another).
fn held(r: &Record) -> Vec<Vec<Quad>> {
    let back = |p: P| [p[0] / r.scale + r.origin[0], p[1] / r.scale + r.origin[1]];
    let mut out: Vec<Vec<Quad>> = Vec::new();
    let mut end: Option<P> = None;
    for q in &r.outline {
        if end != Some(q[0]) {
            out.push(Vec::new());
        }
        end = Some(q[2]);
        out.last_mut().unwrap().push(q.map(back));
    }
    out
}

/// A quadratic cut exactly into n: each part's control is the blossom of
/// its ends.
fn cut(q: Quad, n: usize) -> Vec<Quad> {
    let blossom = |s: f32, t: f32| {
        let w = [(1.0 - s) * (1.0 - t), (1.0 - s) * t + s * (1.0 - t), s * t];
        [0, 1].map(|a| w[0] * q[0][a] + w[1] * q[1][a] + w[2] * q[2][a])
    };
    (0..n)
        .map(|i| {
            let (s, t) = (i as f32 / n as f32, (i + 1) as f32 / n as f32);
            [blossom(s, s), blossom(s, t), blossom(t, t)]
        })
        .collect()
}

/// The exact nonzero area of each of w x h pixels at `px` pixels a unit.
/// coverage::polylines keeps each chord within 1/256 texel of its curve,
/// whatever the curve, an area error near 4e-3 next to a curve; on a grid
/// FINE times finer, summed back in blocks, that is FINE times less.
fn exact(contours: &[Vec<Quad>], px: f32, w: u32, h: u32) -> Vec<f32> {
    const FINE: u32 = 8;
    let s = px * FINE as f32;
    // Cut first too: polylines gives a quadratic 32 chords at most, too
    // few for a long curve on the fine grid.
    let scaled: Vec<Vec<Quad>> = contours
        .iter()
        .map(|c| {
            c.iter()
                .flat_map(|&q| cut(q, 16))
                .map(|q| q.map(|p| [p[0] * s, p[1] * s]))
                .collect()
        })
        .collect();
    let fine = coverage::coverage(&coverage::polylines(&scaled), w * FINE, h * FINE);
    let mut out = vec![0f32; (w * h) as usize];
    for y in 0..h * FINE {
        for x in 0..w * FINE {
            out[((y / FINE) * w + x / FINE) as usize] += fine[(y * w * FINE + x) as usize];
        }
    }
    out.iter().map(|v| v / (FINE * FINE) as f32).collect()
}

/// The decoder is an exact box filter: at any scale it matches the area,
/// to within the reference's own chords and f32.
#[test]
fn matches_the_exact_area() {
    for (i, o) in outlines().iter().enumerate() {
        let r = encode(o, None, false).unwrap();
        for px in [0.5f32, 1.0, 2.0, 4.0, 8.0] {
            let (w, h) = ((24.0 * px).ceil() as u32, (16.0 * px).ceil() as u32);
            let exact = exact(&held(&r), px, w, h);
            let half = 0.5 / px * r.scale;
            let mut worst = 0f32;
            for ty in 0..h {
                for tx in 0..w {
                    let c = [
                        ((tx as f32 + 0.5) / px - r.origin[0]) * r.scale,
                        ((ty as f32 + 0.5) / px - r.origin[1]) * r.scale,
                    ];
                    let want = exact[(ty * w + tx) as usize].min(1.0);
                    worst = worst.max((coverage(&r, c, [half, half]) - want).abs());
                }
            }
            assert!(
                worst < 1e-3,
                "outline {i} at {px} px a unit: off by {worst}"
            );
        }
    }
}

/// The cover bytes of a cell (at 1, 2 and 4 texels a unit): within two
/// steps of the exact coverage after the gamma (one for the byte, one for
/// the chords), and a pure byte only where the exact coverage is 0 or 1.
#[test]
fn cover_bytes() {
    let g = 1.0 / 1.5;
    for (i, o) in outlines().iter().enumerate() {
        let r = encode(o, None, false).unwrap();
        for px in [1.0f32, 2.0, 4.0] {
            let (w, h) = ((24.0 * px).ceil() as u32, (16.0 * px).ceil() as u32);
            let exact = exact(&held(&r), px, w, h);
            // Texel x is outline x * px, texel row h - 1 - ty is exact's ty
            // (the cell's y runs down).
            let at = [r.origin[0] * px, h as f32 - r.origin[1] * px];
            let cell = cover_cell(&r, w, h, at, r.scale / px, 0.0, g);
            for ty in 0..h {
                for tx in 0..w {
                    let b = cell[((h - 1 - ty) * w + tx) as usize];
                    let e = exact[(ty * w + tx) as usize].min(1.0);
                    let want = e.powf(g) * 255.0;
                    if b == 0 || b == 255 {
                        assert!(
                            e < 1e-4 || e > 1.0 - 1e-4,
                            "outline {i} at {px}: texel ({tx}, {ty}) pure {b} but covered {e}"
                        );
                    } else {
                        assert!(
                            (b as f32 - want).abs() <= 2.0
                                || (b == 2 && want < 2.0)
                                || (b == 254 && want > 254.0),
                            "outline {i} at {px}: texel ({tx}, {ty}) byte {b}, want {want}"
                        );
                    }
                }
            }
        }
    }
}

const LATIN: &str =
    "AaBbCcDdEeFfGgHhIiJjKkLlMmNnOoPpQqRrSsTtUuVvWwXxYyZz0123456789@&%$#?!ěščřžýáíéůú";
const CJK: &str = "的一是不了人我在有他这为之大来以个中上们到说国和地也子时道出而要于就下得可你年生自会那后能对着事其里所去行过家十用发天如然作方成者多日都三小军二无同么经法当起与好看学进种将还分此心前面又定见只主没公从鬱龍麗";

/// The test fonts, on the host only: WOW_FONT_TEST_FONTS is a list of
/// font files (':' between); per file its name and the glyphs of LATIN, or
/// of CJK for a font that has them.
fn test_fonts() -> Vec<(String, Vec<Glyph>)> {
    let Ok(list) = std::env::var("WOW_FONT_TEST_FONTS") else {
        return Vec::new();
    };
    list.split(':')
        .map(|path| {
            let data = std::fs::read(path).expect("font");
            let name = std::path::Path::new(path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let cjk = crate::glyph::outline(&data, '的' as u32).is_some();
            let glyphs = (if cjk { CJK } else { LATIN })
                .chars()
                .filter_map(|ch| crate::glyph::outline(&data, ch as u32))
                .filter(|g| !g.contours.is_empty())
                .collect();
            (name, glyphs)
        })
        .collect()
}

/// The texel words against the band decoder, magnified (scale 1 to 4), in
/// a 34-texel cell at 32 px: the same coverage, and the same alpha within
/// an outline's reach.
#[test]
fn texel_words_match() {
    for (name, glyphs) in test_fonts() {
        for radius in [0.0f32, 1.25] {
            let reach = if radius > 0.0 {
                radius + 0.5 + 1.0 / 32.0
            } else {
                0.0
            };
            let (mut worst, mut worst_d) = (0f32, 0f32);
            for gl in &glyphs {
                let r = encode(&gl.contours, None, radius > 0.0).unwrap();
                let k = r.scale * gl.upem / 32.0;
                let ws = texel_words(&r, 34, 34, [1.0, 33.0], k, reach);
                let mut rng = Rng::new(5);
                for s in [1.0f32, 1.5, 2.0, 4.0] {
                    let h = 0.5 / s * k;
                    for _ in 0..200 {
                        let (px, py) = (rng.f(1.0, 33.0), rng.f(1.0, 33.0));
                        let c = [(px - 1.0) * k, (33.0 - py) * k];
                        let rd = (radius + 0.5 / s + 1.0 / 64.0) * k;
                        let t = py as usize * 34 + px as usize;
                        let Some((fill, d)) = word_coverage(&r, ws[t], c, [h, h], rd) else {
                            continue;
                        };
                        let exact = coverage(&r, c, [h, h]);
                        worst = worst.max((fill - exact).abs());
                        if radius > 0.0 {
                            // The alpha as the shader makes it: max(edge, fill).
                            let truth = distance(&r, c, rd);
                            let alpha = |d: f32, f: f32| {
                                ((radius - d / k) * s + 0.5).clamp(0.0, 1.0).max(f)
                            };
                            worst_d = worst_d.max((alpha(d, fill) - alpha(truth, exact)).abs());
                        }
                    }
                }
            }
            assert!(
                worst < 1e-4 && worst_d < 1e-3,
                "{name} radius {radius}: coverage off by {worst}, alpha by {worst_d}"
            );
        }
    }
}

/// A digest of every texel word for the test fonts, plain and outlined, at
/// 32 px and at an offset of the grid: any change to texel_words that
/// should keep its words shows here (cargo test -- --nocapture).
#[test]
fn texel_words_digest() {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (_, glyphs) in test_fonts() {
        for g in &glyphs {
            for radius in [0.0f32, 1.25] {
                let r = encode(&g.contours, None, radius > 0.0).unwrap();
                let k = r.scale * g.upem / 32.0;
                let reach = if radius > 0.0 {
                    radius + 0.5 + 1.0 / 32.0
                } else {
                    0.0
                };
                for at in [[1.0, 33.0], [1.37, 32.81]] {
                    texel_words(&r, 34, 34, at, k, reach).hash(&mut hasher);
                }
            }
        }
    }
    println!("texel words digest {:016x}", hasher.finish());
}
