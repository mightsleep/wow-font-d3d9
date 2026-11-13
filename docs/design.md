# How the text is drawn

The tricks inside the proxy, one numbered item each: problem, trick, why
it holds, what it bought. Fetches a pixel come from the CPU model of the
decoder (`tests/reference.rs`), errors from the bench against the exact
area (`nix run .#bench`), times from an RTX 5060 under DXVK. "Roboto" is
Roboto Bold, "CJK" LXGW WenKai, at 32 or 48 px/em. Layouts are in the
code; this is why they are what they are.

## The box

**1. All we get is a pixel shader.** The client rasterises glyphs into
256x256 A4R4G4B4 caches, cut into cells, and draws each glyph as one quad
over its cell. We patch nothing: a proxy `d3d9.dll` swaps the pixel
shader while a cache is bound. That runs on Windows' D3D9, wined3d, DXVK
and Proton with one 32-bit build, and costs ps_3_0 (no integers, 32
temporaries) in a 32-bit process.

**2. Which font? Ask the stack.** Which glyph sits in which cell lives in
the client's CGxFont, and searching the heap for it took 15 to 21 s. But
the client fills a cache from a callback that gets its TEXTURECACHE, so
inside our `LockRect` hook a pointer to it is a few frames up our stack.
A stack word `w` is it if

    *(*(*w + 0x44) + 0x38) == the texture being locked,
    and w - 20 - 24 i is a CGxFont header

Found at every upload, 75 to 78 words up, in 45 to 267 us. The owner is
whoever is uploading now, so recycled textures cannot fool it.

**3. Map the state, not the draws.** The device holds `phi(s)` of the
client's state `s`: our decoder and constants if the bound texture is a
known cache and the vertex shader a vs_3_0, the client's shader
otherwise. Each setter updates `s` and calls the device only where
`phi(s)` changed; getters answer with `s`; draws pass untouched. `Reset`
and state blocks write the device behind our back, so they re-read `s`
(`src/phi.rs`).

## Integers without integers

**4. Shifts are floors.** Below 2^24 a float holds integers exactly, and
scaling by a power of two only moves the exponent, so

    x >> s             = floor(x * 2^-s)
    (x >> s) & (2^k-1) = floor(x * 2^-s) - 2^k floor(x * 2^-(s+k))

exactly, checked for every `x < 2^24`. AND, OR and XOR of two values have
no such form, so every field is laid out never to need them. Two 12-bit
integers travel as `a * 4096 + b` and come back as `a = floor(v / 4096)`,
`b = v - 4096 a`. Our textures are read at `(t + 0.5) / 256`, exact, where
any filter gives the neighbours weight zero: no sampler state to set.

## The atlas

**5. Sixteen bits as twelve plus four.** Outlines go to our own
A32B32G32R32F atlas as quadratic pieces on a 65536-step grid over the
font's box, one scale per font, so every size shares a record. A piece is
six 16-bit coordinates, 96 bits; a texel is four floats of 24 exact bits,
also 96:

    floats 0..2   top 12 bits of each point, x * 4096 + y
    float 3       low 4 bits of all six: sum of n_j 16^j, j = 0..5

The decoder peels float 3 with five floors. Against the 12-bit grid
before it, texels over 2/255 went 5.03 % to 0.09 % (Roboto) and 15.1 % to
0.01 % (CJK): the error at 4x had been the grid. The same count says two
pieces a texel would leave 48 bits each, coarser than what we left.

**6. Monotone pieces.** Each quadratic is split at its extrema in y, then
x. At an extremum the tangent is parallel to the other axis, so both new
control points get the split point's coordinate exactly and stay in order
on the grid. A monotone piece lies in the box of its ends, and a row cuts
it at most once.

**7. One root, no cases.** Solve `k0 + k1 t + k2 t^2 = v`. With `a = k2`,
`b = -k1/2`, `c = k0 - v` the roots are `(b +- sqrt(b^2 - ac)) / a`, or
equally

    t = c / (b + sign(b) sqrt(b^2 - ac))

which adds magnitudes (no cancellation), never divides by `a`, and gives a
line's root `c / 2b` with no special case. It is the root smaller in
magnitude, and on a monotone piece that is the one in [0, 1]: the roots
are symmetric about the vertex `b/a`, which monotony keeps outside
(0, 1), so the twin of a root `r` in [0, 1] is at least `2 - r >= r` or at
most `-r`. Slug's integer root classification is not needed.

**8. The area right of a piece.** Coverage is the exact box filter over
`[-hx, hx] x [-hy, hy]` around the sample. By Green's theorem, summed over
the contours,

    integral of clamp(hx - x(t), 0, 2hx) y'(t) dt

over the `t` inside the pixel's rows is the inside area in the box: `2hx`
per row where the piece is left of it, `hx - x(t)` where it crosses. The
rows give the `t` range by item 7 on `y`; the clamp switches at most once,
at the root of `x(t) = hx` (or `-hx`). Between switches the integrand is a
quadratic times a linear term, a cubic, which two-point Gauss-Legendre
integrates exactly:

    h (f(m - h/sqrt 3) + f(m + h/sqrt 3)),  m, h the interval's centre, half

Per piece: three roots and four cubics. No sampling.

## The bands

**9. Two lists and a walk that stops.** The glyph is cut into
`ceil(sqrt(n))` bands, at most 16, and each band lists its pieces twice:
by smallest x and by largest x. A pixel left of the band's middle walks
the first list. Pieces wholly left of it add `2hx` per row; the first one
whose smallest x is past it adds nothing, and since the list is sorted,
nor does any after it, so the walk stops. A pixel right of the middle
walks the second list mirrored. Each reads only the pieces between it and
the nearer side: 4.9 to 3.3 pieces a pixel (Latin), 9.3 to 5.1 (CJK).
Pieces reaching into several bands are copied, not indexed, saving a
dependent fetch per piece: 7.9 to 5.4 fetches (Latin), 10.1 to 6.9 (CJK),
for 1.1 and 4.9 KB a glyph of video memory, which a 32-bit process does
not pay for. A band's sum has one sign while contours do not overlap, so
each band adds its absolute value; contours that do overlap are united
first (item 21).

**10. Pieces that add nothing stay out.** A horizontal piece has `y' = 0`
in item 8, and one touching only the band's boundary row has no rows in
it: out of the lists, Roboto 4.70 to 4.38 fetches. Outlined fonts keep
them, since a horizontal edge is as near as any (without, 5.8 % over
16/255 at 2x).

**11. Reach and span.** Each band's header also holds the x range its
pieces occupy, and its widest gap no piece crosses, signed by the winding
there. A pixel outside the reach adds nothing, one inside a positive span
its full width; neither reads a piece. Both are 12-bit pairs in coarse
steps of 16, rounded outwards for the reach and inwards for the span, so
rounding only ever sends a pixel to the pieces. 5.4 to 4.7 fetches
(Roboto), 6.9 to 5.5 (CJK). Trap: a horizontal piece has no single `t`
for its `y`, so its extent is end to end, or a span opens across a
crossbar.

## The cover

**12. Pure texels.** A byte per cache texel: 0 or 255 if no piece's box
meets the texel, 2 to 254 an edge (its exact coverage after the gamma), 1
for "not ours, draw the client's bitmap". If no piece meets a closed
texel, the winding is constant on it, so every box inside has coverage
exactly 0 or 1, at any scale. The decoder reads this byte first: at 1x
(footprint the texel, centre sampled) it is the answer, and so within a
pure texel at any scale. 0.125 to 0.097 ms a pass (Latin), 0.54 to 0.44
(CJK). The client's bitmap cannot do this: its 4-bit alpha 15 means
nearly full, not full.

**13. A whole cell at once.** The bytes come per cell
(`src/coverage.rs`). Chords: a quadratic strays from its chord over
`[0, 1]` by `(2b - a - c) t (1 - t)`, at most `|a - 2b + c| / 4`, so `n`
equal chords stray `|a - 2b + c| / 4n^2`, and a line needs one. Per texel
row, cut at every chord end so crossings are linear and keep their order
within each slab; walk them counting winding, and use only those where it
turns between zero and nonzero (the nonzero rule: overlaps count once).
Each adds to its own column the mean of `clamp(x - c, 0, 1)` over the slab,

    F(u) = 0, u^2/2, u - 1/2   (u <= 0, 0 < u < 1, u >= 1)
    mean = (F(u1) - F(u0)) / (u1 - u0)

and to every column left of it the slab's height, by a difference array
and one prefix sum. Cost goes with crossings, not texels times pieces.
This holds whatever the contours do, and it is the bench's truth; the
cells themselves take item 22 unless item 21 gave up on the outline.

**14. Outlines from a distance.** The outline's alpha is
`saturate(r - d + 1/2)` for `d` the distance to the pieces the walk reads
anyway, and the cover gets a second byte (A8L8: fill, and
`max(outline, fill)`). Distance is 1-Lipschitz, so from the texel's centre
`d - sqrt(1/2) >= r + 1/2` makes the outline 0 over the whole texel and
`d + sqrt(1/2) <= r - 1/2` makes it 1: purity again. A quadratic lies in
the hull of its control points, so their box's distance bounds the
piece's from below and far pieces skip the chords; six chords stray
`|a - 2b + c| / 144`. On the CPU a cell's distances from its texel
centres are worked out once, for these bytes and for the texel words'
nearest pieces (item 15) alike: a fifth off an outlined cell's time.

## Texel words

**15. Runs of a ring.** A magnified pixel centred in texel T lies inside
E, T grown by half a texel and 1/32. Pieces right of E add nothing; those
wholly left across E's rows add width times rows, together the backdrop;
the rest are needed. Since the outline is a closed curve, the pieces near
a small square are a few stretches of consecutive pieces. So the record
keeps a ring (every piece in outline order, written twice so no run
wraps), and a texel's word names three runs: the arcs between the three
largest gaps among its needed pieces. Pieces swept in between add nothing
or count in the run instead of the backdrop. The side with shorter runs
wins. 17 % less text time in the game. On the CPU a texel's word
comes of sets of pieces (a bit a piece: the row's, the column's left and
right of E, the near ones), so two texels with the same sets have the
same word, and along a straight edge the next texel mostly has; and the
right side's runs are only worked out when it needs fewer pieces than the
left's read, since runs read every piece they need at least.

**16. A word that reads as nothing.** Four 16-bit integers
(A16B16G16R16): per run `start * 16 + (count & 15)`, and in the fourth the
counts' top bits, `64 (backdrop + 4)`, `512 mirrored`, `1024 state`. Runs
start below 4096 and hold at most 63 pieces, the backdrop fits -3 to 3;
past that, the bands. State 0 means "the bands", so a word never written,
a cell not ours and a stage left unbound all read as zero and decode as
before words existed.

## The rest

**17. Centroid.** With multisampling, a pixel the quad only partly covers
is shaded at its centre, maybe past the quad's edge, where the coordinate
reads the next glyph's cell: the client's faint slivers beside text. The
decoder interpolates at the centroid, inside the covered part.

**18. A cache that knows it is stale.** Cover bytes and words (0.1 to
0.4 ms a cell) go to append-only files with an Adler-32 per record; a
crash's cut tail is truncated at the next start. A font is named by its
table directory and length (a 26 MB font is not read to be identified),
and the key includes a hash of the encoder's sources (`build.rs`), so a
changed encoder never asks for old files. A glyph's record goes there
too, one a font whatever the size, as the texels the GPU takes: what is
read is what is used, and the outline comes back out of the ring. A
record read is 0.4 us where parsing and encoding the glyph were 10.6
(Latin), 1.4 for 44 (CJK), every session.

**19. Check the reference.** Chords within 1/256 texel leave 4e-3 of
area error next to a curve, so the tests cut curves 16 ways and integrate
on a grid 8 times finer.

**20. One constant an instruction.** ps_3_0 has one read port for the
constant registers: an instruction may read `c0` twice, not `c0` and
`c1`. vkd3d's favourite select, `cmp r, x, c0.x, c1.z` between two
literals, breaks it 86 times in the decoder; DXVK never minds, Windows'
own D3D9 refuses the shader outright (D3DERR_INVALIDCALL). `build.rs`
fixes the bytecode instead of the HLSL (`src/ports.rs`): every constant
but an instruction's first goes through a spare temporary,
`mov r28, c1` just before. Two spares (30 of 32 temporaries), 87 slots
more, and the bench sees the same pixels in the same time. Found by
asking the device which hand-made shaders it takes (all, up to 32768
slots: not the size) and by Microsoft's compiler, whose bytecode it took
and which never reads two constants (`src/draw.rs`, diagnose).

## Overlaps

**21. Pieces, not contours.** Fonts built from parts (a stem and its
bars, Roboto 3: 52 of 80 Latin glyphs) keep their contours overlapping,
and the decoder's sum is the area times the winding: twice where two
overlap, 0.5 off at an edge. The sum needs no contours, though, only
pieces whose crossings count 0 or 1 everywhere. So `encode` cuts the
pieces where they cross and keeps the parts with the fill on one side
only (`src/union.rs`). Where: a quadratic `p(t) = a t^2 + b t + c` has

    a x (p - c) = (a x b) t,   b x (p - c) = -(a x b) t^2
    F(p) = (a x (p - c))^2 + (a x b) (b x (p - c)) = 0

and the other piece put for `p` gives a quartic in its parameter with
whole coefficients under 78 bits, the grid being 16. Its roots are
isolated exactly, by Descartes' rule on the Bernstein coefficients and
halving by sums; a root where one piece's end lies on another is an
exact zero. Which: between two points where something meets a contour
the fill stays on the same sides, so one winding either side of a run's
longest part settles the run. The parts chain into loops by their grid
points. Shared edges (the stem's and a bar's) are cut at each other's
ends and left once. Over 16/255 on the gate's Roboto: 2.76, 0.29 and
0.076 % at 0.5x, 2x and 4x became 0.29, 0.000 and 0.001 %; Morpheus (5
glyphs of 76), in the decoder's CPU model, up to 0.22 % to none. A
glyph's record costs twice the time, once: 5 to 10 us (Latin), 21 to 42
(CJK), on x86_64. An outline whose loops do not close stays as it was.

**22. Sums again.** With no overlaps left, a cell's fill needs no slabs
(item 13): each chord adds, per texel row, the area right of it to the
columns it passes through and its height to the next, and a prefix sum
along the row hands that on, the decoder's own sum on the texel grid.
The same bytes for fonts without overlaps (none of 400 000 differ), in a
fifth of the time: a 12 px Latin cell's fill 10.6 to 1.8 us, a 32 px CJK
one 67.7 to 10.7. Where the union removed a piece from inside the fill
its texels turn pure, a step or two of 255.

## Tried and set aside

- **Hermite fields in the client's cells** (version one): over 16/255 at
  1x, 2x, 4x, Roboto outlined 0.78, 0.67, 1.11 % against 0.000, 0.087,
  0.250 %; CJK 2.3, 2.2, 3.5 % against 0.000, 0.035, 0.119 %.
- **Tiles with a backdrop** (RAVG, Vello): exact, but 4.4 fetches became
  5.35 (Latin) and 6.0 became 7.85 (CJK), for two to four times the atlas;
  few pieces span a band, the rest are copied into every tile right of
  them.
- **Two pieces a texel**: no bits (item 5); vertical line pairs would save
  at most 5 % of fetches for a branch.
- **Bands along instead of across**: 0.5 to 2.5 %.
- **A vertex shader** dilating quads past the cell: not needed yet.
