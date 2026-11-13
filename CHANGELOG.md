# Changes

What a player would notice, newest first. The release notes are cut from
here.

## 0.2.0

Replace `d3d9.dll` with the new one. The cache in `Cache\wow-font-d3d9\`
starts over once, so the first minutes after the update have the
first-time hitches again.

- Fonts drawn with overlapping strokes (Roboto, Morpheus and many addon
  fonts) are filled once where the strokes cross. Before, the crossings
  were counted twice and showed as spots at small sizes.
- Shadows, outlines and dark text get the alpha that suits dark on
  light, white text the one that suits light on dark, colours between
  by their luma. Before, everything got white's, and shadows and
  outlines came out thin. `gamma` in the ini still sets how strong this
  is.
- A font seen for the first time at a size costs less: its outlines are
  kept on disk as well, and working out a cell takes a fraction of what
  it did.
- The stall of 7 to 12 ms shortly after the game starts, while the
  proxy searched the client's memory for its font list, is gone on
  build 12340.
- `WOW_FONT_D3D9_TRACE=1` splits an upload's time into its parts.

Still wrong: everything under
[What is wrong with it](README.md#what-is-wrong-with-it).

## 0.1.0

The first release.
