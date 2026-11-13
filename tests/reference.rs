//! The reference decoder: the pixel shader (shaders/decoder.hlsl) line for
//! line in f32 on the CPU, against which the tests hold the encoder.

use crate::atlas::{COARSE, PACK, Record, WORD_LIST, WORD_NONE, at_t, t_at};
use crate::glyph::{P, Quad};

fn unpack(v: f32) -> P {
    let x = (v / PACK).floor();
    [x, v - x * PACK]
}

/// A piece back from its texel, as the shader takes it apart: the low
/// nibbles come off the fourth float first coordinate first.
fn piece(v: [f32; 4]) -> Quad {
    let mut low = v[3];
    let mut nibble = || {
        let rest = (low / COARSE).floor();
        let n = low - rest * COARSE;
        low = rest;
        n
    };
    let mut q = [[0.0; 2]; 3];
    for (k, p) in q.iter_mut().enumerate() {
        let h = unpack(v[k]);
        p[0] = h[0] * COARSE + nibble();
        p[1] = h[1] * COARSE + nibble();
    }
    q
}

/// The integral of (x - x(t)) y'(t) over the part of [lo, hi] where x(t)
/// < x: [lo, hi], nothing, or one end cut at the root. The integrand is a
/// cubic, which two-point Gauss-Legendre integrates exactly.
fn half_plane(kx: [f32; 3], ky: [f32; 3], lo: f32, hi: f32, x: f32) -> f32 {
    let tx = t_at(kx, x).clamp(lo, hi);
    let t1 = if at_t(kx, lo) < x { lo } else { tx };
    let t2 = if at_t(kx, hi) < x { hi } else { tx };
    let (m, h) = (0.5 * (t1 + t2), 0.5 * (t2 - t1));
    let f = |t: f32| (x - at_t(kx, t)) * (ky[1] + 2.0 * ky[2] * t);
    h * (f(m - h * 0.577_350_27) + f(m + h * 0.577_350_27))
}

/// The signed area of the pixel [-hx, hx] x rows `yr` (relative to the
/// sample) right of one piece, by the piece's direction in y.
fn area_right(q: Quad, hx: f32, yr: [f32; 2]) -> f32 {
    let [p1, p2, p3] = q;
    let kx = [p1[0], 2.0 * (p2[0] - p1[0]), p1[0] - 2.0 * p2[0] + p3[0]];
    let ky = [p1[1], 2.0 * (p2[1] - p1[1]), p1[1] - 2.0 * p2[1] + p3[1]];
    let ya = yr[0].max(p1[1].min(p3[1]));
    let yb = ya.max(yr[1].min(p1[1].max(p3[1])));
    let (ta, tb) = (t_at(ky, ya), t_at(ky, yb));
    let (lo, hi) = (ta.min(tb), ta.max(tb));
    half_plane(kx, ky, lo, hi, hx) - half_plane(kx, ky, lo, hi, -hx)
}

/// Coverage of the pixel centred at `c` with half sizes `h` (grid steps),
/// as an exact box filter. Per band the pixel reaches,
/// over the rows they share (a piece in two bands counts once): left of
/// the band's split, the area right of each piece, walking by smallest x
/// until a piece lies wholly right of the pixel (it and the rest add
/// nothing); right of the split, mirrored, the area left of each piece,
/// walking by largest x. The sum over a band is the area times the
/// winding, the same sign throughout for outlines whose contours do not
/// overlap, so each band adds its absolute value and the total is clamped.
pub fn coverage(r: &Record, c: P, h: [f32; 2]) -> f32 {
    let t = &r.texels;
    let [b, band_h, _, _] = t[0];
    let b0 = ((c[1] - h[1]) / band_h).floor().clamp(0.0, b - 1.0) as usize;
    let b1 = ((c[1] + h[1]) / band_h).floor().clamp(0.0, b - 1.0) as usize;
    let mut total = 0.0;
    for k in b0..=b1 {
        let [first, mid, reach, span] = t[1 + k];
        let (y0, y1) = (k as f32 * band_h, (k + 1) as f32 * band_h);
        let yr = [(y0 - c[1]).max(-h[1]), (y1 - c[1]).min(h[1])];
        if yr[0] >= yr[1] {
            continue;
        }
        // The pixel's columns wholly outside the band's reach, or wholly in
        // its span: no piece read.
        let (x0, x1) = (c[0] - h[0], c[0] + h[0]);
        let [rl, rr] = unpack(reach).map(|v| v * COARSE);
        let rr = rr + COARSE;
        if x1 <= rl || x0 >= rr {
            continue;
        }
        let [s0, s1] = unpack(span.abs()).map(|v| v * COARSE);
        if x0 >= s0 && x1 <= s1 {
            if span > 0.0 {
                total += (yr[1] - yr[0]) * 2.0 * h[0];
            }
            continue;
        }
        let [asc, n] = unpack(first);
        let desc = asc + n;
        let right = c[0] >= mid;
        let start = if right { desc } else { asc };
        let mut sum = 0.0;
        for i in 0..n as usize {
            let v = t[start as usize + i];
            let q = piece(v);
            let rel = q.map(|p| [p[0] - c[0], p[1] - c[1]]);
            let (lo, hi) = (rel[0][0].min(rel[2][0]), rel[0][0].max(rel[2][0]));
            if right {
                if hi <= -h[0] {
                    break;
                }
                sum += area_right(rel.map(|p| [-p[0], p[1]]), h[0], yr);
            } else {
                if lo >= h[0] {
                    break;
                }
                sum += area_right(rel, h[0], yr);
            }
        }
        total += f32::abs(sum);
    }
    (total / (4.0 * h[0] * h[1])).min(1.0)
}

/// The distance in grid steps from `p` to the nearest piece, each through
/// 16 chords (a monotone piece strays from them by under a hundredth of a
/// texel at the sizes the client draws), or `cutoff` if none is nearer: a
/// piece whose box (its three points') lies farther is not looked at.
pub fn distance(r: &Record, p: P, cutoff: f32) -> f32 {
    let seg = |a: P, b: P| {
        let e = [b[0] - a[0], b[1] - a[1]];
        let l = e[0] * e[0] + e[1] * e[1];
        let t = if l > 0.0 {
            (((p[0] - a[0]) * e[0] + (p[1] - a[1]) * e[1]) / l).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let d = [a[0] + e[0] * t - p[0], a[1] + e[1] * t - p[1]];
        d[0] * d[0] + d[1] * d[1]
    };
    let mut best = cutoff * cutoff;
    for q in &r.outline {
        let lo = [0, 1].map(|a| q[0][a].min(q[1][a]).min(q[2][a]) - p[a]);
        let hi = [0, 1].map(|a| q[0][a].max(q[1][a]).max(q[2][a]) - p[a]);
        let out = [0, 1].map(|a| lo[a].max(-hi[a]).max(0.0));
        if out[0] * out[0] + out[1] * out[1] >= best {
            continue;
        }
        for i in 0..16 {
            let d = seg(
                crate::glyph::point(q, i as f32 / 16.0),
                crate::glyph::point(q, (i + 1) as f32 / 16.0),
            );
            best = best.min(d);
        }
    }
    best.sqrt()
}

/// The reference decoder of a texel word, as the shader reads it: the
/// pixel's coverage and its distance to the nearest piece of the runs (in
/// grid steps, at most `cutoff`), or None for WORD_BANDS.
pub fn word_coverage(
    r: &Record,
    word: [u16; 4],
    c: P,
    h: [f32; 2],
    cutoff: f32,
) -> Option<(f32, f32)> {
    let d = word[3] as u32;
    let (state, right, backdrop) = (d >> 10, (d >> 9) & 1, ((d >> 6) & 7) as f32 - 4.0);
    match state as u16 {
        WORD_NONE => return Some((if backdrop != 0.0 { 1.0 } else { 0.0 }, cutoff)),
        WORD_LIST => {}
        // The bands, and anything a word never says.
        _ => return None,
    }
    let sx = if right == 1 { -1.0 } else { 1.0 };
    let mut sum = backdrop * 4.0 * h[0] * h[1];
    let mut best = cutoff * cutoff;
    for j in 0..3 {
        let start = (word[j] >> 4) as usize;
        let count = (word[j] & 15) as usize + 16 * ((d >> (2 * j)) & 3) as usize;
        for i in start..start + count {
            let q = piece(r.texels[i]);
            let rel = q.map(|p| [sx * (p[0] - c[0]), p[1] - c[1]]);
            sum += area_right(rel, h[0], [-h[1], h[1]]);
            for t in 0..16 {
                let (a, b) = (
                    crate::glyph::point(&rel, t as f32 / 16.0),
                    crate::glyph::point(&rel, (t + 1) as f32 / 16.0),
                );
                let e = [b[0] - a[0], b[1] - a[1]];
                let l = e[0] * e[0] + e[1] * e[1];
                let u = if l > 0.0 {
                    (-(a[0] * e[0] + a[1] * e[1]) / l).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let p = [a[0] + e[0] * u, a[1] + e[1] * u];
                best = best.min(p[0] * p[0] + p[1] * p[1]);
            }
        }
    }
    Some(((sum.abs() / (4.0 * h[0] * h[1])).min(1.0), best.sqrt()))
}
