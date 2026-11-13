//! Overlapping contours to the outline of what they fill (docs/design.md,
//! items 21 and 22). The decoder sums the area right of every piece, so
//! where contours overlap it counts the area twice. It needs no contours
//! to count once, only pieces whose crossings add up to 0 or 1 everywhere:
//! the pieces cut where they cross, less the parts with the fill on both
//! sides, in closed loops again.
//!
//! On the encoder's grid throughout, in integers wherever something is
//! decided.

use std::collections::HashSet;

use crate::glyph::Quad;

type Pt = [i64; 2];
type Piece = [Pt; 3];

pub enum Union {
    /// Nothing overlaps: the pieces are their outline already.
    Simple,
    /// The outline's pieces, in closed loops, the fill on one hand of all.
    United(Vec<Quad>),
    /// The loops did not close (crossings the grid cannot tell apart):
    /// the pieces stay as they are, overlaps and all.
    Tangled,
}

fn cross(a: Pt, b: Pt) -> i128 {
    a[0] as i128 * b[1] as i128 - a[1] as i128 * b[0] as i128
}

fn minus(a: Pt, b: Pt) -> Pt {
    [a[0] - b[0], a[1] - b[1]]
}

fn float(p: Pt) -> [f64; 2] {
    [p[0] as f64, p[1] as f64]
}

fn at(q: &Piece, t: f64) -> [f64; 2] {
    let u = 1.0 - t;
    [0, 1].map(|i| u * u * q[0][i] as f64 + 2.0 * u * t * q[1][i] as f64 + t * t * q[2][i] as f64)
}

fn length(q: &Piece) -> f64 {
    ((q[2][0] - q[0][0]) as f64).hypot((q[2][1] - q[0][1]) as f64)
}

/// The t of a monotone piece where its coordinate on `axis` is `v` (item
/// 7's root).
fn param(q: &Piece, axis: usize, v: f64) -> f64 {
    let (p0, p1, p2) = (q[0][axis] as f64, q[1][axis] as f64, q[2][axis] as f64);
    let (b, c) = (p0 - p1, p0 - v);
    let d = (b * b - (p0 - 2.0 * p1 + p2) * c).max(0.0).sqrt();
    let sign = if b != 0.0 { b.signum() } else { c.signum() };
    let den = b + sign * d;
    (if den != 0.0 { c / den } else { 0.0 }).clamp(0.0, 1.0)
}

/// The t of piece `q` at point `x` if `x` is on it, by the axis it runs
/// along.
fn on_piece(q: &Piece, x: [f64; 2]) -> Option<f64> {
    let axis = ((q[2][1] - q[0][1]).abs() > (q[2][0] - q[0][0]).abs()) as usize;
    let t = param(q, axis, x[axis]);
    let p = at(q, t);
    ((p[0] - x[0]).abs() < 1e-3 && (p[1] - x[1]).abs() < 1e-3).then_some(t)
}

/// Where piece `q` meets the parabola of piece `p`: a quartic in q's
/// parameter (g[i] s^i), exact. With p(t) = a t^2 + b t + c,
///
/// ```text
/// a x (p - c) = (a x b) t,   b x (p - c) = -(a x b) t^2,
/// F(p) = (a x (p - c))^2 + (a x b) (b x (p - c)) = 0
/// ```
///
/// and q(s) put for p. Grid points are 16 bits, so a coefficient is under
/// 78. A straight `p` (a x b = 0) is its line, and the quartic a quadratic.
fn quartic(p: &Piece, q: &Piece) -> [i128; 5] {
    let coefficients = |q: &Piece| {
        (
            [0, 1].map(|i| q[0][i] - 2 * q[1][i] + q[2][i]),
            [0, 1].map(|i| 2 * (q[1][i] - q[0][i])),
            q[0],
        )
    };
    let (a, b, c) = coefficients(p);
    let (a2, b2, c2) = coefficients(q);
    let rel = minus(c2, c);
    let d = cross(a, b);
    if d == 0 {
        let dir = if b != [0, 0] { b } else { a };
        return [cross(dir, rel), cross(dir, b2), cross(dir, a2), 0, 0];
    }
    let (u2, u1, u0) = (cross(a, a2), cross(a, b2), cross(a, rel));
    let (v2, v1, v0) = (cross(b, a2), cross(b, b2), cross(b, rel));
    [
        u0 * u0 + d * v0,
        2 * u1 * u0 + d * v1,
        u1 * u1 + 2 * u2 * u0 + d * v2,
        2 * u2 * u1,
        u2 * u2,
    ]
}

/// A quartic's Bernstein coefficients over [0, 1], times 12 to stay whole.
fn bernstein(g: &[i128; 5]) -> [i128; 5] {
    [
        12 * g[0],
        12 * g[0] + 3 * g[1],
        12 * g[0] + 6 * g[1] + 2 * g[2],
        12 * g[0] + 9 * g[1] + 6 * g[2] + 3 * g[3],
        12 * (g[0] + g[1] + g[2] + g[3] + g[4]),
    ]
}

/// The roots of a quartic inside an interval, from its Bernstein
/// coefficients there (times a constant): by Descartes' rule no more
/// roots than changes of sign, and fewer by an even number. Halved (de
/// Casteljau by sums alone, so still exact) until one change is left,
/// then bisected in floats on `g`, the quartic itself.
fn roots(c: [i128; 5], g: &[f64; 5], lo: f64, w: f64, depth: u32, out: &mut Vec<f64>) {
    let (mut changes, mut last) = (0, 0);
    for v in c {
        let s = v.signum();
        if s != 0 {
            changes += (last != 0 && s != last) as u32;
            last = s;
        }
    }
    if changes == 0 {
        return;
    }
    let value = |s: f64| g[0] + s * (g[1] + s * (g[2] + s * (g[3] + s * g[4])));
    if changes == 1 {
        let (mut a, mut b) = (lo, lo + w);
        let (fa, fb) = (value(a), value(b));
        if fa * fb < 0.0 {
            for _ in 0..60 {
                let m = 0.5 * (a + b);
                if (value(m) < 0.0) == (fa < 0.0) {
                    a = m;
                } else {
                    b = m;
                }
            }
            out.push(0.5 * (a + b));
            return;
        }
    }
    // No room for another halving (4 bits each), or deep enough: an odd
    // number of roots is one here, an even number a touch.
    if depth >= 24 || c.iter().any(|v| v.unsigned_abs() >> 120 != 0) {
        if changes % 2 == 1 {
            out.push(lo + 0.5 * w);
        }
        return;
    }
    let r1 = [c[0] + c[1], c[1] + c[2], c[2] + c[3], c[3] + c[4]];
    let r2 = [r1[0] + r1[1], r1[1] + r1[2], r1[2] + r1[3]];
    let r3 = [r2[0] + r2[1], r2[1] + r2[2]];
    let mid = r3[0] + r3[1];
    if mid == 0 {
        out.push(lo + 0.5 * w);
    }
    let h = 0.5 * w;
    let left = [16 * c[0], 8 * r1[0], 4 * r2[0], 2 * r3[0], mid];
    let right = [mid, 2 * r3[1], 4 * r2[2], 8 * r1[3], 16 * c[4]];
    roots(left, g, lo, h, depth + 1, out);
    roots(right, g, lo + h, h, depth + 1, out);
}

/// How often a ray from `x` to the right crosses the pieces, up less down.
fn winding(pieces: &[Piece], x: [f64; 2]) -> i32 {
    let mut w = 0;
    for q in pieces {
        let (y0, y1) = (q[0][1].min(q[2][1]) as f64, q[0][1].max(q[2][1]) as f64);
        // Monotone, so within the box of its ends: only a ray that starts
        // inside the box needs the piece itself.
        let (x0, x1) = (q[0][0].min(q[2][0]) as f64, q[0][0].max(q[2][0]) as f64);
        if y0 <= x[1] && x[1] < y1 && x1 > x[0] && (x0 > x[0] || at(q, param(q, 1, x[1]))[0] > x[0])
        {
            w += if q[2][1] > q[0][1] { 1 } else { -1 };
        }
    }
    w
}

/// Where each piece is cut, its parameter and the grid point: where two
/// cross, and where one's end lies on another.
fn cuts(ps: &[Piece]) -> Vec<Vec<(f64, Pt)>> {
    let boxes: Vec<[i64; 4]> = ps
        .iter()
        .map(|q| {
            [
                q[0][0].min(q[2][0]),
                q[0][0].max(q[2][0]),
                q[0][1].min(q[2][1]),
                q[0][1].max(q[2][1]),
            ]
        })
        .collect();
    let mut cuts: Vec<Vec<(f64, Pt)>> = vec![Vec::new(); ps.len()];
    let mut found = Vec::new();
    for i in 0..ps.len() {
        for j in i + 1..ps.len() {
            let (a, b) = (&boxes[i], &boxes[j]);
            if a[0] > b[1] || b[0] > a[1] || a[2] > b[3] || b[2] > a[3] {
                continue;
            }
            let (p, q) = (&ps[i], &ps[j]);
            // Boxes that share one point, an end of both: neighbours along
            // a contour, mostly.
            let corner = [a[0].max(b[0]), a[2].max(b[2])];
            if corner == [a[1].min(b[1]), a[3].min(b[3])]
                && (p[0] == corner || p[2] == corner)
                && (q[0] == corner || q[2] == corner)
            {
                continue;
            }
            let g = quartic(p, q);
            if g == [0; 5] {
                // Along one curve (two contours sharing an edge): each is
                // cut at the other's ends, and the parts are then the same
                // piece or apart.
                for (i, j) in [(i, j), (j, i)] {
                    for x in [ps[j][0], ps[j][2]] {
                        if x != ps[i][0]
                            && x != ps[i][2]
                            && let Some(t) = on_piece(&ps[i], float(x))
                        {
                            cuts[i].push((t, x));
                        }
                    }
                }
                continue;
            }
            let bern = bernstein(&g);
            // An end of q on p, exactly: p is cut there.
            for (x, zero) in [(q[0], bern[0] == 0), (q[2], bern[4] == 0)] {
                if zero
                    && x != p[0]
                    && x != p[2]
                    && let Some(t) = on_piece(p, float(x))
                {
                    cuts[i].push((t, x));
                }
            }
            found.clear();
            roots(bern, &g.map(|v| v as f64), 0.0, 1.0, 0, &mut found);
            for &s in &found {
                let x = at(q, s);
                let Some(t) = on_piece(p, x) else {
                    continue;
                };
                // Both at one grid point: an end within half a step, or the
                // nearest point.
                let near =
                    |e: Pt| (e[0] as f64 - x[0]).abs() <= 0.5 && (e[1] as f64 - x[1]).abs() <= 0.5;
                let point = [p[0], p[2], q[0], q[2]]
                    .into_iter()
                    .find(|&e| near(e))
                    .unwrap_or([x[0].round() as i64, x[1].round() as i64]);
                cuts[i].push((t, point));
                cuts[j].push((s, point));
            }
        }
    }
    cuts
}

/// The outline of what `pieces` (monotone, on the grid, contour after
/// contour) fill under the nonzero rule.
pub fn unite(pieces: &[Quad]) -> Union {
    let ps: Vec<Piece> = pieces
        .iter()
        .map(|q| q.map(|p| [p[0] as i64, p[1] as i64]))
        .collect();
    let mut cuts = cuts(&ps);
    let cut_any = cuts.iter().any(|c| !c.is_empty());
    // Where something meets a contour: the cuts, and a point two pieces
    // start from (contours through one vertex, which no cut says).
    let mut met: HashSet<Pt> = cuts.iter().flatten().map(|c| c.1).collect();
    let mut seen = HashSet::new();
    met.extend(ps.iter().map(|q| q[0]).filter(|&p| !seen.insert(p)));
    // The parts, each with the blossom of its ends for a control point,
    // back on the grid and inside the box of its ends (monotone still).
    // And where a run of parts starts: a contour's first, and after each
    // point something meets it at. Along a run the fill stays on the same
    // sides.
    let mut parts: Vec<Piece> = Vec::new();
    let mut starts: Vec<usize> = Vec::new();
    let mut end: Option<Pt> = None;
    for (q, cuts) in ps.iter().zip(&mut cuts) {
        cuts.sort_by(|a, b| a.0.total_cmp(&b.0));
        cuts.push((1.0, q[2]));
        let (mut t0, mut p0) = (0.0, q[0]);
        for &(t1, p1) in cuts.iter() {
            if p1 == p0 {
                continue;
            }
            let w = [
                (1.0 - t0) * (1.0 - t1),
                (1.0 - t0) * t1 + t0 * (1.0 - t1),
                t0 * t1,
            ];
            let control = [0, 1].map(|i| {
                let v = w[0] * q[0][i] as f64 + w[1] * q[1][i] as f64 + w[2] * q[2][i] as f64;
                (v.round() as i64).clamp(p0[i].min(p1[i]), p0[i].max(p1[i]))
            });
            if end != Some(p0) || met.contains(&p0) {
                starts.push(parts.len());
            }
            end = Some(p1);
            parts.push([p0, control, p1]);
            (t0, p0) = (t1, p1);
        }
    }
    // A run stays if the fill is on one side of it only: asked a quarter
    // of a step off the middle of its longest part, the farthest from
    // where others come near. With each part, whether the fill is on its
    // right hand.
    let mut kept: Vec<(Piece, bool)> = Vec::new();
    let mut dropped = false;
    starts.push(parts.len());
    for run in starts.windows(2) {
        let run = &parts[run[0]..run[1]];
        let Some(q) = run.iter().max_by(|a, b| length(a).total_cmp(&length(b))) else {
            continue;
        };
        let m = at(q, 0.5);
        let k = 0.25 / length(q);
        let n = [
            k * (q[2][1] - q[0][1]) as f64,
            -k * (q[2][0] - q[0][0]) as f64,
        ];
        let right = winding(&parts, [m[0] + n[0], m[1] + n[1]]) != 0;
        let left = winding(&parts, [m[0] - n[0], m[1] - n[1]]) != 0;
        if right != left {
            kept.extend(run.iter().map(|q| (*q, right)));
        } else {
            dropped = true;
        }
    }
    // The fill on the hand most of the outline has it on, the rest turned
    // round.
    let on_right: f64 = kept
        .iter()
        .map(|(q, right)| if *right { length(q) } else { -length(q) })
        .sum();
    let hand = on_right >= 0.0;
    let turned = kept.iter().any(|k| k.1 != hand);
    if !cut_any && !dropped && !turned {
        return Union::Simple;
    }
    let mut kept: Vec<Piece> = kept
        .into_iter()
        .map(
            |(q, right)| {
                if right == hand { q } else { [q[2], q[1], q[0]] }
            },
        )
        .collect();
    // Two contours along one edge leave it twice, the controls a step
    // apart at most (each rounded on its own).
    kept.sort_by_key(|q| (q[0], q[2], q[1]));
    kept.dedup_by(|b, a| {
        a[0] == b[0] && a[2] == b[2] && (0..2).all(|i| (a[1][i] - b[1][i]).abs() <= 1)
    });
    // Into loops: after each piece one that starts where it ends.
    let mut used = vec![false; kept.len()];
    let mut out: Vec<Quad> = Vec::with_capacity(kept.len());
    for first in 0..kept.len() {
        if used[first] {
            continue;
        }
        let (start, mut cur) = (kept[first][0], first);
        loop {
            used[cur] = true;
            out.push(kept[cur].map(|p| [p[0] as f32, p[1] as f32]));
            let end = kept[cur][2];
            if end == start {
                break;
            }
            let Some(next) = (0..kept.len()).find(|&k| !used[k] && kept[k][0] == end) else {
                return Union::Tangled;
            };
            cur = next;
        }
    }
    Union::United(out)
}
