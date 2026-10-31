# wow-font-d3d9

Vector text for WoW 3.3.5a.

A proxy `d3d9.dll` for the World of Warcraft 3.3.5a client (build 12340)
that draws the game's text from vector outlines. The client rasterises
each glyph with FreeType into a 256x256 texture (a glyph cache) and draws
text from there; the proxy keeps each glyph's outline in textures of its
own beside the cache and swaps in a pixel shader that integrates the
outline over every pixel. Text magnified by the UI or by addons stays
sharp at the edges and corners, where the client's own bitmap goes soft.

This is a one-person project at its first version. It has been played on
Linux with Wine 11.19 and DXVK 3.1.1, and run on Windows 10 with an
AMD RX 480 (driver 31.0.21921.1000); other GPUs and drivers on Windows
are untested, as is the MSVC build. The client's structures are read at fixed offsets, so
any build other than 12340 is unsupported.

## What it does, and what it does not

- It forwards every call to the real `d3d9.dll` and hooks objects by
  pointing them at a patched copy of their vtable. No code in Wow.exe, in
  the real `d3d9.dll` or anywhere else is patched, and nothing outside
  Direct3D 9 is hooked.
- It reads the client's font headers (CGxFont) and glyph lists through
  `ReadProcessMemory` on its own process, where a wrong address fails the
  read and nothing else, and the stack of the thread that calls its
  `LockRect` hook. It writes nothing into the client's memory but the
  vtable pointers of the objects it hooks; the glyph caches keep the
  client's bitmaps.
- It writes files only next to Wow.exe: `wow-font-d3d9.log`, encoded cells
  in `Cache\wow-font-d3d9\`. It reads `wow-font-d3d9.ini` there if present.
- It opens no sockets.
- Like any DLL in the game folder, it shows in the process's module list.
  Private servers run anti-cheat (Warden) that may look at loaded modules;
  whether one minds a font renderer is up to the server. Use it at your
  own risk.

Set `WOW_FONT_D3D9=0` and it only forwards.

## Install

Copy `d3d9.dll` next to `Wow.exe`.

The proxy loads the real `d3d9.dll` from the system directory. If you
already have a `d3d9.dll` in the game folder (DXVK, ReShade), rename it and
point `WOW_FONT_D3D9_REAL` at it.

Under Wine, tell it to take the DLL from the game folder:

    WINEDLLOVERRIDES="d3d9=n,b" wine Wow.exe

The real `d3d9.dll` is then the prefix's: wined3d, or DXVK if it is
installed there.

## Settings

`wow-font-d3d9.ini` next to `Wow.exe`, one `key = number` a line, `#`
comments. It is read again whenever it changes, with the game running.

| key       | default | meaning |
|-----------|---------|---------|
| `gamma`   | 1.5     | alpha = coverage^(1/gamma) |
| `outline` | 0.25    | pixels added to the client's outline radius |
| `v2`      | 1       | 0: the client's bitmaps for every font, as without the proxy |
| `words`   | 1       | 0: magnified text reads the bands, not the texel words; 2: the words' path in colour |

Environment variables:

| variable               | meaning |
|------------------------|---------|
| `WOW_FONT_D3D9`        | `0`: forward only |
| `WOW_FONT_D3D9_REAL`   | path of the real `d3d9.dll` |
| `WOW_FONT_D3D9_FONTS`  | one more folder to look for font files in |
| `WOW_FONT_D3D9_TRACE`  | `1`: CPU and GPU times to the log every 5 s |

## How it works

The tricks, with the reasoning and the measurements behind them, are in
[docs/design.md](docs/design.md).

**Which font.** While the client fills a glyph cache it has the cache's
TEXTURECACHE on its stack, a few frames above the proxy's `LockRect` hook.
The proxy finds it there (at depths it remembers from session to session)
and follows it to the CGxFont header: font name, pixel size, outline
flags, and the list of glyphs in each cell (`src/fonts.rs`,
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
  monotone quadratic pieces on a 16-bit grid over the font's box, sorted
  into horizontal bands, one piece a texel (A32B32G32R32F);
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
by a hash of the font's table directory and of the encoder's source; the
atlas records are cheap enough to encode again each session. Records are appended with an
Adler-32 each; a file cut short by a crash is truncated to its last good
record on the next start (`src/cache.rs`, `src/record.rs`).

## Limits

- Monochrome (pixel) fonts stay the client's bitmaps.
- On Windows' own Direct3D 9 the decoder (about 1500 ps_3_0 instructions)
  has run on one AMD GPU only. If a device refuses it, the log says why
  (lines starting `diag:`) and Microsoft's `d3dcompiler_47` compiles it
  instead (about 11 s at start); if that is refused too, text stays the
  client's.
- Text drawn without a vs_3_0 vertex shader stays the client's.
- A font whose file the proxy cannot find stays the client's.
- Build 12340 only, as above.

## Cost

Measured with `WOW_FONT_D3D9_TRACE=1` on an RTX 5060 laptop GPU and a
Ryzen AI 5 340, Wine 11.19 and DXVK 3.1.1, in towns and menus:

- GPU: 0.14 to 0.57 ms of text per frame, 1.6 to 11.5 % of the frame;
  with Chinese text on screen, 0.87 ms of a 3.2 ms frame.
- CPU, per frame: 3 to 8 us in the texture bind hook.
- A font's first upload with nothing on disk: up to 17.5 ms (48 Chinese
  and 109 Latin cells, and the atlas growing). A 32 px cell's cover and
  texel words take 0.1 to 0.4 ms of CPU; from disk they are only read.
- Finding a font: 2 to 60 us per upload; the first in a session, up to
  0.35 ms.

Accuracy against an exact reference (the glyph's area in each pixel's
box under the nonzero rule, from chords within 1/256 of the box),
Roboto at 32 px with an outline, texels off by more than 16/255: 0 % at
1x, 0.061 % at 2x, 0.18 % at 4x (`nix run .#bench`, Mesa lavapipe).

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
from the repository's root, in the dev shell or out of it (an alias in
`.cargo/config.toml`; `WOW_FONT_TEST_FONTS`, font files with `:` between,
adds the texel words' checks on real fonts). It checks the encoder's
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
`examples/bench.rs` for the rest of its arguments.

`nix run .#asm` needs no GPU: for `shaders/decoder.hlsl` (or the `.hlsl`/`.ps3` files named) it prints the instructions, texture
fetches, flow control, temporaries (vkd3d allows ps_3_0 32) and size;
`--dump` adds the disassembly. A first check of a change before the bench:
the counts are static (a loop counts once), and the decoder waits on
fetches more than on arithmetic, so fewer instructions need not be faster.

## Credits

The client's structure layouts follow [whoa](https://github.com/whoahq/whoa)
and [squall](https://github.com/whoahq/squall), corrected where a dump of
the client disagreed. The MPQ reader follows StormLib's and Ladislav
Zezula's descriptions of the format. Fonts are parsed by
[ttf-parser](https://github.com/harfbuzz/ttf-parser), MPQ sectors inflated
by [miniz_oxide](https://github.com/Frommi/miniz_oxide).

## License

Copyright 2026 mightsleep. GPL-3.0-or-later; see `LICENSE`.
