// The curve decoder of format v2 (ps_3_0), tests/reference.rs line for
// line in the same f32 arithmetic (docs/design.md, items 4 to 16). s0
// only locates the pixel; the column map (s1) names its glyph, the atlas
// (s2) holds the curves. Coverage is the exact box filter.

sampler2D s0 : register(s0);
// Per cache texel column and row of cells: x the record's first texel in
// the atlas (negative: no glyph), y band height * 16 + bands - 1, z and w
// where the glyph's grid starts, in cache texels (x right, y down).
sampler2D colmap : register(s1);
// The records, A32B32G32R32F, row-major.
sampler2D atlas : register(s2);
// A byte a cache texel (atlas::cover_cell): 0 or 255 pure, 1 not ours
// (the client's bitmap), else an edge's coverage after the gamma.
sampler2D cover : register(s3);
// A word a cache texel for magnified pixels (atlas::texel_words): three
// runs of the ring, backdrop, side, state (0 the bands, also what an
// unbound stage reads; 1 the runs; 2 none needed).
sampler2D words : register(s4);

// x cell height, y 1 / cell height, z 1 / gamma, w gamma.
float4 cfg : register(c200);
// x the outline's radius in texels, 0 for a plain font; w 1 to colour the
// words' path (the words knob at 2).
float4 field : register(c201);
// x atlas width, y 1 / atlas width, z 1 / atlas height, w grid steps a
// cache texel (one scale for the font, so one for the draw).
float4 atl : register(c202);
// x 1 / the column map's height (a row a row of cells).
float4 cmap : register(c203);

// The client's own draw: its bitmap times the colour.
float4 client(float2 uv, float4 color)
{
    return tex2Dlod(s0, float4(uv, 0.0, 0.0)) * color;
}

// A plain font's alpha for coverage c drawn in `rgb` (docs/design.md,
// item 23). The device blends in display values: light text over dark
// comes out thin there and wants c^(1/gamma); black over anything wants
// 1 - (1 - c)^(1/gamma), exactly. Between them by the colour's luma.
float tone(float c, float3 rgb)
{
    float light = pow(c, cfg.z);
    float dark = 1.0 - pow(1.0 - c, cfg.z);
    return lerp(dark, light, dot(rgb, float3(0.299, 0.587, 0.114)));
}

static const float PACK = 4096.0;
// Grid steps a coarse step (reach, span), and a piece's low bits.
static const float COARSE = 16.0;

// Atlas texel i, at its centre: exact whatever the filter.
float4 atlas_at(float i)
{
    float row = floor((i + 0.5) * atl.y);
    return tex2Dlod(atlas, float4((i - row * atl.x + 0.5) * atl.y, (row + 0.5) * atl.z, 0.0, 0.0));
}

// Two 12-bit integers from x * 4096 + y: the division is by a power of
// two, so exact, and the floor with it.
float2 unpack(float v)
{
    float x = floor(v * (1.0 / PACK));
    return float2(x, v - x * PACK);
}

// The next nibble off the bottom of w (atlas::piece): exact, as above.
float nibble(inout float w)
{
    float rest = floor(w * (1.0 / COARSE));
    float n = w - rest * COARSE;
    w = rest;
    return n;
}

// The t in [0, 1] where a monotone quadratic coordinate k.x + k.y t +
// k.z t^2 equals v (atlas::t_at): the root smaller in magnitude, no
// division by k.z.
float t_at(float3 k, float v)
{
    float b = -0.5 * k.y;
    float c = k.x - v;
    float d = sqrt(max(b * b - k.z * c, 0.0));
    float den = b + (b != 0.0 ? sign(b) : sign(c)) * d;
    return saturate(den != 0.0 ? c / den : 0.0);
}

float at_t(float3 k, float t)
{
    return k.x + t * (k.y + t * k.z);
}

// The integral of (x - x(t)) y'(t) over the part of [lo, hi] where x(t) <
// x, by two-point Gauss-Legendre, exact for this cubic (atlas::half_plane).
float half_plane(float3 kx, float3 ky, float lo, float hi, float x)
{
    float tx = clamp(t_at(kx, x), lo, hi);
    float t1 = at_t(kx, lo) < x ? lo : tx;
    float t2 = at_t(kx, hi) < x ? hi : tx;
    float m = 0.5 * (t1 + t2);
    float h = 0.5 * (t2 - t1);
    float2 t = m + float2(-h, h) * 0.57735027;
    float2 f = (x - (kx.x + t * (kx.y + t * kx.z))) * (ky.y + 2.0 * ky.z * t);
    return h * (f.x + f.y);
}

// The signed area of the pixel [-hx, hx] x rows yr right of one piece, its
// points relative to the sample (atlas::area_right). Branch free: vkd3d
// spends registers on every if in an inlined function inside a loop.
float area_right(float2 p1, float2 p2, float2 p3, float hx, float2 yr)
{
    float3 kx = float3(p1.x, 2.0 * (p2.x - p1.x), p1.x - 2.0 * p2.x + p3.x);
    float3 ky = float3(p1.y, 2.0 * (p2.y - p1.y), p1.y - 2.0 * p2.y + p3.y);
    float ya = max(yr.x, min(p1.y, p3.y));
    float yb = max(ya, min(yr.y, max(p1.y, p3.y)));
    float ta = t_at(ky, ya);
    float tb = t_at(ky, yb);
    float lo = min(ta, tb);
    float hi = max(ta, tb);
    return half_plane(kx, ky, lo, hi, hx) - half_plane(kx, ky, lo, hi, -hx);
}

// The squared distance from the sample (the origin) to the segment A, B.
float seg_dist2(float2 A, float2 B)
{
    float2 e = B - A;
    float t = saturate(dot(-A, e) / max(dot(e, e), 1e-8));
    float2 d = A + e * t;
    return dot(d, d);
}

// The squared distance from the sample to the quadratic A, B, C through 6
// chords (a chord strays |A - 2B + C| / 144 from the curve, hundredths of a
// texel at the client's sizes); a straight piece is its own chord.
float dist2(float2 A, float2 B, float2 C)
{
    if (all(abs(B - 0.5 * (A + C)) <= 1.0)) {
        return seg_dist2(A, C);
    }
    float2 a = B - A;
    float2 b = A - 2.0 * B + C;
    float2 prev = A;
    float d = 1e30;
    [unroll] for (int i = 1; i <= 6; i++) {
        float t = i / 6.0;
        float2 pt = A + (2.0 * a + b * t) * t;
        d = min(d, seg_dist2(prev, pt));
        prev = pt;
    }
    return d;
}

// Atlas texel i as a piece relative to the sample c, x times sx (-1: the
// mirror).
void piece_at(float i, float2 c, float sx, out float2 p1, out float2 p2, out float2 p3)
{
    float4 piece = atlas_at(i);
    // The top 12 bits as pairs, the low 4 of all six after.
    float w = piece.w;
    p1 = unpack(piece.x) * COARSE;
    p1.x += nibble(w);
    p1.y += nibble(w);
    p2 = unpack(piece.y) * COARSE;
    p2.x += nibble(w);
    p2.y += nibble(w);
    p3 = unpack(piece.z) * COARSE;
    p3.x += nibble(w);
    p3.y += nibble(w);
    p1 -= c;
    p2 -= c;
    p3 -= c;
    p1.x *= sx;
    p2.x *= sx;
    p3.x *= sx;
}

float4 main(float4 color : COLOR0, centroid float2 uv : TEXCOORD0) : COLOR0
{
    float2 p = uv * 256.0;
    float2 fw = max(abs(ddx(p)) + abs(ddy(p)), 1.0 / 64.0);
    float fwa = 0.5 * (fw.x + fw.y);
    bool outlined = field.x > 0.0;
    // At 1x, or within a pure texel, the cover is the answer (L the fill,
    // A for an outlined font max(outline, fill) after the gamma).
    float2 t = floor(p);
    float2 f = p - t;
    float4 cv = tex2Dlod(cover, float4((t + 0.5) / 256.0, 0.0, 0.0));
    float v = round(cv.r * 255.0);
    float va = round(cv.a * 255.0);
    if (v == 1.0) {
        return client(uv, color);
    }
    bool one = all(abs(fw - 1.0) < 1.0 / 64.0) && all(abs(f - 0.5) < 1.0 / 64.0);
    bool within = all(f - 0.5 * fw >= -1.0 / 1024.0) && all(f + 0.5 * fw <= 1.0 + 1.0 / 1024.0);
    bool pure = (v == 0.0 || v == 255.0) && (!outlined || va == 0.0 || va == 255.0);
    if (one || (pure && within)) {
        // The plain byte is c^(1/gamma), a light text's alpha as it is;
        // the coverage back out of it for the rest. The outlined A is its
        // alpha already (atlas.rs).
        float4 o = outlined ? float4(v, v, v, va) / 255.0
            : float4(1.0, 1.0, 1.0, tone(pow(v / 255.0, cfg.w), color.rgb));
        return o * color;
    }
    // Magnified (a footprint of at most a texel and 1/64), the texel's word:
    // the runs of the glyph's ring that may touch the pixel, or none at all.
    // Its fields by powers of two, so the floors are exact.
    float4 wd = 0.0;
    if (all(fw <= 1.0 + 1.0 / 64.0)) {
        wd = round(tex2Dlod(words, float4((t + 0.5) / 256.0, 0.0, 0.0)) * 65535.0);
    }
    float dw = wd.w;
    float state = floor(dw * (1.0 / 1024.0));
    dw -= state * 1024.0;
    float side = floor(dw * (1.0 / 512.0));
    dw -= side * 512.0;
    float backdrop = floor(dw * (1.0 / 64.0));
    dw -= backdrop * 64.0;
    backdrop -= 4.0;
    // The words' path in colour (the words knob at 2): magenta the runs,
    // green none needed; the shortcut and the bands keep the client's.
    if (field.w > 0.5 && (state == 1.0 || state == 2.0)) {
        color.rgb = state == 1.0 ? float3(1.0, 0.0, 1.0) : float3(0.0, 1.0, 0.0);
    }
    if (state == 2.0) {
        float f0 = backdrop != 0.0 ? 1.0 : 0.0;
        return (outlined ? float4(f0, f0, f0, f0) : float4(1.0, 1.0, 1.0, f0)) * color;
    }
    float cells_row = floor((t.y + 0.5) * cfg.y);
    float4 m = tex2Dlod(colmap, float4((t.x + 0.5) / 256.0, (cells_row + 0.5) * cmap.x, 0.0, 0.0));
    if (m.x < 0.0) {
        return client(uv, color);
    }
    // The sample and half the footprint in grid steps (y up, as the font),
    // and how far pieces count: half the footprint for the fill, the
    // outline's radius and half the footprint for its distance.
    float2 c = float2(p.x - m.z, m.w - p.y) * atl.w;
    float2 h = 0.5 * fw * atl.w;
    float rd = outlined ? (field.x + 0.5 * fwa + 1.0 / 64.0) * atl.w : 0.0;
    float2 rr = max(h, rd);
    float band_h = floor(m.y * (1.0 / 16.0));
    float bands = m.y - band_h * 16.0 + 1.0;
    float b0 = clamp(floor((c.y - rr.y) / band_h), 0.0, bands - 1.0);
    float b1 = clamp(floor((c.y + rr.y) / band_h), 0.0, bands - 1.0);
    float total = 0.0;
    float d2 = 1e30;
    if (state == 1.0) {
        // Per run its start (from the record's) * 16 + its count's low 4
        // bits, the counts' top 2 bits each in the rest of w.
        float hi2 = floor(dw * (1.0 / 16.0));
        dw -= hi2 * 16.0;
        float hi1 = floor(dw * 0.25);
        float3 s = floor(wd.xyz * (1.0 / 16.0));
        float3 n = wd.xyz - s * 16.0 + 16.0 * float3(dw - hi1 * 4.0, hi1, hi2);
        float sx = 1.0 - 2.0 * side;
        // The pieces behind the pixel (left of it, mirrored right) across
        // all its rows: its width times its rows each.
        float sum = backdrop * 4.0 * h.x * h.y;
        [loop] for (int i = 0; i < 189; i++) {
            float fi = i;
            if (fi >= n.x + n.y + n.z) {
                break;
            }
            float at = fi < n.x ? s.x + fi : (fi < n.x + n.y ? s.y + fi - n.x : s.z + fi - n.x - n.y);
            float2 p1, p2, p3;
            piece_at(m.x + at, c, sx, p1, p2, p3);
            if (min(p1.x, p3.x) < h.x) {
                sum += area_right(p1, p2, p3, h.x, float2(-h.y, h.y));
            }
            float2 q = max(max(min(min(p1, p2), p3), -max(max(p1, p2), p3)), 0.0);
            if (outlined && dot(q, q) < min(d2, rd * rd)) {
                d2 = min(d2, dist2(p1, p2, p3));
            }
        }
        total = abs(sum);
    }
    // Minified, or past what a word holds: the bands.
    [loop] for (int bi = 0; bi < 16; bi++) {
        if (state == 1.0) {
            break;
        }
        float b = b0 + bi;
        if (b > b1) {
            break;
        }
        // First list * 4096 + count, split, reach, span (atlas.rs).
        float4 band = atlas_at(m.x + 1.0 + b);
        float y0 = b * band_h;
        float2 yr = float2(max(y0 - c.y, -h.y), min(y0 + band_h - c.y, h.y));
        // The right end is the coarse step it falls in (atlas.rs).
        float2 reach = unpack(band.z) * COARSE + float2(0.0, COARSE);
        float2 span = unpack(abs(band.w)) * COARSE;
        bool rows = yr.x < yr.y;
        bool outside = c.x + rr.x <= reach.x || c.x - rr.x >= reach.y;
        bool inside = c.x - rr.x >= span.x && c.x + rr.x <= span.y;
        if (rows && !outside && inside && band.w > 0.0) {
            total += (yr.y - yr.x) * 2.0 * h.x;
        }
        if (!outside && !inside) {
            float2 first = unpack(band.x);
            // Right of the split: the list by largest x, mirrored, so the
            // area left of each piece is the area right of its mirror.
            float right = c.x >= band.y ? 1.0 : 0.0;
            float start = m.x + first.x + right * first.y;
            float sx = 1.0 - 2.0 * right;
            float sum = 0.0;
            [loop] for (int i = 0; i < 255; i++) {
                if (i >= first.y) {
                    break;
                }
                float2 p1, p2, p3;
                piece_at(start + i, c, sx, p1, p2, p3);
                // Wholly right of the pixel and of the outline's reach: it
                // and the rest add nothing.
                if (min(p1.x, p3.x) >= rr.x) {
                    break;
                }
                // Wholly right of the pixel (walked only for the outline's
                // reach) adds no area.
                if (rows && min(p1.x, p3.x) < h.x) {
                    sum += area_right(p1, p2, p3, h.x, yr);
                }
                // The piece lies within the box of its three points, so the
                // box's distance bounds the piece's from below: no nearer
                // than the nearest yet (or the reach), no chords.
                float2 q = max(max(min(min(p1, p2), p3), -max(max(p1, p2), p3)), 0.0);
                if (outlined && dot(q, q) < min(d2, rd * rd)) {
                    d2 = min(d2, dist2(p1, p2, p3));
                }
            }
            total += abs(sum);
        }
    }
    float fill = saturate(total / (4.0 * h.x * h.y));
    if (!outlined) {
        return float4(1.0, 1.0, 1.0, tone(fill, color.rgb)) * color;
    }
    // The outline from the distance in texels, as the client's shows it:
    // the fill in grey, max(outline, fill) in alpha. Whatever the text's
    // colour, the edge that alpha makes is the black outline's: black's
    // alpha (tone).
    float edge = saturate((field.x - sqrt(d2) / atl.w) / fwa + 0.5);
    return float4(fill, fill, fill, 1.0 - pow(1.0 - max(edge, fill), cfg.z)) * color;
}
