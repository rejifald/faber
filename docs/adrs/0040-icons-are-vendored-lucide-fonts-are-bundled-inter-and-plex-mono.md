---
title: Icons are vendored Lucide; fonts are bundled Inter and IBM Plex Mono
type: adr
status: accepted
date: 2026-10-18
owner: rejifald
---

# ADR-0040: Icons are vendored Lucide; fonts are bundled Inter and IBM Plex Mono

## Status

accepted (2026-10-18). Built in `ui/` by #280; the vanilla surfaces are pending. The font files and the icon sprite live beside the
token source of [ADR-0037](0037-every-surface-colours-from-one-token-source.md).

## Context

**Icons.** The popover has an in-house SVG sprite of 11 glyphs (`desktop/src/popover.html`,
`#g-*`), which mixes fills and strokes of 2 to 2.6 px. The panel repeats the pause and play
glyphs inline in `main.js`. The dashboard uses Unicode (⚠ ⏸ ▶ ↻ ⟳ ✓ ●). faber's own domain
needs at least 13 more glyphs that no surface has: container, CPU, memory, disk, host, VM,
trust, hold, pull request, branch, clock, update and pin.

Four sets were checked against those 24 glyphs. Lucide, Tabler and Phosphor cover all of
them; Heroicons misses four (snowflake, pull request, branch, Apple). None is of Russian
origin: Lucide's maintainers are in the Netherlands, Denmark and the UK, Phosphor is in
the US, and Tabler is in Poland. Lucide and Phosphor were rendered on the real popover,
panel and dashboard, with a sheet of all 24 glyphs at 14, 16 and 24 px beside today's
sprite.

**Fonts.** The desktop's stacks start with the system UI font (SF Pro and SF Mono on
macOS), and name Inter and JetBrains Mono only as fallbacks. Nothing is bundled. JetBrains
Mono comes from a company founded by Russian developers. The owner's conventions bar
Russian-origin dependencies, and a named fallback is close enough to one to drop it. The dashboard is set entirely in
monospace. Three options were rendered: today, system fonts only with a sans dashboard,
and bundled Inter with IBM Plex Mono.

## Decision

1. **Icons come from Lucide** (`lucide-static` 1.59.0, ISC), **vendored:** the SVGs
   faber uses are copied into one sprite beside the token file, with the ISC licence and
   the Lucide version recorded in a comment. Lucide is not an npm dependency of the app or
   the daemon. All three surfaces draw from that one sprite. The panel's inline copies and
   the dashboard's Unicode glyphs go away.
2. **faber's own marks stay in-house:** the aperture logo and the offline glyph (the
   aperture with a slash) are drawn by faber and sit in the same sprite.
3. **Icon sizes come from the control ladder** ([ADR-0039](0039-every-control-sizes-from-one-ladder.md):
   12, 14, 16 px). Outside a control, an icon takes the size of the step its text matches.
   The stroke stays Lucide's 2 px.
4. **A new icon is a Lucide glyph added to the sprite.** One Lucide does not have is drawn in
   its grid and stroke, in a reviewed PR, and is marked as faber's own.
5. **Fonts are bundled:** Inter (OFL 1.1, Rasmus Andersson, Sweden) for `--sans`, and IBM
   Plex Mono (OFL 1.1, IBM, US) for `--mono`, as WOFF2 files subset to Latin. They ship in the
   Tauri bundle and are embedded in the daemon binary for the pages it serves, so every OS
   shows the same faces. Weights: Inter 400, 500, 600 and 700; Plex Mono 400, 500 and 600.
6. **The fallback stacks name no other bundled or named face:** `--sans` is `"Inter",
   system-ui, sans-serif` and `--mono` is `"IBM Plex Mono", ui-monospace, monospace`.
   JetBrains Mono, SF Pro Text and Menlo are dropped by name.
7. **The dashboard moves to the sans,** with mono kept for code, commands, versions, IDs and
   values that align.

## Consequences

- The popover and the panel change from SF Pro to Inter on macOS. That is a visible
  change, reviewed with before and after captures.
- The app bundle and the daemon binary each grow by the subset font files, a few hundred
  KB. The release assets are faber's one public artifact (ADR-0031), and OFL allows
  redistributing the fonts there.
- A future Lucide update is a deliberate PR that re-copies the glyphs faber uses. Nothing
  updates on its own.

## Considered and not adopted

- **Phosphor:** it covers every glyph and offers six weights, but its 1.5 px regular weight
  looks lighter than the popover's current icons, and the package was last published in
  2024.
- **Tabler:** close to Lucide in style, with more icons. Lucide was chosen as the nearer
  match to today's sprite.
- **Heroicons:** misses four of faber's glyphs.
- **Keeping the in-house sprite:** every one of the 13 missing glyphs would be drawn by
  hand.
- **System fonts only:** nothing to bundle, but each OS would render different faces.
- **A monospace dashboard:** not chosen. The bundled option was rendered with a sans
  dashboard and mono for code and values.
