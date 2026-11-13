# Changes

What a player would notice, newest first. The release notes are cut from
here.

## 0.2.0

Replace `d3d9.dll` with the new one. The cache in `Cache\wow-font-d3d9\`
starts over once, so the first minutes after the update have the
first-time hitches again.

- Fonts built from overlapping parts (Roboto, a few glyphs of Morpheus)
  are filled once where the parts overlap. Before, both parts were added
  up there, and scaled text had wrong pixels along the edges inside the
  overlap: on Roboto about one text pixel in 36 at half size, now one in
  340 by the same measure. Text at 1:1 was right already.
- Alpha follows the colour drawn: black text, shadows and outlines get
  the alpha that is right for dark over light, white text the one for
  light over dark, colours between by their luma. Before, everything
  got white's, which is too heavy for black: a shadow's edge at half
  coverage had alpha 0.63 where 0.37 is right. Shadows and outlines are
  lighter and sharper for it. `gamma` in the ini still sets how strong
  this is.
- A glyph's outline is kept on disk too, not worked out again every
  session, and a cell's bytes take a fraction of the time they did
  (measured outside the game). Uniting the parts costs some of
  that back the first time a font is seen.
- The stall of 7 to 12 ms shortly after the game starts, while the
  proxy searched the client's memory for its font list, is gone on
  build 12340.
- `WOW_FONT_D3D9_TRACE=1` splits an upload's time into its parts.
- A release now waits for Windows Server 2022 and 2025 to load the DLL
  and take its shader. They have no GPU and look at no pixel.

Still wrong: everything under
[What is wrong with it](README.md#what-is-wrong-with-it).

## 0.1.0

The first release.
