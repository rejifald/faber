---
title: Every control sizes from one ladder
type: adr
status: accepted
date: 2026-10-18
owner: rejifald
---

# ADR-0039: Every control sizes from one ladder

## Status

accepted (2026-10-18). Built in `ui/` by #280; the vanilla surfaces are pending. It uses the token source of
[ADR-0037](0037-every-surface-colours-from-one-token-source.md) and the scales of
[ADR-0038](0038-spacing-radius-and-type-come-from-named-scales.md), and lands after both.

## Context

Every control in faber sets its own height, through its own padding, font and line height
([`docs/design/drift-2026-10-18.md`](../design/drift-2026-10-18.md) § Controls). Measured on
live state, they cluster around three heights that nothing names:

- **About 20 px:** the popover's attention actions (Release, Clear) and the panel's pin
  buttons (20.5).
- **About 26 px:** the popover's ⋯ button and the panel's tabs.
- **33 to 35 px:** Pause and Reconcile in the popover (35) and the panel (33), and the
  dashboard's page actions (35). The dashboard's tabs are 39.

The same primary button is 9 × 12 px in the popover and 8 × 12 px in the panel, so the two
never quite match.

Two ladders were rendered on the real popover, panel and dashboard beside today's
controls: 20 / 26 / 32 (today's rhythm, made exact), and 19 / 22 / 28 (AppKit's small,
regular and large control heights).

## Decision

1. **Every control has one of three sizes,** and each step sets height, side padding, label
   size, icon size, icon-to-label gap and radius together:

   | Step | Height | Side padding | Label | Icon | Gap | Radius |
   | --- | ---: | --- | --- | ---: | --- | --- |
   | `sm` | 20 | `--space-sm` (8) | `--text-caption` (11) | 12 | `--space-2xs` (4) | `--radius-sm` |
   | `md` | 26 | `--space-md` (10) | `--text-small` (12) | 14 | `--space-xs` (6) | `--radius-sm` |
   | `lg` | 32 | `--space-lg` (12) | `--text-body` (13) | 16 | `--space-xs` (6) | `--radius-md` |

   Labels stay at weight 600, as today. A **square** variant of any step (an icon-only
   button) is as wide as it is tall, with no side padding.

   *Amended 2026-10-18 (owner, #283):* the height is a **minimum height**, and it and the icon
   (and a checkbox's box) **scale with the label**: in the React theme both are set in `em` of
   the step's label, `calc(20em / 11)` and `calc(12em / 11)` at `sm`. At 100 % every value is
   the table's. Enlarged text grows the control instead of crowding it, and a square stays as
   tall as a labelled control of its step.

2. **Which step each control takes:**
   - `sm`: inline actions on a row (the attention list's Release and Clear, the panel's
     pin buttons) and chips.
   - `md`: menu items, the ⋯ button (square), tabs, text inputs and selects.
   - `lg`: a surface's own actions: Pause/Resume, Reconcile (square), and the dashboard's
     page actions.

   *Amended 2026-10-18 (owner, #280):* a checkbox takes `size` too, default `md`, in a
   **boxed** shape (`boxed` in `ui/src/kit/ladder.ts`): the step's label size and
   icon-to-label gap, a box the size of the step's icon, and no height of its own, so the row
   is as tall as its label. At `md` that is a 12 px label, a 6 px gap and a 14 px box.
3. **This refines ADR-0038's use lines for controls:** an `sm` control's label is
   `--text-caption`, not `--text-small`. The ladder decides a control's label; the type
   scale's use column covers text that is not a control.
4. **The ladder lives in the token file,** as the per-step values plus one class per step
   (and the square modifier). Controls take a step class, and never set their own height,
   padding, font size, icon size or gap. **A missing size is a change to the ladder,** in
   a PR that supersedes this table. It is never a local value.
5. **Enforcement:** `tests/design_tokens.rs` fails on a rule outside the token file that
   sets `height`, `padding`, `font-size` or `gap` on a control selector (`button`, `input`,
   `select`, `.btn`, `-btn`, menu items, tabs) unless the line carries a reasoned
   `design-tokens: allow` marker.

## Consequences

- The popover's and panel's primary buttons become the same 32 px control, from 35 and 33.
- The dashboard's tabs drop from 39 to 26 px, and its actions from 35 to 32. That is the
  largest visible change, and it is reviewed with before and after captures.
- Group rows in the popover (about 35 px) are rows, not controls, and stay outside the
  ladder.

## Considered and not adopted

- **AppKit sizes (19 / 22 / 28):** native-matching and denser, but the dashboard's actions
  would have dropped to 28 px and its tabs to 22.
