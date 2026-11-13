//! The atlas records of format v2, the cover bytes and the texel words,
//! for shaders/decoder.hlsl (why: docs/design.md, items 5 to 16; the decoder
//! on the CPU: tests/reference.rs).
//!
//! A record, texels of four floats (A32B32G32R32F), grid y up:
//!   0          bands, band height, where the ring starts, its length
//!   1 ..= B    per band: first list * 4096 + count (the second list right
//!              after), the split x, reach l * 4096 + r and span s0 * 4096
//!              + s1 in coarse steps of 16 (span signed by its winding)
//!   lists      one piece a texel: top 12 bits of each point as x * 4096 +
//!              y, the low 4 bits of all six in the fourth float
//!   ring       every piece in outline order, twice (texel_words)

use crate::glyph::{Quad, P};

/// Grid steps across the outline's larger side: 16-bit coordinates.
pub const GRID: f32 = 65536.0;
/// Two 12-bit integers to a float: a * PACK + b, at most 2^24 - 1.
pub const PACK: f32 = 4096.0;
/// Grid steps a step of the coarse 12-bit coordinates (reach, span).
pub const COARSE: f32 = GRID / PACK;
/// Bands at most; the decoder walks at most this many per pixel.
pub const BANDS_MAX: usize = 16;

/// The outline's radius in texels for the client's `outline` and the
/// setting's extra, in quarters.
pub fn outline_radius(outline: u32, extra: f32) -> f32 {
    if outline == 0 {
        0.0
    } else {
        ((outline as f32 + extra) * 4.0).round().clamp(1.0, 15.0) / 4.0
    }
}

pub struct Record {
    pub texels: Vec<[f32; 4]>,
    /// Outline units to grid steps: (v - origin) * scale.
    pub origin: P,
    pub scale: f32,
    /// The pieces in grid steps, contour by contour (not uploaded).
    pub outline: Vec<Quad>,
}

/// Split a quadratic at its extremum along `axis` if inside it, so that
/// each part is monotone there. At the extremum the tangent is parallel
/// to the other axis, so both new control points take the split point's
/// coordinate exactly, and rounding to the grid keeps them in order.
fn split(q: Quad, axis: usize, out: &mut Vec<Quad>) {
    let [p0, c, p1] = q;
    let a = p0[axis] - 2.0 * c[axis] + p1[axis];
    let t = if a != 0.0 {
        (p0[axis] - c[axis]) / a
    } else {
        -1.0
    };
    if t > 0.0 && t < 1.0 {
        let lerp = |u: P, v: P| [u[0] + (v[0] - u[0]) * t, u[1] + (v[1] - u[1]) * t];
        let (mut l, mut r) = (lerp(p0, c), lerp(c, p1));
        let m = lerp(l, r);
        l[axis] = m[axis];
        r[axis] = m[axis];
        out.extend([[p0, l, m], [m, r, p1]]);
    } else {
        out.push(q);
    }
}

/// Monotone in y, then in x; splitting a monotone piece keeps it so.
fn monotone(c: &[Quad]) -> Vec<Quad> {
    let mut y = Vec::new();
    c.iter().for_each(|&q| split(q, 1, &mut y));
    let mut xy = Vec::new();
    y.into_iter().for_each(|q| split(q, 0, &mut xy));
    xy
}

fn pack(p: P) -> f32 {
    p[0] * PACK + p[1]
}

/// A piece's texel: the top 12 bits of each point's coordinates as pairs,
/// then the low 4 bits of all six, 24 bits, in the fourth float.
fn piece_texel(q: &Quad) -> [f32; 4] {
    let hi = |p: P| pack([(p[0] / COARSE).floor(), (p[1] / COARSE).floor()]);
    let low = q
        .iter()
        .flat_map(|p| [p[0], p[1]])
        .rev()
        .fold(0.0, |acc, v| {
            acc * COARSE + (v - (v / COARSE).floor() * COARSE)
        });
    [hi(q[0]), hi(q[1]), hi(q[2]), low]
}

/// The record of an outline in sqrt(pieces) bands (which measured best);
/// None for an empty one or one too big for the packing. `scale` (grid
/// steps an outline unit) is one for a whole font, so that going from the
/// cache's texels to the grid is one constant a draw; None fits this
/// outline alone onto the grid. `outline` keeps the pieces that add no
/// area in the lists too, for an outlined font's distance.
pub fn encode(contours: &[Vec<Quad>], scale: Option<f32>, outline: bool) -> Option<Record> {
    let pts = contours.iter().flatten().flatten();
    let (mut lo, mut hi) = ([f32::MAX; 2], [f32::MIN; 2]);
    for p in pts {
        for a in 0..2 {
            lo[a] = lo[a].min(p[a]);
            hi[a] = hi[a].max(p[a]);
        }
    }
    let side = (hi[0] - lo[0]).max(hi[1] - lo[1]);
    if side.is_nan() || side <= 0.0 {
        return None;
    }
    let scale = scale.unwrap_or((GRID - 1.0) / side);
    // Relative: a 16-bit grid is past f32's absolute 1e-3 there.
    if side * scale > (GRID - 1.0) * (1.0 + 1e-6) {
        return None;
    }
    let grid = |p: P| {
        [
            ((p[0] - lo[0]) * scale).round(),
            ((p[1] - lo[1]) * scale).round(),
        ]
    };
    let pieces: Vec<(Quad, bool)> = contours
        .iter()
        .flat_map(|c| monotone(c))
        .map(|q| q.map(grid))
        .filter(|q| q[0] != q[2] || q[0] != q[1])
        .map(|q| {
            // A line's control is at its midpoint to within the rounding.
            let line = (0..2).all(|a| (2.0 * q[1][a] - q[0][a] - q[2][a]).abs() <= 1.0);
            (q, line)
        })
        .collect();
    let height = pieces
        .iter()
        .flat_map(|(q, _)| q.iter().map(|p| p[1]))
        .fold(0.0f32, f32::max)
        + 1.0;
    let b = ((pieces.len() as f32).sqrt().ceil() as usize).clamp(1, BANDS_MAX);
    let band_h = (height / b as f32).ceil();
    // Monotone in x: the extremes are at the ends.
    let min_x = |q: &Quad| q[0][0].min(q[2][0]);
    let max_x = |q: &Quad| q[0][0].max(q[2][0]);
    let mut bands_out = Vec::with_capacity(b);
    for k in 0..b {
        let (y0, y1) = (k as f32 * band_h, (k + 1) as f32 * band_h);
        let mut ids: Vec<usize> = (0..pieces.len())
            .filter(|&i| {
                let q = &pieces[i].0;
                q[0][1].min(q[2][1]) <= y1 && q[0][1].max(q[2][1]) >= y0
            })
            .collect();
        let left = ids
            .iter()
            .map(|&i| min_x(&pieces[i].0))
            .fold(GRID, f32::min);
        let right = ids.iter().map(|&i| max_x(&pieces[i].0)).fold(0.0, f32::max);
        ids.sort_by(|&i, &j| min_x(&pieces[i].0).total_cmp(&min_x(&pieces[j].0)));
        let mut by_max = ids.clone();
        by_max.sort_by(|&i, &j| max_x(&pieces[j].0).total_cmp(&max_x(&pieces[i].0)));
        let split = ((left + right) * 0.5).floor();
        let (reach, span) = band_spans(&pieces, &ids, y0, y1);
        // Into the lists only pieces that add area: the area right of a
        // piece is the integral of (x - x(t)) y'(t), nothing for a
        // horizontal one (y' = 0) and nothing for one that shares only a
        // boundary row with the band. They still bound the reach and the
        // span above. For an outlined font all go in: the outline is the
        // distance to every piece.
        let adds = |&i: &usize| {
            let q = &pieces[i].0;
            let (lo, hi) = (q[0][1].min(q[2][1]), q[0][1].max(q[2][1]));
            (outline && lo <= y1 && hi >= y0) || (lo < hi && lo < y1 && hi > y0)
        };
        let ids: Vec<usize> = ids.into_iter().filter(adds).collect();
        let by_max: Vec<usize> = by_max.into_iter().filter(adds).collect();
        bands_out.push((ids, by_max, split, reach, span));
    }
    let texel = |i: &usize| piece_texel(&pieces[*i].0);
    let mut texels = vec![[b as f32, band_h, 0.0, 0.0]];
    let mut at = 1 + b;
    for (asc, _, split, reach, span) in &bands_out {
        // The first list and the count in one float, the second list
        // right after the first.
        if at >= PACK as usize || asc.len() >= PACK as usize {
            return None;
        }
        let first = at as f32 * PACK + asc.len() as f32;
        // The reach in coarse steps, outwards: the right end as the step
        // it falls in, read back as that step's end (the ceiling of 65535
        // would be 4096, past 12 bits).
        let reach = pack([(reach.0 / COARSE).floor(), (reach.1 / COARSE).floor()]);
        texels.push([first, *split, reach, *span]);
        at += 2 * asc.len();
    }
    for (asc, desc, ..) in &bands_out {
        texels.extend(asc.iter().chain(desc.iter()).map(texel));
    }
    // The ring: every piece in outline order, twice, so that a run from
    // anywhere in the first copy reads on without wrapping (texel_words).
    texels[0][2] = texels.len() as f32;
    texels[0][3] = pieces.len() as f32;
    for _ in 0..2 {
        texels.extend(pieces.iter().map(|(q, _)| piece_texel(q)));
    }
    Some(Record {
        texels,
        origin: lo,
        scale,
        outline: pieces.into_iter().map(|(q, _)| q).collect(),
    })
}

/// The x range of piece q's part within rows [y0, y1]: monotone in y and
/// in x, so the part's ends are its extremes.
fn extent_in(q: &Quad, y0: f32, y1: f32) -> Option<(f32, f32)> {
    let (qy0, qy1) = (q[0][1].min(q[2][1]), q[0][1].max(q[2][1]));
    let ya = y0.max(qy0);
    let yb = y1.min(qy1);
    if ya > yb {
        return None;
    }
    // Wholly within the rows (a horizontal piece always is, and has no one
    // t for its y): end to end.
    if ya == qy0 && yb == qy1 {
        return Some((q[0][0].min(q[2][0]), q[0][0].max(q[2][0])));
    }
    let (kx, ky) = coefficients(q);
    let (xa, xb) = (at_t(kx, t_at(ky, ya)), at_t(kx, t_at(ky, yb)));
    Some((xa.min(xb), xa.max(xb)))
}

/// A band's reach (where its pieces are, in whole grid steps outwards)
/// and its widest gap between them, the span (item 11), packed: positive
/// inside the outline, negative outside it (a counter), 0 for none. In a
/// gap no curve crosses the band, so the winding is the same all over it;
/// it is taken at the gap's middle by counting the crossings right of it
/// on the band's middle row.
fn band_spans(pieces: &[(Quad, bool)], ids: &[usize], y0: f32, y1: f32) -> ((f32, f32), f32) {
    let mut ext: Vec<(f32, f32)> = ids
        .iter()
        .filter_map(|&i| extent_in(&pieces[i].0, y0, y1))
        .collect();
    if ext.is_empty() {
        return ((GRID - 1.0, 0.0), 0.0);
    }
    ext.sort_by(|a, b| a.0.total_cmp(&b.0));
    let reach = (
        ext[0].0.floor().max(0.0),
        ext.iter()
            .map(|e| e.1)
            .fold(0.0, f32::max)
            .ceil()
            .min(GRID - 1.0),
    );
    let mut best = (0.0f32, 0.0f32);
    let mut end = ext[0].1;
    for e in &ext[1..] {
        if e.0 > end && e.0 - end > best.1 - best.0 {
            best = (end, e.0);
        }
        end = end.max(e.1);
    }
    let (s0, s1) = (best.0.ceil(), best.1.floor());
    if s1 <= s0 {
        return (reach, 0.0);
    }
    let (xm, ym) = (0.5 * (s0 + s1), 0.5 * (y0 + y1));
    let winding: f32 = ids
        .iter()
        .map(|&i| &pieces[i].0)
        .filter(|q| q[0][1].min(q[2][1]) <= ym && ym < q[0][1].max(q[2][1]))
        .filter(|q| {
            let (kx, ky) = coefficients(q);
            at_t(kx, t_at(ky, ym)) > xm
        })
        .map(|q| (q[2][1] - q[0][1]).signum())
        .sum();
    // In coarse steps, inwards.
    let (c0, c1) = ((s0 / COARSE).ceil(), (s1 / COARSE).floor());
    if c1 <= c0 {
        return (reach, 0.0);
    }
    let span = pack([c0, c1]);
    (reach, if winding != 0.0 { span } else { -span })
}

fn coefficients(q: &Quad) -> ([f32; 3], [f32; 3]) {
    let [p1, p2, p3] = *q;
    (
        [p1[0], 2.0 * (p2[0] - p1[0]), p1[0] - 2.0 * p2[0] + p3[0]],
        [p1[1], 2.0 * (p2[1] - p1[1]), p1[1] - 2.0 * p2[1] + p3[1]],
    )
}

/// The t in [0, 1] where k.0 + k.1 t + k.2 t^2 = v on a monotone piece:
/// c / (b + sign(b) sqrt(b^2 - ac)), a = k.2, b = -k.1 / 2, c = k.0 - v
/// (docs/design.md, item 7).
pub fn t_at(k: [f32; 3], v: f32) -> f32 {
    let b = -0.5 * k[1];
    let c = k[0] - v;
    let d = (b * b - k[2] * c).max(0.0).sqrt();
    let s = if b != 0.0 { b.signum() } else { c.signum() };
    let den = b + s * d;
    (if den != 0.0 { c / den } else { 0.0 }).clamp(0.0, 1.0)
}

pub fn at_t(k: [f32; 3], t: f32) -> f32 {
    k[0] + t * (k[1] + t * k[2])
}

// ---- the cover texture: a byte a cache texel (docs/design.md, 12) ----

/// "Not ours": the client's bitmap stands. An edge of tiny coverage is
/// held at EDGE_MIN, so it reads neither pure nor that.
pub const NOT_OURS: u8 = 1;
pub const EDGE_MIN: u8 = 2;

/// A plain font's byte: the texel's fill after the gamma, pure (0 or 255)
/// only where no piece reaches in.
fn plain_byte(cov: f32, reaches: bool, inv_gamma: f32) -> u8 {
    if !reaches {
        return if cov >= 0.5 { 255 } else { 0 };
    }
    (cov.powf(inv_gamma) * 255.0)
        .round()
        .clamp(EDGE_MIN as f32, 254.0) as u8
}

/// An outlined font's two bytes (A8L8) from the texel's fill, whether a
/// piece reaches in, and the distance d from its centre in texels: the
/// fill in L, linear as the client's, and max(outline, fill) after the
/// gamma in A. Pure only where the distance settles the outline over the
/// whole texel (docs/design.md, item 14).
fn outlined_bytes(fill: f32, reaches: bool, d: f32, radius: f32, inv_gamma: f32) -> [u8; 2] {
    let edge = (radius - d + 0.5).clamp(0.0, 1.0);
    let alpha = edge.max(fill);
    let far = d - std::f32::consts::FRAC_1_SQRT_2 >= radius + 0.5;
    let near = d + std::f32::consts::FRAC_1_SQRT_2 <= radius - 0.5;
    // Wholly in the fill: alpha is 1 whatever the distance.
    if !reaches && fill >= 0.5 {
        return [255, 255];
    }
    if !reaches && (far || near) {
        let l = if fill >= 0.5 { 255 } else { 0 };
        let a = if l == 255 || near { 255 } else { 0 };
        return [l, a];
    }
    let l = if reaches {
        (fill * 255.0).round().clamp(EDGE_MIN as f32, 254.0) as u8
    } else if fill >= 0.5 {
        255
    } else {
        0
    };
    let a = (alpha.powf(inv_gamma) * 255.0).round().clamp(1.0, 254.0) as u8;
    [l, a]
}

/// How far a chord may stray from its piece in `cover_cell`, in texels:
/// a byte of the outline's ramp (1 a texel) is 1/255.
const FLAT_TOL: f32 = 1.0 / 256.0;

/// The cover bytes of a whole cell `w` by `h` texels at once, row-major:
/// one byte a texel for a plain font (plain_byte), two for an outlined one
/// (outlined_bytes, `radius` > 0). Grid point (x, y) sits at texel
/// coordinates (x / k + at.x, at.y - y / k), y down.
///
/// The fill from coverage::coverage, the distance from each chord to the
/// texels in its reach, purity from each piece's box. Chords stray under
/// FLAT_TOL, so a byte may be one off the exact one.
pub fn cover_cell(
    r: &Record,
    w: u32,
    h: u32,
    at: P,
    k: f32,
    radius: f32,
    inv_gamma: f32,
) -> Vec<u8> {
    let n = (w * h) as usize;
    let to = |p: P| [p[0] / k + at[0], at[1] - p[1] / k];
    let pieces: Vec<Quad> = r.outline.iter().map(|q| q.map(to)).collect();
    // A texel is impure if a piece's box (of its ends: monotone) overlaps
    // its square: candidates from texel coordinates, a texel to spare, the
    // test itself in grid steps.
    let mut reaches = vec![false; n];
    let half = 0.5 * k;
    for (q, g) in pieces.iter().zip(&r.outline) {
        let (gx0, gx1) = (g[0][0].min(g[2][0]), g[0][0].max(g[2][0]));
        let (gy0, gy1) = (g[0][1].min(g[2][1]), g[0][1].max(g[2][1]));
        let range = |u: f32, v: f32, m: u32| {
            let lo = (u.min(v).floor() - 1.0).max(0.0) as u32;
            let hi = (u.max(v).ceil() + 1.0).clamp(0.0, m as f32) as u32;
            lo..hi.max(lo)
        };
        for y in range(q[0][1], q[2][1], h) {
            let cy = (at[1] - (y as f32 + 0.5)) * k;
            for x in range(q[0][0], q[2][0], w) {
                let cx = (x as f32 + 0.5 - at[0]) * k;
                if gx0 < cx + half && gx1 > cx - half && gy0 < cy + half && gy1 > cy - half {
                    reaches[(y * w + x) as usize] = true;
                }
            }
        }
    }
    let lines = chords(&pieces, &r.outline);
    let fill = crate::coverage::coverage(&lines, w, h);
    if radius <= 0.0 {
        return (0..n)
            .map(|i| plain_byte(fill[i], reaches[i], inv_gamma))
            .collect();
    }
    let d2 = centre_d2(&lines, w, h, radius + 1.5);
    (0..n)
        .flat_map(|i| outlined_bytes(fill[i], reaches[i], d2[i].sqrt(), radius, inv_gamma))
        .collect()
}

/// Placed pieces (texel coordinates, in `outline`'s order) as the contours
/// again, chained end to start, in chords within FLAT_TOL.
fn chords(pieces: &[Quad], outline: &[Quad]) -> Vec<Vec<P>> {
    let mut lines: Vec<Vec<P>> = Vec::new();
    for (i, q) in pieces.iter().enumerate() {
        if i == 0 || outline[i - 1][2] != outline[i][0] {
            lines.push(vec![q[0]]);
        }
        let line = lines.last_mut().unwrap();
        let bend = [
            q[0][0] - 2.0 * q[1][0] + q[2][0],
            q[0][1] - 2.0 * q[1][1] + q[2][1],
        ];
        let chords =
            ((bend[0].hypot(bend[1]) / (4.0 * FLAT_TOL)).sqrt().ceil() as u32).clamp(1, 32);
        line.extend((1..=chords).map(|c| crate::glyph::point(q, c as f32 / chords as f32)));
    }
    for line in &mut lines {
        if line.len() > 1 && line.first() == line.last() {
            line.pop();
        }
    }
    lines
}

/// Squared distances from the centres of a `w` by `h` texel grid to the
/// closed polylines `lines`, at most cut^2, each chord visiting only the
/// texels within the cutoff of its box.
fn centre_d2(lines: &[Vec<P>], w: u32, h: u32, cut: f32) -> Vec<f32> {
    let mut d2 = vec![cut * cut; (w * h) as usize];
    for line in lines {
        for s in 0..line.len() {
            let (a, b) = (line[s], line[(s + 1) % line.len()]);
            let e = [b[0] - a[0], b[1] - a[1]];
            let l = e[0] * e[0] + e[1] * e[1];
            // Centres x + 1/2 within [min - cut, max + cut].
            let span = |u: f32, v: f32, m: u32| {
                let lo = (u.min(v) - cut - 0.5).ceil().max(0.0) as u32;
                let hi = ((u.max(v) + cut - 0.5).floor() + 1.0).clamp(0.0, m as f32) as u32;
                lo..hi.max(lo)
            };
            for y in span(a[1], b[1], h) {
                let py = y as f32 + 0.5 - a[1];
                let row = &mut d2[(y * w) as usize..((y + 1) * w) as usize];
                for x in span(a[0], b[0], w) {
                    let px = x as f32 + 0.5 - a[0];
                    let t = if l > 0.0 {
                        ((px * e[0] + py * e[1]) / l).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    let (dx, dy) = (px - e[0] * t, py - e[1] * t);
                    let v = &mut row[x as usize];
                    *v = v.min(dx * dx + dy * dy);
                }
            }
        }
    }
    d2
}

/// What a texel word tells the decoder (channel 3's top bits). Zero is
/// the bands, so that a word never written (or a stage left unbound, which
/// reads 0 or 1 all over) decodes as before.
pub const WORD_BANDS: u16 = 0;
pub const WORD_LIST: u16 = 1;
pub const WORD_NONE: u16 = 2;
/// Runs a word names, and how long one may be.
const RUNS: usize = 3;
const RUN_MAX: usize = 63;

/// The texel words of a cell `w` by `h` texels (coordinates as
/// cover_cell's), for magnified pixels (docs/design.md, items 15 and 16).
/// A pixel centred in texel T lies in E, T grown by 1/2 + 1/32 texel; an
/// outlined font (`reach` = radius + 1/2 + 1/32) also needs the pieces
/// that may be nearest to T. Pieces past E add nothing, those wholly
/// behind it across its rows make the backdrop, the rest are needed; the
/// word names three runs of the ring that cover them, the shorter side's.
///
/// Per run (start * 16 + (count & 15)); then the counts' top bits (2
/// each), 64 * (backdrop + 4), 512 * mirrored, 1024 * state. WORD_NONE:
/// coverage 1 if the backdrop is not 0, else 0; WORD_BANDS: past what a
/// word holds.
pub fn texel_words(r: &Record, w: u32, h: u32, at: P, k: f32, reach: f32) -> Vec<[u16; 4]> {
    let ring = r.texels[0][2] as usize;
    let n = r.texels[0][3] as usize;
    let g = 0.5 + 1.0 / 32.0;
    // Monotone pieces: each within the box of its ends. Up adds area right
    // of a piece (tests/reference.rs, half_plane).
    let boxes: Vec<[f32; 4]> = r
        .outline
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
    let up: Vec<i32> = r
        .outline
        .iter()
        .map(|q| if q[2][1] > q[0][1] { 1 } else { -1 })
        .collect();
    let mut words = Vec::with_capacity((w * h) as usize);
    // An outlined font's squared distances from texel centres to the
    // outline's chords, as cover_cell has them.
    let cut = reach + 1.0;
    let centre = if reach > 0.0 {
        let to = |p: P| [p[0] / k + at[0], at[1] - p[1] / k];
        let placed: Vec<Quad> = r.outline.iter().map(|q| q.map(to)).collect();
        centre_d2(&chords(&placed, &r.outline), w, h, cut)
    } else {
        Vec::new()
    };
    // One set of buffers for the cell: a texel's lists are short and many.
    let mut s = Scratch::default();
    for ty in 0..h {
        // The pieces that may matter to any texel of the row: across the
        // fill's rows of E (not horizontal), or within the reach of the
        // row. Every test below skips the others anyway.
        let (t2, t3) = ((at[1] - ty as f32 - 1.0) * k, (at[1] - ty as f32) * k);
        s.fill_rows.clear();
        s.fill_rows.extend((0..boxes.len()).filter(|&j| {
            let b = &boxes[j];
            b[2] != b[3] && b[3] > t2 - g * k && b[2] < t3 + g * k
        }));
        s.near_rows.clear();
        if reach > 0.0 {
            s.near_rows.extend(
                (0..boxes.len())
                    .filter(|&j| boxes[j][2] - t3 <= reach * k && t2 - boxes[j][3] <= reach * k),
            );
        }
        for tx in 0..w {
            // T, the texel (y up), and the fill's E.
            let t = [
                (tx as f32 - at[0]) * k,
                (tx as f32 + 1.0 - at[0]) * k,
                t2,
                t3,
            ];
            let fx = [t[0] - g * k, t[1] + g * k];
            let fy = [t[2] - g * k, t[3] + g * k];
            // The distance is from the pixel's centre, in T: a piece may be
            // the nearest if its box is within the reach of T, and no
            // nearer than some other piece is at worst (its distance from
            // T's centre and half T's diagonal).
            s.near.clear();
            if reach > 0.0 {
                let gap = |b: &[f32; 4]| {
                    let dx = (t[0] - b[1]).max(b[0] - t[1]).max(0.0);
                    let dy = (t[2] - b[3]).max(b[2] - t[3]).max(0.0);
                    (dx * dx + dy * dy).sqrt()
                };
                // The nearest chord to T's centre lies within FLAT_TOL of
                // its piece; past the grid's cutoff, no bound.
                let dc = centre[(ty * w + tx) as usize];
                let worst = if dc < cut * cut {
                    (dc.sqrt() + FLAT_TOL + std::f32::consts::FRAC_1_SQRT_2) * k
                } else {
                    f32::MAX
                };
                let bound = worst.min(reach * k);
                s.near.extend(
                    s.near_rows
                        .iter()
                        .copied()
                        .filter(|&j| gap(&boxes[j]) <= bound),
                );
            }
            // Most texels need nothing: no piece across E, and on one side
            // none that ends within its rows. One pass of counts settles
            // them as the walk below would (left first, as it prefers).
            let (mut across, mut ends) = (0, [0, 0]);
            let mut wind = [0i32; 2];
            for &j in &s.fill_rows {
                let b = &boxes[j];
                let spans = b[2] <= fy[0] && b[3] >= fy[1];
                for (side, beyond) in [(0, b[1] <= fx[0]), (1, b[0] >= fx[1])] {
                    if beyond {
                        if spans {
                            wind[side] += up[j];
                        } else {
                            ends[side] += 1;
                        }
                    }
                }
                across += (b[1] > fx[0] && b[0] < fx[1]) as u32;
            }
            if across == 0 {
                // A side with nothing to read: none if solid there or
                // nothing is near.
                let none = |side: usize| ends[side] == 0 && (wind[side] != 0 || s.near.is_empty());
                if let Some(side) = [0, 1].into_iter().find(|&side| none(side)) {
                    let b = wind[side].clamp(-3, 3);
                    words.push([
                        0,
                        0,
                        0,
                        64 * (b + 4) as u16 + 512 * side as u16 + 1024 * WORD_NONE,
                    ]);
                    continue;
                }
            }
            // The left's runs, and the right's only if it may read fewer.
            let (cost_l, back_l) = side_runs(&boxes, &up, &mut s, fx, fy, false, n);
            std::mem::swap(&mut s.runs, &mut s.runs_l);
            let (runs, backdrop, right) = if cost_l > 0 {
                let (cost_r, back_r) = side_runs(&boxes, &up, &mut s, fx, fy, true, n);
                if cost_r < cost_l {
                    (&s.runs, back_r, true)
                } else {
                    (&s.runs_l, back_l, false)
                }
            } else {
                (&s.runs_l, back_l, false)
            };
            let state = if runs.is_empty() {
                WORD_NONE
            } else if backdrop.abs() > 3
                || runs
                    .iter()
                    .any(|&(s, c)| c > RUN_MAX || ring + s >= PACK as usize)
            {
                WORD_BANDS
            } else {
                WORD_LIST
            };
            let mut word = [0u16; 4];
            let mut top = 0u16;
            if state == WORD_LIST {
                for (j, &(s, c)) in runs.iter().enumerate() {
                    word[j] = ((ring + s) * 16 + (c & 15)) as u16;
                    top |= ((c >> 4) as u16) << (2 * j);
                }
            }
            let b = if state == WORD_BANDS {
                0
            } else {
                backdrop.clamp(-3, 3)
            };
            word[3] = top + 64 * (b + 4) as u16 + 512 * right as u16 + 1024 * state;
            words.push(word);
        }
    }
    words
}

/// texel_words' buffers, kept across the texels of a cell.
#[derive(Default)]
struct Scratch {
    fill_rows: Vec<usize>,
    near_rows: Vec<usize>,
    near: Vec<usize>,
    needed: Vec<usize>,
    behind: Vec<usize>,
    merged: Vec<usize>,
    runs: Vec<(usize, usize)>,
    runs_l: Vec<(usize, usize)>,
}

/// One side's runs for a texel (texel_words), into `s.runs`: of the row's
/// pieces, those across E or beyond it (left, mirrored right) ending within
/// its rows are needed, those beyond it across all its rows are behind;
/// the near ones are needed too unless E is wholly inside the fill. The
/// runs are the arcs of the ring (n pieces) between the largest gaps
/// between needed ones, a gap's ties to the first. Returns what the runs
/// read and the backdrop of those behind outside them.
fn side_runs(
    boxes: &[[f32; 4]],
    up: &[i32],
    s: &mut Scratch,
    fx: [f32; 2],
    fy: [f32; 2],
    right: bool,
    n: usize,
) -> (usize, i32) {
    s.needed.clear();
    s.behind.clear();
    for &j in &s.fill_rows {
        let b = &boxes[j];
        let (beyond, past) = if right {
            (b[0] >= fx[1], b[1] <= fx[0])
        } else {
            (b[1] <= fx[0], b[0] >= fx[1])
        };
        if past {
            continue;
        }
        if beyond && b[2] <= fy[0] && b[3] >= fy[1] {
            s.behind.push(j);
        } else {
            s.needed.push(j);
        }
    }
    // Wholly inside the fill all over E, the alpha is 1 whatever the
    // distance; else the near pieces too (one behind then counts in its
    // run, not the backdrop). Both lists ascend: merge them.
    if !s.needed.is_empty() || s.behind.iter().map(|&j| up[j]).sum::<i32>() == 0 {
        s.merged.clear();
        let (mut a, mut b) = (0, 0);
        while a < s.needed.len() || b < s.near.len() {
            let next = match (s.needed.get(a), s.near.get(b)) {
                (Some(&x), Some(&y)) if x == y => {
                    a += 1;
                    b += 1;
                    x
                }
                (Some(&x), Some(&y)) if x < y => {
                    a += 1;
                    x
                }
                (Some(&x), None) => {
                    a += 1;
                    x
                }
                (_, Some(&y)) => {
                    b += 1;
                    y
                }
                (None, None) => unreachable!(),
            };
            s.merged.push(next);
        }
        std::mem::swap(&mut s.needed, &mut s.merged);
    }
    s.runs.clear();
    let len = s.needed.len();
    if len > 0 {
        // The three largest gaps, larger first, ties to the first.
        let mut top: [Option<(usize, usize)>; RUNS] = [None; RUNS];
        for a in 0..len {
            let next = s.needed[(a + 1) % len] + if a + 1 == len { n } else { 0 };
            let gap = (next - s.needed[a] - 1, a);
            let mut cur = Some(gap);
            for slot in top.iter_mut() {
                match (*slot, cur) {
                    (None, Some(c)) => {
                        *slot = Some(c);
                        cur = None;
                    }
                    (Some(o), Some(c)) if c.0 > o.0 || (c.0 == o.0 && c.1 < o.1) => {
                        *slot = Some(c);
                        cur = Some(o);
                    }
                    _ => {}
                }
            }
        }
        let mut cuts: [usize; RUNS] = [usize::MAX; RUNS];
        let mut m = 0;
        for g in top.iter().flatten() {
            cuts[m] = g.1;
            m += 1;
        }
        cuts[..m].sort_unstable();
        for c in 0..m {
            let from = (cuts[(c + m - 1) % m] + 1) % len;
            let (a, e) = (s.needed[from], s.needed[cuts[c]]);
            s.runs.push((a, (e + n - a) % n + 1));
        }
    }
    let runs = &s.runs;
    let inside = |j: usize| runs.iter().any(|&(a, c)| (j + n - a) % n < c);
    let backdrop = s
        .behind
        .iter()
        .filter(|&&j| !inside(j))
        .map(|&j| up[j])
        .sum();
    (runs.iter().map(|r| r.1).sum(), backdrop)
}
