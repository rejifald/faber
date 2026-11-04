---
title: Every surface colours from one token source
type: adr
status: accepted
date: 2026-10-18
owner: rejifald
---

# ADR-0037: Every surface colours from one token source

## Status

accepted (2026-10-18). Built: the token file is `desktop/src/tokens.css`. The popover and the
panel link it from their HTML; the daemon embeds it with `include_str!` (via `concat!`) at the
top of each served page's `<style>` block, so no copy exists to drift. The dashboard's change
is visible and got a design review on its PR.

## Context

faber has three surfaces with a user interface: the tray popover
(`desktop/src/popover.css`), the full panel (`desktop/src/styles.css`) and the web
dashboard (inline in `src/web.rs`, with its sign-in and standby pages in
`src/control.rs` and `src/standby.rs`). The measurement in
[`docs/design/drift-2026-10-18.md`](../design/drift-2026-10-18.md) found:

- **The popover and the panel have one palette, copied by hand.** Their 21 tokens go by
  two naming schemes (`--f-surface2` / `--surface-2`), and 19 of them have identical
  values in both themes. Only the shadow differs, and it differs on purpose: the popover
  floats. Any change to one copy has to be repeated in the other, by hand.
- **The web dashboard has no tokens.** It uses 20 raw colours over 74 declarations, in a
  GitHub-dark palette that is dark only. 11 of those colours are within ΔE 5 (OKLab ×100) of
  a panel dark token, so its dark look is already nearly faber's. The ones that are not are
  a blue link colour where faber's accent is amber, a green primary button, and tinted
  status fills.
- The owner fixed the popover's palette and amber accent on 2026-10-17
  ([`docs/design/popover/anatomy.md`](../design/popover/anatomy.md)).

The options were rendered side by side on live daemon state: the dashboard as it is
today, the dashboard on the faber palette in light, and the same in dark. The popover and
the panel were shown beside them, unchanged by every option.

## Decision

1. **One token file defines faber's colours,** in a light block on `:root` and a dark block
   under `prefers-color-scheme: dark`, and every surface reads it. The popover and the
   panel stop carrying their own copies. The daemon embeds the same file into the pages it
   serves, so the dashboard, sign-in and standby pages use it too.
2. **The token names are semantic roles,** one name per role across all surfaces. Merging
   the two current schemes into one is a rename. It changes no values.
3. **The web dashboard adopts the faber palette and follows the system theme,** light or
   dark, like the app. Its GitHub-dark literals go away. Links and its primary action take
   the amber accent, and its status tints use the `*-soft` roles.
4. **A surface-specific value is a role, not a fork.** The popover's floating shadow is a
   second shadow role in the same file. It is not a second palette.
5. **Enforcement:** `tests/design_tokens.rs` gains the shared file as the only place a
   colour literal may live, and scans the HTML that `src/web.rs`, `src/control.rs` and
   `src/standby.rs` embed.

## Consequences

- One edit changes a colour everywhere, and the two desktop copies can no longer drift.
- The dashboard gains a light mode. That is new UI, given a design review on its
  PR with before and after captures of both themes.
- The dashboard's dark look changes slightly: amber where it showed blue, the faber greys
  in place of GitHub's (11 colours move by less than ΔE 5), and an amber primary in place of
  green.
- Where the file lives, and how the Tauri bundle and the Rust binary both include it, is
  an implementation choice for the PR that builds this. The file is a single source either
  way, and is never copied by hand.

## Considered and not adopted

- **One source, dashboard dark only.** Less visible change, but the dashboard would ignore
  the OS theme while the app beside it follows it.
- **Two sources: the desktop shares one, and the dashboard keeps its GitHub-dark values as
  tokens.** Today's look stays, but the two palettes keep drifting, which is the problem
  this ADR exists to end.
