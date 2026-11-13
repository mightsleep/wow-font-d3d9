//! A glyph's exact area in each texel, from its outline cut into chords:
//! the fill of v2's cover bytes (atlas::cover_cell) and the bench's truth.
//! Two ways: `coverage` under the nonzero rule whatever the contours do,
//! `accumulated` several times faster for contours that do not overlap.

use crate::glyph::{self, P, Quad};

/// How far a curve's chords may stray from it, in texels.
const FLAT_TOL: f32 = 1.0 / 256.0;
const MAX_CHORDS: u32 = 32;

fn add(a: P, b: P) -> P {
    [a[0] + b[0], a[1] + b[1]]
}

fn sub(a: P, b: P) -> P {
    [a[0] - b[0], a[1] - b[1]]
}

fn dot(a: P, b: P) -> f32 {
    a[0] * b[0] + a[1] * b[1]
}

/// Placed contours as closed polylines, each quadratic in as many chords
/// as keep it within FLAT_TOL: it strays from n equal chords by at most
/// |a - 2b + c| / 4n^2, so a line takes one.
pub fn polylines(contours: &[Vec<Quad>]) -> Vec<Vec<P>> {
    contours
        .iter()
        .filter(|c| !c.is_empty())
        .map(|c| {
            let mut line = vec![c[0][0]];
            for q in c {
                let k = sub(add(q[0], q[2]), [2.0 * q[1][0], 2.0 * q[1][1]]);
                let n = ((dot(k, k).sqrt() / (4.0 * FLAT_TOL)).sqrt().ceil() as u32)
                    .clamp(1, MAX_CHORDS);
                line.extend((1..=n).map(|k| glyph::point(q, k as f32 / n as f32)));
            }
            // The last point is the first again.
            line.pop();
            line.dedup();
            line
        })
        .filter(|l| l.len() >= 2)
        .collect()
}

/// The glyph's area in each texel under the nonzero rule. Summing signed
/// areas (Green) and taking abs once counts overlapping contours twice
/// (MORPHEUS, variable fonts), so instead each texel row is cut at every
/// segment end into bands where the crossings are linear in y and keep
/// their order, and only crossings where the winding turns between zero
/// and nonzero count: each adds the integral of its x clamped to a column,
/// in closed form, to every column.
pub fn coverage(lines: &[Vec<P>], w: u32, h: u32) -> Vec<f32> {
    // Segments crossing rows: y low, y high, x at y low, dx/dy, direction.
    let segs: Vec<(f64, f64, f64, f64, i32)> = segments(lines)
        .filter(|(a, b)| a[1] != b[1])
        .map(|(a, b)| {
            let (a, b, dir) = if a[1] < b[1] { (a, b, 1) } else { (b, a, -1) };
            let (ax, ay, bx, by) = (a[0] as f64, a[1] as f64, b[0] as f64, b[1] as f64);
            (ay, by, ax, (bx - ax) / (by - ay), dir)
        })
        .collect();
    let mut cov = vec![0f64; (w * h) as usize];
    let (mut cuts, mut xs) = (Vec::new(), Vec::new());
    let mut near: Vec<(f64, f64, f64, f64, i32)> = Vec::new();
    // Whole columns left of a crossing, as differences: the row's prefix
    // sum hands each its share.
    let mut whole = vec![0f64; w as usize + 1];
    for r in 0..h {
        let (r0, r1) = (r as f64, r as f64 + 1.0);
        near.clear();
        near.extend(segs.iter().filter(|s| s.0 < r1 && s.1 > r0));
        cuts.clear();
        cuts.extend([r0, r1]);
        cuts.extend(
            near.iter()
                .flat_map(|s| [s.0, s.1])
                .filter(|&y| y > r0 && y < r1),
        );
        cuts.sort_by(f64::total_cmp);
        cuts.dedup();
        let row = &mut cov[(r * w) as usize..((r + 1) * w) as usize];
        whole.fill(0.0);
        for band in cuts.windows(2) {
            band_cover(&near, band[0], band[1], row, &mut whole, &mut xs, 0);
        }
        let mut sum = 0.0;
        for (cell, d) in row.iter_mut().zip(&whole) {
            sum += d;
            *cell += sum;
        }
    }
    cov.into_iter().map(|c| c.clamp(0.0, 1.0) as f32).collect()
}

fn band_cover(
    segs: &[(f64, f64, f64, f64, i32)],
    ya: f64,
    yb: f64,
    row: &mut [f64],
    whole: &mut [f64],
    xs: &mut Vec<(f64, f64, f64, i32)>,
    depth: u32,
) {
    if yb - ya < 1e-9 {
        return;
    }
    let ym = 0.5 * (ya + yb);
    xs.clear();
    for &(y0, y1, x0, slope, dir) in segs {
        if y0 < ym && y1 > ym {
            let at = |y: f64| x0 + (y - y0) * slope;
            xs.push((at(ya), at(yb), at(ym), dir));
        }
    }
    xs.sort_by(|a, b| a.2.total_cmp(&b.2));
    let crossed = xs
        .windows(2)
        .any(|p| p[0].0 > p[1].0 + 1e-9 || p[0].1 > p[1].1 + 1e-9);
    if crossed && depth < 12 {
        // Contours cross inside the band: halves keep the order but for a sliver.
        band_cover(segs, ya, ym, row, whole, xs, depth + 1);
        band_cover(segs, ym, yb, row, whole, xs, depth + 1);
        return;
    }
    // The antiderivative of clamp(u, 0, 1).
    let f = |u: f64| match u {
        u if u <= 0.0 => 0.0,
        u if u >= 1.0 => u - 0.5,
        u => 0.5 * u * u,
    };
    let mut winding = 0;
    for &(xa, xb, _, dir) in xs.iter() {
        let before = winding;
        winding += dir;
        if (before == 0) == (winding == 0) {
            continue;
        }
        // Entering takes the area right of x off, leaving adds it.
        let sign = if before == 0 { -1.0 } else { 1.0 };
        // Columns wholly left of the crossing (c + 1 <= x) take all of it,
        // those right (c >= x) none: only the ones it passes through need
        // the integral.
        let w = row.len();
        let full = xa.min(xb).floor().clamp(0.0, w as f64) as usize;
        let end = (xa.max(xb).ceil().clamp(0.0, w as f64) as usize).max(full);
        whole[0] += sign * (yb - ya);
        whole[full] -= sign * (yb - ya);
        for (c, cell) in row.iter_mut().enumerate().take(end).skip(full) {
            let (u0, u1) = (xa - c as f64, xb - c as f64);
            let mean = if (u1 - u0).abs() > 1e-12 {
                (f(u1) - f(u0)) / (u1 - u0)
            } else {
                u0.clamp(0.0, 1.0)
            };
            *cell += sign * mean * (yb - ya);
        }
    }
}

/// A chord's part within a texel row, from x0 to x1 (in 0..=w, either
/// way round), `d` its height there, signed: the area right of it to the
/// columns it passes through, and the rest of its height to the one after
/// (accumulated's prefix sum hands that on).
fn chord_in_row(row: &mut [f64], x0: f64, x1: f64, d: f64) {
    let (xl, xr) = (x0.min(x1), x0.max(x1));
    let (fl, cr) = (xl.floor(), xr.ceil());
    let (il, ir) = (fl as usize, cr as usize);
    if ir <= il + 1 {
        // One column: its mean x says how much of it is right of the chord.
        let xm = 0.5 * (xl + xr) - fl;
        row[il] += d - d * xm;
        row[il + 1] += d * xm;
        return;
    }
    // Per column the chord rises 1 / (xr - xl) of its height: a triangle
    // in the first, trapezoids between, and what is left of the last.
    let s = 1.0 / (xr - xl);
    let f0 = xl - fl;
    let a0 = 0.5 * s * (1.0 - f0) * (1.0 - f0);
    let f1 = xr - cr + 1.0;
    let am = 0.5 * s * f1 * f1;
    row[il] += d * a0;
    if ir == il + 2 {
        row[il + 1] += d * (1.0 - a0 - am);
    } else {
        let a1 = s * (1.5 - f0);
        row[il + 1] += d * (a1 - a0);
        for v in &mut row[il + 2..ir - 1] {
            *v += d * s;
        }
        let a2 = a1 + (ir - il - 3) as f64 * s;
        row[ir - 1] += d * (1.0 - a2 - am);
    }
    row[ir] += d * am;
}

/// The glyph's area in each texel for contours that do not overlap, by
/// signed accumulation (docs/design.md, item 22): each chord adds, per
/// texel row it crosses, the area right of it, and a prefix sum along the
/// row hands every column further right its height. The sum is the area
/// times the winding, so the area itself only while no two contours
/// overlap; then it is `coverage`'s to within rounding, with no sorting
/// and no slabs: the cost goes with chords times rows.
pub fn accumulated(lines: &[Vec<P>], w: u32, h: u32) -> Vec<f32> {
    let (wf, stride) = (w as f64, w as usize + 2);
    let mut acc = vec![0f64; stride * h as usize];
    for (a, b) in segments(lines).filter(|(a, b)| a[1] != b[1]) {
        let (a, b, dir) = if a[1] < b[1] {
            (a, b, 1.0)
        } else {
            (b, a, -1.0)
        };
        let (ax, ay, bx, by) = (a[0] as f64, a[1] as f64, b[0] as f64, b[1] as f64);
        let slope = (bx - ax) / (by - ay);
        let first = ay.max(0.0).floor() as u32;
        let last = by.ceil().clamp(0.0, h as f64) as u32;
        for r in first..last {
            let (ya, yb) = (ay.max(r as f64), by.min(r as f64 + 1.0));
            if yb <= ya {
                continue;
            }
            let row = &mut acc[r as usize * stride..][..stride];
            let (x0, x1) = (ax + (ya - ay) * slope, ax + (yb - ay) * slope);
            let (lo, hi) = (x0.min(x1), x0.max(x1));
            let d = (yb - ya) * dir;
            // Left of the cell everything is right of the chord: its
            // height to the first column. Right of the cell, nothing.
            if hi <= 0.0 {
                row[0] += d;
                continue;
            }
            if lo >= wf {
                continue;
            }
            let (cl, ch) = (lo.max(0.0), hi.min(wf));
            if hi > lo {
                row[0] += d * (cl - lo) / (hi - lo);
                chord_in_row(row, cl, ch, d * (ch - cl) / (hi - lo));
            } else {
                chord_in_row(row, cl, ch, d);
            }
        }
    }
    let mut out = Vec::with_capacity((w * h) as usize);
    for row in acc.chunks_exact(stride) {
        let mut sum = 0.0;
        for v in &row[..w as usize] {
            sum += v;
            out.push(sum.abs().min(1.0) as f32);
        }
    }
    out
}

fn segments(lines: &[Vec<P>]) -> impl Iterator<Item = (P, P)> + '_ {
    lines
        .iter()
        .flat_map(|l| (0..l.len()).map(move |s| (l[s], l[(s + 1) % l.len()])))
}
