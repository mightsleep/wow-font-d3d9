<h1 align="center">wow-font-d3d9</h1>
<p align="center">Vector text for WoW 3.3.5a. Experimental.</p>
<p align="center">
  <a href="#install">install</a> ·
  <a href="#will-it-get-me-banned">bans</a> ·
  <a href="#what-is-wrong-with-it">what is wrong with it</a> ·
  <a href="docs/internals.md">internals</a> ·
  <a href="docs/design.md">design</a>
</p>
<p align="center">
  <a href="https://github.com/mightsleep/wow-font-d3d9/releases/latest"><img alt="release" src="https://img.shields.io/github/v/release/mightsleep/wow-font-d3d9?style=for-the-badge&labelColor=313244&color=a6e3a1"></a>
  <a href="https://github.com/mightsleep/wow-font-d3d9/actions/workflows/ci.yml?query=branch%3Amain"><img alt="ci" src="https://img.shields.io/github/actions/workflow/status/mightsleep/wow-font-d3d9/ci.yml?branch=main&style=for-the-badge&labelColor=313244&label=ci"></a>
  <img alt="client 3.3.5a (12340)" src="https://img.shields.io/badge/client-3.3.5a%20(12340)-fab387?style=for-the-badge&labelColor=313244">
  <img alt="status experimental" src="https://img.shields.io/badge/status-experimental-f38ba8?style=for-the-badge&labelColor=313244">
  <a href="#will-it-get-me-banned"><img alt="bans 0 so far" src="https://img.shields.io/badge/bans-0%20so%20far-94e2d5?style=for-the-badge&labelColor=313244"></a>
  <img alt="license GPL-3.0-or-later" src="https://img.shields.io/badge/license-GPL--3.0--or--later-b4befe?style=for-the-badge&labelColor=313244">
</p>

A proxy `d3d9.dll` for the World of Warcraft 3.3.5a client (build 12340).
The client rasterises each glyph once into a small bitmap and draws text
from that, so text the UI or an addon scales up goes soft. The proxy
keeps each glyph's outline in textures of its own and swaps in a pixel
shader that works out how much of every pixel the outline covers. Edges
and corners stay sharp at any scale. The client still picks the fonts,
sizes, colours and layout; only how the letters are filled changes.

So this is a font rasteriser living inside a Direct3D 9 pixel shader, in
a client from 2010, written by one person. It is a first version and
behaves like one. I play with it daily all the same.

## Where it has run

- A local AzerothCore in a podman container, and Warmane, where I play
  with it daily: Linux, Wine 11.19, DXVK 3.1.1, an RTX 5060 laptop.
- Windows 10 with an AMD RX 480 (driver 31.0.21921.1000). One card is
  the whole Windows test matrix.
- Nowhere else. The MSVC build is untested, and any client build other
  than 12340 is unsupported: the proxy reads the client's structures at
  fixed offsets.

## Will it get me banned

I play with it on Warmane daily and have not been banned (yet), which
is a sample of one. I see no reason why it should happen:

- It patches no code, in Wow.exe, in the real `d3d9.dll` or anywhere. It
  forwards every call and hooks Direct3D objects by pointing them at a
  patched copy of their vtable. Nothing outside Direct3D 9 is hooked.
- It reads the client's font tables through `ReadProcessMemory` on its
  own process and writes nothing into the client's memory but those
  vtable pointers.
- It opens no sockets, and writes files only next to Wow.exe (a log, a
  cache, see below).

What it cannot hide is that it is a DLL in the game folder, so it shows
in the process's module list. Private servers run anti-cheat (Warden)
that may look at that list, and whether a server minds a font renderer
is the server's call. Your account, your risk. `WOW_FONT_D3D9=0` makes
it forward calls and do nothing else.

## Install

1. Download `d3d9.dll` from the
   [latest release](https://github.com/mightsleep/wow-font-d3d9/releases/latest).
2. Copy it next to `Wow.exe`.
3. Start the game. A `wow-font-d3d9.log` next to `Wow.exe` says the proxy
   loaded.

The proxy loads the real `d3d9.dll` from the system directory. If you
already have a `d3d9.dll` in the game folder (DXVK, ReShade), rename it,
say to `d3d9_dxvk.dll`, and point `WOW_FONT_D3D9_REAL` at it. On Windows,
start the game from a `.bat` next to `Wow.exe`:

    set WOW_FONT_D3D9_REAL=d3d9_dxvk.dll
    start Wow.exe

Under Wine, tell it to take the DLL from the game folder:

    WINEDLLOVERRIDES="d3d9=n,b" wine Wow.exe

(in Lutris: Runner options, DLL overrides, `d3d9` = `n,b`). The real
`d3d9.dll` is then the prefix's: wined3d, or DXVK if it is installed
there.

To remove it, delete `d3d9.dll`. The proxy's other files,
`wow-font-d3d9.log`, `wow-font-d3d9.ini` and `Cache\wow-font-d3d9\`, can
go too.

## Settings

`wow-font-d3d9.ini` next to `Wow.exe`, one `key = number` a line, `#`
comments. It is read again whenever it changes, with the game running,
so `v2 = 0` and back compares the proxy's text with the client's in
place.

| key       | default | meaning |
|-----------|---------|---------|
| `gamma`   | 1.5     | alpha = coverage^(1/gamma) for white text, 1 - (1 - coverage)^(1/gamma) for black (shadows, outlines), between by the colour's luma |
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

## What is wrong with it

Known, and not fixed:

- It costs GPU time: half a millisecond of a frame at the median in my
  sessions, 0.2 to 1.3 ms most of the time (RTX 5060 laptop GPU under
  DXVK), about an eighth of the frame. Text with a shadow pays twice,
  since the client draws the shadow as the same letters again in black.
- The first time a font shows up at a size, its cells are worked out on
  the game's thread: hitches of up to some 20 ms were seen, once. After
  that they come from a cache on disk, until the next update of the DLL
  changes the encoder and the cache starts over.
- An outline's alpha is a ramp over the distance to the glyph, which is
  the true area only for a straight edge wider than a pixel. Outlined
  text drawn at half size has about one pixel in seven off by more than
  16/255. At full size and above it is under 1.2 %.
- Black text drawn exactly 1:1 takes its coverage back out of a byte
  kept for white text, and loses up to 16/255 on some edge pixels.
- Monochrome (pixel) fonts, text drawn without a vs_3_0 vertex shader
  and fonts whose file the proxy cannot find stay the client's bitmaps.
- On Windows' own Direct3D 9 the decoder (about 1450 ps_3_0
  instructions) has run on that one AMD card. If a device refuses it,
  the log says why (lines starting `diag:`) and Microsoft's
  `d3dcompiler_47` compiles it instead, about 11 s at start; if that is
  refused too, text stays the client's.
- Other mods that redraw the game's text (Lexara, TweakWoW2's HD Font,
  awesome_wotlk's MSDF mode) take the same draws. Use one at a time;
  nothing has been tried together with them.

Unknown ones are likelier than not. Open an
[issue](https://github.com/mightsleep/wow-font-d3d9/issues) with
`wow-font-d3d9.log`, your OS, GPU and driver (and Wine and DXVK versions
on Linux), a screenshot, and the addons that draw the text in question.
A screenshot with `v2 = 0` beside one with `v2 = 1` shows whether the
problem is the proxy's.

## How it does it

1. **Whose glyphs.** The client keeps every font it makes in one list.
   The proxy finds the list in the client's memory and, whenever the
   client uploads a glyph cache, reads which font it belongs to and which
   glyph sits in which cell.
2. **Outlines.** It reads the font file itself (from the game folder or
   the client's MPQ archives), cuts each glyph into monotone quadratic
   pieces on a 16-bit grid, unites contours that overlap, and stores the
   pieces one a texel in a float texture.
3. **Per texel.** Beside each of the client's glyph caches it keeps a
   byte per texel (the exact coverage at 1:1) and a word naming the few
   pieces a magnified pixel there can touch.
4. **Per pixel.** While a glyph cache is bound, the device gets the
   proxy's pixel shader in place of the client's. Text drawn 1:1 reads
   the byte. Anything else integrates the outline over the pixel's box:
   Green's theorem, one root formula without cases, two-point Gauss,
   which is exact for the cubics that come up. No sampling.
5. **Once.** The bytes, words and outlines go to a cache on disk, so a
   session after the first only reads them.

The long version, with the costs, the build and the tests, is in
[docs/internals.md](docs/internals.md). The tricks, numbered, each with
why it holds and what it bought, are in [docs/design.md](docs/design.md).

## Building

    nix build            # result/d3d9.dll
    nix flake check      # build, lints, tests, shader bytecode, the DLL's imports and exports

Releases are built from a tag and are reproducible:
`nix build github:mightsleep/wow-font-d3d9/vX.Y.Z#release` gives the
same bytes as the download. Building without Nix, and the tests and the
bench, are in [docs/internals.md](docs/internals.md).

## Credits

The client's structure layouts follow [whoa](https://github.com/whoahq/whoa)
and [squall](https://github.com/whoahq/squall), corrected where a dump of
the client disagreed. The MPQ reader follows StormLib's and Ladislav
Zezula's descriptions of the format. Fonts are parsed by
[ttf-parser](https://github.com/harfbuzz/ttf-parser), MPQ sectors inflated
by [miniz_oxide](https://github.com/Frommi/miniz_oxide).

## License

Copyright 2026 mightsleep. GPL-3.0-or-later; see `LICENSE`.
