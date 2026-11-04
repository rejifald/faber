---
title: Spacing, radius and type come from named scales
type: adr
status: accepted
date: 2026-10-18
owner: rejifald
---

# ADR-0038: Spacing, radius and type come from named scales

## Status

accepted (2026-10-18). Built in `ui/` by #280; the vanilla surfaces are pending. It builds on the single token source of
[ADR-0037](0037-every-surface-colours-from-one-token-source.md). *Amended 2026-10-18 (#280):*
the scales live in the Tailwind theme, `ui/src/theme/theme.css`, not in that file;
`ui/scripts/theme.test.mjs` holds them to the tables below. The colours stay in
`desktop/src/tokens.css`, which the theme imports.

## Context

[`docs/design/drift-2026-10-18.md`](../design/drift-2026-10-18.md) measured faber's three
surfaces (popover, panel, web dashboard):

- **Spacing:** 25, 20 and 12 distinct px values. Only 37 to 59 % fall on a 4 px grid, and
  61 to 95 % on a 2 px grid. 6 is the second-most-used value on every surface.
- **Radius:** 13 distinct values across the surfaces (4, 5, 6, 7, 8, 9, 10, 12, 13, 14, 15,
  16, and full).
- **Type:** 16 distinct sizes, including half pixels (9.5, 10.5, 11.5, 12.5), with 9 sizes
  below 11 px in the panel. There is no declared text floor.

Two candidate scales per dimension were rendered on the real popover, panel and dashboard,
side by side with today's look. Every value was snapped to the nearest step, a tie going to
the larger one.

## Decision

1. **Spacing** (margin, padding, gap) uses ten steps: 2, 4, 6, 8, 10, 12, 16, 20, 24, 32 px.
   That is a 4 px grid that keeps 6 and 10, the values the dense menu-style rows depend on.

   | Token | px | Use |
   | --- | ---: | --- |
   | `--space-3xs` | 2 | hairline gaps: pips, stacked meta lines |
   | `--space-2xs` | 4 | tight pairs: a chip's vertical padding, a dot beside its label |
   | `--space-xs` | 6 | the icon-to-label gap in a control, a menu row's vertical padding |
   | `--space-sm` | 8 | the default gap between items in a row |
   | `--space-md` | 10 | a control's vertical padding, the gap between a card's lines |
   | `--space-lg` | 12 | a control's side padding, the gap between stacked rows |
   | `--space-xl` | 16 | a surface's side padding, a card's padding |
   | `--space-2xl` | 20 | the gap between sections |
   | `--space-3xl` | 24 | a page's padding (dashboard) |
   | `--space-4xl` | 32 | a major break: header clearance below the traffic lights |

2. **Radius** has four steps and full, one per role:

   | Token | px | Use |
   | --- | ---: | --- |
   | `--radius-xs` | 4 | small marks: checkboxes, tiny badges |
   | `--radius-sm` | 6 | items: menu rows, chips, inputs, small buttons |
   | `--radius-md` | 10 | controls and cards |
   | `--radius-lg` | 14 | containers: the popover card, panels |
   | `--radius-full` | 999 | pills and dots |

   *Amended 2026-10-18 (owner, #280):* `--radius-xs` (4) added, per § 5. At 6 px a 14 px
   checkbox read as a radio button; the owner chose 4 from a render at 6, 4, 3 and 2.

3. **Type** has five steps and an **11 px floor.** No text is smaller than `--text-caption`,
   and there are no half-pixel sizes:

   | Token | px | Use |
   | --- | ---: | --- |
   | `--text-caption` | 11 | captions, meta lines, ring labels |
   | `--text-small` | 12 | secondary text, small buttons |
   | `--text-body` | 13 | body text, controls, menu items (the macOS menu size) |
   | `--text-title` | 15 | section and card titles |
   | `--text-display` | 20 | the one large figure or heading on a surface |

   `--sans` and `--mono` use the same steps. The dashboard's 14 px monospace body moves
   to `--text-body`.

4. **Values are snapped by use, not by distance.** The previews snapped to the nearest
   step. The build reads each value's role from the tables above, so a 7 px gap between an
   icon and its label becomes `--space-xs` because that is its role, even where 8 is closer.
5. **A missing step is a change to the scale,** in a reviewed PR that amends this ADR's
   table. It is never a local value.
6. **Enforcement:** `tests/design_tokens.rs` extends from colour to these properties. A raw
   px value in `margin`, `padding`, `gap`, `border-radius` or `font-size` outside the
   token file fails unless it carries a reasoned `design-tokens: allow` marker. Layout
   sizes (widths, heights, offsets) stay raw until the control ladder decides them.

## Consequences

- The panel's 9 to 10.5 px meta text grows to 11 px. That is the largest visible change, and
  the reason for the floor.
- Dense rows keep their rhythm, because 6 and 10 survive. The popover's menu-native density
  ([`popover/anatomy.md`](../design/popover/anatomy.md)) is unchanged.
- The build touches every rule in the two desktop stylesheets and the dashboard, so it
  lands after ADR-0037's token file, in its own PR with before and after captures and a
  design review.

## Considered and not adopted

- **A plain 4 px grid (2, 4, 8, 12, 16, 24, 32):** fewer steps, but 6 → 8 and 10 → 12 made
  the rows and the menu visibly airier.
- **Radius on a 4 px grid (4, 8, 12, 16):** two neighbouring steps would have ended up
  serving the same role.
- **macOS text styles (10, 11, 12, 13, 15, 17, 22):** native-feeling, but they keep 10 px
  text and seven steps.
