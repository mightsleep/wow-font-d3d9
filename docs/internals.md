# Inside the proxy

How it works in the order things happen, what it costs, how to build and
test it. The tricks themselves, numbered and measured, are in
[design.md](design.md).

## How it works

The tricks, with the reasoning and the measurements behind them, are in
[design.md](design.md).

**Which font.** The client keeps every font it makes in one list
(`g_fonts`, a static in Wow.exe). The proxy finds the list once a session,
where build 12340 has it or by searching the image's writable sections
for what makes it a list of fonts, and at each upload of a glyph cache
asks which font's texture cache leads to that texture: font name, pixel
size, outline flags, and the list of glyphs in each cell (`src/fonts.rs`,
`src/mem.rs`). The font file is looked up by that name under the game
folder, then in `WOW_FONT_D3D9_FONTS`, then in the client's MPQ archives
(`src/fontfile.rs`, `src/mpq.rs`).

**Cells.** At each `UnlockRect` of a glyph cache, every cell the client
changed is checked against its glyph (the alpha the client just wrote must
hold the glyph's bitmap); the cells that check out are drawn from the
font's outlines instead (`src/texture.rs`, `src/payload.rs`). The client's
texture keeps its bitmaps: the proxy builds textures of its own beside
each glyph cache (`src/v2.rs`, `src/atlas.rs`):

- the atlas, one for every font and size: each glyph's outline as
  monotone quadratic pieces on a 16-bit grid over the font's box, contours
  that overlap united first, sorted into horizontal bands, one piece a
  texel (A32B32G32R32F);
- the column map: per cache column and row of cells, the glyph's record
  in the atlas and where its grid starts;
- the cover: a byte per cache texel, 0 empty and 255 full where no piece
  reaches in, else the texel's exact coverage after the gamma, or a marker
  for a cell the proxy did not take; an outlined font gets a second byte
  for the outline;
- the texel words: per cache texel, up to three runs of the glyph's
  pieces that can touch a pixel centred in it, with the winding left of
  them, so magnified text reads a few pieces instead of whole bands.

**Decoder.** While a glyph cache is on stage 0 and the client's vertex
shader is a vs_3_0, the device gets the decoder (`shaders/decoder.hlsl`,
ps_3_0) in place of the client's pixel shader, with the proxy's textures
on stages 1 to 4 and its constants in c200 to c203. A pixel on a pure
texel, or drawn 1:1, is done after the cover byte. Otherwise the decoder
integrates the glyph over the pixel's box from the pieces in reach, in
the same f32 arithmetic as the CPU's reference, and takes the distance to
them for an outline. A cell marked as not the proxy's draws the client's
bitmap.
The texture coordinate is interpolated at the centroid: with multisampling
(the client's `gxMultisample`) a pixel a glyph's quad only partly covers
is shaded at its centre, which can lie past the quad's edge, and the
coordinate carried there reads the next glyph's cell. The client's own
shader does just that, which shows as faint slivers of other glyphs next
to text; the decoder draws without them.
The swap happens in the state setters (`SetTexture`, `SetPixelShader`,
`SetVertexShader`), which compare against the state the device holds, so
draw calls pass through untouched (`src/phi.rs`).

**Disk cache.** Each cell's cover bytes and texel words go to
`Cache\wow-font-d3d9\`, one file per font file, size and settings, keyed
by a hash of the font's table directory and of the encoder's source; each
font's atlas records go to a file of their own, whatever the size. Any
change to the encoder's source starts the cache over. Records are
appended with an Adler-32 each; a file cut short by a crash is truncated to its last good
record on the next start (`src/cache.rs`, `src/record.rs`).

## Cost

In the game, from `WOW_FONT_D3D9_TRACE=1` on an RTX 5060 laptop GPU and a
Ryzen AI 5 340 under Wine 11.19 and DXVK 3.1.1: 628 five-second windows
of play at about 160 frames a second. Ranges are the middle half of the
windows unless said.

- GPU: text takes 0.52 ms of a frame at the median, 0.30 to 0.87 ms, and
  0.23 to 1.28 ms from the 5th to the 95th percentile. That is 11.5 % of
  the frame's GPU time (6.5 to 17.8 %). Text with a shadow is drawn
  twice: the client sends the same quads again in black.
- CPU, per frame: 8.5 us in the texture bind hook (7.6 to 10.5), for some
  18 runs of text.
- An upload of a glyph cache whose cells are known: one seen alone took
  0.44 ms, 34 us to read the font's cells from the client, 76 to clear
  the proxy's arrays, 206 to check and paste the cells, 112 to send the
  three textures, and 58 before that to compare the upload with the
  last. For a new cache, whose three textures are made then, the
  clearing takes 0.35 to 1 ms.
- The first time a font is seen at a size its cells are worked out on the
  game's thread: passes of 17 and 19 ms were seen, with an encoder slower
  than the table below has it and not timed in the game since. A font
  file's first use in a session is 2 to 4 ms more (the client's own four
  are loaded ahead, on a thread).
- Finding a font: one read, 4 us, at an upload whose owner is known; a
  walk of the client's fonts, 50 us, when it is not. The first in a
  session used to search the client's image for 7 to 12 ms; it now looks
  where 12340 has the list first, which has not been timed in the game.

In the bench on the same GPU (`nix run .#bench`, Roboto Bold at 32 px
with and without an outline and at 14 px): 0.15 ns a pixel for magnified
text (1.5x, 2x) and 0.51 ns for minified (0.5x, 0.75x), where every
pixel walks the bands. A glyph minified to half covers a sixteenth of
the pixels it covers at 2x.

The encoder on an x86_64 host, one thread, per glyph and per cell (the
DLL is 32-bit and has not been timed):

| font | a glyph's record, once | from the disk | a cell: cover and words |
|---|---|---|---|
| FRIZQT 12 px | 9.8 us | 0.4 us | 6.3 us |
| FRIZQT 32 px | 9.9 us | 0.4 us | 14.8 us |
| FRIZQT 32 px, outline 2 | 10.0 us | 0.4 us | 41.3 us |
| LXGW WenKai 16 px, outline 1 | 42 us | 1.4 us | 68.1 us |
| SKURRI 20 px, outline 1 | 76 us | 2.8 us | 102.9 us |

Accuracy against an exact reference (the glyph's area in each pixel's
box under the nonzero rule, from chords within 1/256 of the box) on the
gate's fonts, Roboto at 32 px and Roboto Bold at 20 px with an outline
and Roboto at 14 px without, in white and in black: texels off by more
than 16/255 are 0.82 % at 0.5x, none at 1x and 2x, 0.001 % at 4x
(`nix run .#gate`, Mesa lavapipe). Contours that overlap, as Roboto's
do, count once. The reference takes an outline's alpha as the decoder
defines it, a ramp over the distance. Against the true area of the
widened glyph that ramp is off by more than 16/255 on 13 to 15 % of an
outlined font's pixels at 0.5x, 0.6 to 1.2 % at 1x and 0.2 % or less
from 2x up (radius 1.25, the decoder's CPU model).

## Building

With Nix:

    nix build            # result/d3d9.dll
    nix build .#release  # the release zip: DLL, sample ini, README, LICENSE
    nix flake check      # build, tests, shader bytecode, the DLL's imports and exports, the zip
    nix develop          # cargo build --release lands in target/i686-pc-windows-gnu/release

Releases come from a tag `vX.Y.Z` (`.github/workflows/release.yml`),
which builds the zip twice and publishes it with its SHA-256. The build
is reproducible: `nix build github:mightsleep/wow-font-d3d9/vX.Y.Z#release`
gives the same bytes.

The DLL is built with stable Rust for `i686-pc-windows-gnu` and linked by
nixpkgs' mingw-w64 gcc against the runtime libraries Rust ships for that
target. It imports only system DLLs.

Without Nix on Linux:

    rustup target add i686-pc-windows-gnu
    cargo build --release --target i686-pc-windows-gnu

with an `i686-w64-mingw32-gcc` as the linker
(`CARGO_TARGET_I686_PC_WINDOWS_GNU_LINKER`). A gcc built with the
mcfgthreads thread model (nixpkgs', some distributions') makes the DLL
depend on `libmcfgthread-1.dll`; put the `rust-mingw` component's
`lib/rustlib/i686-pc-windows-gnu/lib/self-contained` first with
`RUSTFLAGS=-L native=...` to avoid that.

On Windows with Visual Studio (untested):

    rustup target add i686-pc-windows-msvc
    cargo build --release --target i686-pc-windows-msvc

The shaders are compiled with vkd3d 2.1 and committed as bytecode
(`shaders/*.ps3`, `*.vs3`); after changing an `.hlsl`, `nix run .#shaders`
writes them again and `nix flake check` fails until they match.

## Tests and bench

`tests/` is a crate of its own that runs on the host: `cargo test-host`
from the repository's root (an alias in `.cargo/config.toml` for
cargo-nextest, which the dev shell has; `WOW_FONT_TEST_FONTS`, font files
with `:` between, adds the texel words' checks on real fonts). It checks the encoder's
invariants against brute force (area under the nonzero rule; the atlas
records through a CPU copy of the decoder, `tests/reference.rs`, the
cover bytes and the texel words against the exact coverage) and
that the disk cache's records read back what was written and survive a
cut or a damaged byte.

`nix run .#bench -- --font FILE:PX:OUTLINE [--lavapipe]` draws glyph
caches built by the proxy's encoder through Direct3D 9 under Wine with
DXVK, the road the game's text takes, and compares every pixel with the
exact reference. `--ps NAME=FILE` adds a compiled shader to compare;
`--lavapipe` runs on the CPU in a virtual X server. See
`examples/bench.rs` for the rest of its arguments. `nix run .#gate` is
the same as a test, the one CI runs: Roboto with and without an outline
on lavapipe, failing when a scale has more texels off by over 16/255
than its limit in `flake.nix`. The bench lays glyphs out by the font's
ascender, not as the client does, so descenders fall off its cells and go
untested.

`nix run .#asm` needs no GPU: for `shaders/decoder.hlsl` (or the `.hlsl`/`.ps3` files named) it prints the instructions, texture
fetches, flow control, temporaries (vkd3d allows ps_3_0 32) and size;
`--dump` adds the disassembly. A first check of a change before the bench:
the counts are static (a loop counts once), and the decoder waits on
fetches more than on arithmetic, so fewer instructions need not be faster.
