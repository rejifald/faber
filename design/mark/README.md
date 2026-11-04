# The faber mark

A cube with its front-corner core cut free: the machine share faber holds, and the control at its
centre. The SVGs here are the masters; every app icon is rendered from them, and
`generate.mjs` writes the masters from the geometry below.

## Geometry

All on a 100-unit tile, in 2:1 isometric (edges step two across for one down, so they land on
whole pixels at small sizes).

| Part | Value |
|---|---|
| Tile, corner radius | 100, r 24 |
| Cube | edge 29, centred on (50,50); its outline is a hexagon from 21 to 79 |
| Core | the front-corner cube at half the edge, its three faces meeting at (50,50) |
| Moat between core and shell | 3 |
| Face tones | top lightest, left middle, right darkest; the core in the accent |

The one-colour glyph and the UI's kit mark (`ui/src/kit/Mark.tsx`) give the shell 0.38 / 0.55 /
0.24 opacity (top / left / right) and the core 0.78 / 1 / 0.56, so the core carries the weight
without colour. One colour is lit from the side, with the top in the middle step: the same alphas
are dark on a light background and bright on a dark one, so a top-lit cube would look lit from
below on light.

The menu-bar glyph is drawn in code (`desktop/src-tauri/src/lib.rs`, `draw_mark`) from the same
geometry and opacities, scaled so the cube's edge is exactly 16px, with the moat opened to 3.6 so
it stays open at menu-bar size. Change all of them together.

## Files

| File | Use |
|---|---|
| `faber-mark-forge.svg` | Primary colourway, full bleed, with the tile gradient and rim light |
| `faber-mark-ink.svg`, `faber-mark-porcelain.svg` | Dark and light alternatives |
| `faber-mark-forge-flat.svg` | Forge without gradients |
| `faber-app-icon-macos.svg` | Forge inside Apple's canvas margin (tile at 824 of 1024) |
| `faber-glyph-mono.svg` | One colour, no tile, cropped to the mark |

## Rendering

```sh
node design/mark/generate.mjs design/mark   # the SVG masters
design/mark/render-icons.sh                 # the bundle icons, from the masters
```

`render-icons.sh` rewrites every file in `desktop/src-tauri/icons/` at its existing size. It needs
macOS (AppKit renders the SVGs, `iconutil` packs the `.icns`) and node.
